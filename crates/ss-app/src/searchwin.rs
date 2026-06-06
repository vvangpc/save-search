//! 搜索窗口：搜索框 + 盘符下拉 + 分类标签栏 + **自绘平滑滚动结果列表**。
//!
//! 结果列表是自绘的逐像素虚拟列表（带惯性缓动动画 + 整窗双缓冲），滚动顺滑无闪烁。
//! 全局热键 Alt+Space 弹出/隐藏；输入即时搜索；分类标签筛选；回车/双击打开；Esc 隐藏。

use std::cell::RefCell;
use std::ffi::c_void;
use std::sync::Arc;

use parking_lot::RwLock;
use ss_core::{Catalog, Category, SearchResult};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontW, DrawTextW, EndPaint, FillRect, InvalidateRect, SelectObject, SetBkMode,
    SetTextColor, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DRAW_TEXT_FORMAT,
    DT_CENTER, DT_END_ELLIPSIS, DT_LEFT, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, HDC, HFONT,
    HGDIOBJ, OUT_DEFAULT_PRECIS, PAINTSTRUCT, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SetFocus, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT, VK_DOWN, VK_END, VK_ESCAPE, VK_HOME,
    VK_NEXT, VK_PRIOR, VK_RETURN, VK_UP,
};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::Controls::{SetScrollInfo, DRAWITEMSTRUCT, ODS_SELECTED, WM_MOUSELEAVE};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::{icons, paint, theme};
use crate::theme::Theme;

pub type SharedCatalog = Arc<RwLock<Option<Catalog>>>;

pub const WM_APP_INDEX_READY: u32 = WM_APP + 2;

const EDIT_ID: u32 = 101;
const COMBO_ID: u32 = 103;
const COUNT_ID: u32 = 104;
const CAT_BASE: u32 = 300;
const ANIM_TIMER: usize = 1;

const EN_CHANGE: u32 = 0x0300;
const CBN_SELCHANGE: u32 = 1;
const EM_SETSEL: u32 = 0x00B1;
const CB_ADDSTRING: u32 = 0x0143;
const CB_RESETCONTENT: u32 = 0x014B;
const CB_GETCURSEL: u32 = 0x0147;
const CB_SETCURSEL: u32 = 0x014E;

const CATS: [(Category, &str); 7] = [
    (Category::All, "全部"),
    (Category::Folder, "文件夹"),
    (Category::Document, "文档"),
    (Category::Image, "图片"),
    (Category::Video, "视频"),
    (Category::Audio, "音频"),
    (Category::Other, "其他"),
];

mod st {
    pub const WS_CHILD: u32 = 0x4000_0000;
    pub const WS_VISIBLE: u32 = 0x1000_0000;
    pub const WS_VSCROLL: u32 = 0x0020_0000;
    pub const WS_TABSTOP: u32 = 0x0001_0000;
    pub const WS_OVERLAPPEDWINDOW: u32 = 0x00CF_0000;
    pub const ES_AUTOHSCROLL: u32 = 0x0080;
    pub const CBS_DROPDOWNLIST: u32 = 0x0003;
    pub const BS_OWNERDRAW: u32 = 0x0000_000B;
    pub const SS_LEFT: u32 = 0x0000_0000;
    pub const WS_EX_CLIENTEDGE: u32 = 0x0000_0200;
    pub const WS_EX_TOOLWINDOW: u32 = 0x0000_0080;
}

struct Ui {
    search_hwnd: HWND,
    edit: HWND,
    list: HWND,
    combo: HWND,
    count: HWND,
    cats: Vec<HWND>,
    results: Vec<SearchResult>,
    drives: Vec<char>,
    category: Category,
    catalog: SharedCatalog,
    font_ui: HFONT,
    font_name: HFONT,
    font_path: HFONT,
    item_h: i32,
    dpi: i32,
    // 平滑滚动状态
    scroll_y: f32,
    target_y: f32,
    sel: i32,
    hover: i32,
    animating: bool,
}

thread_local! {
    static UI: RefCell<Option<Ui>> = const { RefCell::new(None) };
}

fn send(hwnd: HWND, msg: u32, w: usize, l: isize) -> isize {
    unsafe { SendMessageW(hwnd, msg, Some(WPARAM(w)), Some(LPARAM(l))).0 }
}

fn add_combo(combo: HWND, text: &str) {
    let w = crate::wide(text);
    send(combo, CB_ADDSTRING, 0, w.as_ptr() as isize);
}

fn set_font(hwnd: HWND, f: HFONT) {
    send(hwnd, WM_SETFONT, f.0 as usize, 1);
}

unsafe fn make_font(px: i32, weight: i32) -> HFONT {
    CreateFontW(
        -px, 0, 0, 0, weight, 0, 0, 0, DEFAULT_CHARSET, OUT_DEFAULT_PRECIS, CLIP_DEFAULT_PRECIS,
        CLEARTYPE_QUALITY, 0, w!("Segoe UI"),
    )
}

fn handles() -> Option<(HWND, HWND, HWND)> {
    UI.with(|c| c.borrow().as_ref().map(|ui| (ui.search_hwnd, ui.edit, ui.list)))
}

fn sc_dpi(v: i32) -> i32 {
    let dpi = UI.with(|c| c.borrow().as_ref().map(|u| u.dpi).unwrap_or(96));
    v * dpi / 96
}

pub fn init(catalog: SharedCatalog) -> windows::core::Result<HWND> {
    unsafe {
        let hinstance = HINSTANCE(GetModuleHandleW(None)?.0);
        let class = w!("SaveSearchSearchWnd");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance,
            lpszClassName: class,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            ..Default::default()
        };
        RegisterClassW(&wc);

        // 自绘结果列表的窗口类（CS_DBLCLKS：启用双击消息 WM_LBUTTONDBLCLK）
        let list_class = w!("SaveSearchResults");
        let wcl = WNDCLASSW {
            style: CS_DBLCLKS,
            lpfnWndProc: Some(results_proc),
            hInstance: hinstance,
            lpszClassName: list_class,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            ..Default::default()
        };
        RegisterClassW(&wcl);

        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(st::WS_EX_TOOLWINDOW),
            class,
            w!("SaveSearch 搜索"),
            WINDOW_STYLE(st::WS_OVERLAPPEDWINDOW),
            0, 0, 820, 560,
            None, None, Some(hinstance), None,
        )?;

        let dpi = GetDpiForWindow(hwnd).max(96) as i32;
        let sc = |v: i32| v * dpi / 96;

        let edit = CreateWindowExW(
            WINDOW_EX_STYLE(st::WS_EX_CLIENTEDGE),
            w!("EDIT"),
            PCWSTR::null(),
            WINDOW_STYLE(st::WS_CHILD | st::WS_VISIBLE | st::WS_TABSTOP | st::ES_AUTOHSCROLL),
            0, 0, 10, 10,
            Some(hwnd),
            Some(HMENU(EDIT_ID as usize as *mut c_void)),
            Some(hinstance), None,
        )?;
        let combo = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("COMBOBOX"),
            PCWSTR::null(),
            WINDOW_STYLE(
                st::WS_CHILD | st::WS_VISIBLE | st::WS_TABSTOP | st::WS_VSCROLL | st::CBS_DROPDOWNLIST,
            ),
            0, 0, 10, 260,
            Some(hwnd),
            Some(HMENU(COMBO_ID as usize as *mut c_void)),
            Some(hinstance), None,
        )?;
        let list = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            list_class,
            PCWSTR::null(),
            WINDOW_STYLE(st::WS_CHILD | st::WS_VISIBLE | st::WS_VSCROLL),
            0, 0, 10, 10,
            Some(hwnd),
            None,
            Some(hinstance), None,
        )?;
        let count = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("STATIC"),
            PCWSTR::null(),
            WINDOW_STYLE(st::WS_CHILD | st::WS_VISIBLE | st::SS_LEFT),
            0, 0, 10, 10,
            Some(hwnd),
            Some(HMENU(COUNT_ID as usize as *mut c_void)),
            Some(hinstance), None,
        )?;

        let mut cats = Vec::new();
        for (i, (_, label)) in CATS.iter().enumerate() {
            let wl = crate::wide(label);
            let b = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("BUTTON"),
                PCWSTR(wl.as_ptr()),
                WINDOW_STYLE(st::WS_CHILD | st::WS_VISIBLE | st::BS_OWNERDRAW),
                0, 0, 10, 10,
                Some(hwnd),
                Some(HMENU((CAT_BASE + i as u32) as usize as *mut c_void)),
                Some(hinstance), None,
            )?;
            cats.push(b);
        }

        let font_ui = make_font(sc(15), 400);
        let font_name = make_font(sc(15), 600);
        let font_path = make_font(sc(12), 400);
        set_font(edit, font_ui);
        set_font(combo, font_ui);
        set_font(count, font_path);
        for b in &cats {
            set_font(*b, font_ui);
        }

        add_combo(combo, "全部盘");
        send(combo, CB_SETCURSEL, 0, 0);

        UI.with(|c| {
            *c.borrow_mut() = Some(Ui {
                search_hwnd: hwnd,
                edit,
                list,
                combo,
                count,
                cats,
                results: Vec::new(),
                drives: Vec::new(),
                category: Category::All,
                catalog,
                font_ui,
                font_name,
                font_path,
                item_h: sc(46),
                dpi,
                scroll_y: 0.0,
                target_y: 0.0,
                sel: -1,
                hover: -1,
                animating: false,
            });
        });

        theme::apply_window(hwnd, &theme::current());
        layout(hwnd);
        Ok(hwnd)
    }
}

unsafe fn layout(hwnd: HWND) {
    let mut rc = RECT::default();
    if GetClientRect(hwnd, &mut rc).is_err() {
        return;
    }
    let w = rc.right - rc.left;
    let h = rc.bottom - rc.top;
    let m = sc_dpi(10);
    let eh = sc_dpi(30);
    let cw = sc_dpi(92);
    let cath = sc_dpi(30);
    let counth = sc_dpi(20);
    // 先取句柄并释放借用，再在借用外 MoveWindow——MoveWindow(list) 会同步回调
    // results_proc 的 WM_SIZE（内部 borrow_mut），若此时仍持借用会 RefCell 双借用崩溃。
    let Some((edit, combo, list, count, cats)) = UI.with(|c| {
        c.borrow()
            .as_ref()
            .map(|u| (u.edit, u.combo, u.list, u.count, u.cats.clone()))
    }) else {
        return;
    };
    let edit_w = w - 2 * m - cw - sc_dpi(6);
    let _ = MoveWindow(edit, m, m, edit_w, eh, true);
    let _ = MoveWindow(combo, m + edit_w + sc_dpi(6), m, cw, 260, true);
    let row_y = m + eh + sc_dpi(8);
    let n = cats.len() as i32;
    let gap = sc_dpi(4);
    let bw = (w - 2 * m - gap * (n - 1)) / n;
    for (i, b) in cats.iter().enumerate() {
        let x = m + (bw + gap) * i as i32;
        let _ = MoveWindow(*b, x, row_y, bw, cath, true);
    }
    let list_y = row_y + cath + sc_dpi(8);
    let list_h = h - list_y - counth - m;
    let _ = MoveWindow(list, m, list_y, w - 2 * m, list_h.max(sc_dpi(40)), true);
    let _ = MoveWindow(count, m, h - counth, w - 2 * m, counth - sc_dpi(2), true);
}

unsafe fn center(hwnd: HWND) {
    let w = sc_dpi(820);
    let h = sc_dpi(560);
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

fn set_text(hwnd: HWND, s: &str) {
    let w = crate::wide(s);
    unsafe {
        let _ = SetWindowTextW(hwnd, PCWSTR(w.as_ptr()));
    }
}

fn populate_drives() {
    let (combo, cat_arc) =
        match UI.with(|c| c.borrow().as_ref().map(|u| (u.combo, u.catalog.clone()))) {
            Some(t) => t,
            None => return,
        };
    let letters = {
        let g = cat_arc.read();
        match g.as_ref() {
            Some(cat) => cat.drive_letters(),
            None => return,
        }
    };
    send(combo, CB_RESETCONTENT, 0, 0);
    add_combo(combo, "全部盘");
    for d in &letters {
        add_combo(combo, &format!("{}:", d));
    }
    send(combo, CB_SETCURSEL, 0, 0);
    UI.with(|c| {
        if let Some(ui) = c.borrow_mut().as_mut() {
            ui.drives = letters;
        }
    });
    do_search();
}

fn selected_drive(combo: HWND) -> Option<char> {
    let sel = send(combo, CB_GETCURSEL, 0, 0);
    if sel <= 0 {
        return None;
    }
    UI.with(|c| {
        c.borrow()
            .as_ref()
            .and_then(|ui| ui.drives.get((sel - 1) as usize).copied())
    })
}

fn do_search() {
    let (edit, list, combo, count, category) = match UI.with(|c| {
        c.borrow()
            .as_ref()
            .map(|u| (u.edit, u.list, u.combo, u.count, u.category))
    }) {
        Some(t) => t,
        None => return,
    };
    let query = get_text(edit);
    let q = query.trim();
    let results = if q.is_empty() {
        Vec::new()
    } else {
        let drive = selected_drive(combo);
        let limit = ss_config::load_settings().result_limit.max(1);
        let cat_arc = UI.with(|c| c.borrow().as_ref().map(|u| u.catalog.clone()));
        match cat_arc {
            Some(arc) => match arc.read().as_ref() {
                Some(cat) => cat.search(q, drive, category, limit),
                None => Vec::new(),
            },
            None => Vec::new(),
        }
    };
    let ready = UI.with(|c| c.borrow().as_ref().map(|u| u.catalog.read().is_some()).unwrap_or(false));
    if q.is_empty() {
        set_text(count, "");
    } else if !ready {
        set_text(count, "索引构建中…");
    } else {
        set_text(count, &format!("{} 个结果", results.len()));
    }
    UI.with(|c| {
        if let Some(ui) = c.borrow_mut().as_mut() {
            ui.results = results;
            ui.scroll_y = 0.0;
            ui.target_y = 0.0;
            ui.sel = if ui.results.is_empty() { -1 } else { 0 };
            ui.hover = -1;
        }
    });
    unsafe {
        update_scrollbar(list);
        let _ = InvalidateRect(Some(list), None, false);
    }
}

fn open_selected() {
    let (search, sel) = match UI.with(|c| c.borrow().as_ref().map(|u| (u.search_hwnd, u.sel))) {
        Some(t) => t,
        None => return,
    };
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
        let p = crate::wide(path);
        ShellExecuteW(None, w!("open"), PCWSTR(p.as_ptr()), PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL);
    } else {
        let params = crate::wide(&format!("/select,\"{}\"", path));
        ShellExecuteW(None, w!("open"), w!("explorer.exe"), PCWSTR(params.as_ptr()), PCWSTR::null(), SW_SHOWNORMAL);
    }
}

pub fn toggle() {
    let (search, edit, list) =
        match UI.with(|c| c.borrow().as_ref().map(|u| (u.search_hwnd, u.edit, u.list))) {
            Some(t) => t,
            None => return,
        };
    unsafe {
        if IsWindowVisible(search).as_bool() {
            stop_anim(list);
            let _ = ShowWindow(search, SW_HIDE);
        } else {
            theme::apply_window(search, &theme::current());
            center(search);
            let _ = ShowWindow(search, SW_SHOW);
            let _ = SetForegroundWindow(search);
            let _ = SetFocus(Some(edit));
            send(edit, EM_SETSEL, 0, -1);
            let _ = InvalidateRect(Some(search), None, true);
        }
    }
}

pub fn refresh_theme() {
    if let Some((search, _, list)) = handles() {
        unsafe {
            theme::apply_window(search, &theme::current());
            let _ = InvalidateRect(Some(search), None, true);
            let _ = InvalidateRect(Some(list), None, false);
        }
    }
}

pub fn pretranslate(msg: &MSG) -> bool {
    let Some((search, edit, list)) = handles() else {
        return false;
    };
    // 滚轮：按鼠标位置命中列表则转发给列表（实现"悬停即可滚"，不依赖焦点/系统设置）
    if msg.message == WM_MOUSEWHEEL {
        let under = unsafe { WindowFromPoint(msg.pt) };
        if under == list {
            unsafe {
                let _ = SendMessageW(list, WM_MOUSEWHEEL, Some(msg.wParam), Some(msg.lParam));
            }
            return true;
        }
        return false;
    }
    if msg.message != WM_KEYDOWN {
        return false;
    }
    // 搜索框始终持焦：方向键在此控制列表选中，回车/Esc 全局处理
    if msg.hwnd != edit && msg.hwnd != list {
        return false;
    }
    let vk = (msg.wParam.0 as u32) as u16;
    if vk == VK_ESCAPE.0 {
        unsafe {
            stop_anim(list);
            let _ = ShowWindow(search, SW_HIDE);
        }
        return true;
    }
    if vk == VK_RETURN.0 {
        open_selected();
        return true;
    }
    let (count, item_h, _cw, ch, _) = list_metrics(list);
    if count == 0 {
        return false;
    }
    let cur = UI.with(|c| c.borrow().as_ref().map(|u| u.sel).unwrap_or(-1));
    let page = (ch / item_h.max(1)).max(1);
    let new = if vk == VK_DOWN.0 {
        cur + 1
    } else if vk == VK_UP.0 {
        cur - 1
    } else if vk == VK_NEXT.0 {
        cur + page
    } else if vk == VK_PRIOR.0 {
        cur - page
    } else if vk == VK_HOME.0 {
        0
    } else if vk == VK_END.0 {
        count - 1
    } else {
        return false;
    };
    set_sel(list, new);
    true
}

// ---- 分类标签自绘 ----

unsafe fn draw_text(hdc: HDC, s: &str, rect: &mut RECT, color: u32, font: HFONT, flags: DRAW_TEXT_FORMAT) {
    if s.is_empty() {
        return;
    }
    SelectObject(hdc, HGDIOBJ(font.0));
    SetTextColor(hdc, COLORREF(color));
    let mut buf: Vec<u16> = s.encode_utf16().collect();
    DrawTextW(hdc, &mut buf, rect, flags);
}

unsafe fn draw_cat_button(dis: &DRAWITEMSTRUCT) {
    let t = theme::current();
    let idx = (dis.CtlID - CAT_BASE) as usize;
    let cat = CATS.get(idx).map(|c| c.0).unwrap_or(Category::All);
    let label = CATS.get(idx).map(|c| c.1).unwrap_or("");
    let active = UI.with(|c| c.borrow().as_ref().map(|u| u.category == cat).unwrap_or(false));
    let pressed = dis.itemState.0 & ODS_SELECTED.0 != 0;
    let bg = if active {
        t.accent
    } else if pressed {
        t.sel_bg
    } else {
        t.alt_bg
    };
    let fg = if active { 0x00FF_FFFF } else { t.fg };
    let mut rc = dis.rcItem;
    FillRect(dis.hDC, &rc, theme::solid_brush(bg));
    SetBkMode(dis.hDC, TRANSPARENT);
    let font = UI.with(|c| c.borrow().as_ref().map(|u| u.font_ui).unwrap());
    draw_text(dis.hDC, label, &mut rc, fg, font, DT_CENTER | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX);
}

// ---- 结果列表（自绘 + 平滑滚动）----

/// (count, item_h, client_w, client_h, max_scroll)
fn list_metrics(list: HWND) -> (i32, i32, i32, i32, f32) {
    let mut rc = RECT::default();
    let _ = unsafe { GetClientRect(list, &mut rc) };
    let cw = rc.right - rc.left;
    let ch = rc.bottom - rc.top;
    let (count, item_h) = UI.with(|c| {
        c.borrow()
            .as_ref()
            .map(|u| (u.results.len() as i32, u.item_h))
            .unwrap_or((0, 46))
    });
    let total = count * item_h;
    let max_scroll = (total - ch).max(0) as f32;
    (count, item_h, cw, ch, max_scroll)
}

unsafe fn update_scrollbar(list: HWND) {
    let (count, item_h, _cw, ch, _max) = list_metrics(list);
    let total = (count * item_h).max(0);
    let pos = UI.with(|c| c.borrow().as_ref().map(|u| u.scroll_y as i32).unwrap_or(0));
    let si = SCROLLINFO {
        cbSize: std::mem::size_of::<SCROLLINFO>() as u32,
        fMask: SIF_RANGE | SIF_PAGE | SIF_POS,
        nMin: 0,
        nMax: (total - 1).max(0),
        nPage: ch.max(0) as u32,
        nPos: pos,
        nTrackPos: 0,
    };
    SetScrollInfo(list, SB_VERT, &si, true);
}

fn start_anim(list: HWND) {
    let need = UI.with(|c| {
        c.borrow_mut().as_mut().map(|u| {
            if !u.animating {
                u.animating = true;
                true
            } else {
                false
            }
        })
    });
    if need == Some(true) {
        unsafe {
            SetTimer(Some(list), ANIM_TIMER, 15, None);
        }
    }
}

fn set_target(list: HWND, y: f32) {
    let (_, _, _, _, max_scroll) = list_metrics(list);
    if max_scroll <= 0.0 {
        // 内容不足一屏：直接归零，不空转定时器
        UI.with(|c| {
            if let Some(u) = c.borrow_mut().as_mut() {
                u.target_y = 0.0;
                u.scroll_y = 0.0;
            }
        });
        return;
    }
    UI.with(|c| {
        if let Some(u) = c.borrow_mut().as_mut() {
            u.target_y = y.clamp(0.0, max_scroll);
        }
    });
    start_anim(list);
}

fn stop_anim(list: HWND) {
    let was = UI.with(|c| {
        c.borrow_mut()
            .as_mut()
            .map(|u| {
                let w = u.animating;
                u.animating = false;
                w
            })
            .unwrap_or(false)
    });
    if was {
        unsafe {
            KillTimer(Some(list), ANIM_TIMER).ok();
        }
    }
}

/// 设置选中项（钳到范围），滚动到可见并重绘。
fn set_sel(list: HWND, idx: i32) {
    let (count, _, _, _, _) = list_metrics(list);
    if count == 0 {
        return;
    }
    let idx = idx.clamp(0, count - 1);
    UI.with(|c| {
        if let Some(u) = c.borrow_mut().as_mut() {
            u.sel = idx;
        }
    });
    ensure_visible(list, idx);
    unsafe {
        let _ = InvalidateRect(Some(list), None, false);
    }
}

fn ensure_visible(list: HWND, idx: i32) {
    let (_, item_h, _cw, ch, _max) = list_metrics(list);
    let (target, _) = UI.with(|c| c.borrow().as_ref().map(|u| (u.target_y, u.scroll_y)).unwrap_or((0.0, 0.0)));
    let top = (idx * item_h) as f32;
    let bot = top + item_h as f32;
    let mut new_t = target;
    if top < target {
        new_t = top;
    } else if bot > target + ch as f32 {
        new_t = bot - ch as f32;
    }
    if (new_t - target).abs() > 0.5 {
        set_target(list, new_t);
    }
}

unsafe fn draw_one(hdc: HDC, rect: RECT, r: &SearchResult, selected: bool, hover: bool, t: &Theme, fname: HFONT, fpath: HFONT) {
    let bg = if selected {
        t.sel_bg
    } else if hover {
        t.alt_bg
    } else {
        t.bg
    };
    let fg = if selected { t.sel_fg } else { t.fg };
    let path_fg = if selected { t.sel_fg } else { t.path_fg };
    FillRect(hdc, &rect, theme::solid_brush(bg));
    let pad = sc_dpi(8);
    let icon = sc_dpi(20);
    let icy = rect.top + (rect.bottom - rect.top - icon) / 2;
    if let Some(hicon) = icons::icon_for(&r.name, r.is_dir) {
        let _ = DrawIconEx(hdc, rect.left + pad, icy, hicon, icon, icon, 0, None, DI_NORMAL);
    }
    SetBkMode(hdc, TRANSPARENT);
    let tx = rect.left + pad + icon + pad;
    let mut r1 = RECT {
        left: tx,
        top: rect.top + sc_dpi(4),
        right: rect.right - pad,
        bottom: rect.top + (rect.bottom - rect.top) / 2 + sc_dpi(2),
    };
    draw_text(hdc, &r.name, &mut r1, fg, fname, DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS);
    let mut r2 = RECT {
        left: tx,
        top: rect.top + (rect.bottom - rect.top) / 2 - sc_dpi(2),
        right: rect.right - pad,
        bottom: rect.bottom - sc_dpi(2),
    };
    draw_text(hdc, &r.path, &mut r2, path_fg, fpath, DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS);
}

unsafe fn paint_list(list: HWND) {
    let mut ps = PAINTSTRUCT::default();
    let hdc = BeginPaint(list, &mut ps);
    let mut rc = RECT::default();
    let _ = GetClientRect(list, &mut rc);
    let t = theme::current();
    let (item_h, scroll_y, sel, hover, fname, fpath) = UI.with(|c| {
        c.borrow()
            .as_ref()
            .map(|u| (u.item_h, u.scroll_y, u.sel, u.hover, u.font_name, u.font_path))
            .unwrap_or((46, 0.0, -1, -1, HFONT(std::ptr::null_mut()), HFONT(std::ptr::null_mut())))
    });
    let full = rc;
    paint::buffered(hdc, full, |mem, lrc| unsafe {
        FillRect(mem, &lrc, theme::solid_brush(t.bg));
        let count = UI.with(|c| c.borrow().as_ref().map(|u| u.results.len() as i32).unwrap_or(0));
        if count > 0 && item_h > 0 {
            let sy = scroll_y as i32;
            let first = (sy / item_h).max(0);
            let last = ((sy + lrc.bottom) / item_h).min(count - 1);
            for i in first..=last {
                let y = i * item_h - sy;
                let item_rect = RECT {
                    left: 0,
                    top: y,
                    right: lrc.right,
                    bottom: y + item_h,
                };
                let r = UI.with(|c| c.borrow().as_ref().and_then(|u| u.results.get(i as usize).cloned()));
                if let Some(r) = r {
                    draw_one(mem, item_rect, &r, i == sel, i == hover, &t, fname, fpath);
                }
            }
        }
    });
    let _ = EndPaint(list, &ps);
}

fn y_to_index(list: HWND, y: i32) -> i32 {
    let (count, item_h, _, _, _) = list_metrics(list);
    let scroll_y = UI.with(|c| c.borrow().as_ref().map(|u| u.scroll_y).unwrap_or(0.0));
    if item_h <= 0 {
        return -1;
    }
    let idx = (y + scroll_y as i32) / item_h;
    if idx >= 0 && idx < count {
        idx
    } else {
        -1
    }
}

unsafe extern "system" fn results_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => {
            paint_list(hwnd);
            LRESULT(0)
        }
        WM_MOUSEWHEEL => {
            let delta = ((wp.0 >> 16) & 0xFFFF) as i16 as i32;
            let (_, item_h, _, _, _) = list_metrics(hwnd);
            let step = item_h as f32 * 1.5;
            let target = UI.with(|c| c.borrow().as_ref().map(|u| u.target_y).unwrap_or(0.0));
            set_target(hwnd, target - (delta as f32 / 120.0) * step);
            LRESULT(0)
        }
        WM_VSCROLL => {
            let code = (wp.0 as u32) & 0xFFFF;
            let (_, item_h, _, ch, _) = list_metrics(hwnd);
            let target = UI.with(|c| c.borrow().as_ref().map(|u| u.target_y).unwrap_or(0.0));
            match SCROLLBAR_COMMAND(code as i32) {
                SB_LINEUP => set_target(hwnd, target - item_h as f32),
                SB_LINEDOWN => set_target(hwnd, target + item_h as f32),
                SB_PAGEUP => set_target(hwnd, target - ch as f32),
                SB_PAGEDOWN => set_target(hwnd, target + ch as f32),
                SB_THUMBTRACK | SB_THUMBPOSITION => {
                    let mut si = SCROLLINFO {
                        cbSize: std::mem::size_of::<SCROLLINFO>() as u32,
                        fMask: SIF_TRACKPOS,
                        ..Default::default()
                    };
                    let _ = GetScrollInfo(hwnd, SB_VERT, &mut si);
                    let (_, _, _, _, max_scroll) = list_metrics(hwnd);
                    let pos = (si.nTrackPos as f32).clamp(0.0, max_scroll);
                    UI.with(|c| {
                        if let Some(u) = c.borrow_mut().as_mut() {
                            u.scroll_y = pos;
                            u.target_y = pos;
                        }
                    });
                    update_scrollbar(hwnd);
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_TIMER => {
            let (_, _, _, _, max_scroll) = list_metrics(hwnd);
            let done = UI.with(|c| {
                c.borrow_mut().as_mut().map(|u| {
                    u.target_y = u.target_y.clamp(0.0, max_scroll);
                    let d = u.target_y - u.scroll_y;
                    if d.abs() < 0.5 {
                        u.scroll_y = u.target_y;
                        u.animating = false;
                        true
                    } else {
                        u.scroll_y += d * 0.30;
                        false
                    }
                })
            });
            if done == Some(true) {
                KillTimer(Some(hwnd), ANIM_TIMER).ok();
            }
            update_scrollbar(hwnd);
            let _ = InvalidateRect(Some(hwnd), None, false);
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            let y = ((lp.0 >> 16) & 0xFFFF) as i16 as i32;
            let idx = y_to_index(hwnd, y);
            // 焦点交还搜索框，保证可继续打字；点击只改选中
            if let Some(edit) = UI.with(|c| c.borrow().as_ref().map(|u| u.edit)) {
                let _ = SetFocus(Some(edit));
            }
            if idx >= 0 {
                UI.with(|c| {
                    if let Some(u) = c.borrow_mut().as_mut() {
                        u.sel = idx;
                    }
                });
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            LRESULT(0)
        }
        WM_LBUTTONDBLCLK => {
            let y = ((lp.0 >> 16) & 0xFFFF) as i16 as i32;
            let idx = y_to_index(hwnd, y);
            if idx >= 0 {
                UI.with(|c| {
                    if let Some(u) = c.borrow_mut().as_mut() {
                        u.sel = idx;
                    }
                });
                open_selected();
            }
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let y = ((lp.0 >> 16) & 0xFFFF) as i16 as i32;
            let idx = y_to_index(hwnd, y);
            let changed = UI.with(|c| {
                c.borrow_mut().as_mut().map(|u| {
                    if u.hover != idx {
                        u.hover = idx;
                        true
                    } else {
                        false
                    }
                })
            });
            if changed == Some(true) {
                let mut tme = TRACKMOUSEEVENT {
                    cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                let _ = TrackMouseEvent(&mut tme);
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            UI.with(|c| {
                if let Some(u) = c.borrow_mut().as_mut() {
                    u.hover = -1;
                }
            });
            let _ = InvalidateRect(Some(hwnd), None, false);
            LRESULT(0)
        }
        WM_SIZE => {
            let (_, _, _, _, max_scroll) = list_metrics(hwnd);
            UI.with(|c| {
                if let Some(u) = c.borrow_mut().as_mut() {
                    u.scroll_y = u.scroll_y.min(max_scroll);
                    u.target_y = u.target_y.min(max_scroll);
                }
            });
            update_scrollbar(hwnd);
            let _ = InvalidateRect(Some(hwnd), None, false);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_APP_INDEX_READY => {
            populate_drives();
            LRESULT(0)
        }
        WM_DRAWITEM => {
            let dis = &*(lp.0 as *const DRAWITEMSTRUCT);
            if dis.CtlID >= CAT_BASE && (dis.CtlID as usize) < CAT_BASE as usize + CATS.len() {
                draw_cat_button(dis);
            }
            LRESULT(1)
        }
        WM_CTLCOLOREDIT | WM_CTLCOLORLISTBOX | WM_CTLCOLORSTATIC => {
            let t = theme::current();
            let hdc = HDC(wp.0 as *mut c_void);
            SetTextColor(hdc, COLORREF(t.fg));
            SetBkMode(hdc, TRANSPARENT);
            let bg = if msg == WM_CTLCOLORSTATIC { t.bg } else { t.alt_bg };
            return LRESULT(theme::solid_brush(bg).0 as isize);
        }
        WM_ERASEBKGND => {
            let t = theme::current();
            let mut rc = RECT::default();
            let _ = GetClientRect(hwnd, &mut rc);
            FillRect(HDC(wp.0 as *mut c_void), &rc, theme::solid_brush(t.bg));
            LRESULT(1)
        }
        WM_COMMAND => {
            let id = (wp.0 as u32) & 0xFFFF;
            let code = ((wp.0 as u32) >> 16) & 0xFFFF;
            if id == EDIT_ID && code == EN_CHANGE {
                do_search();
            } else if id == COMBO_ID && code == CBN_SELCHANGE {
                do_search();
            } else if id >= CAT_BASE && (id as usize) < CAT_BASE as usize + CATS.len() {
                let cat = CATS[(id - CAT_BASE) as usize].0;
                UI.with(|c| {
                    if let Some(ui) = c.borrow_mut().as_mut() {
                        ui.category = cat;
                    }
                });
                if let Some(ui_cats) = UI.with(|c| c.borrow().as_ref().map(|u| u.cats.clone())) {
                    for b in ui_cats {
                        let _ = InvalidateRect(Some(b), None, true);
                    }
                }
                do_search();
            }
            LRESULT(0)
        }
        WM_SIZE => {
            layout(hwnd);
            LRESULT(0)
        }
        WM_CLOSE => {
            if let Some(list) = UI.with(|c| c.borrow().as_ref().map(|u| u.list)) {
                stop_anim(list);
            }
            let _ = ShowWindow(hwnd, SW_HIDE);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}
