//! NTFS MFT 枚举与 USN 日志读取。
//!
//! - 全量：`build_index_for_drive`（`FSCTL_ENUM_USN_DATA`）。
//! - 增量：`read_changes`（`FSCTL_READ_USN_JOURNAL`），配合 `current_usn_state` 校验。
//! 需要管理员权限打开卷句柄 `\\.\X:`。

use std::ffi::c_void;

use ss_platform::wide;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, ERROR_HANDLE_EOF, GENERIC_READ, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, GetVolumeInformationW, FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_READ,
    FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::Ioctl::{
    FSCTL_ENUM_USN_DATA, FSCTL_QUERY_USN_JOURNAL, FSCTL_READ_USN_JOURNAL, MFT_ENUM_DATA_V0,
    READ_USN_JOURNAL_DATA_V0, USN_JOURNAL_DATA_V0,
};
use windows::Win32::System::IO::DeviceIoControl;

use crate::index::{Change, Index, IndexBuilder};

const USN_REASON_FILE_DELETE: u32 = 0x0000_0200;

struct Handle(HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

fn open_volume(letter: char) -> windows::core::Result<Handle> {
    let path = format!("\\\\.\\{}:", letter.to_ascii_uppercase());
    let wpath = wide(path.as_str());
    let h = unsafe {
        CreateFileW(
            PCWSTR(wpath.as_ptr()),
            GENERIC_READ.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            None,
        )?
    };
    Ok(Handle(h))
}

/// 卷序列号（用于缓存校验）。
fn volume_serial(letter: char) -> u32 {
    let root = format!("{}:\\", letter.to_ascii_uppercase());
    let root_w = wide(root.as_str());
    let mut serial: u32 = 0;
    let _ = unsafe {
        GetVolumeInformationW(
            PCWSTR(root_w.as_ptr()),
            None,
            Some(&mut serial),
            None,
            None,
            None,
        )
    };
    serial
}

fn query_journal(h: &Handle) -> windows::core::Result<USN_JOURNAL_DATA_V0> {
    let mut data = USN_JOURNAL_DATA_V0::default();
    let mut br = 0u32;
    unsafe {
        DeviceIoControl(
            h.0,
            FSCTL_QUERY_USN_JOURNAL,
            None,
            0,
            Some(&mut data as *mut _ as *mut c_void),
            std::mem::size_of::<USN_JOURNAL_DATA_V0>() as u32,
            Some(&mut br),
            None,
        )?;
    }
    Ok(data)
}

/// 查询某盘当前 USN 状态：(卷序列号, 日志ID, FirstUsn, NextUsn)。
pub fn current_usn_state(letter: char) -> windows::core::Result<(u64, u64, i64, i64)> {
    let h = open_volume(letter)?;
    let j = query_journal(&h)?;
    Ok((
        volume_serial(letter) as u64,
        j.UsnJournalID,
        j.FirstUsn,
        j.NextUsn,
    ))
}

/// 解析一段 USN_RECORD_V2 缓冲（从 offset 8 起），追加到 changes。
fn parse_records(buf: &[u8], end: usize, changes: &mut Vec<Change>) {
    let mut offset = 8usize;
    while offset + 60 <= end {
        let rec = &buf[offset..end];
        let record_len = u32::from_le_bytes(rec[0..4].try_into().unwrap()) as usize;
        if record_len < 60 || offset + record_len > end {
            break;
        }
        let frn = u64::from_le_bytes(rec[8..16].try_into().unwrap());
        let parent_frn = u64::from_le_bytes(rec[16..24].try_into().unwrap());
        let reason = u32::from_le_bytes(rec[40..44].try_into().unwrap());
        let attrs = u32::from_le_bytes(rec[52..56].try_into().unwrap());
        let name_len = u16::from_le_bytes(rec[56..58].try_into().unwrap()) as usize;
        let name_off = u16::from_le_bytes(rec[58..60].try_into().unwrap()) as usize;

        if !IndexBuilder::is_root_frn(frn) && name_off + name_len <= record_len && name_len > 0 {
            if reason & USN_REASON_FILE_DELETE != 0 {
                changes.push(Change::Delete { frn });
            } else {
                let name_bytes = &rec[name_off..name_off + name_len];
                let u16s: Vec<u16> = name_bytes
                    .chunks_exact(2)
                    .map(|c| u16::from_le_bytes([c[0], c[1]]))
                    .collect();
                let name = String::from_utf16_lossy(&u16s);
                changes.push(Change::Upsert {
                    frn,
                    parent_frn,
                    attrs,
                    name,
                });
            }
        }
        offset += record_len;
    }
}

/// 读取从 `from_usn` 到当前的增量变更。返回 (变更列表, 新的 next_usn)。
pub fn read_changes(
    letter: char,
    journal_id: u64,
    from_usn: i64,
) -> windows::core::Result<(Vec<Change>, i64)> {
    let h = open_volume(letter)?;
    let mut changes = Vec::new();
    const BUF_SIZE: usize = 1 << 18; // 256 KiB
    let mut out = vec![0u8; BUF_SIZE];
    let mut start = from_usn;

    loop {
        let data = READ_USN_JOURNAL_DATA_V0 {
            StartUsn: start,
            ReasonMask: 0xFFFF_FFFF,
            ReturnOnlyOnClose: 0,
            Timeout: 0,
            BytesToWaitFor: 0,
            UsnJournalID: journal_id,
        };
        let mut br = 0u32;
        let res = unsafe {
            DeviceIoControl(
                h.0,
                FSCTL_READ_USN_JOURNAL,
                Some(&data as *const _ as *const c_void),
                std::mem::size_of::<READ_USN_JOURNAL_DATA_V0>() as u32,
                Some(out.as_mut_ptr() as *mut c_void),
                BUF_SIZE as u32,
                Some(&mut br),
                None,
            )
        };
        if let Err(e) = res {
            return Err(e);
        }
        let end = br as usize;
        if end < 8 {
            break;
        }
        let next = i64::from_le_bytes(out[0..8].try_into().unwrap());
        if end > 8 {
            parse_records(&out, end, &mut changes);
        }
        if next <= start {
            break; // 无进展（USN 单调递增，<= 即停，防异常数据死循环）
        }
        start = next;
        if end <= 8 {
            break; // 仅返回了 next usn，已追上
        }
    }
    Ok((changes, start))
}

/// 枚举单个 NTFS 盘，构建索引并记录 USN 状态（供后续增量）。
pub fn build_index_for_drive(letter: char) -> windows::core::Result<Index> {
    let handle = open_volume(letter)?;

    // 先记录当前 USN 高水位，全量扫描后从此处增量追赶（upsert 幂等，重叠无害）。
    let journal = query_journal(&handle)?;
    let serial = volume_serial(letter) as u64;

    let mut builder = IndexBuilder::new(letter.to_ascii_uppercase() as u8);

    const BUF_SIZE: usize = 1 << 20; // 1 MiB
    let mut out = vec![0u8; BUF_SIZE];
    let mut med = MFT_ENUM_DATA_V0 {
        StartFileReferenceNumber: 0,
        LowUsn: 0,
        HighUsn: i64::MAX,
    };

    loop {
        let mut bytes_returned: u32 = 0;
        let res = unsafe {
            DeviceIoControl(
                handle.0,
                FSCTL_ENUM_USN_DATA,
                Some(&med as *const _ as *const c_void),
                std::mem::size_of::<MFT_ENUM_DATA_V0>() as u32,
                Some(out.as_mut_ptr() as *mut c_void),
                BUF_SIZE as u32,
                Some(&mut bytes_returned),
                None,
            )
        };
        if let Err(e) = res {
            if e.code() == ERROR_HANDLE_EOF.to_hresult() {
                break;
            }
            return Err(e);
        }
        if bytes_returned < 8 {
            break;
        }
        med.StartFileReferenceNumber = u64::from_le_bytes(out[0..8].try_into().unwrap());

        let end = bytes_returned as usize;
        let mut offset = 8usize;
        while offset + 60 <= end {
            let rec = &out[offset..end];
            let record_len = u32::from_le_bytes(rec[0..4].try_into().unwrap()) as usize;
            if record_len < 60 || offset + record_len > end {
                break;
            }
            let frn = u64::from_le_bytes(rec[8..16].try_into().unwrap());
            let parent_frn = u64::from_le_bytes(rec[16..24].try_into().unwrap());
            let attrs = u32::from_le_bytes(rec[52..56].try_into().unwrap());
            let name_len = u16::from_le_bytes(rec[56..58].try_into().unwrap()) as usize;
            let name_off = u16::from_le_bytes(rec[58..60].try_into().unwrap()) as usize;

            if !IndexBuilder::is_root_frn(frn) && name_len > 0 && name_off + name_len <= record_len {
                let name_bytes = &rec[name_off..name_off + name_len];
                let u16s: Vec<u16> = name_bytes
                    .chunks_exact(2)
                    .map(|c| u16::from_le_bytes([c[0], c[1]]))
                    .collect();
                let name = String::from_utf16_lossy(&u16s);
                builder.push(frn, parent_frn, attrs, &name);
            }
            offset += record_len;
        }
    }

    let mut index = builder.finish();
    index.set_usn_state(serial, journal.UsnJournalID, journal.NextUsn);
    Ok(index)
}
