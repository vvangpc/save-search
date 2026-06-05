//! 保存/打开对话框下方的浮窗：列出可快速跳转的文件夹（收藏 + 已打开 + 最近），
//! 单击即让对话框跳转到该文件夹；右键可收藏/取消收藏。
//!
//! 无边框置顶、`WS_EX_NOACTIVATE`（点击不抢对话框焦点）。点击后浮窗保留，
//! 方便再点别的文件夹；对话框关闭时浮窗才消失。

use std::cell::RefCell;
use std::ffi::c_void;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{GetStockObject, GetSysColorBrush, COLOR_WINDOW, DEFAULT_GUI_FONT, HFONT};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::Win32::Foundation::HINSTANCE;

use crate::wide;

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
    pub const LBS_HASSTRINGS: u32 = 0x0040;
    pub const LBS_NOINTEGRALHEIGHT: u32 = 0x0100;
    pub const WS_EX_TOPMOST: u32 = 0x0000_0008;
    pub const WS_EX_TOOLWINDOW: u32 = 0x0000_0080;
    pub const WS_EX_NOACTIVATE: u32 = 0x0800_0000;
}

struct Popup {
    hwnd: HWND,
    list: HWND,
    dialog: HWND,
    /// (显示文本, 真实路径)
    entries: Vec<(String, String)>,
}

thread_local! {
    static POPUP: RefCell<Option<Popup>> = const { RefCell::new(None) };
}

fn send(hwnd: HWND, msg: u32, w: usize, l: isize) -> isize {
    unsafe { SendMessageW(hwnd, msg, Some(WPARAM(w)), Some(LPARAM(l))).0 }
}

fn popup_handles() -> Option<(HWND, HWND)> {
    POPUP.with(|c| c.borrow().as_ref().map(|p| (p.hwnd, p.list)))
}

/// 创建浮窗（隐藏）。
pub fn init() -> windows::core::Result<()> {
    unsafe {
        let hinstance = HINSTANCE(GetModuleHandleW(None)?.0);
        let class = w!("SaveSearchDlgPopup");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance,
            lpszClassName: class,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            hbrBackground: GetSysColorBrush(COLOR_WINDOW),
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
            300,
            120,
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
                    | st::LBS_HASSTRINGS
                    | st::LBS_NOINTEGRALHEIGHT,
            ),
            0,
            0,
            300,
            120,
            Some(hwnd),
            Some(HMENU(LIST_ID as usize as *mut c_void)),
            Some(hinstance),
            None,
        )?;

        let font = HFONT(GetStockObject(DEFAULT_GUI_FONT).0);
        send(list, WM_SETFONT, font.0 as usize, 1);

        POPUP.with(|c| {
            *c.borrow_mut() = Some(Popup {
                hwnd,
                list,
                dialog: HWND(std::ptr::null_mut()),
                entries: Vec::new(),
            });
        });
        Ok(())
    }
}

/// 在 `dialog` 下方显示浮窗。`entries` 为 (显示文本, 真实路径)。
pub fn show_for(dialog: HWND, entries: Vec<(String, String)>) {
    if entries.is_empty() {
        hide();
        return;
    }
    let Some((hwnd, list)) = popup_handles() else {
        return;
    };
    send(list, LB_RESETCONTENT, 0, 0);
    for (disp, _) in &entries {
        let w = wide(disp);
        send(list, LB_ADDSTRING, 0, w.as_ptr() as isize);
    }
    POPUP.with(|c| {
        if let Some(p) = c.borrow_mut().as_mut() {
            p.dialog = dialog;
            p.entries = entries;
        }
    });
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
        let item_h = send(list, LB_GETITEMHEIGHT, 0, 0).max(16) as i32;
        let w = (r.right - r.left).clamp(280, 620);
        let h = (count.min(14) * item_h + 6).clamp(40, 360);
        let x = r.left;
        let y = r.bottom + 2;
        let _ = MoveWindow(hwnd, x, y, w, h, true);
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

fn navigate_selected() {
    let (dialog, path) = match POPUP.with(|c| {
        c.borrow().as_ref().and_then(|p| {
            let sel = send(p.list, LB_GETCURSEL, 0, 0);
            if sel < 0 {
                return None;
            }
            p.entries.get(sel as usize).map(|e| (p.dialog, e.1.clone()))
        })
    }) {
        Some(t) => t,
        None => return,
    };
    if dialog.0.is_null() {
        return;
    }
    // 导航到该文件夹，并记录为最近位置；浮窗保留以便再换。
    ss_shell::navigate_dialog(dialog, &path);
    ss_config::record_recent(&path);
}

/// 右键：对选中项进行 收藏/取消收藏。
unsafe fn show_context_menu(popup_hwnd: HWND, x: i32, y: i32) {
    let (list, path) = match POPUP.with(|c| {
        c.borrow().as_ref().and_then(|p| {
            let sel = send(p.list, LB_GETCURSEL, 0, 0);
            if sel < 0 {
                return None;
            }
            p.entries.get(sel as usize).map(|e| (p.list, e.1.clone()))
        })
    }) {
        Some(t) => t,
        None => return,
    };
    let _ = list;

    let Ok(menu) = CreatePopupMenu() else {
        return;
    };
    let fav = ss_config::is_favorite(&path);
    let text = wide(if fav { "移出收藏" } else { "添加到收藏 ★" });
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

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
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
            // 若坐标为 (-1,-1)（键盘菜单键）则用窗口左上角
            let (mx, my) = if x == -1 && y == -1 {
                let mut r = RECT::default();
                let _ = GetWindowRect(hwnd, &mut r);
                (r.left + 20, r.top + 20)
            } else {
                (x, y)
            };
            let _ = POINT { x: mx, y: my };
            show_context_menu(hwnd, mx, my);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}
