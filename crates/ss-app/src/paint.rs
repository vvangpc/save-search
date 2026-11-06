//! 自绘双缓冲：先画到内存 DC，再一次性 BitBlt 到屏幕，消除滚动闪烁/卡顿。
//!
//! 内存 DC + 位图按线程缓存复用（够大就直接用，不够才重建）：平滑滚动动画每 15ms 重绘
//! 整个列表，原先每帧 CreateCompatibleBitmap/DeleteObject 一张整窗位图（~1-2 MB），
//! GDI 堆反复分配释放是滚动时的主要开销之一。窗口隐藏时 [`release`] 归还。

use std::cell::Cell;

use windows::Win32::Foundation::{COLORREF, RECT, SIZE};
use windows::core::w;
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateFontW, DeleteDC, DeleteObject,
    DrawTextW, FillRect, GetDC, GetTextExtentPoint32W, ReleaseDC, SelectObject, SetBkMode,
    SetPixelV, SetTextColor, ANTIALIASED_QUALITY, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS,
    DEFAULT_CHARSET, DRAW_TEXT_FORMAT, HBITMAP, HDC, HFONT, HGDIOBJ, OUT_DEFAULT_PRECIS, SRCCOPY,
    TRANSPARENT,
};

use crate::theme;

thread_local! {
    /// (内存 DC, 位图, 宽, 高)；DC 为空表示无缓存。
    static CACHE: Cell<(isize, isize, i32, i32)> = const { Cell::new((0, 0, 0, 0)) };
    /// 缓存正被外层 `buffered` 占用（嵌套调用时内层退回临时分配）。
    static BUSY: Cell<bool> = const { Cell::new(false) };
}

unsafe fn free_cache() {
    let (dc, bmp, _, _) = CACHE.with(|c| c.replace((0, 0, 0, 0)));
    if bmp != 0 {
        let _ = DeleteObject(HGDIOBJ(bmp as *mut _));
    }
    if dc != 0 {
        let _ = DeleteDC(HDC(dc as *mut _));
    }
}

/// 释放缓存的后备位图（窗口隐藏时调用，不让整窗大小的位图常驻）。
pub fn release() {
    if BUSY.with(|b| b.get()) {
        return;
    }
    unsafe { free_cache() }
}

/// 取一块至少 `w×h` 的缓存内存 DC；尺寸不足则重建为恰好 `w×h`（不取 max：避免曾经
/// 最大化过一次就长期占着一张大位图）。失败返回 None。
unsafe fn acquire(hdc: HDC, w: i32, h: i32) -> Option<HDC> {
    let (dc, bmp, cw, ch) = CACHE.with(|c| c.get());
    if dc != 0 && bmp != 0 && cw >= w && ch >= h {
        return Some(HDC(dc as *mut _));
    }
    free_cache();
    let mem = CreateCompatibleDC(Some(hdc));
    if mem.is_invalid() {
        return None;
    }
    let bitmap: HBITMAP = CreateCompatibleBitmap(hdc, w, h);
    if bitmap.is_invalid() {
        let _ = DeleteDC(mem);
        return None;
    }
    // 位图常驻选入；原 1×1 默认位图随 DeleteDC 一起回收，不必换回
    SelectObject(mem, HGDIOBJ(bitmap.0));
    CACHE.with(|c| c.set((mem.0 as isize, bitmap.0 as isize, w, h)));
    Some(mem)
}

/// 在 `rc`（控件 DC 坐标）区域内双缓冲绘制。
/// 回调收到内存 DC 与「以 (0,0) 为原点」的本地 RECT，按本地坐标作画即可。
pub unsafe fn buffered<F: FnOnce(HDC, RECT)>(hdc: HDC, rc: RECT, f: F) {
    let w = rc.right - rc.left;
    let h = rc.bottom - rc.top;
    if w <= 0 || h <= 0 {
        return;
    }
    let local = RECT {
        left: 0,
        top: 0,
        right: w,
        bottom: h,
    };
    if !BUSY.with(|b| b.replace(true)) {
        if let Some(mem) = acquire(hdc, w, h) {
            f(mem, local);
            let _ = BitBlt(hdc, rc.left, rc.top, w, h, Some(mem), 0, 0, SRCCOPY);
            BUSY.with(|b| b.set(false));
            return;
        }
        BUSY.with(|b| b.set(false));
    }
    // 嵌套调用或缓存创建失败：退回一次性分配
    let mem = CreateCompatibleDC(Some(hdc));
    let bmp = CreateCompatibleBitmap(hdc, w, h);
    let old = SelectObject(mem, HGDIOBJ(bmp.0));
    f(mem, local);
    let _ = BitBlt(hdc, rc.left, rc.top, w, h, Some(mem), 0, 0, SRCCOPY);
    SelectObject(mem, old);
    let _ = DeleteObject(HGDIOBJ(bmp.0));
    let _ = DeleteDC(mem);
}

/// 把 `fg` 按覆盖率 `a`（0 到 1）叠到 `bg` 上。颜色是 COLORREF。
fn blend(bg: u32, fg: u32, a: f32) -> u32 {
    let mix = |shift: u32| {
        let b = ((bg >> shift) & 0xFF) as f32;
        let f = ((fg >> shift) & 0xFF) as f32;
        ((b + (f - b) * a + 0.5) as u32) << shift
    };
    mix(0) | mix(8) | mix(16)
}

/// 画抗锯齿的圆角矩形。GDI 的 `RoundRect` 有锯齿，所以四个角逐像素按覆盖率混色。
/// `bg` 是矩形后面的颜色，`border` 是 1 像素的边框色。
pub unsafe fn round_rect(hdc: HDC, r: RECT, radius: i32, fill: u32, border: Option<u32>, bg: u32) {
    let (w, h) = (r.right - r.left, r.bottom - r.top);
    if w <= 0 || h <= 0 {
        return;
    }
    let rad = radius.min(w / 2).min(h / 2).max(0);
    let brush = theme::solid_brush(fill);
    let middle = RECT { left: r.left + rad, top: r.top, right: r.right - rad, bottom: r.bottom };
    FillRect(hdc, &middle, brush);
    let sides = RECT { left: r.left, top: r.top + rad, right: r.right, bottom: r.bottom - rad };
    FillRect(hdc, &sides, brush);
    let edge = border.unwrap_or(fill);
    if let Some(b) = border {
        let line = theme::solid_brush(b);
        for s in [
            RECT { left: r.left + rad, top: r.top, right: r.right - rad, bottom: r.top + 1 },
            RECT { left: r.left + rad, top: r.bottom - 1, right: r.right - rad, bottom: r.bottom },
            RECT { left: r.left, top: r.top + rad, right: r.left + 1, bottom: r.bottom - rad },
            RECT { left: r.right - 1, top: r.top + rad, right: r.right, bottom: r.bottom - rad },
        ] {
            FillRect(hdc, &s, line);
        }
    }
    let radius_f = rad as f32;
    for dy in 0..rad {
        for dx in 0..rad {
            // 像素中心到圆角圆心的距离
            let fx = radius_f - dx as f32 - 0.5;
            let fy = radius_f - dy as f32 - 0.5;
            let d = (fx * fx + fy * fy).sqrt();
            let outer = (radius_f - d + 0.5).clamp(0.0, 1.0);
            let mut color = blend(bg, edge, outer);
            if border.is_some() {
                let inner = (radius_f - 1.0 - d + 0.5).clamp(0.0, 1.0);
                color = blend(color, fill, inner);
            }
            let c = COLORREF(color);
            let _ = SetPixelV(hdc, r.left + dx, r.top + dy, c);
            let _ = SetPixelV(hdc, r.right - 1 - dx, r.top + dy, c);
            let _ = SetPixelV(hdc, r.left + dx, r.bottom - 1 - dy, c);
            let _ = SetPixelV(hdc, r.right - 1 - dx, r.bottom - 1 - dy, c);
        }
    }
}

/// 画文字。文字为空时什么也不做。
pub unsafe fn text(hdc: HDC, s: &str, rect: &mut RECT, color: u32, font: HFONT, flags: DRAW_TEXT_FORMAT) {
    if s.is_empty() {
        return;
    }
    SelectObject(hdc, HGDIOBJ(font.0));
    SetTextColor(hdc, COLORREF(color));
    SetBkMode(hdc, TRANSPARENT);
    let mut buf: Vec<u16> = s.encode_utf16().collect();
    DrawTextW(hdc, &mut buf, rect, flags);
}

/// 用指定字体测量文字的像素宽度。
pub fn measure(font: HFONT, s: &str) -> i32 {
    if s.is_empty() {
        return 0;
    }
    unsafe {
        let hdc = GetDC(None);
        let old = SelectObject(hdc, HGDIOBJ(font.0));
        let w: Vec<u16> = s.encode_utf16().collect();
        let mut size = SIZE::default();
        let _ = GetTextExtentPoint32W(hdc, &w, &mut size);
        SelectObject(hdc, old);
        ReleaseDC(None, hdc);
        size.cx
    }
}

/// 界面文字字体。
pub unsafe fn ui_font(px: i32, weight: i32) -> HFONT {
    CreateFontW(
        -px, 0, 0, 0, weight, 0, 0, 0, DEFAULT_CHARSET, OUT_DEFAULT_PRECIS, CLIP_DEFAULT_PRECIS,
        CLEARTYPE_QUALITY, 0, w!("Segoe UI"),
    )
}

/// 图标字体。用灰度抗锯齿，小图标不会出现 ClearType 的彩色毛边。
pub unsafe fn icon_font(px: i32, weight: i32) -> HFONT {
    CreateFontW(
        -px, 0, 0, 0, weight, 0, 0, 0, DEFAULT_CHARSET, OUT_DEFAULT_PRECIS, CLIP_DEFAULT_PRECIS,
        ANTIALIASED_QUALITY, 0, w!("Segoe MDL2 Assets"),
    )
}
