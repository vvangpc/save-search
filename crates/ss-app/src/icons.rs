//! 按扩展名缓存的系统文件类型图标（`SHGetFileInfoW` + `SHGFI_USEFILEATTRIBUTES`，不碰磁盘）。
//!
//! 图标在后台线程加载，加载好后通知界面重绘，尚未加载时调用方不画图标。
//! 图标数量有界（每种扩展名一个 + 文件夹一个），缓存到进程退出，OS 在退出时回收。

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::mpsc::{channel, Sender};
use std::sync::{Mutex, OnceLock};

use windows::core::PCWSTR;
use windows::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows::Win32::UI::Shell::{
    SHGetFileInfoW, SHFILEINFOW, SHGFI_ICON, SHGFI_SMALLICON, SHGFI_USEFILEATTRIBUTES,
};
use windows::Win32::UI::WindowsAndMessaging::HICON;

const DIR_KEY: &str = "\u{1}dir";
const NO_EXT_KEY: &str = "\u{1}none";

/// 键 → 图标句柄。0 表示正在加载或加载失败。
static CACHE: Mutex<Option<HashMap<String, isize>>> = Mutex::new(None);
static REQUESTS: OnceLock<Sender<String>> = OnceLock::new();

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn ext_of(name: &str) -> String {
    match name.rfind('.') {
        Some(pos) if pos + 1 < name.len() => name[pos + 1..].to_ascii_lowercase(),
        _ => String::new(),
    }
}

fn load_icon(key: &str) -> isize {
    let (attrs, sample) = if key == DIR_KEY {
        (FILE_ATTRIBUTE_DIRECTORY, wide("folder"))
    } else if key == NO_EXT_KEY {
        (FILE_ATTRIBUTE_NORMAL, wide("file"))
    } else {
        (FILE_ATTRIBUTE_NORMAL, wide(&format!("a.{key}")))
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
    if r != 0 {
        sfi.hIcon.0 as isize
    } else {
        0
    }
}

/// 启动加载线程，并预加载文件夹图标。`on_loaded` 在加载线程上调用。
pub fn start(on_loaded: fn()) {
    let (tx, rx) = channel::<String>();
    if REQUESTS.set(tx).is_err() {
        return;
    }
    std::thread::spawn(move || {
        let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        for key in rx {
            let icon = load_icon(&key);
            if let Some(map) = CACHE.lock().unwrap_or_else(|p| p.into_inner()).as_mut() {
                map.insert(key, icon);
            }
            if icon != 0 {
                on_loaded();
            }
        }
    });
    let _ = icon_for("", true);
}

/// 取缓存的文件/文件夹小图标。还没加载好时返回 None，并安排加载。
pub fn icon_for(name: &str, is_dir: bool) -> Option<HICON> {
    let key = if is_dir {
        DIR_KEY.to_string()
    } else {
        let e = ext_of(name);
        if e.is_empty() {
            NO_EXT_KEY.to_string()
        } else {
            e
        }
    };
    let mut guard = CACHE.lock().unwrap_or_else(|p| p.into_inner());
    let map = guard.get_or_insert_with(HashMap::new);
    if let Some(&h) = map.get(&key) {
        return (h != 0).then_some(HICON(h as *mut c_void));
    }
    map.insert(key.clone(), 0);
    drop(guard);
    if let Some(tx) = REQUESTS.get() {
        let _ = tx.send(key);
    }
    None
}
