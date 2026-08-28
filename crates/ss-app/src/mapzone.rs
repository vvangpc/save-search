//! 桌面「文件夹映射区」：把任意文件夹映射为贴在桌面层的面板（图标 + 文件名 + 修改时间）。
//!
//! 窗口钉在桌面层（`WM_WINDOWPOSCHANGING` 强制 `HWND_BOTTOM`：壁纸之上、普通窗口之下）；
//! 折叠态只剩标题条，悬停自动展开、鼠标离开延迟收起，可锁定常开；面板内搜索框过滤；
//! 展开期间每 2 秒轮询 `read_dir` 刷新内容。支持多实例，配置持久化在 settings.json。

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;
use std::time::UNIX_EPOCH;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{
    COLORREF, FILETIME, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, SYSTEMTIME, WPARAM,
};
use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, DeleteObject, DrawTextW, EndPaint, FillRect, FrameRect, GetMonitorInfoW,
    InvalidateRect, MonitorFromRect, MonitorFromWindow, ScreenToClient, SelectObject, SetBkMode,
    SetTextColor,
    DRAW_TEXT_FORMAT, DT_CENTER, DT_END_ELLIPSIS, DT_LEFT, DT_NOPREFIX, DT_RIGHT, DT_SINGLELINE,
    DT_VCENTER, DT_WORDBREAK, HDC, HFONT, HGDIOBJ, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    MONITOR_DEFAULTTONULL, PAINTSTRUCT, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};
use windows::Win32::UI::Controls::{SetScrollInfo, WM_MOUSELEAVE};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    TrackMouseEvent, TME_HOVER, TME_LEAVE, TME_NONCLIENT, TRACKMOUSEEVENT,
};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::{icons, paint, theme};

/// Win+D 等把面板最小化后的自愈消息（WM_APP 区间）。
pub const WM_APP_ZONE_RESTORE: u32 = WM_APP + 10;

// 定时器：主窗上的收起判定轮询 / 内容刷新；列表子窗上的平滑滚动动画
const TIMER_WATCH: usize = 1;
const TIMER_REFRESH: usize = 2;
const TIMER_ANIM: usize = 3;

const EDIT_ID: u32 = 401;
const EN_CHANGE: u32 = 0x0300;
const EM_SETCUEBANNER: u32 = 0x1501;

// 标题栏右键菜单 / 条目右键菜单 命令 id（TPM_RETURNCMD 本地消化，不走 WM_COMMAND）
const IDM_ZONE_LOCK: u32 = 1;
const IDM_ZONE_CHANGE: u32 = 2;
const IDM_ZONE_OPEN: u32 = 3;
const IDM_ZONE_REMOVE: u32 = 4;
const IDM_ITEM_REVEAL: u32 = 5;

// WM_MOUSEHOVER / WM_NCMOUSEHOVER（windows-rs 里散落在 Controls，直接用数值省 feature）
const MSG_MOUSEHOVER: u32 = 0x02A1;
const MSG_NCMOUSEHOVER: u32 = 0x02A0;

/// 单个文件夹最多列出的条目数（防误映射巨型文件夹拖慢 UI）。
const MAX_ITEMS: usize = 1000;
/// 收起判定：连续多少个 WATCH tick（250ms/tick）指针在窗外才收起。
const LEAVE_TICKS: u32 = 3;

mod st {
    pub const WS_POPUP: u32 = 0x8000_0000;
    pub const WS_CHILD: u32 = 0x4000_0000;
    pub const WS_VSCROLL: u32 = 0x0020_0000;
    pub const WS_CLIPCHILDREN: u32 = 0x0200_0000;
    pub const ES_AUTOHSCROLL: u32 = 0x0080;
    pub const WS_EX_TOOLWINDOW: u32 = 0x0000_0080;
}

struct Item {
    name: String,
    is_dir: bool,
    /// FILETIME（100ns 自 1601），排序用
    mtime: u64,
    /// 预格式化的本地时间文本（绘制热路径零转换）
    mtime_str: String,
}

struct Zone {
    hwnd: HWND,
    list: HWND,
    edit: HWND,
    /// 持久化配置的内存真相；变更后调 `save_zones()` 落盘
    cfg: ss_config::MapZone,
    /// 创建顺序（保存时按此排序，保证 settings.json 顺序稳定）
    seq: u32,
    items: Vec<Item>,
    /// 过滤后可见条目在 `items` 中的索引（搜索框空 = 全部）
    view: Vec<u32>,
    query: String,
    /// read_dir 失败时的提示文案（文件夹被删/U 盘拔出）
    error: Option<String>,
    expanded: bool,
    dragging: bool,
    menu_open: bool,
    /// 连续「指针在本窗之外」的 WATCH tick 计数
    leave_ticks: u32,
    // 列表平滑滚动状态（每实例独立）
    scroll_y: f32,
    target_y: f32,
    sel: i32,
    hover: i32,
    animating: bool,
    font_title: HFONT,
    font_name: HFONT,
    font_time: HFONT,
    font_glyph: HFONT,
    item_h: i32,
    title_h: i32,
    search_h: i32,
    dpi: i32,
}

thread_local! {
    static ZONES: RefCell<HashMap<isize, Zone>> = RefCell::new(HashMap::new());
    static SEQ: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

fn key(hwnd: HWND) -> isize {
    hwnd.0 as isize
}

fn send(hwnd: HWND, msg: u32, w: usize, l: isize) -> isize {
    unsafe { SendMessageW(hwnd, msg, Some(WPARAM(w)), Some(LPARAM(l))).0 }
}

fn set_font(hwnd: HWND, f: HFONT) {
    send(hwnd, WM_SETFONT, f.0 as usize, 1);
}

unsafe fn make_font(px: i32, weight: i32, face: PCWSTR) -> HFONT {
    use windows::Win32::Graphics::Gdi::{
        CreateFontW, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, OUT_DEFAULT_PRECIS,
    };
    CreateFontW(
        -px, 0, 0, 0, weight, 0, 0, 0, DEFAULT_CHARSET, OUT_DEFAULT_PRECIS, CLIP_DEFAULT_PRECIS,
        CLEARTYPE_QUALITY, 0, face,
    )
}

/// 读 Zone 的只读快照片段。借用期内不得调 Win32（防 wndproc 重入双借用）。
fn read_zone<R>(hwnd: HWND, f: impl FnOnce(&Zone) -> R) -> Option<R> {
    ZONES.with(|c| c.borrow().get(&key(hwnd)).map(f))
}

fn with_zone<R>(hwnd: HWND, f: impl FnOnce(&mut Zone) -> R) -> Option<R> {
    ZONES.with(|c| c.borrow_mut().get_mut(&key(hwnd)).map(f))
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

fn leaf_of(path: &str) -> &str {
    let p = path.trim_end_matches('\\');
    match p.rfind('\\') {
        Some(i) => &p[i + 1..],
        None => p,
    }
}

fn join_path(dir: &str, name: &str) -> String {
    format!("{}\\{}", dir.trim_end_matches('\\'), name)
}

// ---- 时间格式化（不引 chrono：FILETIME → 本地 SYSTEMTIME 手工拼） ----

fn to_local_st(ft_100ns: u64) -> Option<SYSTEMTIME> {
    let ft = FILETIME {
        dwLowDateTime: (ft_100ns & 0xFFFF_FFFF) as u32,
        dwHighDateTime: (ft_100ns >> 32) as u32,
    };
    let mut st = SYSTEMTIME::default();
    unsafe { FileTimeToSystemTime(&ft, &mut st).ok()? };
    let mut lt = SYSTEMTIME::default();
    unsafe { SystemTimeToTzSpecificLocalTime(None, &st, &mut lt).ok()? };
    Some(lt)
}

/// UNIX SystemTime → FILETIME 数值（100ns 自 1601-01-01）。1970 与 1601 差 11644473600 秒。
fn systemtime_to_ft(t: std::time::SystemTime) -> u64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => (d.as_nanos() / 100) as u64 + 116_444_736_000_000_000,
        Err(_) => 0, // 1970 前的时间戳：显示为空即可
    }
}

/// 同年 `MM/dd HH:mm`，跨年 `yyyy/MM/dd`（列表窄处友好）。
fn format_mtime(ft: u64, now_year: u16) -> String {
    match to_local_st(ft) {
        Some(st) if st.wYear == now_year => format!(
            "{:02}/{:02} {:02}:{:02}",
            st.wMonth, st.wDay, st.wHour, st.wMinute
        ),
        Some(st) => format!("{}/{:02}/{:02}", st.wYear, st.wMonth, st.wDay),
        None => String::new(),
    }
}

// ---- 数据 ----

/// 读文件夹内容：名称 / 是否目录 / 修改时间，按 mtime 降序，截断 MAX_ITEMS。
fn read_folder(path: &str) -> Result<Vec<Item>, String> {
    let rd = std::fs::read_dir(path).map_err(|_| "文件夹不可用".to_string())?;
    let now_year = to_local_st(systemtime_to_ft(std::time::SystemTime::now()))
        .map(|st| st.wYear)
        .unwrap_or(0);
    let mut items: Vec<Item> = Vec::new();
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let mtime = e
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .map(systemtime_to_ft)
            .unwrap_or(0);
        items.push(Item {
            name,
            is_dir,
            mtime,
            mtime_str: String::new(), // 排序截断后再格式化，省掉被丢弃条目的转换
        });
    }
    items.sort_by(|a, b| b.mtime.cmp(&a.mtime));
    items.truncate(MAX_ITEMS);
    for it in &mut items {
        it.mtime_str = format_mtime(it.mtime, now_year);
    }
    Ok(items)
}

/// 按搜索框文本重建 view（大小写不敏感 contains）。
fn apply_filter(z: &mut Zone) {
    let q = z.query.to_lowercase();
    z.view = z
        .items
        .iter()
        .enumerate()
        .filter(|(_, it)| q.is_empty() || it.name.to_lowercase().contains(&q))
        .map(|(i, _)| i as u32)
        .collect();
    z.sel = -1;
    z.hover = -1;
}

/// 重读文件夹并与当前快照比较；有变化则重灌列表并返回 true。
fn refresh_items(hwnd: HWND) -> bool {
    let path = match read_zone(hwnd, |z| z.cfg.path.clone()) {
        Some(p) => p,
        None => return false,
    };
    let fresh = read_folder(&path);
    let changed = with_zone(hwnd, |z| match fresh {
        Ok(items) => {
            let same = z.error.is_none()
                && z.items.len() == items.len()
                && z.items.iter().zip(items.iter()).all(|(a, b)| {
                    a.name == b.name && a.mtime == b.mtime && a.is_dir == b.is_dir
                });
            if same {
                return false;
            }
            z.items = items;
            z.error = None;
            apply_filter(z);
            let max = ((z.view.len() as i32 * z.item_h) as f32).max(0.0);
            z.scroll_y = z.scroll_y.min(max);
            z.target_y = z.target_y.min(max);
            true
        }
        Err(e) => {
            let same = z.error.as_deref() == Some(e.as_str());
            z.items.clear();
            z.view.clear();
            z.error = Some(e);
            !same
        }
    })
    .unwrap_or(false);
    changed
}

// ---- 持久化 ----

/// 收集全部映射区当前配置 → settings.map_zones 落盘（按创建顺序稳定排序）。
fn save_zones() {
    let mut v: Vec<(u32, ss_config::MapZone)> =
        ZONES.with(|c| c.borrow().values().map(|z| (z.seq, z.cfg.clone())).collect());
    v.sort_by_key(|(seq, _)| *seq);
    let mut s = ss_config::load_settings();
    s.map_zones = v.into_iter().map(|(_, cfg)| cfg).collect();
    ss_config::save_settings(&s);
}

// ---- 初始化 / 创建 ----

/// 注册窗口类并按配置创建所有映射区。启动时调用一次。
pub fn init_all() -> windows::core::Result<()> {
    unsafe {
        let hinstance = HINSTANCE(GetModuleHandleW(None)?.0);
        let wc = WNDCLASSW {
            lpfnWndProc: Some(zone_proc),
            hInstance: hinstance,
            lpszClassName: w!("SaveSearchMapZone"),
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            ..Default::default()
        };
        RegisterClassW(&wc);
        let wcl = WNDCLASSW {
            style: CS_DBLCLKS,
            lpfnWndProc: Some(zlist_proc),
            hInstance: hinstance,
            lpszClassName: w!("SaveSearchZoneList"),
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            ..Default::default()
        };
        RegisterClassW(&wcl);
    }
    for cfg in ss_config::load_settings().map_zones {
        let _ = create_zone(cfg);
    }
    Ok(())
}

/// 托盘菜单「新建映射区」：选文件夹 → 建面板 → 落盘。
pub fn new_zone_interactive(owner: HWND) {
    let picked = unsafe {
        let _ = SetForegroundWindow(owner); // 保证 IFileOpenDialog 正常前置
        crate::settings_win::pick_folder(owner)
    };
    let Some(path) = picked else { return };
    let dpi = unsafe { GetDpiForWindow(owner).max(96) as i32 };
    let sc = |v: i32| v * dpi / 96;
    let n = ZONES.with(|c| c.borrow().len() as i32);
    let cfg = ss_config::MapZone {
        path,
        x: sc(120) + n * sc(48),
        y: sc(120) + n * sc(48),
        w: sc(320),
        h: sc(440),
        locked: false,
    };
    let _ = create_zone(cfg);
    save_zones();
}

/// 主题变更后刷新全部面板外观。
pub fn refresh_theme() {
    let handles: Vec<(HWND, HWND)> =
        ZONES.with(|c| c.borrow().values().map(|z| (z.hwnd, z.list)).collect());
    let t = theme::current();
    for (hwnd, list) in handles {
        theme::apply_window(hwnd, &t);
        unsafe {
            let _ = InvalidateRect(Some(hwnd), None, true);
            let _ = InvalidateRect(Some(list), None, false);
        }
    }
}

/// 滚轮按鼠标位置转发给映射区列表（不依赖焦点）。主消息循环调用。
pub fn pretranslate(msg: &MSG) -> bool {
    if msg.message != WM_MOUSEWHEEL {
        return false;
    }
    let under = unsafe { WindowFromPoint(msg.pt) };
    let hit = ZONES.with(|c| c.borrow().values().any(|z| z.list == under));
    if hit {
        unsafe {
            let _ = SendMessageW(under, WM_MOUSEWHEEL, Some(msg.wParam), Some(msg.lParam));
        }
        return true;
    }
    false
}

fn create_zone(mut cfg: ss_config::MapZone) -> windows::core::Result<HWND> {
    unsafe {
        // 位置钳制：保存的位置已不在任何显示器上（拔了显示器）→ 挪回主屏附近
        let r = RECT {
            left: cfg.x,
            top: cfg.y,
            right: cfg.x + cfg.w.max(50),
            bottom: cfg.y + cfg.h.max(50),
        };
        if MonitorFromRect(&r, MONITOR_DEFAULTTONULL).0.is_null() {
            cfg.x = 100;
            cfg.y = 100;
        }

        let hinstance = HINSTANCE(GetModuleHandleW(None)?.0);
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(st::WS_EX_TOOLWINDOW),
            w!("SaveSearchMapZone"),
            w!(""),
            WINDOW_STYLE(st::WS_POPUP | st::WS_CLIPCHILDREN),
            cfg.x,
            cfg.y,
            cfg.w.max(50),
            cfg.h.max(50),
            None,
            None,
            Some(hinstance),
            None,
        )?;

        let dpi = GetDpiForWindow(hwnd).max(96) as i32;
        let sc = |v: i32| v * dpi / 96;
        // 老配置/跨屏 DPI 变化后的宽高兜底
        cfg.w = cfg.w.clamp(sc(200), sc(800));
        cfg.h = cfg.h.clamp(sc(160), sc(1200));

        let edit = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("EDIT"),
            PCWSTR::null(),
            WINDOW_STYLE(st::WS_CHILD | st::ES_AUTOHSCROLL),
            0,
            0,
            10,
            10,
            Some(hwnd),
            Some(HMENU(EDIT_ID as usize as *mut c_void)),
            Some(hinstance),
            None,
        )?;
        let list = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("SaveSearchZoneList"),
            PCWSTR::null(),
            WINDOW_STYLE(st::WS_CHILD | st::WS_VSCROLL),
            0,
            0,
            10,
            10,
            Some(hwnd),
            None,
            Some(hinstance),
            None,
        )?;

        let font_title = make_font(sc(14), 600, w!("Segoe UI"));
        let font_name = make_font(sc(13), 400, w!("Segoe UI"));
        let font_time = make_font(sc(11), 400, w!("Segoe UI"));
        let font_glyph = make_font(sc(12), 400, w!("Segoe MDL2 Assets"));
        set_font(edit, font_name);
        let cue = crate::wide("搜索文件…");
        send(edit, EM_SETCUEBANNER, 1, cue.as_ptr() as isize);

        let locked = cfg.locked;
        let seq = SEQ.with(|c| {
            let v = c.get();
            c.set(v + 1);
            v
        });
        ZONES.with(|c| {
            c.borrow_mut().insert(
                key(hwnd),
                Zone {
                    hwnd,
                    list,
                    edit,
                    cfg,
                    seq,
                    items: Vec::new(),
                    view: Vec::new(),
                    query: String::new(),
                    error: None,
                    expanded: false,
                    dragging: false,
                    menu_open: false,
                    leave_ticks: 0,
                    scroll_y: 0.0,
                    target_y: 0.0,
                    sel: -1,
                    hover: -1,
                    animating: false,
                    font_title,
                    font_name,
                    font_time,
                    font_glyph,
                    item_h: sc(28),
                    title_h: sc(32),
                    search_h: sc(30),
                    dpi,
                },
            );
        });

        theme::apply_window(hwnd, &theme::current());
        // Win11 圆角（Win10 静默忽略 → 方角）
        let corner: i32 = 2; // DWMWCP_ROUND
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &corner as *const _ as *const c_void,
            4,
        );

        // 锁定常开的面板启动即展开，其余折叠成标题条
        set_expanded(hwnd, locked);
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        Ok(hwnd)
    }
}

// ---- 布局 / 展开折叠 ----

/// 展开态下排布搜索框与列表（客户区内边距 1px 留给描边）。
fn layout_children(hwnd: HWND) {
    let m = read_zone(hwnd, |z| {
        (z.edit, z.list, z.title_h, z.search_h, z.dpi, z.expanded)
    });
    let Some((edit, list, title_h, search_h, dpi, expanded)) = m else {
        return;
    };
    if !expanded {
        unsafe {
            let _ = ShowWindow(edit, SW_HIDE);
            let _ = ShowWindow(list, SW_HIDE);
        }
        return;
    }
    let sc = |v: i32| v * dpi / 96;
    let mut rc = RECT::default();
    unsafe {
        let _ = GetClientRect(hwnd, &mut rc);
        let w = rc.right - rc.left;
        let h = rc.bottom - rc.top;
        let pad = sc(8);
        let eh = search_h - sc(8);
        let _ = MoveWindow(edit, pad, title_h + sc(2), (w - 2 * pad).max(10), eh, true);
        let ltop = title_h + search_h;
        let _ = MoveWindow(list, 1, ltop, (w - 2).max(10), (h - ltop - 1).max(10), true);
        let _ = ShowWindow(edit, SW_SHOWNA);
        let _ = ShowWindow(list, SW_SHOWNA);
    }
}

/// 展开/收起：改窗口高度（同一窗口），启停定时器，展开瞬间刷新内容。
fn set_expanded(hwnd: HWND, expand: bool) {
    let m = read_zone(hwnd, |z| (z.title_h, z.cfg.h, z.expanded));
    let Some((title_h, cfg_h, _was)) = m else { return };

    let mut wr = RECT::default();
    unsafe {
        let _ = GetWindowRect(hwnd, &mut wr);
    }
    let w = wr.right - wr.left;
    let new_h = if expand {
        // 向下展开，钳到所在显示器工作区（放不下则减小高度）
        let mut h = cfg_h;
        unsafe {
            let hmon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
            let mut mi = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            if GetMonitorInfoW(hmon, &mut mi).as_bool() {
                let avail = mi.rcWork.bottom - wr.top;
                h = h.min(avail).max(title_h + 60);
            }
        }
        h
    } else {
        title_h
    };

    with_zone(hwnd, |z| {
        z.expanded = expand;
        z.leave_ticks = 0;
        if !expand {
            z.animating = false;
        }
    });

    unsafe {
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_BOTTOM),
            wr.left,
            wr.top,
            w,
            new_h,
            SWP_NOACTIVATE,
        );
    }
    layout_children(hwnd);
    unsafe {
        if expand {
            let changed = refresh_items(hwnd);
            let list = read_zone(hwnd, |z| z.list);
            if let Some(list) = list {
                update_scrollbar(list);
                if changed {
                    let _ = InvalidateRect(Some(list), None, false);
                }
            }
            SetTimer(Some(hwnd), TIMER_WATCH, 250, None);
            SetTimer(Some(hwnd), TIMER_REFRESH, 2000, None);
        } else {
            let list = read_zone(hwnd, |z| z.list);
            if let Some(list) = list {
                KillTimer(Some(list), TIMER_ANIM).ok();
            }
            KillTimer(Some(hwnd), TIMER_WATCH).ok();
            KillTimer(Some(hwnd), TIMER_REFRESH).ok();
        }
        let _ = InvalidateRect(Some(hwnd), None, true);
    }
}

/// 标题栏右侧「锁定」按钮热区（客户区坐标）。
fn pin_rect(hwnd: HWND) -> RECT {
    let mut rc = RECT::default();
    let _ = unsafe { GetClientRect(hwnd, &mut rc) };
    let w = rc.right;
    let (title_h, dpi) = read_zone(hwnd, |z| (z.title_h, z.dpi)).unwrap_or((32, 96));
    let sc = |v: i32| v * dpi / 96;
    RECT {
        left: w - sc(30),
        top: (title_h - sc(22)) / 2,
        right: w - sc(6),
        bottom: (title_h + sc(22)) / 2,
    }
}

fn pt_in(rc: &RECT, x: i32, y: i32) -> bool {
    x >= rc.left && x < rc.right && y >= rc.top && y < rc.bottom
}

// ---- 打开条目 ----

unsafe fn open_item(path: &str, is_dir: bool) {
    // 映射区语义是「快捷访问」：文件直接用默认程序打开，目录开资源管理器
    let p = crate::wide(path);
    let _ = is_dir; // 两种情况都是 open 动词，目录自动落到资源管理器
    ShellExecuteW(
        None,
        w!("open"),
        PCWSTR(p.as_ptr()),
        PCWSTR::null(),
        PCWSTR::null(),
        SW_SHOWNORMAL,
    );
}

unsafe fn reveal_item(path: &str) {
    let params = crate::wide(&format!("/select,\"{}\"", path));
    ShellExecuteW(
        None,
        w!("open"),
        w!("explorer.exe"),
        PCWSTR(params.as_ptr()),
        PCWSTR::null(),
        SW_SHOWNORMAL,
    );
}

// ---- 菜单 ----

unsafe fn show_title_menu(hwnd: HWND, x: i32, y: i32) {
    let m = read_zone(hwnd, |z| (z.cfg.locked, z.cfg.path.clone()));
    let Some((locked, path)) = m else { return };
    let Ok(menu) = CreatePopupMenu() else { return };
    let t_open = crate::wide("打开此文件夹");
    let t_change = crate::wide("更换文件夹…");
    let t_lock = crate::wide("锁定常开");
    let t_remove = crate::wide("移除映射区");
    let _ = AppendMenuW(menu, MF_STRING, IDM_ZONE_OPEN as usize, PCWSTR(t_open.as_ptr()));
    let _ = AppendMenuW(menu, MF_STRING, IDM_ZONE_CHANGE as usize, PCWSTR(t_change.as_ptr()));
    let lock_flags = if locked { MF_STRING | MF_CHECKED } else { MF_STRING };
    let _ = AppendMenuW(menu, lock_flags, IDM_ZONE_LOCK as usize, PCWSTR(t_lock.as_ptr()));
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
    let _ = AppendMenuW(menu, MF_STRING, IDM_ZONE_REMOVE as usize, PCWSTR(t_remove.as_ptr()));

    with_zone(hwnd, |z| z.menu_open = true);
    let _ = SetForegroundWindow(hwnd);
    let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON, x, y, Some(0), hwnd, None);
    // KB135788：无标题弹窗上的菜单需事后 Post 一条消息，否则下次点击外部不收起
    let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
    let _ = DestroyMenu(menu);
    with_zone(hwnd, |z| z.menu_open = false);

    match cmd.0 as u32 {
        IDM_ZONE_OPEN => open_item(&path, true),
        IDM_ZONE_CHANGE => {
            if let Some(p) = crate::settings_win::pick_folder(hwnd) {
                with_zone(hwnd, |z| {
                    z.cfg.path = p;
                    z.items.clear();
                    z.view.clear();
                    z.error = None;
                    z.scroll_y = 0.0;
                    z.target_y = 0.0;
                });
                save_zones();
                let _ = refresh_items(hwnd);
                let list = read_zone(hwnd, |z| z.list);
                let _ = InvalidateRect(Some(hwnd), None, true);
                if let Some(list) = list {
                    update_scrollbar(list);
                    let _ = InvalidateRect(Some(list), None, false);
                }
            }
        }
        IDM_ZONE_LOCK => {
            let now_locked = with_zone(hwnd, |z| {
                z.cfg.locked = !z.cfg.locked;
                z.cfg.locked
            })
            .unwrap_or(false);
            save_zones();
            if now_locked {
                let expanded = read_zone(hwnd, |z| z.expanded).unwrap_or(false);
                if !expanded {
                    set_expanded(hwnd, true);
                }
            }
            let _ = InvalidateRect(Some(hwnd), None, true);
        }
        IDM_ZONE_REMOVE => {
            let _ = DestroyWindow(hwnd);
        }
        _ => {}
    }
}

unsafe fn show_item_menu(zone: HWND, list: HWND, x: i32, y: i32) {
    let sel = read_zone(zone, |z| z.sel).unwrap_or(-1);
    if sel < 0 {
        return;
    }
    let path = read_zone(zone, |z| {
        z.view
            .get(sel as usize)
            .and_then(|&i| z.items.get(i as usize))
            .map(|it| join_path(&z.cfg.path, &it.name))
    })
    .flatten();
    let Some(path) = path else { return };
    let Ok(menu) = CreatePopupMenu() else { return };
    let t_reveal = crate::wide("在资源管理器中显示");
    let _ = AppendMenuW(menu, MF_STRING, IDM_ITEM_REVEAL as usize, PCWSTR(t_reveal.as_ptr()));
    with_zone(zone, |z| z.menu_open = true);
    let _ = SetForegroundWindow(zone);
    let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON, x, y, Some(0), list, None);
    let _ = PostMessageW(Some(zone), WM_NULL, WPARAM(0), LPARAM(0));
    let _ = DestroyMenu(menu);
    with_zone(zone, |z| z.menu_open = false);
    if cmd.0 as u32 == IDM_ITEM_REVEAL {
        reveal_item(&path);
    }
}

// ---- 主窗绘制 ----

unsafe fn draw_text(
    hdc: HDC,
    s: &str,
    rect: &mut RECT,
    color: u32,
    font: HFONT,
    flags: DRAW_TEXT_FORMAT,
) {
    if s.is_empty() {
        return;
    }
    SelectObject(hdc, HGDIOBJ(font.0));
    SetTextColor(hdc, COLORREF(color));
    let mut buf: Vec<u16> = s.encode_utf16().collect();
    DrawTextW(hdc, &mut buf, rect, flags);
}

unsafe fn paint_zone(hwnd: HWND) {
    let mut ps = PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut ps);
    let mut rc = RECT::default();
    let _ = GetClientRect(hwnd, &mut rc);
    let t = theme::current();
    let m = read_zone(hwnd, |z| {
        (
            z.cfg.path.clone(),
            z.cfg.locked,
            z.expanded,
            z.title_h,
            z.dpi,
            z.font_title,
            z.font_glyph,
            z.items.len() >= MAX_ITEMS,
        )
    });
    paint::buffered(hdc, rc, |mem, lrc| {
        FillRect(mem, &lrc, theme::solid_brush(t.bg));
        let Some((path, locked, expanded, title_h, dpi, f_title, f_glyph, truncated)) = m else {
            FrameRect(mem, &lrc, theme::solid_brush(t.border));
            return;
        };
        let sc = |v: i32| v * dpi / 96;
        // 标题带底色（与列表区分）
        let band = RECT {
            left: lrc.left,
            top: lrc.top,
            right: lrc.right,
            bottom: title_h.min(lrc.bottom),
        };
        FillRect(mem, &band, theme::solid_brush(t.alt_bg));
        SetBkMode(mem, TRANSPARENT);
        // 左侧折叠指示箭头（MDL2 ChevronDown/Up）
        let chev = if expanded { "\u{E70E}" } else { "\u{E70D}" };
        let mut rchev = RECT {
            left: sc(8),
            top: band.top,
            right: sc(26),
            bottom: band.bottom,
        };
        draw_text(mem, chev, &mut rchev, t.path_fg, f_glyph, DT_CENTER | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX);
        // 标题：文件夹名（截断上限时加提示）
        let mut title = leaf_of(&path).to_string();
        if title.is_empty() {
            title = path.clone();
        }
        if truncated {
            title.push_str("（前 1000 项）");
        }
        let mut rt = RECT {
            left: sc(28),
            top: band.top,
            right: band.right - sc(32),
            bottom: band.bottom,
        };
        draw_text(mem, &title, &mut rt, t.fg, f_title, DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS);
        // 右侧锁定按钮（MDL2 Pin/Pinned）
        let pin = if locked { "\u{E840}" } else { "\u{E718}" };
        let pin_color = if locked { t.accent } else { t.path_fg };
        let mut rp = RECT {
            left: band.right - sc(30),
            top: band.top,
            right: band.right - sc(6),
            bottom: band.bottom,
        };
        draw_text(mem, pin, &mut rp, pin_color, f_glyph, DT_CENTER | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX);
        // 标题带下缘分隔线（展开时）
        if expanded && lrc.bottom > title_h {
            let sep = RECT {
                left: lrc.left,
                top: title_h - 1,
                right: lrc.right,
                bottom: title_h,
            };
            FillRect(mem, &sep, theme::solid_brush(t.border));
        }
        // 1px 描边
        FrameRect(mem, &lrc, theme::solid_brush(t.border));
    });
    let _ = EndPaint(hwnd, &ps);
}

// ---- 主窗 wndproc ----

unsafe extern "system" fn zone_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_WINDOWPOSCHANGING => {
            // 钉在桌面层：任何 z 序提升企图都压回本带最底（壁纸之上、普通窗口之下）
            let pos = &mut *(lp.0 as *mut WINDOWPOS);
            pos.hwndInsertAfter = HWND_BOTTOM;
            pos.flags &= !SWP_NOZORDER;
            LRESULT(0)
        }
        WM_SYSCOMMAND => {
            // Win+D / 最小化企图：桌面部件语义上就该留在桌面，吞掉
            if (wp.0 as u32) & 0xFFF0 == SC_MINIMIZE {
                return LRESULT(0);
            }
            DefWindowProcW(hwnd, msg, wp, lp)
        }
        WM_SIZE => {
            if wp.0 as u32 == SIZE_MINIMIZED {
                // 兜底自愈：真被最小化了就异步恢复（同步 Restore 会打断发起方的内部流程）
                let _ = PostMessageW(Some(hwnd), WM_APP_ZONE_RESTORE, WPARAM(0), LPARAM(0));
            }
            LRESULT(0)
        }
        WM_APP_ZONE_RESTORE => {
            let _ = ShowWindow(hwnd, SW_RESTORE);
            let _ = SetWindowPos(
                hwnd,
                Some(HWND_BOTTOM),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
            LRESULT(0)
        }
        WM_NCHITTEST => {
            // 标题带 → HTCAPTION（系统拖动），锁定按钮热区除外
            let x = (lp.0 & 0xFFFF) as i16 as i32;
            let y = ((lp.0 >> 16) & 0xFFFF) as i16 as i32;
            let mut wr = RECT::default();
            let _ = GetWindowRect(hwnd, &mut wr);
            let (cx, cy) = (x - wr.left, y - wr.top);
            let title_h = read_zone(hwnd, |z| z.title_h).unwrap_or(32);
            if cy < title_h {
                let pr = pin_rect(hwnd);
                if pt_in(&pr, cx, cy) {
                    return LRESULT(HTCLIENT as isize);
                }
                return LRESULT(HTCAPTION as isize);
            }
            LRESULT(HTCLIENT as isize)
        }
        WM_NCMOUSEMOVE => {
            // 折叠态：悬停标题带 200ms 后自动展开（防鼠标飞过就弹开）
            let expanded = read_zone(hwnd, |z| z.expanded).unwrap_or(true);
            if !expanded {
                let mut tme = TRACKMOUSEEVENT {
                    cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_HOVER | TME_NONCLIENT,
                    hwndTrack: hwnd,
                    dwHoverTime: 200,
                };
                let _ = TrackMouseEvent(&mut tme);
            }
            DefWindowProcW(hwnd, msg, wp, lp)
        }
        WM_MOUSEMOVE => {
            let expanded = read_zone(hwnd, |z| z.expanded).unwrap_or(true);
            if !expanded {
                let mut tme = TRACKMOUSEEVENT {
                    cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_HOVER,
                    hwndTrack: hwnd,
                    dwHoverTime: 200,
                };
                let _ = TrackMouseEvent(&mut tme);
            }
            LRESULT(0)
        }
        MSG_NCMOUSEHOVER | MSG_MOUSEHOVER => {
            let expanded = read_zone(hwnd, |z| z.expanded).unwrap_or(true);
            if !expanded {
                set_expanded(hwnd, true);
            }
            LRESULT(0)
        }
        WM_NCLBUTTONDBLCLK => {
            // 双击标题：手动切换展开/收起
            let expanded = read_zone(hwnd, |z| z.expanded).unwrap_or(false);
            set_expanded(hwnd, !expanded);
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            let x = (lp.0 & 0xFFFF) as i16 as i32;
            let y = ((lp.0 >> 16) & 0xFFFF) as i16 as i32;
            let pr = pin_rect(hwnd);
            if pt_in(&pr, x, y) {
                let now_locked = with_zone(hwnd, |z| {
                    z.cfg.locked = !z.cfg.locked;
                    z.cfg.locked
                })
                .unwrap_or(false);
                save_zones();
                let expanded = read_zone(hwnd, |z| z.expanded).unwrap_or(false);
                if now_locked && !expanded {
                    set_expanded(hwnd, true);
                }
                let _ = InvalidateRect(Some(hwnd), None, true);
            }
            LRESULT(0)
        }
        WM_ENTERSIZEMOVE => {
            with_zone(hwnd, |z| z.dragging = true);
            LRESULT(0)
        }
        WM_EXITSIZEMOVE => {
            let mut wr = RECT::default();
            let _ = GetWindowRect(hwnd, &mut wr);
            with_zone(hwnd, |z| {
                z.dragging = false;
                z.cfg.x = wr.left;
                z.cfg.y = wr.top;
                z.cfg.w = wr.right - wr.left;
                if z.expanded {
                    z.cfg.h = wr.bottom - wr.top;
                }
            });
            save_zones();
            LRESULT(0)
        }
        WM_TIMER => {
            match wp.0 {
                TIMER_WATCH => {
                    // 收起判定：指针连续在窗外（含子控件树之外）≥ LEAVE_TICKS 才收起
                    let mut pt = POINT::default();
                    let _ = GetCursorPos(&mut pt);
                    let under = WindowFromPoint(pt);
                    let root = GetAncestor(under, GA_ROOT);
                    let inside = root == hwnd;
                    let should_collapse = with_zone(hwnd, |z| {
                        if inside || z.cfg.locked || z.menu_open || z.dragging {
                            z.leave_ticks = 0;
                            false
                        } else {
                            z.leave_ticks += 1;
                            z.leave_ticks >= LEAVE_TICKS
                        }
                    })
                    .unwrap_or(false);
                    if should_collapse {
                        set_expanded(hwnd, false);
                    }
                }
                TIMER_REFRESH => {
                    if refresh_items(hwnd) {
                        let list = read_zone(hwnd, |z| z.list);
                        if let Some(list) = list {
                            update_scrollbar(list);
                            let _ = InvalidateRect(Some(list), None, false);
                        }
                        let _ = InvalidateRect(Some(hwnd), None, false); // 标题「前 1000 项」可能变
                    }
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_COMMAND => {
            let id = (wp.0 as u32) & 0xFFFF;
            let code = ((wp.0 as u32) >> 16) & 0xFFFF;
            if id == EDIT_ID && code == EN_CHANGE {
                let edit = read_zone(hwnd, |z| z.edit);
                let q = edit.map(|e| get_text(e)).unwrap_or_default();
                let list = with_zone(hwnd, |z| {
                    z.query = q;
                    apply_filter(z);
                    z.scroll_y = 0.0;
                    z.target_y = 0.0;
                    z.list
                });
                if let Some(list) = list {
                    update_scrollbar(list);
                    let _ = InvalidateRect(Some(list), None, false);
                }
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
            show_title_menu(hwnd, mx, my);
            LRESULT(0)
        }
        WM_CTLCOLOREDIT => {
            let t = theme::current();
            let hdc = HDC(wp.0 as *mut c_void);
            SetTextColor(hdc, COLORREF(t.fg));
            SetBkMode(hdc, TRANSPARENT);
            LRESULT(theme::solid_brush(t.alt_bg).0 as isize)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => {
            paint_zone(hwnd);
            LRESULT(0)
        }
        WM_DPICHANGED => {
            let new_dpi = ((wp.0 as u32) & 0xFFFF) as i32;
            let sug = &*(lp.0 as *const RECT);
            let (sx, sy, sw, sh) = (sug.left, sug.top, sug.right - sug.left, sug.bottom - sug.top);
            let fonts = with_zone(hwnd, |z| {
                let old = (z.font_title, z.font_name, z.font_time, z.font_glyph);
                let ratio = |v: i32| (v as i64 * new_dpi as i64 / z.dpi.max(1) as i64) as i32;
                z.cfg.w = ratio(z.cfg.w);
                z.cfg.h = ratio(z.cfg.h);
                z.dpi = new_dpi;
                let sc = |v: i32| v * new_dpi / 96;
                z.item_h = sc(28);
                z.title_h = sc(32);
                z.search_h = sc(30);
                z.font_title = make_font(sc(14), 600, w!("Segoe UI"));
                z.font_name = make_font(sc(13), 400, w!("Segoe UI"));
                z.font_time = make_font(sc(11), 400, w!("Segoe UI"));
                z.font_glyph = make_font(sc(12), 400, w!("Segoe MDL2 Assets"));
                (old, z.edit, z.font_name, z.expanded, z.title_h)
            });
            if let Some(((f1, f2, f3, f4), edit, font_name, expanded, title_h)) = fonts {
                let _ = DeleteObject(HGDIOBJ(f1.0));
                let _ = DeleteObject(HGDIOBJ(f2.0));
                let _ = DeleteObject(HGDIOBJ(f3.0));
                let _ = DeleteObject(HGDIOBJ(f4.0));
                set_font(edit, font_name);
                // 位置用系统建议 RECT；高度按当前展开态取
                let h = if expanded { sh } else { title_h };
                let _ = SetWindowPos(hwnd, Some(HWND_BOTTOM), sx, sy, sw, h, SWP_NOACTIVATE);
                with_zone(hwnd, |z| {
                    z.cfg.x = sx;
                    z.cfg.y = sy;
                    z.cfg.w = sw;
                    if z.expanded {
                        z.cfg.h = sh;
                    }
                });
                save_zones();
                layout_children(hwnd);
                let _ = InvalidateRect(Some(hwnd), None, true);
            }
            LRESULT(0)
        }
        WM_NCDESTROY => {
            let z = ZONES.with(|c| c.borrow_mut().remove(&key(hwnd)));
            if let Some(z) = z {
                let _ = DeleteObject(HGDIOBJ(z.font_title.0));
                let _ = DeleteObject(HGDIOBJ(z.font_name.0));
                let _ = DeleteObject(HGDIOBJ(z.font_time.0));
                let _ = DeleteObject(HGDIOBJ(z.font_glyph.0));
                // 只有「移除映射区」会 DestroyWindow；进程退出不销毁窗口，不会误清配置
                save_zones();
            }
            DefWindowProcW(hwnd, msg, wp, lp)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

// ---- 列表子窗（自绘虚拟列表 + 平滑滚动，复刻 searchwin::results_proc 模式） ----

fn zone_of_list(list: HWND) -> HWND {
    unsafe { GetParent(list).unwrap_or_default() }
}

/// (可见条数, item_h, 客户高, 最大滚动)。
fn zlist_metrics(list: HWND) -> (i32, i32, i32, f32) {
    let mut rc = RECT::default();
    let _ = unsafe { GetClientRect(list, &mut rc) };
    let ch = rc.bottom - rc.top;
    let zone = zone_of_list(list);
    let (count, item_h) =
        read_zone(zone, |z| (z.view.len() as i32, z.item_h)).unwrap_or((0, 28));
    let max_scroll = ((count * item_h - ch).max(0)) as f32;
    (count, item_h, ch, max_scroll)
}

unsafe fn update_scrollbar(list: HWND) {
    let (count, item_h, ch, _max) = zlist_metrics(list);
    let zone = zone_of_list(list);
    let pos = read_zone(zone, |z| z.scroll_y as i32).unwrap_or(0);
    let si = SCROLLINFO {
        cbSize: std::mem::size_of::<SCROLLINFO>() as u32,
        fMask: SIF_RANGE | SIF_PAGE | SIF_POS,
        nMin: 0,
        nMax: (count * item_h - 1).max(0),
        nPage: ch.max(0) as u32,
        nPos: pos,
        nTrackPos: 0,
    };
    SetScrollInfo(list, SB_VERT, &si, true);
}

fn start_anim(list: HWND) {
    let zone = zone_of_list(list);
    let need = with_zone(zone, |z| {
        if !z.animating {
            z.animating = true;
            true
        } else {
            false
        }
    });
    if need == Some(true) {
        unsafe {
            SetTimer(Some(list), TIMER_ANIM, 15, None);
        }
    }
}

fn set_target(list: HWND, y: f32) {
    let (_, _, _, max_scroll) = zlist_metrics(list);
    let zone = zone_of_list(list);
    if max_scroll <= 0.0 {
        with_zone(zone, |z| {
            z.target_y = 0.0;
            z.scroll_y = 0.0;
        });
        return;
    }
    with_zone(zone, |z| {
        z.target_y = y.clamp(0.0, max_scroll);
    });
    start_anim(list);
}

fn y_to_index(list: HWND, y: i32) -> i32 {
    let (count, item_h, _, _) = zlist_metrics(list);
    let zone = zone_of_list(list);
    let scroll_y = read_zone(zone, |z| z.scroll_y).unwrap_or(0.0);
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

unsafe fn paint_zlist(list: HWND) {
    let mut ps = PAINTSTRUCT::default();
    let hdc = BeginPaint(list, &mut ps);
    let mut rc = RECT::default();
    let _ = GetClientRect(list, &mut rc);
    let t = theme::current();
    let zone = zone_of_list(list);
    let meta = read_zone(zone, |z| {
        (
            z.item_h,
            z.scroll_y,
            z.sel,
            z.hover,
            z.font_name,
            z.font_time,
            z.dpi,
            z.error.clone(),
            z.view.len() as i32,
        )
    });
    paint::buffered(hdc, rc, |mem, lrc| {
        FillRect(mem, &lrc, theme::solid_brush(t.bg));
        let Some((item_h, scroll_y, sel, hover, f_name, f_time, dpi, error, count)) = meta else {
            return;
        };
        let sc = |v: i32| v * dpi / 96;
        // 错误 / 空态：居中提示
        let hint = if let Some(e) = &error {
            Some(e.clone())
        } else if count == 0 {
            Some("（无匹配文件）".to_string())
        } else {
            None
        };
        if let Some(hint) = hint {
            SetBkMode(mem, TRANSPARENT);
            let mut r = RECT {
                left: sc(12),
                top: lrc.top + sc(16),
                right: lrc.right - sc(12),
                bottom: lrc.bottom,
            };
            draw_text(mem, &hint, &mut r, t.path_fg, f_name, DT_CENTER | DT_WORDBREAK | DT_NOPREFIX);
            return;
        }
        if item_h <= 0 {
            return;
        }
        let sy = scroll_y as i32;
        let first = (sy / item_h).max(0);
        let last = ((sy + lrc.bottom) / item_h).min(count - 1);
        for i in first..=last {
            let y = i * item_h - sy;
            let row = RECT {
                left: 0,
                top: y,
                right: lrc.right,
                bottom: y + item_h,
            };
            let item = read_zone(zone, |z| {
                z.view
                    .get(i as usize)
                    .and_then(|&ix| z.items.get(ix as usize))
                    .map(|it| (it.name.clone(), it.is_dir, it.mtime_str.clone()))
            })
            .flatten();
            let Some((name, is_dir, mtime_str)) = item else {
                continue;
            };
            let bg = if i == sel {
                t.sel_bg
            } else if i == hover {
                t.alt_bg
            } else {
                t.bg
            };
            let fg = if i == sel { t.sel_fg } else { t.fg };
            let time_fg = if i == sel { t.sel_fg } else { t.path_fg };
            FillRect(mem, &row, theme::solid_brush(bg));
            let pad = sc(8);
            let icon = sc(18);
            let icy = row.top + (item_h - icon) / 2;
            if let Some(hicon) = icons::icon_for(&name, is_dir) {
                let _ = DrawIconEx(mem, row.left + pad, icy, hicon, icon, icon, 0, None, DI_NORMAL);
            }
            SetBkMode(mem, TRANSPARENT);
            let time_w = sc(84);
            let mut rn = RECT {
                left: row.left + pad + icon + sc(6),
                top: row.top,
                right: row.right - time_w - pad,
                bottom: row.bottom,
            };
            draw_text(mem, &name, &mut rn, fg, f_name, DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS);
            let mut rt = RECT {
                left: row.right - time_w - pad,
                top: row.top,
                right: row.right - pad,
                bottom: row.bottom,
            };
            draw_text(mem, &mtime_str, &mut rt, time_fg, f_time, DT_RIGHT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX);
        }
    });
    let _ = EndPaint(list, &ps);
}

unsafe extern "system" fn zlist_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => {
            paint_zlist(hwnd);
            LRESULT(0)
        }
        WM_MOUSEWHEEL => {
            let delta = ((wp.0 >> 16) & 0xFFFF) as i16 as i32;
            let (_, item_h, _, _) = zlist_metrics(hwnd);
            let step = item_h as f32 * 1.5;
            let zone = zone_of_list(hwnd);
            let target = read_zone(zone, |z| z.target_y).unwrap_or(0.0);
            set_target(hwnd, target - (delta as f32 / 120.0) * step);
            LRESULT(0)
        }
        WM_VSCROLL => {
            let code = (wp.0 as u32) & 0xFFFF;
            let (_, item_h, ch, max_scroll) = zlist_metrics(hwnd);
            let zone = zone_of_list(hwnd);
            let target = read_zone(zone, |z| z.target_y).unwrap_or(0.0);
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
                    let pos = (si.nTrackPos as f32).clamp(0.0, max_scroll);
                    with_zone(zone, |z| {
                        z.scroll_y = pos;
                        z.target_y = pos;
                    });
                    update_scrollbar(hwnd);
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_TIMER => {
            if wp.0 == TIMER_ANIM {
                let (_, _, _, max_scroll) = zlist_metrics(hwnd);
                let zone = zone_of_list(hwnd);
                let done = with_zone(zone, |z| {
                    z.target_y = z.target_y.clamp(0.0, max_scroll);
                    let d = z.target_y - z.scroll_y;
                    if d.abs() < 0.5 {
                        z.scroll_y = z.target_y;
                        z.animating = false;
                        true
                    } else {
                        z.scroll_y += d * 0.30;
                        false
                    }
                });
                if done == Some(true) {
                    KillTimer(Some(hwnd), TIMER_ANIM).ok();
                }
                update_scrollbar(hwnd);
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            let y = ((lp.0 >> 16) & 0xFFFF) as i16 as i32;
            let idx = y_to_index(hwnd, y);
            let zone = zone_of_list(hwnd);
            if idx >= 0 {
                with_zone(zone, |z| z.sel = idx);
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            LRESULT(0)
        }
        WM_LBUTTONDBLCLK => {
            let y = ((lp.0 >> 16) & 0xFFFF) as i16 as i32;
            let idx = y_to_index(hwnd, y);
            let zone = zone_of_list(hwnd);
            if idx >= 0 {
                let item = with_zone(zone, |z| {
                    z.sel = idx;
                    z.view
                        .get(idx as usize)
                        .and_then(|&ix| z.items.get(ix as usize))
                        .map(|it| (join_path(&z.cfg.path, &it.name), it.is_dir))
                })
                .flatten();
                if let Some((path, is_dir)) = item {
                    open_item(&path, is_dir);
                }
            }
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let y = ((lp.0 >> 16) & 0xFFFF) as i16 as i32;
            let idx = y_to_index(hwnd, y);
            let zone = zone_of_list(hwnd);
            let changed = with_zone(zone, |z| {
                if z.hover != idx {
                    z.hover = idx;
                    true
                } else {
                    false
                }
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
            let zone = zone_of_list(hwnd);
            with_zone(zone, |z| z.hover = -1);
            let _ = InvalidateRect(Some(hwnd), None, false);
            LRESULT(0)
        }
        WM_CONTEXTMENU => {
            let x = (lp.0 & 0xFFFF) as i16 as i32;
            let y = ((lp.0 >> 16) & 0xFFFF) as i16 as i32;
            let zone = zone_of_list(hwnd);
            if x != -1 || y != -1 {
                // 屏幕坐标 → 列表客户坐标，选中命中条目再弹菜单
                let mut pt = POINT { x, y };
                let _ = ScreenToClient(hwnd, &mut pt);
                let idx = y_to_index(hwnd, pt.y);
                if idx >= 0 {
                    with_zone(zone, |z| z.sel = idx);
                    let _ = InvalidateRect(Some(hwnd), None, false);
                    show_item_menu(zone, hwnd, x, y);
                }
            }
            LRESULT(0)
        }
        WM_SIZE => {
            let (_, _, _, max_scroll) = zlist_metrics(hwnd);
            let zone = zone_of_list(hwnd);
            with_zone(zone, |z| {
                z.scroll_y = z.scroll_y.min(max_scroll);
                z.target_y = z.target_y.min(max_scroll);
            });
            update_scrollbar(hwnd);
            let _ = InvalidateRect(Some(hwnd), None, false);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}
