//! 保存/打开对话框下方的浮窗：名称在上、完整路径在独立的浅色带中，右侧标明来源。
//!
//! 无边框置顶、`WS_EX_NOACTIVATE`（点击不抢对话框焦点）。单击导航且保留浮窗，对话框关闭才消失。
//!
//! 收藏和最近位置立即显示。已打开的文件夹和路径是否存在在后台线程读取，
//! 这样 Explorer 无响应或网络路径卡住时，浮窗仍能显示。

use std::cell::RefCell;
use std::ffi::c_void;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use ss_shell::NavError;
use ss_ui_model::{
    initial_entries, leaf_of, mark_dead, merge_open, without_loading, EntrySource,
    PopupEntry,
};

use windows::core::{w, PCWSTR, PWSTR};
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateRoundRectRgn, DeleteObject, EndPaint, FillRect, GetMonitorInfoW,
    InvalidateRect, MonitorFromWindow, ScreenToClient, SetBkMode, SetTextColor,
    SetWindowRgn, DT_END_ELLIPSIS, DT_LEFT, DT_NOPREFIX, DT_PATH_ELLIPSIS, DT_RIGHT,
    DT_SINGLELINE, DT_VCENTER, HDC, HFONT, HGDIOBJ, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    PAINTSTRUCT, TRANSPARENT,
};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Controls::{
    InitCommonControlsEx, INITCOMMONCONTROLSEX, ICC_WIN95_CLASSES,
    DRAWITEMSTRUCT, MEASUREITEMSTRUCT, ODS_SELECTED, TOOLTIPS_CLASSW, TTTOOLINFOW,
    TTF_SUBCLASS, TTM_ADDTOOLW, TTM_DELTOOLW, TTM_NEWTOOLRECTW, TTM_SETMAXTIPWIDTH,
    TTS_ALWAYSTIP, TTS_NOPREFIX, TTM_POP,
};
use windows::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::paint::text as draw_text;
use crate::theme;

const WM_APP_OPEN_FOLDERS: u32 = WM_APP + 20;
const WM_APP_DEAD_PATHS: u32 = WM_APP + 21;
const LOADING_TIMER: usize = 1;
/// 读取已打开文件夹超过这个时间就不再显示“正在读取”。
const LOADING_TIMEOUT_MS: u32 = 3000;
/// 同一时间只运行一个读取线程。Explorer 卡住时不会不断创建新线程。
static READER_RUNNING: AtomicBool = AtomicBool::new(false);

const LIST_ID: u32 = 201;
/// 行高和列表四周的留白（96 DPI）。
const ITEM_H: i32 = 56;
const INSET: i32 = 6;
const MAX_WIDTH: i32 = 620;
const MAX_VISIBLE_ROWS: i32 = 6;
const LBN_SELCHANGE: u32 = 1;
const IDM_FAV: u32 = 1;
const LB_SETITEMHEIGHT: u32 = 0x01A0;
const LB_ITEMFROMPOINT: u32 = 0x01A9;

mod st {
    pub const WS_POPUP: u32 = 0x8000_0000;
    pub const WS_CLIPCHILDREN: u32 = 0x0200_0000;
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
    entries: Vec<PopupEntry>,
    /// 上次读到的已打开文件夹，下次显示时先用它，避免列表跳动。
    open_cache: Vec<String>,
    colors: ss_config::PopupColors,
    tooltip: HWND,
    /// 系统提示框引用这些 UTF-16 缓冲；删除工具后才能释放。
    tooltip_text: Vec<Vec<u16>>,
    /// 跳转失败的提示，显示在列表上方。
    notice: Option<String>,
    font_name: HFONT,
    font_path: HFONT,
    item_h: i32,
    dpi: i32,
    /// Windows 11 由系统画圆角；更早的系统用窗口区域裁剪。
    rounded_by_system: bool,
}

thread_local! {
    static POPUP: RefCell<Option<Popup>> = const { RefCell::new(None) };
}

fn send(hwnd: HWND, msg: u32, w: usize, l: isize) -> isize {
    unsafe { SendMessageW(hwnd, msg, Some(WPARAM(w)), Some(LPARAM(l))).0 }
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
            // 阴影让浮窗和对话框分开
            style: CS_DROPSHADOW,
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
            WINDOW_STYLE(st::WS_POPUP | st::WS_CLIPCHILDREN),
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
        let font_name = crate::paint::ui_font(dpi * 15 / 96, 600);
        let font_path = crate::paint::ui_font(dpi * 12 / 96, 400);
        let _ = InitCommonControlsEx(&INITCOMMONCONTROLSEX {
            dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32, dwICC: ICC_WIN95_CLASSES,
        });
        let tooltip = CreateWindowExW(
            WINDOW_EX_STYLE(st::WS_EX_TOPMOST | st::WS_EX_NOACTIVATE),
            TOOLTIPS_CLASSW, PCWSTR::null(), WINDOW_STYLE(st::WS_POPUP | TTS_ALWAYSTIP | TTS_NOPREFIX),
            0, 0, 0, 0, Some(hwnd), None, Some(hinstance), None,
        )?;
        send(tooltip, TTM_SETMAXTIPWIDTH, 0, (dpi * 620 / 96) as isize);
        let _ = SetWindowSubclass(list, Some(list_subclass), 1, 0);

        let rounded_by_system = theme::round_corners(hwnd);
        POPUP.with(|c| {
            *c.borrow_mut() = Some(Popup {
                hwnd,
                list,
                dialog: HWND(std::ptr::null_mut()),
                entries: Vec::new(),
                open_cache: Vec::new(),
                colors: ss_config::PopupColors::default(),
                tooltip,
                tooltip_text: Vec::new(),
                notice: None,
                font_name,
                font_path,
                item_h: dpi * ITEM_H / 96,
                dpi,
                rounded_by_system,
            });
        });
        // LBS_OWNERDRAWFIXED 的 WM_MEASUREITEM 在控件创建时发出，彼时 POPUP 尚未
        // 初始化、行高取了 96dpi 兜底值 40，高 DPI 下与 reposition 的窗口尺寸计算
        // 不一致（条目偏矮、底部留白）；这里用真实 DPI 重设。
        send(list, LB_SETITEMHEIGHT, 0, (dpi * ITEM_H / 96) as isize);
        Ok(())
    }
}

fn popup_handles() -> Option<(HWND, HWND)> {
    POPUP.with(|c| c.borrow().as_ref().map(|p| (p.hwnd, p.list)))
}

/// 每行有独立的提示区域，悬停能查看完整名称和路径。
fn rebuild_tooltips() {
    let Some((tooltip, list, old_count, entries)) = POPUP.with(|c| c.borrow().as_ref()
        .map(|p| (p.tooltip, p.list, p.tooltip_text.len(), p.entries.clone()))) else { return };
    send(tooltip, TTM_POP, 0, 0);
    for i in 0..old_count {
        let tool = TTTOOLINFOW { cbSize: std::mem::size_of::<TTTOOLINFOW>() as u32, hwnd: list, uId: i + 1, ..Default::default() };
        send(tooltip, TTM_DELTOOLW, 0, &tool as *const _ as isize);
    }
    let tools = POPUP.with(|c| {
        let mut b = c.borrow_mut();
        let Some(p) = b.as_mut() else { return Vec::new() };
        p.tooltip_text = entries.iter().map(|e| crate::wide(&format!("{}\n{}", leaf_of(&e.path), e.path))).collect();
        p.tooltip_text.iter_mut().enumerate().filter(|(i, _)| !entries[*i].path.is_empty()).map(|(i, text)| {
            TTTOOLINFOW {
                cbSize: std::mem::size_of::<TTTOOLINFOW>() as u32,
                uFlags: TTF_SUBCLASS,
                hwnd: list,
                uId: i + 1,
                lpszText: PWSTR(text.as_mut_ptr()),
                ..Default::default()
            }
        }).collect::<Vec<_>>()
    });
    for tool in tools { send(tooltip, TTM_ADDTOOLW, 0, &tool as *const _ as isize); }
    update_tooltip_rects();
}

fn update_tooltip_rects() {
    let Some((tooltip, list, count)) = POPUP.with(|c| c.borrow().as_ref()
        .map(|p| (p.tooltip, p.list, p.tooltip_text.len()))) else { return };
    let mut client = RECT::default();
    let _ = unsafe { GetClientRect(list, &mut client) };
    for i in 0..count {
        let mut rect = RECT::default();
        if send(list, LB_GETITEMRECT, i, &mut rect as *mut _ as isize) < 0 { continue; }
        if rect.bottom <= 0 || rect.top >= client.bottom { rect = RECT::default(); }
        else { rect.top = rect.top.max(0); rect.bottom = rect.bottom.min(client.bottom); }
        let tool = TTTOOLINFOW { cbSize: std::mem::size_of::<TTTOOLINFOW>() as u32, hwnd: list, uId: i + 1, rect, ..Default::default() };
        send(tooltip, TTM_NEWTOOLRECTW, 0, &tool as *const _ as isize);
    }
}

unsafe extern "system" fn list_subclass(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM, _id: usize, _data: usize) -> LRESULT {
    let result = DefSubclassProc(hwnd, msg, wp, lp);
    if matches!(msg, WM_VSCROLL | WM_MOUSEWHEEL | WM_KEYDOWN) {
        if let Some(tip) = POPUP.with(|c| c.borrow().as_ref().map(|p| p.tooltip)) { send(tip, TTM_POP, 0, 0); }
    }
    if matches!(msg, WM_VSCROLL | WM_MOUSEWHEEL | WM_KEYDOWN | WM_SIZE | LB_SETTOPINDEX | LB_SETCURSEL) {
        update_tooltip_rects();
    }
    result
}

/// 把条目放进列表。内容没有变化时不重灌，保留选中和滚动位置，也不闪烁。
fn set_entries(entries: Vec<PopupEntry>) {
    let Some((_, list)) = popup_handles() else {
        return;
    };
    let old_paths = POPUP.with(|c| {
        let mut b = c.borrow_mut();
        let p = b.as_mut()?;
        if p.entries == entries {
            return None;
        }
        let old = p.entries.iter().map(|e| e.path.clone()).collect::<Vec<String>>();
        p.entries = entries;
        Some(old)
    });
    let Some(old_paths) = old_paths else {
        return;
    };
    let count = POPUP.with(|c| c.borrow().as_ref().map_or(0, |p| p.entries.len()));
    let top = send(list, LB_GETTOPINDEX, 0, 0);
    let sel = send(list, LB_GETCURSEL, 0, 0);
    let sel = usize::try_from(sel).ok().and_then(|i| old_paths.get(i)).and_then(|old| {
        POPUP.with(|c| c.borrow().as_ref().and_then(|p| p.entries.iter().position(|e| &e.path == old)))
    });
    send(list, WM_SETREDRAW, 0, 0);
    send(list, LB_RESETCONTENT, 0, 0);
    for _ in 0..count {
        send(list, LB_ADDSTRING, 0, 0);
    }
    if let Some(i) = sel {
        send(list, LB_SETCURSEL, i, 0);
    }
    send(list, LB_SETTOPINDEX, top.max(0) as usize, 0);
    send(list, WM_SETREDRAW, 1, 0);
    rebuild_tooltips();
    let _ = unsafe { InvalidateRect(Some(list), None, true) };
}

/// 在 `dialog` 下方显示浮窗。收藏和最近位置立即显示，其余内容在后台读取。
pub fn show_for(dialog: HWND) {
    let Some((hwnd, list)) = popup_handles() else {
        return;
    };
    let s = ss_config::load_settings();
    let (same_dialog, open_cache, old_entries) = POPUP.with(|c| {
        c.borrow().as_ref().map_or((false, Vec::new(), Vec::new()), |p| {
            (p.dialog == dialog, p.open_cache.clone(), p.entries.clone())
        })
    });
    let favorites = if s.popup_show_favorites { ss_config::favorites() } else { Vec::new() };
    let recent = if s.popup_show_recent { ss_config::recent() } else { Vec::new() };
    let open = if s.popup_show_open { open_cache } else { Vec::new() };
    let started = start_reader(hwnd, s.popup_show_open, &favorites, &recent);
    // 只有这个对话框第一次显示时才出现“正在读取”，来回切换前台不闪烁
    let loading = started && s.popup_show_open && !same_dialog;
    let mut entries = initial_entries(&favorites, &recent, &open, loading);
    let dead: Vec<String> = old_entries.iter().filter(|e| e.dead).map(|e| e.path.clone()).collect();
    mark_dead(&mut entries, &dead);
    if entries.is_empty() {
        hide();
        return;
    }
    POPUP.with(|c| {
        if let Some(p) = c.borrow_mut().as_mut() {
            p.dialog = dialog;
            p.colors = s.popup_colors;
            if !same_dialog {
                p.notice = None;
            }
        }
    });
    set_entries(entries);
    reposition(dialog, hwnd, list);
    unsafe {
        if loading {
            SetTimer(Some(hwnd), LOADING_TIMER, LOADING_TIMEOUT_MS, None);
        }
        let _ = ShowWindow(hwnd, SW_SHOWNA);
    }
}

/// 把 `Vec<String>` 发回界面线程。发送失败时回收。
fn post_paths(hwnd: isize, msg: u32, paths: Vec<String>) {
    let raw = Box::into_raw(Box::new(paths));
    let target = HWND(hwnd as *mut c_void);
    let posted = unsafe { PostMessageW(Some(target), msg, WPARAM(0), LPARAM(raw as isize)) };
    if posted.is_err() {
        drop(unsafe { Box::from_raw(raw) });
    }
}

/// 启动后台读取。已有读取在进行时返回 false。
fn start_reader(hwnd: HWND, read_open: bool, favorites: &[String], recent: &[String]) -> bool {
    if READER_RUNNING.swap(true, Ordering::AcqRel) {
        return false;
    }
    let hwnd_raw = hwnd.0 as isize;
    let mut check: Vec<String> = favorites.iter().chain(recent).cloned().collect();
    std::thread::spawn(move || {
        let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        if read_open {
            let open: Vec<String> = ss_shell::enumerate_open_folders_by_z_order()
                .into_iter()
                .map(|f| f.path)
                .collect();
            check.extend(open.iter().cloned());
            post_paths(hwnd_raw, WM_APP_OPEN_FOLDERS, open);
        }
        // 网络路径可能很慢，放在最后，不影响已打开文件夹的显示
        let dead: Vec<String> = check.into_iter().filter(|p| !Path::new(p).exists()).collect();
        post_paths(hwnd_raw, WM_APP_DEAD_PATHS, dead);
        READER_RUNNING.store(false, Ordering::Release);
    });
    true
}

fn on_open_folders(open: Vec<String>) {
    let s = ss_config::load_settings();
    let favorites = if s.popup_show_favorites { ss_config::favorites() } else { Vec::new() };
    let recent = if s.popup_show_recent { ss_config::recent() } else { Vec::new() };
    let merged = POPUP.with(|c| {
        c.borrow_mut().as_mut().map(|p| {
            p.open_cache = open.clone();
            let visible_open = if s.popup_show_open { open.as_slice() } else { &[] };
            merge_open(&p.entries, visible_open, &favorites, &recent)
        })
    });
    let Some(merged) = merged else {
        return;
    };
    set_entries(merged);
    relayout();
}

fn on_dead_paths(dead: Vec<String>) {
    let updated = POPUP.with(|c| {
        c.borrow().as_ref().map(|p| {
            let mut e = p.entries.clone();
            mark_dead(&mut e, &dead);
            e
        })
    });
    if let Some(updated) = updated {
        set_entries(updated);
    }
}

/// 条目数量或提示变化后重新计算位置和大小。
fn relayout() {
    let Some(dialog) = current_dialog() else {
        return;
    };
    if let Some((hwnd, list)) = popup_handles() {
        reposition(dialog, hwnd, list);
    }
}

fn reposition(dialog: HWND, hwnd: HWND, list: HWND) {
    unsafe {
        let mut r = RECT::default();
        if GetWindowRect(dialog, &mut r).is_err() {
            return;
        }
        let count = send(list, LB_GETCOUNT, 0, 0).max(1) as i32;
        let item_h = POPUP.with(|c| c.borrow().as_ref().map(|p| p.item_h).unwrap_or(ITEM_H));
        let w = (r.right - r.left).clamp(sc(300), sc(MAX_WIDTH));
        let notice_h = POPUP.with(|c| {
            c.borrow().as_ref().map_or(0, |p| if p.notice.is_some() { sc(30) } else { 0 })
        });
        let inset = sc(INSET);
        let rows = count.min(MAX_VISIBLE_ROWS);
        let chrome_h = inset * 2 + notice_h;
        let mut h = rows * item_h + chrome_h;
        // 相对对话框水平居中
        let mut x = r.left + (r.right - r.left - w) / 2;
        let mut y = r.bottom + sc(2);
        // 优先在对话框下方展示；上方能容纳全部条目时改放上方。
        // 两侧都不够时减少可见条数，按完整行收缩；其余条目保留滚动访问。
        let hmon = MonitorFromWindow(dialog, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if GetMonitorInfoW(hmon, &mut mi).as_bool() {
            let wr = mi.rcWork;
            let work_h = (wr.bottom - wr.top).max(1);
            let gap = sc(2);
            let below = (wr.bottom - r.bottom - gap).clamp(0, work_h);
            let above = (r.top - gap - wr.top).clamp(0, work_h);
            if below >= h {
                y = r.bottom + gap;
            } else if above >= h {
                y = r.top - gap - h;
            } else {
                let below_rows = ((below - chrome_h).max(0) / item_h).min(rows);
                let above_rows = ((above - chrome_h).max(0) / item_h).min(rows);
                let visible_rows = below_rows.max(above_rows);
                if visible_rows > 0 {
                    h = visible_rows * item_h + chrome_h;
                    y = if below_rows >= above_rows { r.bottom + gap } else { r.top - gap - h };
                } else {
                    // 对话框几乎铺满工作区时，至少保留一条可用行，避免浮窗撑满屏幕。
                    h = (item_h + chrome_h).min(work_h);
                }
            }
            y = y.clamp(wr.top, (wr.bottom - h).max(wr.top));
            x = x.clamp(wr.left, (wr.right - w).max(wr.left));
        }
        let _ = MoveWindow(hwnd, x, y, w, h, true);
        let _ = MoveWindow(list, inset, inset + notice_h, w - inset * 2, h - inset * 2 - notice_h, true);
        update_tooltip_rects();
        let by_system = POPUP.with(|c| c.borrow().as_ref().is_some_and(|p| p.rounded_by_system));
        if !by_system {
            let d = sc(10);
            let rgn = CreateRoundRectRgn(0, 0, w + 1, h + 1, d, d);
            SetWindowRgn(hwnd, Some(rgn), true);
        }
        let _ = InvalidateRect(Some(hwnd), None, false);
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
        if let Some(tip) = POPUP.with(|c| c.borrow().as_ref().map(|p| p.tooltip)) { send(tip, TTM_POP, 0, 0); }
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

fn entry_at(idx: usize) -> Option<PopupEntry> {
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
    let Some(entry) = entry_at(sel as usize) else {
        return;
    };
    if dialog.0.is_null() || entry.source == EntrySource::Loading {
        send(list, LB_SETCURSEL, usize::MAX, 0);
        return;
    }
    if entry.dead {
        set_notice(Some("这个位置已不存在。".to_string()));
        send(list, LB_SETCURSEL, usize::MAX, 0);
        return;
    }
    match ss_shell::navigate_dialog_checked(dialog, &entry.path) {
        Ok(()) => {
            set_notice(None);
            ss_config::record_recent(&entry.path);
        }
        Err(e) => {
            set_notice(Some(nav_error_text(&e)));
            // 取消选中，这样再点一次同一项可以重试
            send(list, LB_SETCURSEL, usize::MAX, 0);
        }
    }
}

fn nav_error_text(e: &NavError) -> String {
    match e {
        NavError::TargetNotResponding => {
            "无法跳转：目标程序没有响应。文件名没有被改动，可以再点一次重试。".to_string()
        }
        NavError::FilenameBoxNotFound => "无法跳转：这个对话框里找不到文件名框。".to_string(),
        NavError::Automation(m) => format!("无法跳转：{m}"),
    }
}

fn set_notice(notice: Option<String>) {
    let changed = POPUP.with(|c| {
        c.borrow_mut().as_mut().is_some_and(|p| {
            let changed = p.notice != notice;
            p.notice = notice;
            changed
        })
    });
    if changed {
        relayout();
        if let Some((hwnd, _)) = popup_handles() {
            let _ = unsafe { InvalidateRect(Some(hwnd), None, false) };
        }
    }
}

unsafe fn paint_notice(hwnd: HWND) {
    let mut ps = PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut ps);
    let t = theme::current();
    let mut rc = RECT::default();
    let _ = GetClientRect(hwnd, &mut rc);
    FillRect(hdc, &rc, theme::solid_brush(t.bg));
    let state = POPUP.with(|c| c.borrow().as_ref().map(|p| (p.notice.clone(), p.font_path)));
    if let Some((Some(text), font)) = state {
        let inset = sc(INSET);
        let band = RECT { left: rc.left + inset, top: rc.top + inset, right: rc.right - inset, bottom: rc.top + inset + sc(26) };
        crate::paint::round_rect(hdc, band, sc(6), t.err_bg, None, t.bg);
        SetBkMode(hdc, TRANSPARENT);
        let mut tr = RECT { left: band.left + sc(10), right: band.right - sc(10), ..band };
        draw_text(hdc, &text, &mut tr, t.err, font, DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS);
    }
    let _ = EndPaint(hwnd, &ps);
}

unsafe fn show_context_menu(popup_hwnd: HWND, x: i32, y: i32) {
    let list = POPUP.with(|c| c.borrow().as_ref().map(|p| p.list));
    let Some(list) = list else { return };
    // 按鼠标所在的行，而不是当前选中的行
    let mut pt = POINT { x, y };
    let _ = ScreenToClient(list, &mut pt);
    let lparam = ((pt.y as isize & 0xFFFF) << 16) | (pt.x as isize & 0xFFFF);
    let hit = send(list, LB_ITEMFROMPOINT, 0, lparam);
    // 高位为 1 表示不在任何一行上
    if (hit >> 16) & 0xFFFF != 0 {
        return;
    }
    let Some(entry) = entry_at((hit & 0xFFFF) as usize) else {
        return;
    };
    if entry.source == EntrySource::Loading {
        return;
    }
    let path = entry.path;

    let Ok(menu) = CreatePopupMenu() else { return };
    let fav = ss_config::is_favorite(&path);
    let text = crate::wide(if fav { "移出收藏" } else { "添加到收藏" });
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
    // KB135788：NOACTIVATE 浮窗上的弹出菜单需事后 Post 一条消息，否则下次点击外部不收起
    let _ = PostMessageW(Some(popup_hwnd), WM_NULL, WPARAM(0), LPARAM(0));
    let _ = DestroyMenu(menu);
    if cmd.0 == IDM_FAV as i32 {
        ss_config::toggle_favorite(&path);
        // 立即更新星标，不用等下次打开
        if let Some(dialog) = current_dialog() {
            show_for(dialog);
        }
    }
}

// ---- 自绘 ----

/// 任意自定义底色下仍保持文字可读，默认配色继续使用主题文字色。
fn readable_on(bg: u32, preferred: u32) -> u32 {
    let luminance = |color: u32| {
        let channel = |shift: u32| {
            let v = ((color >> shift) & 0xff) as f64 / 255.0;
            if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
        };
        channel(0) * 0.2126 + channel(8) * 0.7152 + channel(16) * 0.0722
    };
    let bg_l = luminance(bg);
    let fg_l = luminance(preferred);
    if (bg_l.max(fg_l) + 0.05) / (bg_l.min(fg_l) + 0.05) >= 4.5 { preferred }
    else if bg_l > 0.179 { 0 } else { 0x00FF_FFFF }
}

fn path_band_color(row: u32, surface: u32) -> u32 {
    let channel = |shift: u32| {
        ((((row >> shift) & 0xff) + 4 * ((surface >> shift) & 0xff)) / 5) << shift
    };
    channel(0) | channel(8) | channel(16)
}

unsafe fn draw_item(dis: &DRAWITEMSTRUCT) {
    let t = theme::current();
    let selected = dis.itemState.0 & ODS_SELECTED.0 != 0;
    let entry = if dis.itemID == u32::MAX {
        None
    } else {
        entry_at(dis.itemID as usize)
    };
    // 兜底空 HFONT（POPUP 未初始化时 DC 用默认字体退化绘制），与其他绘制站点一致，
    // 避免 panic=abort 下裸 unwrap 拉死进程
    let null_font = HFONT(std::ptr::null_mut());
    let (fn_name, fn_path, colors) = POPUP
        .with(|c| c.borrow().as_ref().map(|p| (p.font_name, p.font_path, p.colors)))
        .unwrap_or((null_font, null_font, ss_config::PopupColors::default()));

    crate::paint::buffered(dis.hDC, dis.rcItem, |hdc, rc| unsafe {
        FillRect(hdc, &rc, theme::solid_brush(t.bg));
        let Some(entry) = &entry else {
            return;
        };
        SetBkMode(hdc, TRANSPARENT);
        let pad = sc(10);
        if entry.source == EntrySource::Loading {
            let mut lr = RECT { left: rc.left + pad, top: rc.top, right: rc.right - pad, bottom: rc.bottom };
            draw_text(hdc, "正在读取已打开的资源管理器文件夹…", &mut lr, t.path_fg, fn_path, DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS);
            return;
        }
        let row_bg = if entry.dead { t.alt_bg } else if !colors.enabled { t.bg } else {
            match entry.source {
                EntrySource::Open => colors.open,
                EntrySource::Favorite => colors.favorite,
                EntrySource::Recent => colors.recent,
                EntrySource::Loading => t.bg,
            }
        };
        let row = RECT { bottom: rc.bottom - sc(2), ..rc };
        let border = (selected && !entry.dead).then_some(t.accent);
        crate::paint::round_rect(hdc, row, sc(6), row_bg, border, t.bg);
        let name_fg = readable_on(row_bg, if entry.dead { t.path_fg } else { t.fg });
        let path = &entry.path;
        let source = match (entry.source, entry.dead) {
            (EntrySource::Favorite, true) => "收藏（已失效）",
            (_, true) => "已失效",
            (EntrySource::Favorite, false) => "收藏",
            (EntrySource::Recent, false) => "最近打开",
            (EntrySource::Open, false) => "已打开",
            (EntrySource::Loading, false) => "",
        };
        let source_w = crate::paint::measure(fn_path, source);
        let mut rt = RECT { left: rc.right - pad - source_w, top: rc.top + sc(4), right: rc.right - pad, bottom: rc.top + sc(24) };
        draw_text(hdc, source, &mut rt, readable_on(row_bg, t.path_fg), fn_path, DT_RIGHT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX);

        let tx = rc.left + pad;
        let right = rt.left - sc(10);
        let mut r1 = RECT { left: tx, top: rc.top + sc(4), right, bottom: rc.top + sc(24) };
        draw_text(hdc, leaf_of(path), &mut r1, name_fg, fn_name, DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS);
        let band = RECT { left: tx, top: rc.top + sc(27), right: rc.right - pad, bottom: rc.bottom - sc(6) };
        let band_bg = path_band_color(row_bg, t.bg);
        crate::paint::round_rect(hdc, band, sc(3), band_bg, None, row_bg);
        let mut r2 = RECT { left: band.left + sc(5), right: band.right - sc(5), ..band };
        draw_text(hdc, path, &mut r2, readable_on(band_bg, t.path_fg), fn_path, DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_PATH_ELLIPSIS);
    });
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            paint_notice(hwnd);
            LRESULT(0)
        }
        WM_APP_OPEN_FOLDERS => {
            let open = Box::from_raw(lp.0 as *mut Vec<String>);
            let _ = KillTimer(Some(hwnd), LOADING_TIMER);
            on_open_folders(*open);
            LRESULT(0)
        }
        WM_APP_DEAD_PATHS => {
            let dead = Box::from_raw(lp.0 as *mut Vec<String>);
            on_dead_paths(*dead);
            LRESULT(0)
        }
        WM_TIMER if wp.0 == LOADING_TIMER => {
            let _ = KillTimer(Some(hwnd), LOADING_TIMER);
            let rest = POPUP.with(|c| c.borrow().as_ref().map(|p| without_loading(&p.entries)));
            if let Some(rest) = rest {
                set_entries(rest);
                relayout();
            }
            LRESULT(0)
        }
        WM_DPICHANGED => {
            // 位置和大小跟随对话框，由 relayout 计算，不用系统建议的矩形
            let dpi = ((wp.0 & 0xFFFF) as i32).max(96);
            let font_name = crate::paint::ui_font(dpi * 15 / 96, 600);
            let font_path = crate::paint::ui_font(dpi * 12 / 96, 400);
            let item_h = dpi * ITEM_H / 96;
            let old = POPUP.with(|c| {
                c.borrow_mut().as_mut().map(|p| {
                    let old = [p.font_name, p.font_path];
                    p.font_name = font_name;
                    p.font_path = font_path;
                    p.item_h = item_h;
                    p.dpi = dpi;
                    old
                })
            });
            for f in old.into_iter().flatten() {
                let _ = DeleteObject(HGDIOBJ(f.0));
            }
            if let Some(tip) = POPUP.with(|c| c.borrow().as_ref().map(|p| p.tooltip)) {
                send(tip, TTM_SETMAXTIPWIDTH, 0, (dpi * MAX_WIDTH / 96) as isize);
            }
            if let Some((_, list)) = popup_handles() {
                send(list, LB_SETITEMHEIGHT, 0, item_h as isize);
                let _ = InvalidateRect(Some(list), None, true);
            }
            relayout();
            LRESULT(0)
        }
        WM_MEASUREITEM => {
            let mis = &mut *(lp.0 as *mut MEASUREITEMSTRUCT);
            let h = POPUP.with(|c| c.borrow().as_ref().map(|p| p.item_h).unwrap_or(ITEM_H));
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
            LRESULT(theme::solid_brush(t.bg).0 as isize)
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
