//! 自绘双缓冲：先画到内存 DC，再一次性 BitBlt 到屏幕，消除滚动闪烁/卡顿。
//!
//! 内存 DC + 位图按线程缓存复用（够大就直接用，不够才重建）：平滑滚动动画每 15ms 重绘
//! 整个列表，原先每帧 CreateCompatibleBitmap/DeleteObject 一张整窗位图（~1-2 MB），
//! GDI 堆反复分配释放是滚动时的主要开销之一。窗口隐藏时 [`release`] 归还。

use std::cell::Cell;

use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, SelectObject,
    HBITMAP, HDC, HGDIOBJ, SRCCOPY,
};

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
