//! 搜索窗口（原生 Win32：搜索框 EDIT + 盘符下拉 COMBOBOX + 结果列表 LISTBOX）。
//!
//! 全局热键弹出/隐藏；输入即时搜索（百万文件约 1ms，可在 UI 线程同步搜索）；
//! 盘符下拉可限定「指定盘符搜索」或搜全部盘；回车/双击打开选中项；Esc 隐藏。
//! 关闭按钮只隐藏不销毁，保持后台常驻。

use std::cell::RefCell;
use std::ffi::c_void;
use std::sync::Arc;

use parking_lot::RwLock;
use ss_core::{Catalog, SearchResult};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    GetStockObject, GetSysColorBrush, COLOR_WINDOW, DEFAULT_GUI_FONT, HFONT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{SetFocus, VK_DOWN, VK_ESCAPE, VK_RETURN};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::wide;

pub type SharedCatalog = Arc<RwLock<Option<Catalog>>>;

/// 索引构建完成后由后台线程 PostMessage 通知（回填盘符下拉）。
pub const WM_APP_INDEX_READY: u32 = WM_APP + 2;

const EDIT_ID: u32 = 101;
const LIST_ID: u32 = 102;
const COMBO_ID: u32 = 103;

// 通知码 / 消息（避免 windows crate 常量 feature 门控差异，直接用数值）
const EN_CHANGE: u32 = 0x0300;
const LBN_DBLCLK: u32 = 2;
const CBN_SELCHANGE: u32 = 1;
const EM_SETSEL: u32 = 0x00B1;
const CB_ADDSTRING: u32 = 0x0143;
const CB_RESETCONTENT: u32 = 0x014B;
const CB_GETCURSEL: u32 = 0x0147;
const CB_SETCURSEL: u32 = 0x014E;

// 窗口/控件样式（用原始数值构造 newtype，规避各常量类型不一致）
mod st {
    pub const WS_CHILD: u32 = 0x4000_0000;
    pub const WS_VISIBLE: u32 = 0x1000_0000;
    pub const WS_VSCROLL: u32 = 0x0020_0000;
    pub const WS_TABSTOP: u32 = 0x0001_0000;
    pub const WS_OVERLAPPEDWINDOW: u32 = 0x00CF_0000;
    pub const ES_AUTOHSCROLL: u32 = 0x0080;
    pub const LBS_NOTIFY: u32 = 0x0001;
    pub const LBS_HASSTRINGS: u32 = 0x0040;
    pub const LBS_NOINTEGRALHEIGHT: u32 = 0x0100;
    pub const CBS_DROPDOWNLIST: u32 = 0x0003;
    pub const WS_EX_CLIENTEDGE: u32 = 0x0000_0200;
    pub const WS_EX_TOOLWINDOW: u32 = 0x0000_0080;
}

struct Ui {
    search_hwnd: HWND,
    edit: HWND,
    list: HWND,
    combo: HWND,
    results: Vec<SearchResult>,
    drives: Vec<char>,
    catalog: SharedCatalog,
}

thread_local! {
    static UI: RefCell<Option<Ui>> = const { RefCell::new(None) };
}

/// 取出窗口句柄（Copy）并立即释放借用，避免在会重入窗口过程的 Win32 调用
/// （ShowWindow/MoveWindow/SetFocus 等会同步派发 WM_SIZE/WM_ACTIVATE）期间
/// 还持有 RefCell 借用，否则双重借用 panic。
fn handles() -> Option<(HWND, HWND, HWND, HWND)> {
    UI.with(|c| {
        c.borrow()
            .as_ref()
            .map(|ui| (ui.search_hwnd, ui.edit, ui.list, ui.combo))
    })
}

#[inline]
fn lo_word(x: u32) -> u32 {
    x & 0xFFFF
}
#[inline]
fn hi_word(x: u32) -> u32 {
    (x >> 16) & 0xFFFF
}

fn send(hwnd: HWND, msg: u32, w: usize, l: isize) -> isize {
    unsafe { SendMessageW(hwnd, msg, Some(WPARAM(w)), Some(LPARAM(l))).0 }
}

fn add_text(ctrl: HWND, msg: u32, text: &str) {
    let w = wide(text);
    send(ctrl, msg, 0, w.as_ptr() as isize);
}

/// 创建搜索窗口（隐藏），保存到线程局部状态。
pub fn init(catalog: SharedCatalog) -> windows::core::Result<HWND> {
    unsafe {
        let hinstance = HINSTANCE(GetModuleHandleW(None)?.0);
        let class = w!("SaveSearchSearchWnd");

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
            WINDOW_EX_STYLE(st::WS_EX_TOOLWINDOW),
            class,
            w!("SaveSearch 搜索"),
            WINDOW_STYLE(st::WS_OVERLAPPEDWINDOW),
            0,
            0,
            760,
            480,
            None,
            None,
            Some(hinstance),
            None,
        )?;

        let edit = CreateWindowExW(
            WINDOW_EX_STYLE(st::WS_EX_CLIENTEDGE),
            w!("EDIT"),
            PCWSTR::null(),
            WINDOW_STYLE(st::WS_CHILD | st::WS_VISIBLE | st::WS_TABSTOP | st::ES_AUTOHSCROLL),
            0,
            0,
            10,
            10,
            Some(hwnd),
            Some(HMENU(EDIT_ID as usize as *mut c_void)),
            Some(hinstance),
            None,
        )?;

        let combo = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("COMBOBOX"),
            PCWSTR::null(),
            WINDOW_STYLE(
                st::WS_CHILD | st::WS_VISIBLE | st::WS_TABSTOP | st::WS_VSCROLL | st::CBS_DROPDOWNLIST,
            ),
            0,
            0,
            10,
            240,
            Some(hwnd),
            Some(HMENU(COMBO_ID as usize as *mut c_void)),
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
            10,
            10,
            Some(hwnd),
            Some(HMENU(LIST_ID as usize as *mut c_void)),
            Some(hinstance),
            None,
        )?;

        let font = HFONT(GetStockObject(DEFAULT_GUI_FONT).0);
        for ctrl in [edit, combo, list] {
            send(ctrl, WM_SETFONT, font.0 as usize, 1);
        }

        // 下拉先放一个占位项，索引建完后回填
        add_text(combo, CB_ADDSTRING, "全部盘");
        send(combo, CB_SETCURSEL, 0, 0);

        UI.with(|c| {
            *c.borrow_mut() = Some(Ui {
                search_hwnd: hwnd,
                edit,
                list,
                combo,
                results: Vec::new(),
                drives: Vec::new(),
                catalog,
            });
        });
        layout(hwnd);
        Ok(hwnd)
    }
}

unsafe fn layout(hwnd: HWND) {
    let mut rc = RECT::default();
    if GetClientRect(hwnd, &mut rc).is_ok() {
        let w = rc.right - rc.left;
        let h = rc.bottom - rc.top;
        let m = 8;
        let eh = 26;
        let cw = 84;
        if let Some((_s, edit, list, combo)) = handles() {
            let edit_w = w - 2 * m - cw - 6;
            let _ = MoveWindow(edit, m, m, edit_w, eh, true);
            let _ = MoveWindow(combo, m + edit_w + 6, m, cw, 240, true);
            let _ = MoveWindow(list, m, m + eh + 6, w - 2 * m, h - (m + eh + 6) - m, true);
        }
    }
}

unsafe fn center(hwnd: HWND, w: i32, h: i32) {
    let sw = GetSystemMetrics(SM_CXSCREEN);
    let sh = GetSystemMetrics(SM_CYSCREEN);
    let _ = MoveWindow(hwnd, (sw - w) / 2, (sh - h) / 3, w, h, true);
}

fn get_text(edit: HWND) -> String {
    let len = send(edit, WM_GETTEXTLENGTH, 0, 0) as usize;
    if len == 0 {
        return String::new();
    }
    let mut buf = vec![0u16; len + 1];
    let n = send(edit, WM_GETTEXT, buf.len(), buf.as_mut_ptr() as isize) as usize;
    String::from_utf16_lossy(&buf[..n.min(buf.len())])
}

/// 索引就绪后回填盘符下拉。
fn populate_drives() {
    let Some((_s, _e, _l, combo)) = handles() else {
        return;
    };
    let cat_arc = UI.with(|c| c.borrow().as_ref().map(|ui| ui.catalog.clone()));
    let Some(cat_arc) = cat_arc else {
        return;
    };
    let letters = {
        let g = cat_arc.read();
        match g.as_ref() {
            Some(cat) => cat.drive_letters(),
            None => return,
        }
    };
    send(combo, CB_RESETCONTENT, 0, 0);
    add_text(combo, CB_ADDSTRING, "全部盘");
    for d in &letters {
        add_text(combo, CB_ADDSTRING, &format!("{}:", d));
    }
    send(combo, CB_SETCURSEL, 0, 0);
    UI.with(|c| {
        if let Some(ui) = c.borrow_mut().as_mut() {
            ui.drives = letters;
        }
    });
    do_search(); // 若窗口已打开则刷新结果
}

fn selected_drive(combo: HWND) -> Option<char> {
    let sel = send(combo, CB_GETCURSEL, 0, 0);
    if sel <= 0 {
        return None; // 0 = 全部盘
    }
    UI.with(|c| {
        c.borrow()
            .as_ref()
            .and_then(|ui| ui.drives.get((sel - 1) as usize).copied())
    })
}

fn do_search() {
    let Some((_s, edit, list, combo)) = handles() else {
        return;
    };
    let query = get_text(edit);
    send(list, LB_RESETCONTENT, 0, 0);
    UI.with(|c| {
        if let Some(ui) = c.borrow_mut().as_mut() {
            ui.results.clear();
        }
    });
    let q = query.trim();
    if q.is_empty() {
        return;
    }
    let drive = selected_drive(combo);
    let cat_arc = UI.with(|c| c.borrow().as_ref().map(|ui| ui.catalog.clone()));
    let Some(cat_arc) = cat_arc else {
        return;
    };
    let guard = cat_arc.read();
    match guard.as_ref() {
        None => add_text(list, LB_ADDSTRING, "（索引构建中，请稍候…）"),
        Some(cat) => {
            let res = cat.search(q, drive, 200);
            if res.is_empty() {
                add_text(list, LB_ADDSTRING, "（无匹配）");
            }
            for r in &res {
                add_text(list, LB_ADDSTRING, &format!("{}    {}", r.name, r.path));
            }
            UI.with(|c| {
                if let Some(ui) = c.borrow_mut().as_mut() {
                    ui.results = res;
                }
            });
        }
    }
}

fn open_selected() {
    let Some((search, _edit, list, _combo)) = handles() else {
        return;
    };
    let sel = send(list, LB_GETCURSEL, 0, 0);
    let idx = if sel < 0 { 0usize } else { sel as usize };
    let item = UI.with(|c| {
        c.borrow()
            .as_ref()
            .and_then(|ui| ui.results.get(idx).map(|r| (r.path.clone(), r.is_dir)))
    });
    if let Some((path, is_dir)) = item {
        unsafe {
            open_path(&path, is_dir);
            let _ = ShowWindow(search, SW_HIDE);
        }
    }
}

unsafe fn open_path(path: &str, is_dir: bool) {
    if is_dir {
        let p = wide(path);
        ShellExecuteW(
            None,
            w!("open"),
            PCWSTR(p.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
    } else {
        let params = wide(&format!("/select,\"{}\"", path));
        ShellExecuteW(
            None,
            w!("open"),
            w!("explorer.exe"),
            PCWSTR(params.as_ptr()),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
    }
}

/// 切换搜索窗显示。
pub fn toggle() {
    let Some((search, edit, _list, _combo)) = handles() else {
        return;
    };
    unsafe {
        if IsWindowVisible(search).as_bool() {
            let _ = ShowWindow(search, SW_HIDE);
        } else {
            center(search, 760, 480);
            let _ = ShowWindow(search, SW_SHOW);
            let _ = SetForegroundWindow(search);
            let _ = SetFocus(Some(edit));
            send(edit, EM_SETSEL, 0, -1);
        }
    }
}

/// 在消息分发前拦截搜索框/列表的回车、Esc、下箭头。返回 true 表示已处理。
pub fn pretranslate(msg: &MSG) -> bool {
    if msg.message != WM_KEYDOWN {
        return false;
    }
    let Some((search, edit, list, _combo)) = handles() else {
        return false;
    };
    if msg.hwnd != edit && msg.hwnd != list {
        return false;
    }
    let vk = (msg.wParam.0 as u32) as u16;
    if vk == VK_ESCAPE.0 {
        unsafe {
            let _ = ShowWindow(search, SW_HIDE);
        }
        true
    } else if vk == VK_RETURN.0 {
        open_selected();
        true
    } else if vk == VK_DOWN.0 && msg.hwnd == edit {
        unsafe {
            let _ = SetFocus(Some(list));
        }
        send(list, LB_SETCURSEL, 0, 0);
        true
    } else {
        false
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_APP_INDEX_READY => {
            populate_drives();
            LRESULT(0)
        }
        WM_COMMAND => {
            let id = lo_word(wp.0 as u32);
            let code = hi_word(wp.0 as u32);
            if id == EDIT_ID && code == EN_CHANGE {
                do_search();
            } else if id == COMBO_ID && code == CBN_SELCHANGE {
                do_search();
            } else if id == LIST_ID && code == LBN_DBLCLK {
                open_selected();
            }
            LRESULT(0)
        }
        WM_SIZE => {
            layout(hwnd);
            LRESULT(0)
        }
        WM_CLOSE => {
            let _ = ShowWindow(hwnd, SW_HIDE);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}
