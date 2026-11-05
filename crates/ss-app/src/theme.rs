//! 界面颜色（只有一套浅色）+ GDI 画刷缓存 + 标题栏外观。
//!
//! 颜色用 COLORREF（0x00BBGGRR）。

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;

use windows::Win32::Foundation::{COLORREF, HWND};
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_CAPTION_COLOR, DWMWA_TEXT_COLOR, DWMWA_USE_IMMERSIVE_DARK_MODE,
};
use windows::Win32::Graphics::Gdi::{CreateSolidBrush, HBRUSH};

/// 由 r/g/b 组成 COLORREF（0x00BBGGRR）。
const fn rgb(r: u8, g: u8, b: u8) -> u32 {
    (r as u32) | ((g as u32) << 8) | ((b as u32) << 16)
}

#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub bg: u32,
    pub fg: u32,
    pub path_fg: u32,
    pub accent: u32,
    pub sel_bg: u32,
    pub sel_fg: u32,
    pub alt_bg: u32,
}

const LIGHT: Theme = Theme {
    bg: rgb(0xFF, 0xFF, 0xFF),
    fg: rgb(0x1C, 0x24, 0x30),
    path_fg: rgb(0x66, 0x70, 0x85),
    accent: rgb(0x1F, 0x5F, 0xBF),
    sel_bg: rgb(0xE6, 0xEF, 0xFB),
    sel_fg: rgb(0x1C, 0x24, 0x30),
    alt_bg: rgb(0xF4, 0xF6, 0xF8),
};

thread_local! {
    static BRUSHES: RefCell<HashMap<u32, isize>> = RefCell::new(HashMap::new());
}

pub fn current() -> Theme {
    LIGHT
}

/// 取（缓存的）实心画刷。生命周期随进程，不释放。
pub fn solid_brush(color: u32) -> HBRUSH {
    BRUSHES.with(|c| {
        let mut m = c.borrow_mut();
        if let Some(&h) = m.get(&color) {
            return HBRUSH(h as *mut c_void);
        }
        let h = unsafe { CreateSolidBrush(COLORREF(color)) };
        m.insert(color, h.0 as isize);
        h
    })
}

/// 应用窗口外观：浅色标题栏，Windows 11 下标题栏颜色和窗口一致（Windows 10 忽略这两项）。
pub fn apply_window(hwnd: HWND) {
    let t = current();
    unsafe {
        let dark: i32 = 0;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            &dark as *const _ as *const c_void,
            4,
        );
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_CAPTION_COLOR,
            &t.bg as *const _ as *const c_void,
            4,
        );
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_TEXT_COLOR,
            &t.fg as *const _ as *const c_void,
            4,
        );
    }
}
