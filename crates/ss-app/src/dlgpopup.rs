//! 保存/打开对话框下方的浮窗：自绘双行（文件夹图标 + 名称 + 灰色路径 + 收藏/最近标记）。
//!
//! 无边框置顶、`WS_EX_NOACTIVATE`（点击不抢对话框焦点）。单击导航且保留浮窗，对话框关闭才消失。

use std::cell::RefCell;
use std::ffi::c_void;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateFontW, DrawTextW, FillRect, SelectObject, SetBkMode, SetTextColor, CLEARTYPE_QUALITY,
    CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DRAW_TEXT_FORMAT, DT_END_ELLIPSIS, DT_LEFT, DT_NOPREFIX,
    DT_RIGHT, DT_SINGLELINE, DT_VCENTER, HDC, HFONT, HGDIOBJ, OUT_DEFAULT_PRECIS, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Controls::{DRAWITEMSTRUCT, MEASUREITEMSTRUCT, ODS_SELECTED};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::{icons, theme};

/// 浮窗条目来源（决定右侧标记）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    Favorite,
    Open,
    Recent,
}

const LIST_ID: u32 = 201;
const LBN_SELCHANGE: u32 = 1;
const IDM_FAV: u32 = 1;

mod st {
    pub const WS_POPUP: u32 = 0x8000_0000;
    pub const WS_BORDER: u32 = 0x0080_0000;
    pub const WS_CHILD: u32 = 0x4000_0000;
    pub const WS_VISIBLE: u32 = 0x1000_0000;
    pub const WS_VSCROLL: u32 = 0x0020_0000;
    pub const LBS_NOTIFY: u32 = 0x0001;
    pub const LBS_OWNERDRAWFIXED: u32 = 0x0010;
    pub const LBS_NOINTEGRALHEIGHT: u32 = 0x0100;
    pub const WS_EX_TOPMOST: u32 = 0x0000_0008;
    pub const WS_EX_TOOLWINDOW: u32 = 0x0000_0080;
    pub const WS_EX_NOACTIVATE: u32 = 0x0800_0000;
}

struct Popup {
    hwnd: HWND,
    list: HWND,
    dialog: HWND,
    entries: Vec<(EntryKind, String)>,
    font_name: HFONT,
    font_path: HFONT,
    item_h: i32,
    dpi: i32,
}

thread_local! {
    static POPUP: RefCell<Option<Popup>> = const { RefCell::new(None) };
}

fn send(hwnd: HWND, msg: u32, w: usize, l: isize) -> isize {
    unsafe { SendMessageW(hwnd, msg, Some(WPARAM(w)), Some(LPARAM(l))).0 }
}

unsafe fn make_font(px: i32, weight: i32) -> HFONT {
    CreateFontW(
        -px, 0, 0, 0, weight, 0, 0, 0, DEFAULT_CHARSET, OUT_DEFAULT_PRECIS, CLIP_DEFAULT_PRECIS,
        CLEARTYPE_QUALITY, 0, w!("Segoe UI"),
    )
}

fn sc(v: i32) -> i32 {
    let dpi = POPUP.with(|c| c.borrow().as_ref().map(|p| p.dpi).unwrap_or(96));
    v * dpi / 96
}

pub fn init() -> windows::core::Result<()> {
    unsafe {
        let hinstance = HINSTANCE(GetModuleHandleW(None)?.0);
        let class = w!("SaveSearchDlgPopup");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance,
            lpszClassName: class,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            ..Default::default()
        };
        RegisterClassW(&wc);

        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(st::WS_EX_TOPMOST | st::WS_EX_TOOLWINDOW | st::WS_EX_NOACTIVATE),
            class,
            w!(""),
            WINDOW_STYLE(st::WS_POPUP | st::WS_BORDER),
            0,
            0,
            320,
            160,
            None,
            None,
            Some(hinstance),
            None,
        )?;

        let list = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("LISTBOX"),
            PCWSTR::null(),
            WINDOW_STYLE(
                st::WS_CHILD
                    | st::WS_VISIBLE
                    | st::WS_VSCROLL
                    | st::LBS_NOTIFY
                    | st::LBS_OWNERDRAWFIXED
                    | st::LBS_NOINTEGRALHEIGHT,
            ),
            0,
            0,
            320,
            160,
            Some(hwnd),
            Some(HMENU(LIST_ID as usize as *mut c_void)),
            Some(hinstance),
            None,
        )?;

        let dpi = GetDpiForWindow(hwnd).max(96) as i32;
        let font_name = make_font(dpi * 14 / 96, 600);
        let font_path = make_font(dpi * 11 / 96, 400);

        POPUP.with(|c| {
            *c.borrow_mut() = Some(Popup {
                hwnd,
                list,
                dialog: HWND(std::ptr::null_mut()),
                entries: Vec::new(),
                font_name,
                font_path,
                item_h: dpi * 40 / 96,
                dpi,
            });
        });
        Ok(())
    }
}

fn popup_handles() -> Option<(HWND, HWND)> {
    POPUP.with(|c| c.borrow().as_ref().map(|p| (p.hwnd, p.list)))
}

/// 在 `dialog` 下方显示浮窗。`entries` 为 (来源, 路径)。
pub fn show_for(dialog: HWND, entries: Vec<(EntryKind, String)>) {
    if entries.is_empty() {
        hide();
        return;
    }
    let Some((hwnd, list)) = popup_handles() else {
        return;
    };
    send(list, LB_RESETCONTENT, 0, 0);
    for _ in 0..entries.len() {
        send(list, LB_ADDSTRING, 0, 0);
    }
    POPUP.with(|c| {
        if let Some(p) = c.borrow_mut().as_mut() {
            p.dialog = dialog;
            p.entries = entries;
        }
    });
    unsafe {
        theme::apply_window(hwnd, &theme::current());
    }
    reposition(dialog, hwnd, list);
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOWNA);
    }
}

fn reposition(dialog: HWND, hwnd: HWND, list: HWND) {
    unsafe {
        let mut r = RECT::default();
        if GetWindowRect(dialog, &mut r).is_err() {
            return;
        }
        let count = send(list, LB_GETCOUNT, 0, 0).max(1) as i32;
        let item_h = POPUP.with(|c| c.borrow().as_ref().map(|p| p.item_h).unwrap_or(40));
        let w = (r.right - r.left).clamp(sc(300), sc(640));
        let h = (count.min(12) * item_h + sc(6)).clamp(sc(44), sc(420));
        // 相对对话框水平居中
        let x = r.left + (r.right - r.left - w) / 2;
        let _ = MoveWindow(hwnd, x, r.bottom + sc(2), w, h, true);
        let _ = MoveWindow(list, 0, 0, w, h, true);
    }
}

pub fn on_dialog_moved(dialog: HWND) {
    let cur = POPUP.with(|c| c.borrow().as_ref().map(|p| p.dialog));
    if cur == Some(dialog) {
        if let Some((hwnd, list)) = popup_handles() {
            reposition(dialog, hwnd, list);
        }
    }
}

/// 前台窗口变化：仅当关联对话框在前台时显示浮窗，否则隐藏（避免对话框被遮挡后浮窗仍霸屏置顶）。
pub fn on_foreground(fg: HWND) {
    let (hwnd, list, dlg) = match POPUP.with(|c| {
        c.borrow().as_ref().map(|p| (p.hwnd, p.list, p.dialog))
    }) {
        Some(t) => t,
        None => return,
    };
    if dlg.0.is_null() {
        return;
    }
    unsafe {
        if fg == dlg {
            reposition(dlg, hwnd, list);
            let _ = ShowWindow(hwnd, SW_SHOWNA);
        } else if fg != hwnd {
            let _ = ShowWindow(hwnd, SW_HIDE);
        }
    }
}

pub fn current_dialog() -> Option<HWND> {
    POPUP.with(|c| {
        c.borrow().as_ref().and_then(|p| {
            if p.dialog.0.is_null() {
                None
            } else {
                Some(p.dialog)
            }
        })
    })
}

pub fn hide() {
    if let Some((hwnd, _)) = popup_handles() {
        unsafe {
            let _ = ShowWindow(hwnd, SW_HIDE);
        }
        POPUP.with(|c| {
            if let Some(p) = c.borrow_mut().as_mut() {
                p.dialog = HWND(std::ptr::null_mut());
            }
        });
    }
}

fn entry_at(idx: usize) -> Option<(EntryKind, String)> {
    POPUP.with(|c| c.borrow().as_ref().and_then(|p| p.entries.get(idx).cloned()))
}

fn navigate_selected() {
    let (dialog, list) = match POPUP.with(|c| c.borrow().as_ref().map(|p| (p.dialog, p.list))) {
        Some(t) => t,
        None => return,
    };
    let sel = send(list, LB_GETCURSEL, 0, 0);
    if sel < 0 {
        return;
    }
    let Some((_, path)) = entry_at(sel as usize) else {
        return;
    };
    if dialog.0.is_null() {
        return;
    }
    ss_shell::navigate_dialog(dialog, &path);
    ss_config::record_recent(&path);
}

unsafe fn show_context_menu(popup_hwnd: HWND, x: i32, y: i32) {
    let list = POPUP.with(|c| c.borrow().as_ref().map(|p| p.list));
    let Some(list) = list else { return };
    let sel = send(list, LB_GETCURSEL, 0, 0);
    if sel < 0 {
        return;
    }
    let Some((_, path)) = entry_at(sel as usize) else {
        return;
    };

    let Ok(menu) = CreatePopupMenu() else { return };
    let fav = ss_config::is_favorite(&path);
    let text = crate::wide(if fav { "移出收藏" } else { "添加到收藏 ★" });
    let _ = AppendMenuW(menu, MF_STRING, IDM_FAV as usize, PCWSTR(text.as_ptr()));
    let _ = SetForegroundWindow(popup_hwnd);
    let cmd = TrackPopupMenu(
        menu,
        TPM_RETURNCMD | TPM_RIGHTBUTTON,
        x,
        y,
        Some(0),
        popup_hwnd,
        None,
    );
    let _ = DestroyMenu(menu);
    if cmd.0 == IDM_FAV as i32 {
        ss_config::toggle_favorite(&path);
    }
}

// ---- 自绘 ----

unsafe fn draw_text(hdc: HDC, s: &str, rect: &mut RECT, color: u32, font: HFONT, flags: DRAW_TEXT_FORMAT) {
    if s.is_empty() {
        return;
    }
    SelectObject(hdc, HGDIOBJ(font.0));
    SetTextColor(hdc, COLORREF(color));
    let mut buf: Vec<u16> = s.encode_utf16().collect();
    DrawTextW(hdc, &mut buf, rect, flags);
}

fn leaf_of(path: &str) -> &str {
    let p = path.trim_end_matches('\\');
    match p.rfind('\\') {
        Some(i) => &p[i + 1..],
        None => p,
    }
}

unsafe fn draw_item(dis: &DRAWITEMSTRUCT) {
    let t = theme::current();
    let selected = dis.itemState.0 & ODS_SELECTED.0 != 0;
    let bg = if selected { t.sel_bg } else { t.bg };
    let fg = if selected { t.sel_fg } else { t.fg };
    let path_fg = if selected { t.sel_fg } else { t.path_fg };
    let entry = if dis.itemID == u32::MAX {
        None
    } else {
        entry_at(dis.itemID as usize)
    };
    let (fn_name, fn_path) =
        POPUP.with(|c| c.borrow().as_ref().map(|p| (p.font_name, p.font_path)).unwrap());

    crate::paint::buffered(dis.hDC, dis.rcItem, |hdc, rc| unsafe {
        FillRect(hdc, &rc, theme::solid_brush(bg));
        let Some((kind, path)) = &entry else {
            return;
        };
        let pad = sc(8);
        let icon = sc(18);
        let icy = rc.top + (rc.bottom - rc.top - icon) / 2;
        if let Some(hicon) = icons::icon_for("folder", true) {
            let _ = DrawIconEx(hdc, rc.left + pad, icy, hicon, icon, icon, 0, None, DI_NORMAL);
        }
        SetBkMode(hdc, TRANSPARENT);
        let tx = rc.left + pad + icon + pad;
        let tag_w = sc(56);
        let mut r1 = RECT {
            left: tx,
            top: rc.top + sc(3),
            right: rc.right - tag_w,
            bottom: rc.top + (rc.bottom - rc.top) / 2 + sc(2),
        };
        draw_text(hdc, leaf_of(path), &mut r1, fg, fn_name, DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS);
        let mut r2 = RECT {
            left: tx,
            top: rc.top + (rc.bottom - rc.top) / 2 - sc(2),
            right: rc.right - pad,
            bottom: rc.bottom - sc(2),
        };
        draw_text(hdc, path, &mut r2, path_fg, fn_path, DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS);

        let tag = match kind {
            EntryKind::Favorite => "★ 收藏",
            EntryKind::Recent => "最近",
            EntryKind::Open => "",
        };
        if !tag.is_empty() {
            let color = if matches!(kind, EntryKind::Favorite) {
                if selected { t.sel_fg } else { t.accent }
            } else {
                path_fg
            };
            let mut rt = RECT {
                left: rc.right - tag_w,
                top: rc.top,
                right: rc.right - sc(6),
                bottom: rc.bottom,
            };
            draw_text(hdc, tag, &mut rt, color, fn_path, DT_RIGHT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX);
        }
    });
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_MEASUREITEM => {
            let mis = &mut *(lp.0 as *mut MEASUREITEMSTRUCT);
            let h = POPUP.with(|c| c.borrow().as_ref().map(|p| p.item_h).unwrap_or(40));
            mis.itemHeight = h as u32;
            LRESULT(1)
        }
        WM_DRAWITEM => {
            let dis = &*(lp.0 as *const DRAWITEMSTRUCT);
            if dis.CtlID == LIST_ID {
                draw_item(dis);
            }
            LRESULT(1)
        }
        WM_CTLCOLORLISTBOX => {
            let t = theme::current();
            let hdc = HDC(wp.0 as *mut c_void);
            SetTextColor(hdc, COLORREF(t.fg));
            SetBkMode(hdc, TRANSPARENT);
            return LRESULT(theme::solid_brush(t.bg).0 as isize);
        }
        WM_COMMAND => {
            let code = ((wp.0 as u32) >> 16) & 0xFFFF;
            let id = (wp.0 as u32) & 0xFFFF;
            if id == LIST_ID && code == LBN_SELCHANGE {
                navigate_selected();
            }
            LRESULT(0)
        }
        WM_CONTEXTMENU => {
            let x = (lp.0 & 0xFFFF) as i16 as i32;
            let y = ((lp.0 >> 16) & 0xFFFF) as i16 as i32;
            let (mx, my) = if x == -1 && y == -1 {
                let mut r = RECT::default();
                let _ = GetWindowRect(hwnd, &mut r);
                (r.left + 20, r.top + 20)
            } else {
                (x, y)
            };
            show_context_menu(hwnd, mx, my);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}
