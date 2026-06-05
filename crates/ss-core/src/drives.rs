//! 固定 NTFS 盘符枚举。

use ss_platform::wide;
use windows::core::PCWSTR;
use windows::Win32::Storage::FileSystem::{
    GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW,
};

/// DRIVE_FIXED（GetDriveTypeW 返回值）。
const DRIVE_FIXED: u32 = 3;

/// 返回所有「固定磁盘且文件系统为 NTFS」的盘符（大写，如 ['C','D']）。
pub fn ntfs_fixed_drives() -> Vec<char> {
    let mut out = Vec::new();
    let mask = unsafe { GetLogicalDrives() };
    for i in 0..26u32 {
        if mask & (1 << i) == 0 {
            continue;
        }
        let letter = (b'A' + i as u8) as char;
        let root = format!("{}:\\", letter);
        let root_w = wide(root.as_str());
        let dtype = unsafe { GetDriveTypeW(PCWSTR(root_w.as_ptr())) };
        if dtype != DRIVE_FIXED {
            continue;
        }
        if is_ntfs(&root_w) {
            out.push(letter);
        }
    }
    out
}

fn is_ntfs(root_w: &[u16]) -> bool {
    let mut fsname = [0u16; 16];
    let ok = unsafe {
        GetVolumeInformationW(
            PCWSTR(root_w.as_ptr()),
            None,
            None,
            None,
            None,
            Some(&mut fsname),
        )
    };
    if ok.is_err() {
        return false;
    }
    let name = String::from_utf16_lossy(&fsname);
    name.trim_end_matches('\0').eq_ignore_ascii_case("NTFS")
}
