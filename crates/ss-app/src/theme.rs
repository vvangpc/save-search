//! 主题：颜色方案 + GDI 画刷缓存 + DWM 窗口外观（深色标题栏 / Win11 毛玻璃）。
//!
//! 颜色用 COLORREF（0x00BBGGRR）。`Theme` 是 Copy，当前主题存线程局部。

use std::cell::Cell;
use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;

use windows::Win32::Foundation::{COLORREF, HWND};
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_CAPTION_COLOR, DWMWA_SYSTEMBACKDROP_TYPE, DWMWA_TEXT_COLOR,
    DWMWA_USE_IMMERSIVE_DARK_MODE,
};
use windows::Win32::Graphics::Gdi::{CreateSolidBrush, HBRUSH};

/// 由 r/g/b 组成 COLORREF（0x00BBGGRR）。
const fn rgb(r: u8, g: u8, b: u8) -> u32 {
    (r as u32) | ((g as u32) << 8) | ((b as u32) << 16)
}

#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub is_dark: bool,
    pub acrylic: bool,
    pub bg: u32,
    pub fg: u32,
    pub path_fg: u32,
    pub accent: u32,
    pub sel_bg: u32,
    pub sel_fg: u32,
    pub border: u32,
    pub alt_bg: u32,
}

pub const BUSINESS_LIGHT: Theme = Theme {
    is_dark: false,
    acrylic: false,
    bg: rgb(0xF7, 0xF8, 0xFA),
    fg: rgb(0x1E, 0x20, 0x24),
    path_fg: rgb(0x86, 0x8C, 0x96),
    accent: rgb(0x2D, 0x6C, 0xDF),
    sel_bg: rgb(0xD8, 0xE6, 0xFF),
    sel_fg: rgb(0x10, 0x12, 0x16),
    border: rgb(0xDD, 0xDF, 0xE3),
    alt_bg: rgb(0xFF, 0xFF, 0xFF),
};

pub const BUSINESS_DARK: Theme = Theme {
    is_dark: true,
    acrylic: false,
    bg: rgb(0x20, 0x22, 0x26),
    fg: rgb(0xEC, 0xEC, 0xEC),
    path_fg: rgb(0x9A, 0x9F, 0xA8),
    accent: rgb(0x4C, 0x9A, 0xFF),
    sel_bg: rgb(0x2F, 0x4B, 0x76),
    sel_fg: rgb(0xFF, 0xFF, 0xFF),
    border: rgb(0x3A, 0x3D, 0x43),
    alt_bg: rgb(0x26, 0x29, 0x2E),
};

/// 暖阳浅色（Solarized Light）——暖调护眼浅色。
pub const SOLARIZED_LIGHT: Theme = Theme {
    is_dark: false,
    acrylic: false,
    bg: rgb(0xFD, 0xF6, 0xE3),
    fg: rgb(0x58, 0x6E, 0x75),
    path_fg: rgb(0x93, 0xA1, 0xA1),
    accent: rgb(0x26, 0x8B, 0xD2),
    sel_bg: rgb(0xE6, 0xDF, 0xC8),
    sel_fg: rgb(0x07, 0x36, 0x42),
    border: rgb(0xEE, 0xE8, 0xD5),
    alt_bg: rgb(0xEE, 0xE8, 0xD5),
};

pub fn theme_by_name(name: &str) -> Theme {
    match name {
        "business_dark" => BUSINESS_DARK,
        "solarized" => SOLARIZED_LIGHT,
        _ => BUSINESS_LIGHT,
    }
}

thread_local! {
    static CURRENT: Cell<Theme> = const { Cell::new(BUSINESS_LIGHT) };
    static BRUSHES: RefCell<HashMap<u32, isize>> = RefCell::new(HashMap::new());
}

pub fn current() -> Theme {
    CURRENT.with(|c| c.get())
}
pub fn set_current(t: Theme) {
    CURRENT.with(|c| c.set(t));
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

/// 应用窗口外观：深色标题栏 + （毛玻璃主题下）Win11 亚克力背景。
pub fn apply_window(hwnd: HWND, t: &Theme) {
    unsafe {
        let dark: i32 = if t.is_dark { 1 } else { 0 };
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            &dark as *const _ as *const c_void,
            4,
        );
        // DWMSBT_TRANSIENTWINDOW = 3（亚克力）；非毛玻璃用 DWMSBT_AUTO = 0
        let backdrop: i32 = if t.acrylic { 3 } else { 0 };
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_SYSTEMBACKDROP_TYPE,
            &backdrop as *const _ as *const c_void,
            4,
        );
        // Win11：标题栏背景/文字颜色随主题（Win10 忽略该属性）
        let caption: u32 = t.bg;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_CAPTION_COLOR,
            &caption as *const _ as *const c_void,
            4,
        );
        let text: u32 = t.fg;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_TEXT_COLOR,
            &text as *const _ as *const c_void,
            4,
        );
    }
}
