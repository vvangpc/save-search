//! 自绘双缓冲：把单个列表项先画到内存 DC，再一次性 BitBlt 到屏幕，消除滚动闪烁/卡顿。

use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, SelectObject, HDC,
    HGDIOBJ, SRCCOPY,
};

/// 在 `rc`（控件 DC 坐标）区域内双缓冲绘制。
/// 回调收到内存 DC 与「以 (0,0) 为原点」的本地 RECT，按本地坐标作画即可。
pub unsafe fn buffered<F: FnOnce(HDC, RECT)>(hdc: HDC, rc: RECT, f: F) {
    let w = rc.right - rc.left;
    let h = rc.bottom - rc.top;
    if w <= 0 || h <= 0 {
        return;
    }
    let mem = CreateCompatibleDC(Some(hdc));
    let bmp = CreateCompatibleBitmap(hdc, w, h);
    let old = SelectObject(mem, HGDIOBJ(bmp.0));
    let local = RECT {
        left: 0,
        top: 0,
        right: w,
        bottom: h,
    };
    f(mem, local);
    let _ = BitBlt(hdc, rc.left, rc.top, w, h, Some(mem), 0, 0, SRCCOPY);
    SelectObject(mem, old);
    let _ = DeleteObject(HGDIOBJ(bmp.0));
    let _ = DeleteDC(mem);
}
