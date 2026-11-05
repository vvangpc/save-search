//! 界面颜色（只有一套浅色）+ GDI 画刷缓存 + 标题栏外观。
//!
//! 颜色用 COLORREF（0x00BBGGRR）。

use std::cell::RefCell;
use std::ffi::c_void;

use windows::Win32::Foundation::{COLORREF, HWND};
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_BORDER_COLOR, DWMWA_CAPTION_COLOR, DWMWA_TEXT_COLOR,
    DWMWA_USE_IMMERSIVE_DARK_MODE, DWMWA_WINDOW_CORNER_PREFERENCE,
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
    pub alt_bg: u32,
    pub accent_fg: u32,
    pub border: u32,
    pub warn: u32,
    pub warn_bg: u32,
    pub err: u32,
    pub err_bg: u32,
    pub mark: u32,
    pub thumb: u32,
    pub ok: u32,
}

const LIGHT: Theme = Theme {
    bg: rgb(0xFF, 0xFF, 0xFF),
    fg: rgb(0x1C, 0x24, 0x30),
    path_fg: rgb(0x66, 0x70, 0x85),
    accent: rgb(0x1F, 0x5F, 0xBF),
    sel_bg: rgb(0xE6, 0xEF, 0xFB),
    alt_bg: rgb(0xF4, 0xF6, 0xF8),
    accent_fg: rgb(0xFF, 0xFF, 0xFF),
    border: rgb(0xDF, 0xE3, 0xE8),
    warn: rgb(0x7F, 0x4D, 0x00),
    warn_bg: rgb(0xFF, 0xF4, 0xE0),
    err: rgb(0xA3, 0x26, 0x26),
    err_bg: rgb(0xFD, 0xEA, 0xEA),
    mark: rgb(0xFF, 0xE9, 0x9A),
    thumb: rgb(0xC4, 0xCA, 0xD2),
    ok: rgb(0x1B, 0x7A, 0x43),
};

thread_local! {
    static BRUSHES: RefCell<Vec<(u32, isize)>> = const { RefCell::new(Vec::new()) };
}

pub fn current() -> Theme {
    LIGHT
}

/// 取（缓存的）实心画刷。生命周期随进程，不释放。
pub fn solid_brush(color: u32) -> HBRUSH {
    BRUSHES.with(|c| {
        let mut m = c.borrow_mut();
        if let Some((_, h)) = m.iter().find(|(c, _)| *c == color) {
            return HBRUSH(*h as *mut c_void);
        }
        let h = unsafe { CreateSolidBrush(COLORREF(color)) };
        m.push((color, h.0 as isize));
        h
    })
}

/// 把无边框的小窗口设成圆角。Windows 11 由系统处理（带抗锯齿和细边框），返回 true；
/// 更早的系统不支持时返回 false，由调用方用窗口区域裁剪。
pub fn round_corners(hwnd: HWND) -> bool {
    const DWMWCP_ROUND: i32 = 2;
    let t = current();
    unsafe {
        let ok = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &DWMWCP_ROUND as *const _ as *const c_void,
            4,
        )
        .is_ok();
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_BORDER_COLOR,
            &t.border as *const _ as *const c_void,
            4,
        );
        ok
    }
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
