//! 搜索窗口：搜索框 + 盘符下拉 + 分类标签栏 + **自绘平滑滚动结果列表**。
//!
//! 结果列表是自绘的逐像素虚拟列表（带惯性缓动动画 + 整窗双缓冲），滚动顺滑无闪烁。
//! 全局热键 Alt+Space 弹出/隐藏；输入即时搜索；分类标签筛选；回车/双击定位；Ctrl+双击打开；Esc 隐藏。

use std::cell::RefCell;
use std::ffi::c_void;
use std::rc::Rc;
use std::sync::Arc;

use parking_lot::{Mutex, RwLock};
use ss_core::{Catalog, Category, CategoryCounts, DriveState, Scope, ScopeStatus, SearchResult};
use ss_ui_model::{
    all_drives_label, banner, body, compact, ease_toward, highlight_segments,
    is_partial_selection, leaf_of, moved, parent_of, reselect, scope_index, scope_key,
    scroll_from_thumb, scroll_thumb, status_line, Banner, Body, BuildProgress, DriveFailure,
    ListGeometry, RequestSeq, ScopeProblem, SelMove, ViewInput,
};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, ClientToScreen, DeleteObject, EndPaint, FillRect, GetMonitorInfoW, InvalidateRect,
    MonitorFromPoint, SetBkColor, SetBkMode, SetTextColor, DT_CENTER, DT_END_ELLIPSIS, DT_LEFT,
    DT_NOPREFIX, DT_PATH_ELLIPSIS, DT_RIGHT, DT_SINGLELINE, DT_VCENTER, HDC, HFONT, HGDIOBJ,
    MONITORINFO, MONITOR_DEFAULTTONEAREST, PAINTSTRUCT, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, GetDpiForWindow, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, ReleaseCapture, SetCapture, SetFocus, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT, VK_C, VK_CONTROL, VK_DOWN,
    VK_END, VK_ESCAPE, VK_HOME, VK_NEXT, VK_PRIOR, VK_RETURN, VK_SHIFT, VK_UP,
};
use windows::Win32::UI::Controls::{DRAWITEMSTRUCT, ODS_SELECTED, WM_MOUSELEAVE};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::meta::{self, RowMeta};
use crate::search_worker::{self, Done, Job};
use crate::paint::{measure as measure_text, text as draw_text};
use crate::theme::Theme;
use crate::{icons, paint, theme};

pub type SharedCatalog = Arc<RwLock<Option<Catalog>>>;

pub const WM_APP_INDEX_READY: u32 = WM_APP + 2;
pub const WM_APP_INDEX_PROGRESS: u32 = WM_APP + 3;
pub const WM_APP_SEARCH_DONE: u32 = WM_APP + 4;
/// USN 线程每轮轮询后发送：刷新状态栏，索引有变化时重新搜索。
pub const WM_APP_INDEX_TICK: u32 = WM_APP + 5;
pub const WM_APP_ICONS_READY: u32 = WM_APP + 6;
pub const WM_APP_META_READY: u32 = WM_APP + 7;

const EDIT_ID: u32 = 101;
const COMBO_ID: u32 = 103;
const SCOPE_ID: u32 = 104;
/// 圆角半径（96 DPI）。
const RADIUS: i32 = 6;
/// 输入区和标签栏的高度（96 DPI）。
const INPUT_H: i32 = 48;
const BAR_H: i32 = 40;
const CAT_BASE: u32 = 300;
const ANIM_TIMER: usize = 1;
const TOAST_TIMER: usize = 2;
const TOAST_MS: u32 = 2400;
/// 大小和时间缓存的上限，超过时清空。
const META_CACHE_MAX: usize = 400;
const EM_GETSEL: u32 = 0x00B0;

const EN_CHANGE: u32 = 0x0300;
const EM_SETSEL: u32 = 0x00B1;
const CB_ADDSTRING: u32 = 0x0143;
const CB_RESETCONTENT: u32 = 0x014B;
const CB_GETCURSEL: u32 = 0x0147;
const CB_SETCURSEL: u32 = 0x014E;
const CB_GETCOUNT: u32 = 0x0146;
const CB_GETLBTEXT: u32 = 0x0148;
const CB_GETLBTEXTLEN: u32 = 0x0149;

// (分类, 标签, 保存到设置里的键)。顺序对应 Alt+1…8。
const CATS: [(Category, &str, &str); 8] = [
    (Category::All, "全部", ""),
    (Category::Folder, "文件夹", "folder"),
    (Category::Document, "文档", "document"),
    (Category::Image, "图片", "image"),
    (Category::Media, "音视频", "media"),
    (Category::Archive, "压缩包", "archive"),
    (Category::App, "程序", "app"),
    (Category::Other, "其他", "other"),
];

mod st {
    pub const WS_CHILD: u32 = 0x4000_0000;
    pub const WS_VISIBLE: u32 = 0x1000_0000;
    pub const WS_TABSTOP: u32 = 0x0001_0000;
    pub const WS_OVERLAPPEDWINDOW: u32 = 0x00CF_0000;
    pub const WS_CLIPCHILDREN: u32 = 0x0200_0000;
    pub const ES_AUTOHSCROLL: u32 = 0x0080;
    pub const CBS_DROPDOWNLIST: u32 = 0x0003;
    pub const BS_OWNERDRAW: u32 = 0x0000_000B;
    pub const WS_EX_TOOLWINDOW: u32 = 0x0000_0080;
}

/// 可点击文字或按钮的动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Act {
    SearchAllScopes,
    ClearCategory,
    RemovePreset,
    RetryFailedDrives,
    /// 打开或定位选中项，参数表示是否直接打开文件。
    Activate(bool),
    CopyPath,
}

/// 空输入页的语法提示。
const HINTS: [(&str, &str); 5] = [
    ("report 2024", "同时包含两个词"),
    ("*.pdf", "以 .pdf 结尾"),
    ("ext:pdf,docx", "按扩展名筛选"),
    ("dir:", "只看文件夹"),
    ("工作\\ 报告", "路径包含“工作”、文件名包含“报告”"),
];

/// 一次搜索的结果摘要（`results` 之外的部分）。
#[derive(Default)]
struct Summary {
    total_hits: usize,
    elapsed_ms: u32,
    scope_problem: Option<ScopeProblem>,
}

/// 正在建立索引的盘。索引线程写入，界面线程读取。
static BUILDING: Mutex<Vec<BuildProgress>> = Mutex::new(Vec::new());
static LAST_PROGRESS_POST: Mutex<Option<std::time::Instant>> = Mutex::new(None);

/// 记录索引线程报告的状态，返回是否值得通知界面。进度事件很多，所以限制频率。
pub fn record_index_state(letter: char, state: DriveState) -> bool {
    let mut building = BUILDING.lock();
    let is_progress = if let DriveState::Building { done, estimated } = state {
        match building.iter_mut().find(|p| p.letter == letter) {
            Some(p) => {
                p.done = done;
                p.estimated = estimated;
            }
            None => building.push(BuildProgress { letter, done, estimated }),
        }
        true
    } else {
        building.retain(|p| p.letter != letter);
        false
    };
    drop(building);
    let mut last = LAST_PROGRESS_POST.lock();
    if is_progress && last.is_some_and(|t| t.elapsed().as_millis() < 200) {
        return false;
    }
    *last = Some(std::time::Instant::now());
    true
}

struct Ui {
    search_hwnd: HWND,
    edit: HWND,
    list: HWND,
    combo: HWND,
    banner_wnd: HWND,
    foot_wnd: HWND,
    cats: Vec<HWND>,
    /// 空输入页的分组标题在这些行之前：最近位置、收藏。-1 表示这一组没有内容。
    heads: [i32; 2],
    scope_btn: HWND,
    /// 上次布局时分类标签的宽度，没有变化时不重排。
    tab_widths: Vec<i32>,
    /// 正在拖动滚动条：鼠标相对滑块顶部的位置。
    drag_grab: Option<i32>,
    foot_hits: Vec<(RECT, Act)>,
    font_query: HFONT,
    font_title: HFONT,
    font_chevron: HFONT,
    counts: CategoryCounts,
    metas: Vec<(String, Rc<RowMeta>)>,
    toast: Option<String>,
    seq: RequestSeq,
    /// 当前结果对应的索引版本。
    shown_generation: u64,
    extra_h: i32,
    query_text: String,
    summary: Summary,
    index_ready: bool,
    failed: Vec<DriveFailure>,
    total_files: usize,
    view_body: Body,
    view_banner: Option<Banner>,
    status: String,
    list_hits: Vec<(RECT, Act)>,
    banner_hits: Vec<(RECT, Act)>,
    /// Rc：绘制时克隆一次句柄即可在 RefCell 借用外读取，不必每帧逐项 clone 字符串
    results: Rc<Vec<SearchResult>>,
    drives: Vec<char>,
    presets: Vec<String>,
    category: Category,
    catalog: SharedCatalog,
    font_ui: HFONT,
    font_path: HFONT,
    font_icon: HFONT,
    item_h: i32,
    dpi: i32,
    combo_w: i32,
    /// 系统设置允许动画。
    animate: bool,
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

/// 结果行的高度（96 DPI）。
const ITEM_H: i32 = 44;

struct Fonts {
    ui: HFONT,
    path: HFONT,
    query: HFONT,
    title: HFONT,
    icon: HFONT,
    chevron: HFONT,
}

unsafe fn make_fonts(dpi: i32) -> Fonts {
    let sc = |v: i32| v * dpi / 96;
    Fonts {
        ui: paint::ui_font(sc(14), 400),
        path: paint::ui_font(sc(12), 400),
        query: paint::ui_font(sc(18), 400),
        title: paint::ui_font(sc(16), 600),
        icon: paint::icon_font(sc(16), 400),
        chevron: paint::icon_font(sc(9), 400),
    }
}

fn handles() -> Option<(HWND, HWND, HWND)> {
    UI.with(|c| c.borrow().as_ref().map(|ui| (ui.search_hwnd, ui.edit, ui.list)))
}

fn sc_dpi(v: i32) -> i32 {
    let dpi = UI.with(|c| c.borrow().as_ref().map(|u| u.dpi).unwrap_or(96));
    v * dpi / 96
}

/// 根据当前选中项文本，自适应调整范围下拉框宽度并重排。
fn update_combo_width() {
    let (search, combo, font) =
        match UI.with(|c| c.borrow().as_ref().map(|u| (u.search_hwnd, u.combo, u.font_ui))) {
            Some(t) => t,
            None => return,
        };
    let text = combo_text(combo, send(combo, CB_GETCURSEL, 0, 0));
    let tw = measure_text(font, &text);
    let mut client = RECT::default();
    let _ = unsafe { GetClientRect(search, &mut client) };
    let cw_total = client.right - client.left;
    let min_w = sc_dpi(92);
    let max_w = (cw_total / 3).max(min_w);
    let w = (tw + sc_dpi(38)).clamp(min_w, max_w);
    UI.with(|c| {
        if let Some(u) = c.borrow_mut().as_mut() {
            u.combo_w = w;
        }
    });
    unsafe {
        layout(search);
    }
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
            hIcon: crate::load_app_icon(0, 0).unwrap_or_default(),
            ..Default::default()
        };
        RegisterClassW(&wc);

        // 横幅和状态栏共用的自绘窗口类
        let strip_class = w!("SaveSearchStrip");
        let wcs = WNDCLASSW {
            lpfnWndProc: Some(strip_proc),
            hInstance: hinstance,
            lpszClassName: strip_class,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            ..Default::default()
        };
        RegisterClassW(&wcs);

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
            WINDOW_STYLE(st::WS_OVERLAPPEDWINDOW | st::WS_CLIPCHILDREN),
            0, 0, 820, 560,
            None, None, Some(hinstance), None,
        )?;

        let dpi = GetDpiForWindow(hwnd).max(96) as i32;
        let sc = |v: i32| v * dpi / 96;

        let edit = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("EDIT"),
            PCWSTR::null(),
            WINDOW_STYLE(st::WS_CHILD | st::WS_VISIBLE | st::WS_TABSTOP | st::ES_AUTOHSCROLL),
            0, 0, 10, 10,
            Some(hwnd),
            Some(HMENU(EDIT_ID as usize as *mut c_void)),
            Some(hinstance), None,
        )?;
        // 隐藏的下拉框只用来保存范围列表，界面上显示的是 scope_btn
        let combo = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("COMBOBOX"),
            PCWSTR::null(),
            WINDOW_STYLE(st::WS_CHILD | st::CBS_DROPDOWNLIST),
            0, 0, 10, 260,
            Some(hwnd),
            Some(HMENU(COMBO_ID as usize as *mut c_void)),
            Some(hinstance), None,
        )?;
        let scope_btn = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("BUTTON"),
            PCWSTR::null(),
            WINDOW_STYLE(st::WS_CHILD | st::WS_VISIBLE | st::BS_OWNERDRAW),
            0, 0, 10, 10,
            Some(hwnd),
            Some(HMENU(SCOPE_ID as usize as *mut c_void)),
            Some(hinstance), None,
        )?;
        let list = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            list_class,
            PCWSTR::null(),
            WINDOW_STYLE(st::WS_CHILD | st::WS_VISIBLE),
            0, 0, 10, 10,
            Some(hwnd),
            None,
            Some(hinstance), None,
        )?;
        let banner_wnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            strip_class,
            PCWSTR::null(),
            WINDOW_STYLE(st::WS_CHILD),
            0, 0, 10, 10,
            Some(hwnd),
            None,
            Some(hinstance), None,
        )?;
        let foot_wnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            strip_class,
            PCWSTR::null(),
            WINDOW_STYLE(st::WS_CHILD | st::WS_VISIBLE),
            0, 0, 10, 10,
            Some(hwnd),
            None,
            Some(hinstance), None,
        )?;

        let mut cats = Vec::new();
        for (i, (_, label, _)) in CATS.iter().enumerate() {
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

        let fonts = make_fonts(dpi);
        set_font(edit, fonts.query);
        for b in &cats {
            set_font(*b, fonts.ui);
        }

        add_combo(combo, "全部盘");
        send(combo, CB_SETCURSEL, 0, 0);

        UI.with(|c| {
            *c.borrow_mut() = Some(Ui {
                search_hwnd: hwnd,
                edit,
                list,
                combo,
                banner_wnd,
                foot_wnd,
                cats,
                heads: [-1, -1],
                scope_btn,
                tab_widths: Vec::new(),
                drag_grab: None,
                foot_hits: Vec::new(),
                font_query: fonts.query,
                font_title: fonts.title,
                font_chevron: fonts.chevron,
                counts: CategoryCounts::default(),
                metas: Vec::new(),
                toast: None,
                seq: RequestSeq::default(),
                shown_generation: 0,
                extra_h: 0,
                query_text: String::new(),
                summary: Summary::default(),
                index_ready: false,
                failed: Vec::new(),
                total_files: 0,
                view_body: Body::Home,
                view_banner: None,
                status: String::new(),
                list_hits: Vec::new(),
                banner_hits: Vec::new(),
                results: Rc::new(Vec::new()),
                drives: Vec::new(),
                presets: Vec::new(),
                category: {
                    let key = ss_config::load_settings().last_category;
                    CATS.iter().find(|c| c.2 == key).map_or(Category::All, |c| c.0)
                },
                catalog: catalog.clone(),
                font_ui: fonts.ui,
                font_path: fonts.path,
                font_icon: fonts.icon,
                item_h: sc(ITEM_H),
                dpi,
                combo_w: sc(92),
                animate: animations_enabled(),
                scroll_y: 0.0,
                target_y: 0.0,
                sel: -1,
                hover: -1,
                animating: false,
            });
        });

        theme::apply_window(hwnd);
        search_worker::start(catalog, hwnd.0 as isize);
        layout(hwnd);
        // 跨重启记忆：预填上次搜索词（显示时由 toggle 全选，用户打字即整体覆盖）
        let last = ss_config::last_query();
        if !last.is_empty() {
            set_text(edit, &last);
        }
        do_search();
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
    let cw = UI.with(|c| c.borrow().as_ref().map(|u| u.combo_w).unwrap_or(sc_dpi(92)));
    let (input_h, bar_h, foot_h, banner_h) = (sc_dpi(INPUT_H), sc_dpi(BAR_H), sc_dpi(30), sc_dpi(32));
    // 先取句柄并释放借用，再在借用外 MoveWindow——MoveWindow(list) 会同步回调
    // results_proc 的 WM_SIZE（内部 borrow_mut），若此时仍持借用会 RefCell 双借用崩溃。
    let Some((edit, scope_btn, list, banner_wnd, foot_wnd, has_banner)) = UI.with(|c| {
        c.borrow().as_ref().map(|u| {
            (u.edit, u.scope_btn, u.list, u.banner_wnd, u.foot_wnd, u.view_banner.is_some())
        })
    }) else {
        return;
    };
    let edit_h = sc_dpi(26);
    let edit_x = sc_dpi(42);
    let _ = MoveWindow(edit, edit_x, (input_h - edit_h) / 2, w - edit_x - sc_dpi(14), edit_h, true);
    let (ctl_h, m) = (sc_dpi(28), sc_dpi(10));
    let ctl_y = input_h + (bar_h - ctl_h) / 2;
    let _ = MoveWindow(scope_btn, w - m - cw, ctl_y, cw, ctl_h, true);
    layout_tabs(w - 2 * m - cw - sc_dpi(8), m, ctl_y, ctl_h, true);
    let mut list_y = input_h + bar_h;
    if has_banner {
        let _ = MoveWindow(banner_wnd, 0, list_y, w, banner_h, true);
        let _ = ShowWindow(banner_wnd, SW_SHOWNA);
        list_y += banner_h;
    } else {
        let _ = ShowWindow(banner_wnd, SW_HIDE);
    }
    let list_h = h - list_y - foot_h;
    let _ = MoveWindow(list, 0, list_y, w, list_h.max(sc_dpi(40)), true);
    let _ = MoveWindow(foot_wnd, 0, h - foot_h, w, foot_h, true);
}

/// 按内容排分类标签。放不下时平分宽度。
unsafe fn layout_tabs(avail: i32, left: i32, y: i32, h: i32, force: bool) {
    let Some(cats) = UI.with(|c| c.borrow().as_ref().map(|u| u.cats.clone())) else {
        return;
    };
    let gap = sc_dpi(2);
    let mut widths: Vec<i32> = CATS.iter().map(|c| tab_width(c.0, c.1)).collect();
    let total: i32 = widths.iter().sum::<i32>() + gap * (widths.len() as i32 - 1);
    if total > avail {
        let each = (avail - gap * (widths.len() as i32 - 1)) / widths.len() as i32;
        widths.iter_mut().for_each(|w| *w = each);
    }
    let same = UI.with(|c| c.borrow().as_ref().is_some_and(|u| u.tab_widths == widths));
    if same && !force {
        return;
    }
    let mut x = left;
    for (b, w) in cats.iter().zip(&widths) {
        let _ = MoveWindow(*b, x, y, *w, h, true);
        x += w + gap;
    }
    UI.with(|c| {
        if let Some(u) = c.borrow_mut().as_mut() {
            u.tab_widths = widths;
        }
    });
}

/// 系统的“显示动画”设置。读取失败时按允许处理。
fn animations_enabled() -> bool {
    let mut on = windows::core::BOOL(1);
    let _ = unsafe {
        SystemParametersInfoW(
            SPI_GETCLIENTAREAANIMATION,
            0,
            Some(&mut on as *mut _ as *mut c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    };
    on.as_bool()
}

/// 窗口移到 DPI 不同的显示器：重算字体、行高和布局。
unsafe fn on_dpi_changed(hwnd: HWND, wp: WPARAM, lp: LPARAM) {
    let dpi = ((wp.0 & 0xFFFF) as i32).max(96);
    let fonts = make_fonts(dpi);
    let Some((old, edit, cats)) = UI.with(|c| {
        c.borrow_mut().as_mut().map(|u| {
            let old = [u.font_ui, u.font_path, u.font_query, u.font_title, u.font_icon, u.font_chevron];
            let old_dpi = u.dpi.max(1);
            u.font_ui = fonts.ui;
            u.font_path = fonts.path;
            u.font_query = fonts.query;
            u.font_title = fonts.title;
            u.font_icon = fonts.icon;
            u.font_chevron = fonts.chevron;
            u.item_h = ITEM_H * dpi / 96;
            u.extra_h = u.extra_h * dpi / old_dpi;
            u.combo_w = u.combo_w * dpi / old_dpi;
            u.scroll_y = u.scroll_y * dpi as f32 / old_dpi as f32;
            u.target_y = u.scroll_y;
            u.dpi = dpi;
            (old, u.edit, u.cats.clone())
        })
    }) else {
        return;
    };
    set_font(edit, fonts.query);
    for b in &cats {
        set_font(*b, fonts.ui);
    }
    for f in old {
        let _ = DeleteObject(HGDIOBJ(f.0));
    }
    // 建议的矩形由系统给出，移动后会触发 WM_SIZE 重新布局
    let r = &*(lp.0 as *const RECT);
    let _ = MoveWindow(hwnd, r.left, r.top, r.right - r.left, r.bottom - r.top, true);
    update_combo_width();
    let _ = InvalidateRect(Some(hwnd), None, true);
}

/// 在鼠标所在显示器的工作区居中偏上（原先固定按主屏尺寸算，多显示器时总弹在主屏）。
unsafe fn center(hwnd: HWND) {
    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);
    let hmon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
    // 按目标显示器的 DPI 计算大小，窗口可能还在另一块显示器上
    let (mut dpi_x, mut dpi_y) = (96u32, 96u32);
    let _ = GetDpiForMonitor(hmon, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y);
    let dpi = (dpi_x as i32).max(96);
    let w = 820 * dpi / 96;
    let h = 560 * dpi / 96;
    let mut mi = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    let (x, y) = if GetMonitorInfoW(hmon, &mut mi).as_bool() {
        let wr = mi.rcWork;
        (
            wr.left + (wr.right - wr.left - w) / 2,
            wr.top + (wr.bottom - wr.top - h) / 3,
        )
    } else {
        let sw = GetSystemMetrics(SM_CXSCREEN);
        let sh = GetSystemMetrics(SM_CYSCREEN);
        ((sw - w) / 2, (sh - h) / 3)
    };
    let _ = MoveWindow(hwnd, x, y, w, h, true);
    // 跨显示器时第一次移动触发 WM_DPICHANGED，系统建议的矩形可能和这里不同，再定位一次
    let _ = MoveWindow(hwnd, x, y, w, h, true);
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

/// 重建搜索范围下拉框：全部盘 + 各盘 + 预设文件夹。索引就绪后或设置变更后调用。
pub fn refresh_scopes() {
    let (combo, cat_arc) =
        match UI.with(|c| c.borrow().as_ref().map(|u| (u.combo, u.catalog.clone()))) {
            Some(t) => t,
            None => return,
        };
    let (letters, failed, total_files) = {
        let g = cat_arc.read();
        match g.as_ref() {
            Some(cat) => {
                let failed: Vec<DriveFailure> = cat
                    .drive_statuses()
                    .into_iter()
                    .filter_map(|s| match s.state {
                        DriveState::Failed { reason } => {
                            Some(DriveFailure { letter: s.letter, reason })
                        }
                        _ => None,
                    })
                    .collect();
                (cat.drive_letters(), failed, cat.total_nodes())
            }
            None => return,
        }
    };
    let settings = ss_config::load_settings();
    // 索引第一次就绪时恢复上次的范围，之后（设置变更）保持当前选择
    let already_ready = UI.with(|c| c.borrow().as_ref().is_some_and(|u| u.index_ready));
    let wanted = if already_ready {
        let (drive, folder) = selected_scope(combo);
        scope_key(drive, folder.as_deref())
    } else {
        settings.last_scope
    };
    let presets = settings.preset_folders;
    send(combo, CB_RESETCONTENT, 0, 0);
    add_combo(combo, "全部盘");
    for d in &letters {
        add_combo(combo, &format!("{d}:"));
    }
    for f in &presets {
        add_combo(combo, &format!("文件夹 {f}"));
    }
    send(combo, CB_SETCURSEL, scope_index(&wanted, &letters, &presets), 0);
    UI.with(|c| {
        if let Some(ui) = c.borrow_mut().as_mut() {
            ui.drives = letters;
            ui.presets = presets;
            ui.index_ready = true;
            ui.failed = failed;
            ui.total_files = total_files;
        }
    });
    update_combo_width();
    do_search();
}

/// 当前下拉选择 → (盘符过滤, 文件夹过滤)。0=全部盘；1..=盘符；其后=预设文件夹。
fn selected_scope(combo: HWND) -> (Option<char>, Option<String>) {
    let sel = send(combo, CB_GETCURSEL, 0, 0);
    if sel <= 0 {
        return (None, None);
    }
    UI.with(|c| {
        let b = c.borrow();
        let Some(ui) = b.as_ref() else {
            return (None, None);
        };
        let i = sel as usize;
        if i <= ui.drives.len() {
            (ui.drives.get(i - 1).copied(), None)
        } else {
            let pi = i - 1 - ui.drives.len();
            (None, ui.presets.get(pi).cloned())
        }
    })
}

/// 空输入页的行：先最近位置后收藏。已收藏的位置只出现在收藏组。返回行和分组标题的位置。
fn home_rows() -> (Vec<SearchResult>, [i32; 2]) {
    let to_row = |path: String| SearchResult {
        name: leaf_of(&path).to_string(),
        is_dir: true,
        match_ranges: Vec::new(),
        path,
    };
    let favorites = ss_config::favorites();
    let mut rows: Vec<SearchResult> = ss_config::recent()
        .into_iter()
        .filter(|p| !favorites.iter().any(|f| f.eq_ignore_ascii_case(p)))
        .map(to_row)
        .collect();
    let recent = rows.len() as i32;
    let has_favorites = !favorites.is_empty();
    rows.extend(favorites.into_iter().map(to_row));
    let heads = [if recent > 0 { 0 } else { -1 }, if has_favorites { recent } else { -1 }];
    (rows, heads)
}

fn to_scope(drive: Option<char>, folder: Option<String>) -> Scope {
    match (folder, drive) {
        (Some(f), _) => Scope::Folder(f),
        (None, Some(d)) => Scope::Drive(d),
        (None, None) => Scope::All,
    }
}

/// 用户改变了搜索条件：选中第一条。
fn do_search() {
    start_search(false);
}

/// 发起搜索。新结果到达前保留旧列表；空输入和索引未就绪时不需要工作线程。
fn start_search(keep_position: bool) {
    let Some((edit, combo, category, ready)) = UI.with(|c| {
        c.borrow().as_ref().map(|u| (u.edit, u.combo, u.category, u.index_ready))
    }) else {
        return;
    };
    let query = get_text(edit);
    let q = query.trim().to_string();
    let seq = UI.with(|c| c.borrow_mut().as_mut().map(|u| u.seq.begin())).unwrap_or(0);
    if q.is_empty() || !ready {
        search_worker::submit(None);
        let (rows, heads) = if q.is_empty() { home_rows() } else { (Vec::new(), [-1, -1]) };
        show_results(q, rows, heads, Summary::default(), false);
        return;
    }
    let (drive, folder) = selected_scope(combo);
    let settings = ss_config::load_settings();
    search_worker::submit(Some(Job {
        seq,
        query: q,
        scope: to_scope(drive, folder),
        category,
        limit: settings.result_limit,
        exclude_dirs: settings.exclude_dirs,
        hide_system: settings.hide_system,
        keep_position,
    }));
}

/// 工作线程的结果到达。过期的直接丢弃。
fn on_search_done(done: Done) {
    let current = UI.with(|c| c.borrow().as_ref().is_some_and(|u| u.seq.is_current(done.seq)));
    if !current {
        return;
    }
    let resp = done.response;
    let summary = Summary {
        total_hits: resp.total_hits,
        elapsed_ms: resp.elapsed_ms,
        scope_problem: match resp.scope_status {
            ScopeStatus::Ok => None,
            ScopeStatus::FolderMissing => Some(ScopeProblem::FolderMissing),
            ScopeStatus::DriveNotIndexed => Some(ScopeProblem::DriveNotIndexed),
        },
    };
    UI.with(|c| {
        if let Some(u) = c.borrow_mut().as_mut() {
            if u.shown_generation != resp.generation {
                // 文件可能变了，缓存的大小和时间不再可靠
                u.metas.clear();
            }
            u.shown_generation = resp.generation;
            u.counts = resp.category_counts;
        }
    });
    show_results(done.query, resp.items, [-1, -1], summary, done.keep_position);
}

/// 窗口可见且索引有变化时重新搜索。任何时候都不等写锁，忙就留到下一轮。
fn refresh_if_stale() {
    let Some((search, catalog, shown, query_empty, ready)) = UI.with(|c| {
        c.borrow().as_ref().map(|u| {
            (u.search_hwnd, u.catalog.clone(), u.shown_generation, u.query_text.is_empty(), u.index_ready)
        })
    }) else {
        return;
    };
    if !unsafe { IsWindowVisible(search) }.as_bool() {
        return;
    }
    if ready && !query_empty {
        let generation = catalog.try_read().and_then(|g| g.as_ref().map(|cat| cat.generation()));
        if generation.is_some_and(|g| g != shown) {
            start_search(true);
        }
    }
    refresh_view();
}

/// 显示一次搜索的结果。
fn show_results(query: String, items: Vec<SearchResult>, heads: [i32; 2], summary: Summary, keep_position: bool) {
    let Some(list) = UI.with(|c| c.borrow().as_ref().map(|u| u.list)) else {
        return;
    };
    let extra_h = if query.is_empty() { sc_dpi(28) + sc_dpi(22) * HINTS.len() as i32 + sc_dpi(12) } else { 0 };
    UI.with(|c| {
        if let Some(ui) = c.borrow_mut().as_mut() {
            let old_path = usize::try_from(ui.sel)
                .ok()
                .and_then(|i| ui.results.get(i))
                .map(|r| r.path.clone());
            let old_sel = ui.sel;
            ui.results = Rc::new(items);
            ui.heads = heads;
            ui.query_text = query;
            ui.summary = summary;
            ui.extra_h = extra_h;
            if keep_position {
                ui.sel = reselect(old_path.as_deref(), old_sel, ui.results.iter().map(|r| r.path.as_str()));
            } else {
                ui.scroll_y = 0.0;
                ui.target_y = 0.0;
                // 空输入页和原型一样不预选，按方向键才开始选
                ui.sel = if ui.results.is_empty() || ui.query_text.is_empty() { -1 } else { 0 };
                ui.hover = -1;
            }
        }
    });
    let max_scroll = geometry(list).max_scroll();
    UI.with(|c| {
        if let Some(ui) = c.borrow_mut().as_mut() {
            ui.scroll_y = ui.scroll_y.min(max_scroll);
            ui.target_y = ui.target_y.min(max_scroll);
        }
    });
    refresh_view();
    update_tab_layout();
    invalidate_tabs();
    unsafe {
        update_scrollbar(list);
    }
}

/// 按当前状态重新计算主体内容、横幅和状态栏，并重绘。
fn refresh_view() {
    let Some((search, combo, list, banner_wnd, foot_wnd)) = UI.with(|c| {
        c.borrow().as_ref().map(|u| (u.search_hwnd, u.combo, u.list, u.banner_wnd, u.foot_wnd))
    }) else {
        return;
    };
    let (drive, folder) = selected_scope(combo);
    let label = match (&folder, drive) {
        (Some(f), _) => f.clone(),
        (None, Some(d)) => format!("{d} 盘"),
        (None, None) => UI.with(|c| {
            c.borrow().as_ref().map(|u| all_drives_label(&u.drives)).unwrap_or_default()
        }),
    };
    let scope_is_all = folder.is_none() && drive.is_none();
    let building = BUILDING.lock().clone();
    let banner_changed = UI.with(|c| {
        let mut b = c.borrow_mut();
        let Some(ui) = b.as_mut() else {
            return false;
        };
        let input = ViewInput {
            query_empty: ui.query_text.is_empty(),
            index_ready: ui.index_ready,
            scope_problem: ui.summary.scope_problem,
            shown: ui.results.len(),
            total_hits: ui.summary.total_hits,
            scope_is_all,
            category_is_all: ui.category == Category::All,
            failed: &ui.failed,
            building: &building,
            scope_label: &label,
            elapsed_ms: ui.summary.elapsed_ms,
            total_files: ui.total_files,
            synced_secs_ago: crate::synced_secs_ago(),
        };
        let new_body = body(&input);
        let new_banner = banner(&input);
        let status = status_line(&input);
        let changed = new_banner.is_some() != ui.view_banner.is_some();
        ui.view_body = new_body;
        ui.view_banner = new_banner;
        ui.status = status;
        changed
    });
    unsafe {
        if banner_changed {
            layout(search);
        }
        let _ = InvalidateRect(Some(list), None, false);
        let _ = InvalidateRect(Some(banner_wnd), None, false);
        let _ = InvalidateRect(Some(foot_wnd), None, false);
    }
}

/// 切换分类标签并重新搜索。
fn set_category(category: Category) {
    UI.with(|c| {
        if let Some(ui) = c.borrow_mut().as_mut() {
            ui.category = category;
        }
    });
    invalidate_tabs();
    do_search();
}

/// 命中数变化后重排分类标签。
fn update_tab_layout() {
    if let Some(search) = UI.with(|c| c.borrow().as_ref().map(|u| u.search_hwnd)) {
        unsafe {
            let mut rc = RECT::default();
            let _ = GetClientRect(search, &mut rc);
            let cw = UI.with(|c| c.borrow().as_ref().map_or(0, |u| u.combo_w));
            let (m, ctl_h) = (sc_dpi(10), sc_dpi(28));
            let y = sc_dpi(INPUT_H) + (sc_dpi(BAR_H) - ctl_h) / 2;
            layout_tabs(rc.right - 2 * m - cw - sc_dpi(8), m, y, ctl_h, false);
        }
    }
}

fn invalidate_tabs() {
    if let Some(cats) = UI.with(|c| c.borrow().as_ref().map(|u| u.cats.clone())) {
        for b in cats {
            let _ = unsafe { InvalidateRect(Some(b), None, true) };
        }
    }
}

/// 响应横幅或空状态里的操作。
fn run_act(act: Act) {
    let Some((combo, failed)) = UI.with(|c| {
        c.borrow().as_ref().map(|u| (u.combo, u.failed.iter().map(|f| f.letter).collect::<Vec<char>>()))
    }) else {
        return;
    };
    match act {
        Act::SearchAllScopes => {
            send(combo, CB_SETCURSEL, 0, 0);
            update_combo_width();
            do_search();
        }
        Act::ClearCategory => set_category(Category::All),
        Act::RemovePreset => {
            if let (_, Some(folder)) = selected_scope(combo) {
                let _ = ss_config::update_settings(|s| s.preset_folders.retain(|f| f != &folder));
            }
            refresh_scopes();
        }
        Act::RetryFailedDrives => {
            crate::rebuild_drives(failed);
        }
        Act::Activate(open_directly) => activate_selected(open_directly),
        Act::CopyPath => {
            copy_selected(false);
        }
    }
}

/// 窗口隐藏时保存搜索词、范围和分类（跨重启记忆）。搜索词为空时保留原值；内容没有变化时不写盘。
fn persist_last_query() {
    let Some((edit, combo, category, ready)) = UI.with(|c| {
        c.borrow().as_ref().map(|u| (u.edit, u.combo, u.category, u.index_ready))
    }) else {
        return;
    };
    let q = get_text(edit);
    let q = q.trim().to_string();
    let (drive, folder) = selected_scope(combo);
    let scope = scope_key(drive, folder.as_deref());
    let key = CATS.iter().find(|c| c.0 == category).map_or("", |c| c.2);
    // 写盘失败时进程内的值已更新，下次隐藏会再试，这里不打断用户
    let _ = ss_config::update_settings(|s| {
        if !q.is_empty() {
            s.last_query = q;
        }
        // 索引就绪前范围下拉框还是空的，不能覆盖上次的范围
        if ready {
            s.last_scope = scope;
        }
        s.last_category = key.to_string();
    });
}

/// 当前选中的结果：（路径，文件名）。
fn selected_item() -> Option<(String, String)> {
    UI.with(|c| {
        let b = c.borrow();
        let ui = b.as_ref()?;
        let r = ui.results.get(usize::try_from(ui.sel).ok()?)?;
        Some((r.path.clone(), r.name.clone()))
    })
}

/// 在状态栏显示一条短提示。
fn show_toast(text: String) {
    let Some(foot) = UI.with(|c| {
        c.borrow_mut().as_mut().map(|u| {
            u.toast = Some(text);
            u.foot_wnd
        })
    }) else {
        return;
    };
    unsafe {
        SetTimer(Some(foot), TOAST_TIMER, TOAST_MS, None);
        let _ = InvalidateRect(Some(foot), None, false);
    }
}

/// 把操作失败告诉用户。路径已不存在时同时把这一行标为失效。
fn report_action_error(path: &str, e: ss_shell::ActionError) {
    if matches!(e, ss_shell::ActionError::NotFound(_)) {
        let list = UI.with(|c| {
            c.borrow_mut().as_mut().map(|u| {
                put_meta(&mut u.metas, path.to_string(), Rc::new(RowMeta::Gone));
                u.list
            })
        });
        if let Some(list) = list {
            let _ = unsafe { InvalidateRect(Some(list), None, false) };
        }
        show_toast("这个文件已不存在".to_string());
    } else {
        show_toast(e.to_string());
    }
}

/// 默认定位；Ctrl+双击或底部“打开”操作直接打开。成功后隐藏窗口。
fn activate_selected(open_directly: bool) {
    let Some((search, list)) = UI.with(|c| c.borrow().as_ref().map(|u| (u.search_hwnd, u.list))) else {
        return;
    };
    let Some((path, _)) = selected_item() else {
        return;
    };
    let result = if open_directly {
        ss_shell::open_deelevated(&path)
    } else {
        ss_shell::reveal(&path)
    };
    match result {
        Ok(()) => unsafe { hide_window(search, list) },
        Err(e) => report_action_error(&path, e),
    }
}

/// Ctrl+C 复制路径，Ctrl+Shift+C 复制文件名。
fn copy_selected(name_only: bool) -> bool {
    let Some(search) = UI.with(|c| c.borrow().as_ref().map(|u| u.search_hwnd)) else {
        return false;
    };
    let Some((path, name)) = selected_item() else {
        return false;
    };
    let result = if name_only {
        ss_shell::copy_text(search, &name)
    } else {
        ss_shell::copy_path(search, &path)
    };
    match result {
        Ok(()) if name_only => show_toast(format!("已复制文件名：{name}")),
        Ok(()) => show_toast(format!("已复制路径：{path}")),
        Err(e) => report_action_error(&path, e),
    }
    true
}

/// 在屏幕坐标处弹出选中项的系统右键菜单。
fn context_menu_at(x: i32, y: i32) {
    let Some(search) = UI.with(|c| c.borrow().as_ref().map(|u| u.search_hwnd)) else {
        return;
    };
    let Some((path, _)) = selected_item() else {
        return;
    };
    if let Err(e) = ss_shell::show_context_menu(search, &path, x, y) {
        report_action_error(&path, e);
    }
}

fn key_down(vk: u16) -> bool {
    unsafe { GetKeyState(vk as i32) < 0 }
}

fn put_meta(metas: &mut Vec<(String, Rc<RowMeta>)>, path: String, m: Rc<RowMeta>) {
    match metas.iter_mut().find(|(p, _)| *p == path) {
        Some(entry) => entry.1 = m,
        None => metas.push((path, m)),
    }
}

/// 把读取线程的结果放进缓存。
fn absorb_meta() {
    let results = meta::take_results();
    let list = UI.with(|c| {
        c.borrow_mut().as_mut().map(|u| {
            if u.metas.len() > META_CACHE_MAX {
                u.metas.clear();
            }
            for (path, m) in results {
                put_meta(&mut u.metas, path, Rc::new(m));
            }
            u.list
        })
    });
    if let Some(list) = list {
        let _ = unsafe { InvalidateRect(Some(list), None, false) };
    }
}

/// 隐藏搜索窗的统一出口（Esc / 关闭 / 热键切换）：记住查询词、停动画、归还绘图后备缓冲。
unsafe fn hide_window(search: HWND, list: HWND) {
    persist_last_query();
    stop_anim(list);
    let _ = ShowWindow(search, SW_HIDE);
    paint::release();
}

pub fn toggle() {
    let (search, edit, list) =
        match UI.with(|c| c.borrow().as_ref().map(|u| (u.search_hwnd, u.edit, u.list))) {
            Some(t) => t,
            None => return,
        };
    unsafe {
        if IsWindowVisible(search).as_bool() {
            hide_window(search, list);
        } else {
            center(search);
            UI.with(|c| {
                if let Some(u) = c.borrow_mut().as_mut() {
                    u.animate = animations_enabled();
                }
            });
            let _ = ShowWindow(search, SW_SHOW);
            let _ = SetForegroundWindow(search);
            let _ = SetFocus(Some(edit));
            send(edit, EM_SETSEL, 0, -1);
            let _ = InvalidateRect(Some(search), None, true);
            refresh_if_stale();
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
    if msg.message != WM_KEYDOWN && msg.message != WM_SYSKEYDOWN {
        return false;
    }
    // 搜索框始终持焦：方向键在此控制列表选中，回车/Esc 全局处理
    if msg.hwnd != edit && msg.hwnd != list {
        return false;
    }
    if msg.message == WM_SYSKEYDOWN {
        // Alt+1…8 切换分类
        let digit = (msg.wParam.0 as u32).wrapping_sub(u32::from(b'1')) as usize;
        if let Some(cat) = CATS.get(digit) {
            set_category(cat.0);
            return true;
        }
        return false;
    }
    let vk = (msg.wParam.0 as u32) as u16;
    if vk == VK_ESCAPE.0 {
        unsafe {
            hide_window(search, list);
        }
        return true;
    }
    if vk == VK_RETURN.0 {
        activate_selected(false);
        return true;
    }
    if vk == VK_C.0 && key_down(VK_CONTROL.0) {
        // 输入框只选中了部分文字时，让它复制这部分文字
        let mut start = 0u32;
        let mut end = 0u32;
        send(edit, EM_GETSEL, &mut start as *mut u32 as usize, &mut end as *mut u32 as isize);
        let len = send(edit, WM_GETTEXTLENGTH, 0, 0) as u32;
        if is_partial_selection(start, end, len) {
            return false;
        }
        return copy_selected(key_down(VK_SHIFT.0));
    }
    let how = if vk == VK_DOWN.0 {
        SelMove::Down
    } else if vk == VK_UP.0 {
        SelMove::Up
    } else if vk == VK_NEXT.0 {
        SelMove::PageDown
    } else if vk == VK_PRIOR.0 {
        SelMove::PageUp
    } else if vk == VK_HOME.0 {
        SelMove::Home
    } else if vk == VK_END.0 {
        SelMove::End
    } else {
        return false;
    };
    let g = geometry(list);
    let cur = UI.with(|c| c.borrow().as_ref().map(|u| u.sel).unwrap_or(-1));
    let Some(new) = moved(cur, how, g.count, g.page()) else {
        return false;
    };
    set_sel(list, new);
    true
}

// ---- 分类标签 / 范围按钮 ----

/// 分类标签的宽度：文字、命中数和内边距。
fn tab_width(cat: Category, label: &str) -> i32 {
    let (font_ui, font_small, count) = UI
        .with(|c| {
            c.borrow().as_ref().map(|u| {
                let count = (!u.query_text.is_empty() && u.index_ready).then(|| compact(u.counts.get(cat)));
                (u.font_ui, u.font_path, count)
            })
        })
        .unwrap_or((HFONT(std::ptr::null_mut()), HFONT(std::ptr::null_mut()), None));
    let n = count.as_deref().map_or(0, |n| sc_dpi(4) + measure_text(font_small, n));
    measure_text(font_ui, label) + n + sc_dpi(20)
}

unsafe fn draw_cat_button(dis: &DRAWITEMSTRUCT) {
    let t = theme::current();
    let idx = (dis.CtlID - CAT_BASE) as usize;
    let cat = CATS.get(idx).map(|c| c.0).unwrap_or(Category::All);
    let label = CATS.get(idx).map(|c| c.1).unwrap_or("");
    let pressed = dis.itemState.0 & ODS_SELECTED.0 != 0;
    // 兜底空 HFONT（UI 未初始化时 DC 用默认字体退化绘制），避免 panic=abort 下裸 unwrap 拉死进程
    let null_font = HFONT(std::ptr::null_mut());
    let (active, font_ui, font_small, count) = UI
        .with(|c| {
            c.borrow().as_ref().map(|u| {
                let count = (!u.query_text.is_empty() && u.index_ready).then(|| compact(u.counts.get(cat)));
                (u.category == cat, u.font_ui, u.font_path, count)
            })
        })
        .unwrap_or((false, null_font, null_font, None));
    let radius = sc_dpi(RADIUS);
    let gap = sc_dpi(4);
    paint::buffered(dis.hDC, dis.rcItem, |hdc, rc| unsafe {
        FillRect(hdc, &rc, theme::solid_brush(t.alt_bg));
        if active {
            paint::round_rect(hdc, rc, radius, t.accent, None, t.alt_bg);
        } else if pressed {
            paint::round_rect(hdc, rc, radius, t.sel_bg, None, t.alt_bg);
        }
        let fg = if active { t.accent_fg } else { t.fg };
        // 文字 + 命中数整体居中；放不下时只画文字
        let lw = measure_text(font_ui, label);
        let avail = rc.right - rc.left;
        let flags = DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX;
        let count = count.as_deref().filter(|n| lw + gap + measure_text(font_small, n) <= avail - gap);
        let cw = count.map_or(0, |n| gap + measure_text(font_small, n));
        let start = rc.left + (avail - lw - cw) / 2;
        let mut lr = RECT { left: start, top: rc.top, right: rc.right, bottom: rc.bottom };
        draw_text(hdc, label, &mut lr, fg, font_ui, flags);
        if let Some(n) = count {
            let mut nr = RECT { left: start + lw + gap, top: rc.top, right: rc.right, bottom: rc.bottom };
            draw_text(hdc, n, &mut nr, if active { fg } else { t.path_fg }, font_small, flags);
        }
    });
}

/// 范围下拉框当前选中项的文字。
fn combo_text(combo: HWND, index: isize) -> String {
    if index < 0 {
        return String::new();
    }
    let len = send(combo, CB_GETLBTEXTLEN, index as usize, 0);
    if len <= 0 {
        return String::new();
    }
    let mut buf = vec![0u16; len as usize + 1];
    let n = send(combo, CB_GETLBTEXT, index as usize, buf.as_mut_ptr() as isize);
    String::from_utf16_lossy(&buf[..(n.max(0) as usize).min(buf.len())])
}

unsafe fn draw_scope_button(dis: &DRAWITEMSTRUCT) {
    let t = theme::current();
    let null_font = HFONT(std::ptr::null_mut());
    let (combo, font_ui, font_chevron) = UI
        .with(|c| c.borrow().as_ref().map(|u| (Some(u.combo), u.font_ui, u.font_chevron)))
        .unwrap_or((None, null_font, null_font));
    let label = combo.map_or(String::new(), |h| combo_text(h, send(h, CB_GETCURSEL, 0, 0)));
    let pressed = dis.itemState.0 & ODS_SELECTED.0 != 0;
    let radius = sc_dpi(RADIUS);
    let (pad, chev_w) = (sc_dpi(10), sc_dpi(14));
    paint::buffered(dis.hDC, dis.rcItem, |hdc, rc| unsafe {
        FillRect(hdc, &rc, theme::solid_brush(t.alt_bg));
        let fill = if pressed { t.sel_bg } else { t.bg };
        paint::round_rect(hdc, rc, radius, fill, Some(t.border), t.alt_bg);
        let mut tr = RECT { left: rc.left + pad, top: rc.top, right: rc.right - pad - chev_w, bottom: rc.bottom };
        draw_text(hdc, &label, &mut tr, t.fg, font_ui, DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_PATH_ELLIPSIS);
        let mut cr = RECT { left: rc.right - pad - chev_w, top: rc.top, right: rc.right - pad, bottom: rc.bottom };
        draw_text(hdc, "\u{E70D}", &mut cr, t.path_fg, font_chevron, DT_RIGHT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX);
    });
}

/// 点击范围按钮：弹出系统菜单选择范围。
unsafe fn open_scope_menu(owner: HWND) {
    let Some((combo, button)) = UI.with(|c| c.borrow().as_ref().map(|u| (u.combo, u.scope_btn))) else {
        return;
    };
    let Ok(menu) = CreatePopupMenu() else {
        return;
    };
    let current = send(combo, CB_GETCURSEL, 0, 0);
    for i in 0..send(combo, CB_GETCOUNT, 0, 0) {
        let text = crate::wide(&combo_text(combo, i));
        let flags = if i == current { MF_STRING | MF_CHECKED } else { MF_STRING };
        let _ = AppendMenuW(menu, flags, (i + 1) as usize, PCWSTR(text.as_ptr()));
    }
    let mut r = RECT::default();
    let _ = GetWindowRect(button, &mut r);
    let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTALIGN, r.right, r.bottom + sc_dpi(2), Some(0), owner, None);
    let _ = DestroyMenu(menu);
    if cmd.0 > 0 && (cmd.0 as isize - 1) != current {
        send(combo, CB_SETCURSEL, (cmd.0 - 1) as usize, 0);
        update_combo_width();
        do_search();
    }
}

// ---- 结果列表（自绘 + 平滑滚动）----

fn geometry(list: HWND) -> ListGeometry {
    let mut rc = RECT::default();
    let _ = unsafe { GetClientRect(list, &mut rc) };
    let view_h = rc.bottom - rc.top;
    UI.with(|c| {
        c.borrow()
            .as_ref()
            .map(|u| ListGeometry {
                count: u.results.len() as i32,
                item_h: u.item_h,
                view_h,
                extra_h: u.extra_h,
                header_h: if u.query_text.is_empty() { u.dpi * 28 / 96 } else { 0 },
                heads: u.heads,
            })
            .unwrap_or(ListGeometry::plain(0, 46, view_h))
    })
}

unsafe fn update_scrollbar(list: HWND) {
    let _ = InvalidateRect(Some(list), None, false);
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
    let max_scroll = geometry(list).max_scroll();
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
    let animate = UI.with(|c| {
        c.borrow_mut().as_mut().is_some_and(|u| {
            u.target_y = y.clamp(0.0, max_scroll);
            if !u.animate {
                u.scroll_y = u.target_y;
            }
            u.animate
        })
    });
    if animate {
        start_anim(list);
    } else {
        unsafe {
            update_scrollbar(list);
        }
    }
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
    let count = geometry(list).count;
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
    let target = UI.with(|c| c.borrow().as_ref().map(|u| u.target_y).unwrap_or(0.0));
    let new_t = geometry(list).ensure_visible(idx, target);
    if (new_t - target).abs() > 0.5 {
        set_target(list, new_t);
    }
}

/// 画文件名，命中部分带圆角底色。放不下时最后一段以省略号结尾。
#[allow(clippy::too_many_arguments)]
unsafe fn draw_name(hdc: HDC, r: &SearchResult, suffix: &str, rect: RECT, fg: u32, font: HFONT, mark: u32, row_bg: u32) {
    let flags = DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS;
    let mut x = rect.left;
    let segments = highlight_segments(&r.name, &r.match_ranges);
    let suffix_seg = (suffix, false);
    for (text, hit) in segments.iter().chain(std::iter::once(&suffix_seg)) {
        if text.is_empty() {
            continue;
        }
        if x >= rect.right {
            break;
        }
        let w = measure_text(font, text);
        let mut sr = RECT { left: x, top: rect.top, right: (x + w).min(rect.right), bottom: rect.bottom };
        if *hit {
            let h = sc_dpi(18);
            let top = rect.top + (rect.bottom - rect.top - h) / 2;
            let bg = RECT { left: sr.left, top, right: sr.right, bottom: top + h };
            paint::round_rect(hdc, bg, sc_dpi(3), mark, None, row_bg);
        }
        draw_text(hdc, text, &mut sr, fg, font, flags);
        x += w;
    }
}

#[allow(clippy::too_many_arguments)]
unsafe fn draw_one(hdc: HDC, rect: RECT, r: &SearchResult, row_meta: Option<&RowMeta>, selected: bool, hover: bool, t: &Theme, fname: HFONT, fpath: HFONT) {
    let row_bg = if selected {
        t.sel_bg
    } else if hover {
        t.alt_bg
    } else {
        t.bg
    };
    if row_bg != t.bg {
        let inset = sc_dpi(6);
        let band = RECT { left: rect.left + inset, top: rect.top + 1, right: rect.right - inset, bottom: rect.bottom - 1 };
        paint::round_rect(hdc, band, sc_dpi(RADIUS), row_bg, None, t.bg);
    }
    let pad = sc_dpi(14);
    let gap = sc_dpi(10);
    let icon = sc_dpi(18);
    let icy = rect.top + (rect.bottom - rect.top - icon) / 2;
    if let Some(hicon) = icons::icon_for(&r.name, r.is_dir) {
        let _ = DrawIconEx(hdc, rect.left + pad, icy, hicon, icon, icon, 0, None, DI_NORMAL);
    }
    let tx = rect.left + pad + icon + gap;
    let gone = matches!(row_meta, Some(RowMeta::Gone));
    let right = if matches!(row_meta, Some(RowMeta::Info { .. })) {
        rect.right - sc_dpi(112)
    } else {
        rect.right - pad
    };
    let mid = rect.top + (rect.bottom - rect.top) / 2;
    if let Some(RowMeta::Info { size, modified }) = row_meta {
        let flags = DT_RIGHT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX;
        let col = rect.right - sc_dpi(108);
        let edge = rect.right - pad;
        if size.is_empty() {
            let mut tr = RECT { left: col, top: rect.top, right: edge, bottom: rect.bottom };
            draw_text(hdc, modified, &mut tr, t.path_fg, fpath, flags);
        } else {
            let mut tr = RECT { left: col, top: rect.top + sc_dpi(3), right: edge, bottom: mid + sc_dpi(1) };
            draw_text(hdc, size, &mut tr, t.path_fg, fpath, flags);
            let mut tr = RECT { left: col, top: mid - sc_dpi(1), right: edge, bottom: rect.bottom - sc_dpi(3) };
            draw_text(hdc, modified, &mut tr, t.path_fg, fpath, flags);
        }
    }
    let name_rect = RECT { left: tx, top: rect.top + sc_dpi(3), right, bottom: mid + sc_dpi(2) };
    let name_fg = if gone { t.path_fg } else { t.fg };
    draw_name(hdc, r, if gone { "（已不存在）" } else { "" }, name_rect, name_fg, fname, t.mark, row_bg);
    let mut path_rect = RECT { left: tx, top: mid - sc_dpi(1), right, bottom: rect.bottom - sc_dpi(3) };
    draw_text(hdc, parent_of(&r.path), &mut path_rect, t.path_fg, fpath, DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_PATH_ELLIPSIS);
}

unsafe fn paint_list(list: HWND) {
    let mut ps = PAINTSTRUCT::default();
    let hdc = BeginPaint(list, &mut ps);
    let mut rc = RECT::default();
    let _ = GetClientRect(list, &mut rc);
    let t = theme::current();
    let g = geometry(list);
    let null_font = HFONT(std::ptr::null_mut());
    let (scroll_y, sel, hover, fname, fpath, ftitle, view_body) = UI.with(|c| {
        c.borrow()
            .as_ref()
            .map(|u| (u.scroll_y, u.sel, u.hover, u.font_ui, u.font_path, u.font_title, u.view_body))
            .unwrap_or((0.0, -1, -1, null_font, null_font, null_font, Body::Home))
    });
    paint::buffered(hdc, rc, |mem, lrc| unsafe {
        FillRect(mem, &lrc, theme::solid_brush(t.bg));
        SetBkMode(mem, TRANSPARENT);
        if let Some(hits) = paint_message(mem, lrc, view_body, &t, ftitle, fpath) {
            UI.with(|c| {
                if let Some(u) = c.borrow_mut().as_mut() {
                    u.list_hits = hits;
                }
            });
            return;
        }
        UI.with(|c| {
            if let Some(u) = c.borrow_mut().as_mut() {
                u.list_hits.clear();
            }
        });
        let Some(results) = UI.with(|c| c.borrow().as_ref().map(|u| u.results.clone())) else {
            return;
        };
        let sy = scroll_y as i32;
        let mut missing: Vec<String> = Vec::new();
        if let Some((first, last)) = g.visible(scroll_y) {
            for i in first..=last {
                let y = g.row_top(i) - sy;
                let item_rect = RECT { left: 0, top: y, right: lrc.right, bottom: y + g.item_h };
                if let Some(r) = results.get(i as usize) {
                    let row_meta = UI.with(|c| {
                        c.borrow().as_ref().and_then(|u| {
                            u.metas.iter().find(|(p, _)| *p == r.path).map(|(_, m)| m.clone())
                        })
                    });
                    if row_meta.is_none() {
                        missing.push(r.path.clone());
                    }
                    draw_one(mem, item_rect, r, row_meta.as_deref(), i == sel, i == hover, &t, fname, fpath);
                }
            }
        }
        if view_body == Body::Home {
            for (head, title) in g.heads.iter().zip(["最近位置", "收藏"]) {
                if *head >= 0 {
                    let top = g.row_top(*head) - g.header_h - sy;
                    paint_group_title(mem, lrc, top, g.header_h, title, &t, fpath);
                }
            }
            paint_hints(mem, lrc, g.row_top(g.count) - sy, &t, fpath);
        }
        if let Some((top, h)) = scroll_thumb(g.total(), g.view_h, scroll_y, sc_dpi(28)) {
            let w = sc_dpi(5);
            let x = lrc.right - w - sc_dpi(3);
            let thumb = RECT { left: x, top: top + sc_dpi(2), right: x + w, bottom: top + h - sc_dpi(2) };
            paint::round_rect(mem, thumb, w, t.thumb, None, t.bg);
        }
        if !missing.is_empty() {
            UI.with(|c| {
                if let Some(u) = c.borrow_mut().as_mut() {
                    let loading = Rc::new(RowMeta::Loading);
                    for p in &missing {
                        put_meta(&mut u.metas, p.clone(), loading.clone());
                    }
                }
            });
            meta::request(missing);
        }
    });
    let _ = EndPaint(list, &ps);
}

/// 列表区域的提示文字（索引中、范围失效、无结果）。不是这些状态时返回 `None`。
/// 返回的是可点击按钮的区域。
unsafe fn paint_message(hdc: HDC, rc: RECT, view: Body, t: &Theme, ftitle: HFONT, fsub: HFONT) -> Option<Vec<(RECT, Act)>> {
    let query = UI.with(|c| c.borrow().as_ref().map(|u| u.query_text.clone()).unwrap_or_default());
    let (title, sub, actions): (String, &str, Vec<(&str, Act)>) = match view {
        Body::Home | Body::Results => return None,
        Body::Indexing => ("索引建立中".to_string(), "完成后会自动显示结果。", Vec::new()),
        Body::ScopeInvalid => {
            ("无法搜索这个文件夹".to_string(), "它可能已被删除或改名。", Vec::new())
        }
        Body::NoResults { offer_all_scopes, offer_clear_category } => {
            let mut acts = Vec::new();
            if offer_all_scopes {
                acts.push(("搜索全部盘", Act::SearchAllScopes));
            }
            if offer_clear_category {
                acts.push(("清除分类", Act::ClearCategory));
            }
            (format!("没有找到“{query}”"), "可以换个关键字，或扩大搜索范围。", acts)
        }
    };
    let flags = DT_CENTER | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS;
    let top = rc.top + sc_dpi(56);
    let side = sc_dpi(22);
    let mut r = RECT { left: rc.left + side, top, right: rc.right - side, bottom: top + sc_dpi(26) };
    draw_text(hdc, &title, &mut r, t.fg, ftitle, flags);
    let mut r = RECT { left: rc.left + side, top: top + sc_dpi(28), right: rc.right - side, bottom: top + sc_dpi(50) };
    draw_text(hdc, sub, &mut r, t.path_fg, fsub, flags);

    let gap = sc_dpi(8);
    let pad = sc_dpi(14);
    let widths: Vec<i32> = actions.iter().map(|(l, _)| measure_text(fsub, l) + pad * 2).collect();
    let total: i32 = widths.iter().sum::<i32>() + gap * (widths.len() as i32 - 1).max(0);
    let mut x = (rc.right - rc.left - total) / 2;
    let y = top + sc_dpi(64);
    let mut hits = Vec::new();
    for (i, ((label, act), w)) in actions.iter().zip(&widths).enumerate() {
        let b = RECT { left: x, top: y, right: x + w, bottom: y + sc_dpi(28) };
        let primary = i == 0;
        let radius = sc_dpi(RADIUS);
        if primary {
            paint::round_rect(hdc, b, radius, t.accent, None, t.bg);
        } else {
            paint::round_rect(hdc, b, radius, t.bg, Some(t.border), t.bg);
        }
        let mut lr = b;
        draw_text(hdc, label, &mut lr, if primary { t.accent_fg } else { t.fg }, fsub, DT_CENTER | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX);
        hits.push((b, *act));
        x += w + gap;
    }
    Some(hits)
}

unsafe fn paint_group_title(hdc: HDC, rc: RECT, top: i32, h: i32, title: &str, t: &Theme, font: HFONT) {
    let mut r = RECT { left: rc.left + sc_dpi(14), top: top + sc_dpi(6), right: rc.right - sc_dpi(14), bottom: top + h };
    draw_text(hdc, title, &mut r, t.path_fg, font, DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX);
}

/// 空输入页底部的语法提示。
unsafe fn paint_hints(hdc: HDC, rc: RECT, top: i32, t: &Theme, font: HFONT) {
    let title_h = sc_dpi(28);
    paint_group_title(hdc, rc, top, title_h, "搜索语法", t, font);
    let line = sc_dpi(22);
    let left = rc.left + sc_dpi(14);
    let right = rc.right - sc_dpi(14);
    let flags = DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS;
    let code_w = sc_dpi(120);
    let mut y = top + title_h;
    for (code, desc) in HINTS {
        let mut r = RECT { left, top: y, right: left + code_w, bottom: y + line };
        draw_text(hdc, code, &mut r, t.fg, font, flags);
        let mut r = RECT { left: left + code_w, top: y, right, bottom: y + line };
        draw_text(hdc, desc, &mut r, t.path_fg, font, flags);
        y += line;
    }
}

/// 横幅和状态栏的绘制。
unsafe fn paint_strip(hwnd: HWND) {
    let mut ps = PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut ps);
    let mut rc = RECT::default();
    let _ = GetClientRect(hwnd, &mut rc);
    let t = theme::current();
    let state = UI.with(|c| {
        c.borrow().as_ref().map(|u| {
            let status = u.toast.clone().unwrap_or_else(|| u.status.clone());
            (u.foot_wnd, u.view_banner.clone(), status, u.toast.is_some(), u.font_path)
        })
    });
    if let Some((foot_wnd, view_banner, status, is_toast, font)) = state {
        paint::buffered(hdc, rc, |mem, lrc| unsafe {
            SetBkMode(mem, TRANSPARENT);
            if hwnd == foot_wnd {
                let hits = paint_foot(mem, lrc, &status, is_toast, &t, font);
                UI.with(|c| {
                    if let Some(u) = c.borrow_mut().as_mut() {
                        u.foot_hits = hits;
                    }
                });
            } else if let Some(b) = &view_banner {
                let hits = paint_banner(mem, lrc, b, &t, font);
                UI.with(|c| {
                    if let Some(u) = c.borrow_mut().as_mut() {
                        u.banner_hits = hits;
                    }
                });
            }
        });
    }
    let _ = EndPaint(hwnd, &ps);
}

unsafe fn paint_foot(hdc: HDC, rc: RECT, status: &str, is_toast: bool, t: &Theme, font: HFONT) -> Vec<(RECT, Act)> {
    FillRect(hdc, &rc, theme::solid_brush(t.alt_bg));
    let line = RECT { left: rc.left, top: rc.top, right: rc.right, bottom: rc.top + 1 };
    FillRect(hdc, &line, theme::solid_brush(t.border));
    let keys = [
        ("定位", "Enter / 双击", Act::Activate(false)),
        ("打开", "Ctrl+双击", Act::Activate(true)),
        ("复制路径", "Ctrl+C", Act::CopyPath),
    ];
    let flags = DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX;
    let pad = sc_dpi(5);
    let chip_h = sc_dpi(18);
    let chip_top = rc.top + (rc.bottom - rc.top - chip_h) / 2;
    let mut right = rc.right - sc_dpi(14);
    let mut hits = Vec::new();
    for (label, key, act) in keys.iter().rev() {
        let kw = measure_text(font, key) + pad * 2;
        let chip = RECT { left: right - kw, top: chip_top, right, bottom: chip_top + chip_h };
        paint::round_rect(hdc, chip, sc_dpi(4), t.bg, Some(t.border), t.alt_bg);
        let mut kr = chip;
        draw_text(hdc, key, &mut kr, t.path_fg, font, DT_CENTER | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX);
        let lw = measure_text(font, label);
        let label_left = chip.left - sc_dpi(4) - lw;
        let mut lr = RECT { left: label_left, top: rc.top, right: chip.left, bottom: rc.bottom };
        draw_text(hdc, label, &mut lr, t.path_fg, font, flags);
        hits.push((RECT { left: label_left, top: rc.top, right, bottom: rc.bottom }, *act));
        right = label_left - sc_dpi(12);
    }
    let color = if is_toast { t.accent } else { t.path_fg };
    let mut r = RECT { left: rc.left + sc_dpi(14), top: rc.top, right, bottom: rc.bottom };
    draw_text(hdc, status, &mut r, color, font, flags | DT_END_ELLIPSIS);
    hits
}

unsafe fn paint_banner(hdc: HDC, rc: RECT, b: &Banner, t: &Theme, font: HFONT) -> Vec<(RECT, Act)> {
    let (text, bg, fg, permille, actions): (&str, u32, u32, Option<u16>, Vec<(&str, Act)>) = match b {
        Banner::Indexing { text, permille } => (text, t.warn_bg, t.warn, *permille, Vec::new()),
        Banner::Truncated { text } => (text, t.warn_bg, t.warn, None, Vec::new()),
        Banner::DriveFailed { text } => (text, t.err_bg, t.err, None, vec![("重试", Act::RetryFailedDrives)]),
        Banner::ScopeInvalid { text } => (
            text,
            t.err_bg,
            t.err,
            None,
            vec![("改搜全部盘", Act::SearchAllScopes), ("从预设中移除", Act::RemovePreset)],
        ),
    };
    FillRect(hdc, &rc, theme::solid_brush(bg));
    let pad = sc_dpi(14);
    let gap = sc_dpi(12);
    let mut right = rc.right - pad;
    let mut hits = Vec::new();
    for (label, act) in actions.iter().rev() {
        let w = measure_text(font, label);
        let r = RECT { left: right - w, top: rc.top, right, bottom: rc.bottom };
        let mut lr = r;
        draw_text(hdc, label, &mut lr, t.accent, font, DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX);
        hits.push((r, *act));
        right -= w + gap;
    }
    let bar = permille.map(|_| sc_dpi(4));
    let mut tr = RECT { left: rc.left + pad, top: rc.top, right, bottom: rc.bottom - bar.unwrap_or(0) };
    draw_text(hdc, text, &mut tr, fg, font, DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS);
    if let (Some(p), Some(h)) = (permille, bar) {
        let track = RECT { left: rc.left + pad, top: rc.bottom - h - sc_dpi(3), right: rc.right - pad, bottom: rc.bottom - sc_dpi(3) };
        paint::round_rect(hdc, track, h, t.border, None, bg);
        let w = (track.right - track.left) * p as i32 / 1000;
        let fill = RECT { right: track.left + w, ..track };
        paint::round_rect(hdc, fill, h, t.accent, None, t.border);
    }
    hits
}

fn act_at(hits: &[(RECT, Act)], x: i32, y: i32) -> Option<Act> {
    hits.iter()
        .find(|(r, _)| x >= r.left && x < r.right && y >= r.top && y < r.bottom)
        .map(|(_, a)| *a)
}

unsafe extern "system" fn strip_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => {
            paint_strip(hwnd);
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let x = (lp.0 & 0xFFFF) as i16 as i32;
            let y = ((lp.0 >> 16) & 0xFFFF) as i16 as i32;
            let act = UI.with(|c| {
                c.borrow().as_ref().and_then(|u| {
                    if hwnd == u.banner_wnd {
                        act_at(&u.banner_hits, x, y)
                    } else {
                        act_at(&u.foot_hits, x, y)
                    }
                })
            });
            if let Some(act) = act {
                run_act(act);
            }
            LRESULT(0)
        }
        WM_TIMER if wp.0 == TOAST_TIMER => {
            let _ = KillTimer(Some(hwnd), TOAST_TIMER);
            UI.with(|c| {
                if let Some(u) = c.borrow_mut().as_mut() {
                    u.toast = None;
                }
            });
            let _ = InvalidateRect(Some(hwnd), None, false);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

fn y_to_index(list: HWND, y: i32) -> i32 {
    let scroll_y = UI.with(|c| c.borrow().as_ref().map(|u| u.scroll_y).unwrap_or(0.0));
    geometry(list).index_at(y, scroll_y).unwrap_or(-1)
}

/// 如果 (x, y) 在滚动条区域，开始拖动。
unsafe fn start_thumb_drag(list: HWND, x: i32, y: i32) -> bool {
    let mut rc = RECT::default();
    let _ = GetClientRect(list, &mut rc);
    if x < rc.right - sc_dpi(12) {
        return false;
    }
    let g = geometry(list);
    let scroll = UI.with(|c| c.borrow().as_ref().map(|u| u.scroll_y).unwrap_or(0.0));
    let Some((top, h)) = scroll_thumb(g.total(), g.view_h, scroll, sc_dpi(28)) else {
        return false;
    };
    // 点在滑块外时让滑块中心跳到鼠标处
    let grab = if y >= top && y < top + h { y - top } else { h / 2 };
    UI.with(|c| {
        if let Some(u) = c.borrow_mut().as_mut() {
            u.drag_grab = Some(grab);
        }
    });
    SetCapture(list);
    drag_thumb_to(list, y, grab, h);
    true
}

unsafe fn drag_thumb_to(list: HWND, y: i32, grab: i32, thumb_h: i32) {
    let g = geometry(list);
    let pos = scroll_from_thumb(g.total(), g.view_h, thumb_h, y - grab);
    UI.with(|c| {
        if let Some(u) = c.borrow_mut().as_mut() {
            u.scroll_y = pos;
            u.target_y = pos;
        }
    });
    let _ = InvalidateRect(Some(list), None, false);
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
            let step = geometry(hwnd).item_h as f32 * 1.5;
            let target = UI.with(|c| c.borrow().as_ref().map(|u| u.target_y).unwrap_or(0.0));
            set_target(hwnd, target - (delta as f32 / 120.0) * step);
            LRESULT(0)
        }
        WM_TIMER => {
            let max_scroll = geometry(hwnd).max_scroll();
            let done = UI.with(|c| {
                c.borrow_mut().as_mut().map(|u| {
                    u.target_y = u.target_y.clamp(0.0, max_scroll);
                    let (pos, arrived) = ease_toward(u.scroll_y, u.target_y);
                    u.scroll_y = pos;
                    if arrived {
                        u.animating = false;
                    }
                    arrived
                })
            });
            if done == Some(true) {
                KillTimer(Some(hwnd), ANIM_TIMER).ok();
            }
            let _ = InvalidateRect(Some(hwnd), None, false);
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            let x = (lp.0 & 0xFFFF) as i16 as i32;
            let y = ((lp.0 >> 16) & 0xFFFF) as i16 as i32;
            let idx = y_to_index(hwnd, y);
            // 焦点交还搜索框，保证可继续打字；点击只改选中
            if let Some(edit) = UI.with(|c| c.borrow().as_ref().map(|u| u.edit)) {
                let _ = SetFocus(Some(edit));
            }
            if start_thumb_drag(hwnd, x, y) {
                return LRESULT(0);
            }
            if let Some(act) = UI.with(|c| c.borrow().as_ref().and_then(|u| act_at(&u.list_hits, x, y))) {
                run_act(act);
                return LRESULT(0);
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
        WM_LBUTTONUP => {
            let dragging = UI.with(|c| {
                c.borrow_mut().as_mut().is_some_and(|u| u.drag_grab.take().is_some())
            });
            if dragging {
                let _ = ReleaseCapture();
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
                activate_selected(key_down(VK_CONTROL.0));
            }
            LRESULT(0)
        }
        WM_RBUTTONUP => {
            let x = (lp.0 & 0xFFFF) as i16 as i32;
            let y = ((lp.0 >> 16) & 0xFFFF) as i16 as i32;
            let idx = y_to_index(hwnd, y);
            if idx >= 0 {
                UI.with(|c| {
                    if let Some(u) = c.borrow_mut().as_mut() {
                        u.sel = idx;
                    }
                });
                let _ = InvalidateRect(Some(hwnd), None, false);
                let mut pt = POINT { x, y };
                let _ = ClientToScreen(hwnd, &mut pt);
                context_menu_at(pt.x, pt.y);
            }
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let y = ((lp.0 >> 16) & 0xFFFF) as i16 as i32;
            if let Some(grab) = UI.with(|c| c.borrow().as_ref().and_then(|u| u.drag_grab)) {
                let g = geometry(hwnd);
                if let Some((_, h)) = scroll_thumb(g.total(), g.view_h, 0.0, sc_dpi(28)) {
                    drag_thumb_to(hwnd, y, grab, h);
                }
                return LRESULT(0);
            }
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
            let max_scroll = geometry(hwnd).max_scroll();
            UI.with(|c| {
                if let Some(u) = c.borrow_mut().as_mut() {
                    u.scroll_y = u.scroll_y.min(max_scroll);
                    u.target_y = u.target_y.min(max_scroll);
                }
            });
            let _ = InvalidateRect(Some(hwnd), None, false);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

/// 搜索窗的背景：输入区、标签栏底色、分隔线和放大镜。
unsafe fn paint_frame(hdc: HDC, hwnd: HWND) {
    let t = theme::current();
    let mut rc = RECT::default();
    let _ = GetClientRect(hwnd, &mut rc);
    FillRect(hdc, &rc, theme::solid_brush(t.bg));
    let (input_h, bar_h) = (sc_dpi(INPUT_H), sc_dpi(BAR_H));
    let bar = RECT { left: rc.left, top: input_h, right: rc.right, bottom: input_h + bar_h };
    FillRect(hdc, &bar, theme::solid_brush(t.alt_bg));
    let border = theme::solid_brush(t.border);
    for y in [input_h, input_h + bar_h - 1] {
        let line = RECT { left: rc.left, top: y, right: rc.right, bottom: y + 1 };
        FillRect(hdc, &line, border);
    }
    if let Some(font) = UI.with(|c| c.borrow().as_ref().map(|u| u.font_icon)) {
        let mut r = RECT { left: rc.left + sc_dpi(14), top: rc.top, right: rc.left + sc_dpi(40), bottom: input_h };
        draw_text(hdc, "\u{E721}", &mut r, t.path_fg, font, DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX);
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_APP_INDEX_READY => {
            refresh_scopes();
            LRESULT(0)
        }
        WM_APP_INDEX_PROGRESS => {
            refresh_view();
            LRESULT(0)
        }
        WM_APP_SEARCH_DONE => {
            let done = Box::from_raw(lp.0 as *mut Done);
            on_search_done(*done);
            LRESULT(0)
        }
        WM_APP_INDEX_TICK => {
            refresh_if_stale();
            LRESULT(0)
        }
        WM_APP_META_READY => {
            absorb_meta();
            LRESULT(0)
        }
        WM_APP_ICONS_READY => {
            if let Some(list) = UI.with(|c| c.borrow().as_ref().map(|u| u.list)) {
                let _ = InvalidateRect(Some(list), None, false);
            }
            LRESULT(0)
        }
        WM_DRAWITEM => {
            let dis = &*(lp.0 as *const DRAWITEMSTRUCT);
            if dis.CtlID == SCOPE_ID {
                draw_scope_button(dis);
            } else if dis.CtlID >= CAT_BASE && (dis.CtlID as usize) < CAT_BASE as usize + CATS.len() {
                draw_cat_button(dis);
            }
            LRESULT(1)
        }
        WM_CTLCOLOREDIT => {
            let t = theme::current();
            let hdc = HDC(wp.0 as *mut c_void);
            SetTextColor(hdc, COLORREF(t.fg));
            SetBkColor(hdc, COLORREF(t.bg));
            LRESULT(theme::solid_brush(t.bg).0 as isize)
        }
        WM_ERASEBKGND => {
            paint_frame(HDC(wp.0 as *mut c_void), hwnd);
            LRESULT(1)
        }
        WM_COMMAND => {
            let id = (wp.0 as u32) & 0xFFFF;
            let code = ((wp.0 as u32) >> 16) & 0xFFFF;
            if id == EDIT_ID && code == EN_CHANGE {
                do_search();
            } else if id == SCOPE_ID {
                open_scope_menu(hwnd);
            } else if id >= CAT_BASE && (id as usize) < CAT_BASE as usize + CATS.len() {
                set_category(CATS[(id - CAT_BASE) as usize].0);
            }
            LRESULT(0)
        }
        WM_SIZE => {
            layout(hwnd);
            let _ = InvalidateRect(Some(hwnd), None, true);
            LRESULT(0)
        }
        WM_DPICHANGED => {
            on_dpi_changed(hwnd, wp, lp);
            LRESULT(0)
        }
        WM_CLOSE => {
            if let Some(list) = UI.with(|c| c.borrow().as_ref().map(|u| u.list)) {
                hide_window(hwnd, list);
            } else {
                let _ = ShowWindow(hwnd, SW_HIDE);
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}
