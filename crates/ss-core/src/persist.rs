//! 索引磁盘缓存（自定义二进制：长度前缀 + 原始字节块，快速读写）。
//!
//! FRN→id 映射以两个并行数组（frns / ids）落盘，加载时重建 HashMap。

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::Path;

use crate::index::Index;

const MAGIC: &[u8; 8] = b"SSIDX02\0";

fn write_vec<T: Copy>(w: &mut impl Write, v: &[T]) -> io::Result<()> {
    w.write_all(&(v.len() as u64).to_le_bytes())?;
    let bytes =
        unsafe { std::slice::from_raw_parts(v.as_ptr() as *const u8, std::mem::size_of_val(v)) };
    w.write_all(bytes)
}

fn bad(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

fn read_vec<T: Copy>(r: &mut impl Read, file_len: u64) -> io::Result<Vec<T>> {
    let mut lenb = [0u8; 8];
    r.read_exact(&mut lenb)?;
    let n = u64::from_le_bytes(lenb) as usize;
    // 防御：声明的字节数不可能超过文件总长，拦住损坏头导致的超大分配
    if (n as u64).saturating_mul(std::mem::size_of::<T>() as u64) > file_len {
        return Err(bad("vec too large"));
    }
    let mut v: Vec<T> = Vec::with_capacity(n);
    unsafe {
        let bytes =
            std::slice::from_raw_parts_mut(v.as_mut_ptr() as *mut u8, n * std::mem::size_of::<T>());
        r.read_exact(bytes)?;
        v.set_len(n);
    }
    Ok(v)
}

fn read_u64(r: &mut impl Read) -> io::Result<u64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(u64::from_le_bytes(b))
}
fn read_i64(r: &mut impl Read) -> io::Result<i64> {
    Ok(read_u64(r)? as i64)
}

pub fn save_index(idx: &Index, path: &Path) -> io::Result<()> {
    // 写临时文件后原子改名，避免半截损坏
    let tmp = path.with_extension("tmp");
    {
        let f = File::create(&tmp)?;
        let mut w = BufWriter::new(f);
        w.write_all(MAGIC)?;
        w.write_all(&[idx.drive_letter])?;
        w.write_all(&idx.volume_serial.to_le_bytes())?;
        w.write_all(&idx.usn_journal_id.to_le_bytes())?;
        w.write_all(&idx.next_usn.to_le_bytes())?;
        write_vec(&mut w, &idx.names_off)?;
        write_vec(&mut w, &idx.names_len)?;
        write_vec(&mut w, &idx.parent)?;
        write_vec(&mut w, &idx.flags)?;
        write_vec(&mut w, &idx.name_pool)?;
        let mut frns = Vec::with_capacity(idx.frn_to_id.len());
        let mut ids = Vec::with_capacity(idx.frn_to_id.len());
        for (k, v) in &idx.frn_to_id {
            frns.push(*k);
            ids.push(*v);
        }
        write_vec(&mut w, &frns)?;
        write_vec(&mut w, &ids)?;
        w.flush()?;
        w.get_ref().sync_all()?; // fsync：flush 只刷用户缓冲，sync_all 才落盘
    }
    // rename 遇杀软/索引器瞬时占用做短重试（与 ss-config 的原子写一致）
    let mut last = None;
    for i in 0..3u64 {
        match std::fs::rename(&tmp, path) {
            Ok(()) => return Ok(()),
            Err(e) => {
                last = Some(e);
                std::thread::sleep(std::time::Duration::from_millis(20 * (i + 1)));
            }
        }
    }
    let _ = std::fs::remove_file(&tmp);
    Err(last.unwrap())
}

pub fn load_index(path: &Path) -> io::Result<Index> {
    let f = File::open(path)?;
    let file_len = f.metadata()?.len();
    let mut r = BufReader::new(f);
    let mut magic = [0u8; 8];
    r.read_exact(&mut magic)?;
    if &magic != MAGIC {
        return Err(bad("bad magic"));
    }
    let mut one = [0u8; 1];
    r.read_exact(&mut one)?;
    let drive_letter = one[0];
    let volume_serial = read_u64(&mut r)?;
    let usn_journal_id = read_u64(&mut r)?;
    let next_usn = read_i64(&mut r)?;
    let names_off: Vec<u32> = read_vec(&mut r, file_len)?;
    let names_len: Vec<u16> = read_vec(&mut r, file_len)?;
    let parent: Vec<u32> = read_vec(&mut r, file_len)?;
    let flags: Vec<u16> = read_vec(&mut r, file_len)?;
    let name_pool: Vec<u8> = read_vec(&mut r, file_len)?;
    let frns: Vec<u64> = read_vec(&mut r, file_len)?;
    let ids: Vec<u32> = read_vec(&mut r, file_len)?;

    // 一致性校验：截断/位翻转的缓存若不在此拦截，越界下标会在搜索时 panic
    //（release 为 panic=abort，整个进程直接退出）。校验失败按坏缓存处理 → 全量重建。
    let n = names_off.len();
    if n == 0
        || n > u32::MAX as usize
        || names_len.len() != n
        || parent.len() != n
        || flags.len() != n
        || frns.len() != ids.len()
    {
        return Err(bad("inconsistent array lengths"));
    }
    let pool = name_pool.len();
    for i in 0..n {
        if names_off[i] as usize + names_len[i] as usize > pool {
            return Err(bad("name range out of pool"));
        }
        let p = parent[i];
        if p != u32::MAX && p as usize >= n {
            return Err(bad("parent out of range"));
        }
    }
    if ids.iter().any(|&id| id as usize >= n) {
        return Err(bad("frn id out of range"));
    }

    let mut frn_to_id = HashMap::with_capacity(frns.len());
    for (k, v) in frns.into_iter().zip(ids) {
        frn_to_id.insert(k, v);
    }

    // 墓碑计数不落盘（文件格式不变），加载后按 flags 重算（O(n) 位测试，毫秒级）
    let tombstones = flags
        .iter()
        .filter(|f| **f & crate::index::IS_DELETED != 0)
        .count() as u32;

    Ok(Index {
        names_off,
        names_len,
        parent,
        flags,
        name_pool,
        frn_to_id,
        drive_letter,
        volume_serial,
        usn_journal_id,
        next_usn,
        tombstones,
    })
}

#[cfg(test)]
mod tests {
    use crate::index::{Change, IndexBuilder};

    #[test]
    fn roundtrip_recomputes_tombstones() {
        let mut b = IndexBuilder::new(b'C');
        b.push(100, 5, 0, "a.txt");
        b.push(200, 5, 0, "b.txt");
        let mut idx = b.finish();
        idx.apply(&Change::Delete { frn: 100 });
        assert_eq!(idx.tombstones, 1);

        let path = std::env::temp_dir().join(format!(
            "ss-core-persist-test-{}.ssidx",
            std::process::id()
        ));
        super::save_index(&idx, &path).unwrap();
        let loaded = super::load_index(&path).unwrap();
        let _ = std::fs::remove_file(&path);

        assert_eq!(loaded.tombstones, 1); // 不落盘，按 flags 重算后一致
        assert_eq!(loaded.len(), idx.len());
    }
}
