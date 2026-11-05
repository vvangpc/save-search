//! 盘符枚举：固定 NTFS 盘，以及带不可索引原因的完整卷列表。

use ss_platform::wide;
use windows::core::PCWSTR;
use windows::Win32::Storage::FileSystem::{
    GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW,
};

/// DRIVE_FIXED（GetDriveTypeW 返回值）。
const DRIVE_FIXED: u32 = 3;
const DRIVE_REMOVABLE: u32 = 2;
const DRIVE_REMOTE: u32 = 4;
const DRIVE_CDROM: u32 = 5;
const DRIVE_RAMDISK: u32 = 6;

/// 盘的类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriveKind {
    Fixed,
    Removable,
    Network,
    Optical,
    RamDisk,
    Unknown,
}

/// 一个卷的信息，供设置页显示。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeInfo {
    pub letter: char,
    pub label: String,
    /// 文件系统名。读不到时（网络盘、无介质）为空。
    pub file_system: String,
    pub kind: DriveKind,
    pub indexable: bool,
    /// 不可索引的原因；可索引时为 `None`。
    pub reason: Option<String>,
}

fn drive_kind(dtype: u32) -> DriveKind {
    match dtype {
        DRIVE_FIXED => DriveKind::Fixed,
        DRIVE_REMOVABLE => DriveKind::Removable,
        DRIVE_REMOTE => DriveKind::Network,
        DRIVE_CDROM => DriveKind::Optical,
        DRIVE_RAMDISK => DriveKind::RamDisk,
        _ => DriveKind::Unknown,
    }
}

/// 不可索引的原因。`file_system=None` 表示读不到卷信息。
fn unindexable_reason(kind: DriveKind, file_system: Option<&str>) -> Option<String> {
    let kind_name = match kind {
        DriveKind::Fixed => None,
        DriveKind::Removable => Some("可移动磁盘"),
        DriveKind::Network => Some("网络盘"),
        DriveKind::Optical => Some("光盘"),
        DriveKind::RamDisk => Some("内存盘"),
        DriveKind::Unknown => Some("未知类型的盘"),
    };
    if let Some(name) = kind_name {
        return Some(format!("不支持{name}，只能索引固定磁盘"));
    }
    match file_system {
        None => Some("无法读取卷信息".to_string()),
        Some(fs) if fs.eq_ignore_ascii_case("NTFS") => None,
        Some(fs) => Some(format!("文件系统是 {fs}，只能索引 NTFS")),
    }
}

/// 列出所有已分配盘符的卷。网络盘不读卷信息，断开的映射盘可能让调用卡住很久。
pub fn list_volumes() -> Vec<VolumeInfo> {
    let mask = unsafe { GetLogicalDrives() };
    (0..26u32)
        .filter(|i| mask & (1 << i) != 0)
        .map(|i| {
            let letter = (b'A' + i as u8) as char;
            let root_w = wide(format!("{letter}:\\").as_str());
            let kind = drive_kind(unsafe { GetDriveTypeW(PCWSTR(root_w.as_ptr())) });
            let info = if kind == DriveKind::Network {
                None
            } else {
                volume_label_and_fs(&root_w)
            };
            let reason = unindexable_reason(kind, info.as_ref().map(|(_, fs)| fs.as_str()));
            let (label, file_system) = info.unwrap_or_default();
            VolumeInfo {
                letter,
                label,
                file_system,
                kind,
                indexable: reason.is_none(),
                reason,
            }
        })
        .collect()
}

fn utf16_until_nul(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

fn volume_label_and_fs(root_w: &[u16]) -> Option<(String, String)> {
    let mut label = [0u16; 261];
    let mut fs = [0u16; 32];
    unsafe {
        GetVolumeInformationW(
            PCWSTR(root_w.as_ptr()),
            Some(&mut label),
            None,
            None,
            None,
            Some(&mut fs),
        )
    }
    .ok()?;
    Some((utf16_until_nul(&label), utf16_until_nul(&fs)))
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasons_explain_why_a_volume_cannot_be_indexed() {
        assert_eq!(unindexable_reason(DriveKind::Fixed, Some("NTFS")), None);
        assert_eq!(unindexable_reason(DriveKind::Fixed, Some("ntfs")), None);
        assert!(unindexable_reason(DriveKind::Fixed, Some("exFAT")).unwrap().contains("exFAT"));
        assert!(unindexable_reason(DriveKind::Fixed, None).unwrap().contains("卷信息"));
        assert!(unindexable_reason(DriveKind::Network, None).unwrap().contains("网络盘"));
        assert!(unindexable_reason(DriveKind::Removable, Some("NTFS")).unwrap().contains("可移动"));
    }

    #[test]
    fn list_volumes_agrees_with_ntfs_fixed_drives() {
        let volumes = list_volumes();
        let indexable: Vec<char> =
            volumes.iter().filter(|v| v.indexable).map(|v| v.letter).collect();
        assert_eq!(indexable, ntfs_fixed_drives());
        assert!(volumes.iter().all(|v| v.indexable == v.reason.is_none()));
    }
}
