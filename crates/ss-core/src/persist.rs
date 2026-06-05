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

fn read_vec<T: Copy>(r: &mut impl Read) -> io::Result<Vec<T>> {
    let mut lenb = [0u8; 8];
    r.read_exact(&mut lenb)?;
    let n = u64::from_le_bytes(lenb) as usize;
    // 防御性上限：单数组最多 5 亿元素
    if n > 500_000_000 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "vec too large"));
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
    }
    std::fs::rename(&tmp, path)
}

pub fn load_index(path: &Path) -> io::Result<Index> {
    let f = File::open(path)?;
    let mut r = BufReader::new(f);
    let mut magic = [0u8; 8];
    r.read_exact(&mut magic)?;
    if &magic != MAGIC {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "bad magic"));
    }
    let mut one = [0u8; 1];
    r.read_exact(&mut one)?;
    let drive_letter = one[0];
    let volume_serial = read_u64(&mut r)?;
    let usn_journal_id = read_u64(&mut r)?;
    let next_usn = read_i64(&mut r)?;
    let names_off: Vec<u32> = read_vec(&mut r)?;
    let names_len: Vec<u16> = read_vec(&mut r)?;
    let parent: Vec<u32> = read_vec(&mut r)?;
    let flags: Vec<u16> = read_vec(&mut r)?;
    let name_pool: Vec<u8> = read_vec(&mut r)?;
    let frns: Vec<u64> = read_vec(&mut r)?;
    let ids: Vec<u32> = read_vec(&mut r)?;

    let mut frn_to_id = HashMap::with_capacity(frns.len());
    for (k, v) in frns.into_iter().zip(ids) {
        frn_to_id.insert(k, v);
    }

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
    })
}
