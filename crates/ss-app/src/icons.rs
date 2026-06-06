//! 按扩展名缓存的系统文件类型图标（`SHGetFileInfoW` + `SHGFI_USEFILEATTRIBUTES`，不碰磁盘）。
//!
//! 图标数量有界（每种扩展名一个 + 文件夹一个），缓存到进程退出，OS 在退出时回收。

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;

use windows::core::PCWSTR;
use windows::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL};
use windows::Win32::UI::Shell::{
    SHGetFileInfoW, SHFILEINFOW, SHGFI_ICON, SHGFI_SMALLICON, SHGFI_USEFILEATTRIBUTES,
};
use windows::Win32::UI::WindowsAndMessaging::HICON;

thread_local! {
    static CACHE: RefCell<HashMap<String, isize>> = RefCell::new(HashMap::new());
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn ext_of(name: &str) -> String {
    match name.rfind('.') {
        Some(pos) if pos + 1 < name.len() => name[pos + 1..].to_ascii_lowercase(),
        _ => String::new(),
    }
}

/// 取（缓存的）文件/文件夹小图标。失败返回 None（调用方跳过画图标）。
pub fn icon_for(name: &str, is_dir: bool) -> Option<HICON> {
    let key = if is_dir {
        "\u{1}dir".to_string()
    } else {
        let e = ext_of(name);
        if e.is_empty() {
            "\u{1}none".to_string()
        } else {
            e
        }
    };

    if let Some(h) = CACHE.with(|c| c.borrow().get(&key).copied()) {
        return Some(HICON(h as *mut c_void));
    }

    let (attrs, sample) = if is_dir {
        (FILE_ATTRIBUTE_DIRECTORY, wide("folder"))
    } else {
        let e = ext_of(name);
        let s = if e.is_empty() {
            "file".to_string()
        } else {
            format!("a.{e}")
        };
        (FILE_ATTRIBUTE_NORMAL, wide(&s))
    };

    let mut sfi = SHFILEINFOW::default();
    let r = unsafe {
        SHGetFileInfoW(
            PCWSTR(sample.as_ptr()),
            attrs,
            Some(&mut sfi),
            std::mem::size_of::<SHFILEINFOW>() as u32,
            SHGFI_ICON | SHGFI_SMALLICON | SHGFI_USEFILEATTRIBUTES,
        )
    };
    if r != 0 && !sfi.hIcon.0.is_null() {
        CACHE.with(|c| c.borrow_mut().insert(key, sfi.hIcon.0 as isize));
        Some(sfi.hIcon)
    } else {
        None
    }
}
