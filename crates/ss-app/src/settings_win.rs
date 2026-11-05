//! 设置窗口。除文字输入框外全部自绘，这样复选框、按钮、表格和列表行都能用圆角。
//!
//! 窗口分两部分：可滚动的内容区（子窗口，输入框是它的子窗口，滚动时自动被裁剪）
//! 和固定在底部的按钮区。内容区每次都按当前状态完整走一遍布局（[`Walk`]），
//! 同一段代码既用来计算可点击区域，也用来绘制。
//!
//! 收藏夹的排序和删除立即生效，其余内容在点“保存”时校验并写入。

use std::cell::RefCell;
use std::ffi::c_void;

use ss_core::{DriveState, DriveStatus, VolumeInfo};
use ss_ui_model::{
    compact, index_stats, parse_hotkey, parse_recent_max, parse_result_limit, scroll_from_thumb,
    scroll_thumb,
};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, DeleteObject, EndPaint, FillRect, GetMonitorInfoW, InvalidateRect,
    MonitorFromWindow, SetBkColor, SetTextColor, DRAW_TEXT_FORMAT, DT_CENTER, DT_END_ELLIPSIS,
    DT_LEFT, DT_NOPREFIX, DT_PATH_ELLIPSIS, DT_SINGLELINE, DT_VCENTER, HDC, HFONT, HGDIOBJ,
    MONITORINFO, MONITOR_DEFAULTTONEAREST, PAINTSTRUCT,
};
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_INPROC_SERVER};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::WM_MOUSELEAVE;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetFocus, GetKeyState, ReleaseCapture, SetCapture, SetFocus, TrackMouseEvent, TME_LEAVE,
    TRACKMOUSEEVENT, VK_ESCAPE, VK_RETURN, VK_SHIFT, VK_TAB,
};
use windows::Win32::UI::Shell::{FileOpenDialog, IFileOpenDialog, FOS_PICKFOLDERS, SIGDN_FILESYSPATH};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::paint::{self, measure, text};
use crate::theme::{self, Theme};

/// 尺寸都是 96 DPI 下的值。
const WINDOW_W: i32 = 640;
const WINDOW_H: i32 = 720;
const FOOT_H: i32 = 58;
const RADIUS: i32 = 6;
const SIDE: i32 = 20;
/// 表单第二列的起点。
const FIELD_X: i32 = 132;
const INPUT_H: i32 = 30;
const ROW_H: i32 = 32;

const ES_NUMBER: u32 = 0x2000;
const ES_AUTOHSCROLL: u32 = 0x0080;
const EM_SETSEL: u32 = 0x00B1;
const WS_TABSTOP_RAW: u32 = 0x0001_0000;
const CAPTION_DLG: u32 = 0x00CC_0000; // caption | sysmenu | border
const WS_CHILD_RAW: u32 = 0x4000_0000;
const WS_VISIBLE_RAW: u32 = 0x1000_0000;
const WS_CLIPCHILDREN_RAW: u32 = 0x0200_0000;

/// 复选框对应的设置。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flag {
    HideSystem,
    Autostart,
    Popup,
    ShowFavorites,
    ShowRecent,
    ShowOpen,
}

/// 可编辑的文件夹列表。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Folders {
    Exclude,
    Preset,
}

/// 可点击的区域。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hit {
    Flag(Flag),
    Drive(usize),
    Rebuild,
    Add(Folders),
    Remove(Folders, usize),
    FavUp(usize),
    FavDown(usize),
    FavRemove(usize),
    Save,
    Cancel,
}

struct DriveRow {
    letter: char,
    title: String,
    file_system: String,
    files: String,
    status: String,
    healthy: bool,
    indexable: bool,
    checked: bool,
}

#[derive(Clone, Copy)]
struct Fonts {
    body: HFONT,
    small: HFONT,
    heading: HFONT,
    icon: HFONT,
    check: HFONT,
}

unsafe fn make_fonts(dpi: i32) -> Fonts {
    let sc = |v: i32| v * dpi / 96;
    Fonts {
        body: paint::ui_font(sc(13), 400),
        small: paint::ui_font(sc(12), 400),
        heading: paint::ui_font(sc(14), 600),
        icon: paint::icon_font(sc(11), 400),
        check: paint::icon_font(sc(10), 700),
    }
}

unsafe fn delete_fonts(f: Fonts) {
    for font in [f.body, f.small, f.heading, f.icon, f.check] {
        let _ = DeleteObject(HGDIOBJ(font.0));
    }
}

struct SUi {
    hwnd: HWND,
    content: HWND,
    /// 输入框：热键、结果上限、最近条数。
    edits: [HWND; 3],
    drives: Vec<DriveRow>,
    exclude: Vec<String>,
    presets: Vec<String>,
    favorites: Vec<String>,
    hide_system: bool,
    autostart: bool,
    popup: bool,
    show_favorites: bool,
    show_recent: bool,
    show_open: bool,
    stats: String,
    hotkey_hint: Option<String>,
    limit_hint: Option<String>,
    recent_hint: Option<String>,
    foot_hint: Option<String>,
    fonts: Fonts,
    dpi: i32,
    scroll: i32,
    total_h: i32,
    hits: Vec<(RECT, Hit)>,
    foot_hits: Vec<(RECT, Hit)>,
    hover: Option<Hit>,
    drag_grab: Option<i32>,
}

impl SUi {
    fn flag_mut(&mut self, f: Flag) -> &mut bool {
        match f {
            Flag::HideSystem => &mut self.hide_system,
            Flag::Autostart => &mut self.autostart,
            Flag::Popup => &mut self.popup,
            Flag::ShowFavorites => &mut self.show_favorites,
            Flag::ShowRecent => &mut self.show_recent,
            Flag::ShowOpen => &mut self.show_open,
        }
    }

    fn folders_mut(&mut self, which: Folders) -> &mut Vec<String> {
        match which {
            Folders::Exclude => &mut self.exclude,
            Folders::Preset => &mut self.presets,
        }
    }
}

thread_local! {
    static SUI: RefCell<Option<SUi>> = const { RefCell::new(None) };
}

fn send(hwnd: HWND, msg: u32, w: usize, l: isize) -> isize {
    unsafe { SendMessageW(hwnd, msg, Some(WPARAM(w)), Some(LPARAM(l))).0 }
}

fn get_text(hwnd: HWND) -> String {
    let len = send(hwnd, WM_GETTEXTLENGTH, 0, 0) as usize;
    if len == 0 {
        return String::new();
    }
    let mut buf = vec![0u16; len + 1];
    let n = send(hwnd, WM_GETTEXT, buf.len(), buf.as_mut_ptr() as isize) as usize;
    String::from_utf16_lossy(&buf[..n.min(buf.len())])
}

/// 弹出系统「选择文件夹」对话框，返回所选完整路径。
unsafe fn pick_folder(owner: HWND) -> Option<String> {
    let dlg: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
    let opts = dlg.GetOptions().ok()?;
    dlg.SetOptions(opts | FOS_PICKFOLDERS).ok()?;
    if dlg.Show(Some(owner)).is_err() {
        return None; // 用户取消
    }
    let item = dlg.GetResult().ok()?;
    let pw = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
    let s = pw.to_string().ok()?;
    CoTaskMemFree(Some(pw.0 as *const c_void));
    Some(s)
}

/// 打开设置窗口（若已开则前置）。
pub fn open() {
    if let Some(h) = SUI.with(|c| c.borrow().as_ref().map(|u| u.hwnd)) {
        unsafe {
            let _ = ShowWindow(h, SW_SHOW);
            let _ = SetForegroundWindow(h);
        }
        refresh_index_info();
        return;
    }
    let _ = unsafe { create() };
}

/// 一个盘的状态文字和是否正常。
fn drive_status(v: &VolumeInfo, statuses: Option<&[DriveStatus]>) -> (String, bool) {
    if !v.indexable {
        let reason = v.reason.as_deref().unwrap_or("不支持");
        return (format!("不能索引：{reason}"), false);
    }
    let Some(all) = statuses else {
        return ("索引中".to_string(), true);
    };
    match all.iter().find(|s| s.letter == v.letter) {
        None => ("未索引，重启后生效".to_string(), true),
        Some(s) => match (&s.state, &s.last_error) {
            (DriveState::Failed { reason }, _) => (format!("失败：{reason}"), false),
            (DriveState::Ready, Some(e)) => (format!("已就绪，{e}"), false),
            (DriveState::Ready, None) => match crate::synced_secs_ago() {
                Some(secs) if secs < 60 => (format!("已就绪，{secs} 秒前更新"), true),
                _ => ("已就绪".to_string(), true),
            },
            _ => ("索引中".to_string(), true),
        },
    }
}

/// 从索引读取盘列表需要的信息。
fn drive_rows(selected: &[char]) -> Vec<DriveRow> {
    let (statuses, counts) = crate::CATALOG
        .get()
        .and_then(|cat| cat.read().as_ref().map(|c| (c.drive_statuses(), c.drive_node_counts())))
        .map_or((None, Vec::new()), |(s, n)| (Some(s), n));
    ss_core::list_volumes()
        .iter()
        .map(|v| {
            let (status, healthy) = drive_status(v, statuses.as_deref());
            let files = counts.iter().find(|(l, _)| *l == v.letter).map_or("无".to_string(), |(_, n)| compact(*n));
            DriveRow {
                letter: v.letter,
                title: if v.label.is_empty() {
                    format!("{}:", v.letter)
                } else {
                    format!("{}: {}", v.letter, v.label)
                },
                file_system: v.file_system.clone(),
                files,
                status,
                healthy,
                indexable: v.indexable,
                checked: v.indexable && (selected.is_empty() || selected.contains(&v.letter)),
            }
        })
        .collect()
}

fn stats_text() -> String {
    match crate::CATALOG.get().and_then(|cat| cat.read().as_ref().map(|c| (c.total_nodes(), c.memory_bytes()))) {
        Some((nodes, memory)) => index_stats(nodes, memory, crate::synced_secs_ago()),
        None => "索引建立中".to_string(),
    }
}

/// 重新读取索引统计和盘状态，保留用户还没保存的勾选。
fn refresh_index_info() {
    let stats = stats_text();
    let selected = SUI.with(|c| {
        c.borrow().as_ref().map(|u| {
            let all = u.drives.iter().filter(|d| d.indexable).all(|d| d.checked);
            if all {
                Vec::new()
            } else {
                u.drives.iter().filter(|d| d.checked).map(|d| d.letter).collect::<Vec<char>>()
            }
        })
    });
    let Some(selected) = selected else {
        return;
    };
    let rows = drive_rows(&selected);
    SUI.with(|c| {
        if let Some(u) = c.borrow_mut().as_mut() {
            u.drives = rows;
            u.stats = stats;
        }
    });
    invalidate_content();
}

unsafe fn create() -> windows::core::Result<()> {
    let hinstance = HINSTANCE(GetModuleHandleW(None)?.0);
    let class = w!("SaveSearchSettings");
    let wc = WNDCLASSW {
        lpfnWndProc: Some(wndproc),
        hInstance: hinstance,
        lpszClassName: class,
        hCursor: LoadCursorW(None, IDC_ARROW)?,
        hIcon: crate::load_app_icon(0, 0).unwrap_or_default(),
        ..Default::default()
    };
    RegisterClassW(&wc);
    let content_class = w!("SaveSearchSettingsContent");
    let wcc = WNDCLASSW {
        lpfnWndProc: Some(content_proc),
        hInstance: hinstance,
        lpszClassName: content_class,
        hCursor: LoadCursorW(None, IDC_ARROW)?,
        ..Default::default()
    };
    RegisterClassW(&wcc);

    let hwnd = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        class,
        w!("SaveSearch 设置"),
        WINDOW_STYLE(CAPTION_DLG | WS_CLIPCHILDREN_RAW),
        0,
        0,
        WINDOW_W,
        WINDOW_H,
        None,
        None,
        Some(hinstance),
        None,
    )?;

    let dpi = GetDpiForWindow(hwnd).max(96) as i32;
    let sc = |v: i32| v * dpi / 96;

    // 按 DPI 放大，高度不超过工作区，并居中
    let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
    let mon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
    let wr = if GetMonitorInfoW(mon, &mut mi).as_bool() {
        mi.rcWork
    } else {
        RECT { left: 0, top: 0, right: GetSystemMetrics(SM_CXSCREEN), bottom: GetSystemMetrics(SM_CYSCREEN) }
    };
    let ww = sc(WINDOW_W);
    let wh = sc(WINDOW_H).min(wr.bottom - wr.top);
    let _ = MoveWindow(
        hwnd,
        wr.left + (wr.right - wr.left - ww) / 2,
        wr.top + (wr.bottom - wr.top - wh) / 2,
        ww,
        wh,
        false,
    );

    let content = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        content_class,
        PCWSTR::null(),
        WINDOW_STYLE(WS_CHILD_RAW | WS_VISIBLE_RAW | WS_CLIPCHILDREN_RAW),
        0,
        0,
        10,
        10,
        Some(hwnd),
        None,
        Some(hinstance),
        None,
    )?;

    let s = ss_config::load_settings();
    let fonts = make_fonts(dpi);
    let make_edit = |value: &str, style: u32| -> HWND {
        let v = crate::wide(value);
        let h = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("EDIT"),
            PCWSTR(v.as_ptr()),
            WINDOW_STYLE(WS_CHILD_RAW | WS_VISIBLE_RAW | WS_TABSTOP_RAW | ES_AUTOHSCROLL | style),
            0,
            0,
            10,
            10,
            Some(content),
            None,
            Some(hinstance),
            None,
        )
        .unwrap_or(HWND(std::ptr::null_mut()));
        send(h, WM_SETFONT, fonts.body.0 as usize, 1);
        h
    };
    let edits = [
        make_edit(&s.hotkey, 0),
        make_edit(&s.result_limit.to_string(), ES_NUMBER),
        make_edit(&s.recent_max.to_string(), ES_NUMBER),
    ];
    let hotkey_hint = crate::hotkey_failed().then(|| "这个组合已被其他程序占用，请换一个。".to_string());

    SUI.with(|c| {
        *c.borrow_mut() = Some(SUi {
            hwnd,
            content,
            edits,
            drives: drive_rows(&s.indexed_drives),
            exclude: s.exclude_dirs,
            presets: s.preset_folders,
            favorites: ss_config::favorites(),
            hide_system: s.hide_system,
            autostart: s.autostart,
            popup: s.popup_enabled,
            show_favorites: s.popup_show_favorites,
            show_recent: s.popup_show_recent,
            show_open: s.popup_show_open,
            stats: stats_text(),
            hotkey_hint,
            limit_hint: None,
            recent_hint: None,
            foot_hint: None,
            fonts,
            dpi,
            scroll: 0,
            total_h: 0,
            hits: Vec::new(),
            foot_hits: Vec::new(),
            hover: None,
            drag_grab: None,
        });
    });
    layout_content();
    theme::apply_window(hwnd);
    let _ = ShowWindow(hwnd, SW_SHOW);
    let _ = SetForegroundWindow(hwnd);
    Ok(())
}

/// 按窗口大小摆放内容区。
unsafe fn layout_content() {
    let Some((hwnd, content, dpi)) = SUI.with(|c| c.borrow().as_ref().map(|u| (u.hwnd, u.content, u.dpi))) else {
        return;
    };
    let mut rc = RECT::default();
    let _ = GetClientRect(hwnd, &mut rc);
    let foot = FOOT_H * dpi / 96;
    let _ = MoveWindow(content, 0, 0, rc.right, rc.bottom - foot, true);
    relayout();
}

/// 一次布局走读。`hdc` 为 `None` 时只计算位置。
struct Walk<'a> {
    u: &'a SUi,
    hdc: Option<HDC>,
    t: Theme,
    width: i32,
    hits: Vec<(RECT, Hit)>,
    edit_rects: [RECT; 3],
}

impl Walk<'_> {
    fn sc(&self, v: i32) -> i32 {
        v * self.u.dpi / 96
    }

    /// 内容坐标（96 DPI）转窗口坐标。
    fn rect(&self, x: i32, y: i32, w: i32, h: i32) -> RECT {
        let (left, top) = (self.sc(x), self.sc(y) - self.u.scroll);
        RECT { left, top, right: left + self.sc(w), bottom: top + self.sc(h) }
    }

    fn logical_width(&self) -> i32 {
        self.width * 96 / self.u.dpi
    }

    unsafe fn text(&self, s: &str, mut r: RECT, color: u32, font: HFONT, flags: DRAW_TEXT_FORMAT) {
        if let Some(hdc) = self.hdc {
            text(hdc, s, &mut r, color, font, flags | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX);
        }
    }

    unsafe fn label(&self, s: &str, x: i32, y: i32, w: i32, h: i32) {
        self.text(s, self.rect(x, y, w, h), self.t.fg, self.u.fonts.body, DT_LEFT | DT_END_ELLIPSIS);
    }

    unsafe fn heading(&self, s: &str, y: i32) {
        self.text(s, self.rect(SIDE, y, 300, 22), self.t.fg, self.u.fonts.heading, DT_LEFT);
    }

    unsafe fn hint(&self, s: &str, x: i32, y: i32) {
        self.text(s, self.rect(x, y, 440, 18), self.t.err, self.u.fonts.small, DT_LEFT | DT_END_ELLIPSIS);
    }

    /// 复选框，返回占用的宽度（96 DPI）。
    unsafe fn check(&mut self, s: &str, x: i32, y: i32, value: bool, enabled: bool, hit: Hit) -> i32 {
        let font = self.u.fonts.body;
        let w = 22 + measure(font, s) * 96 / self.u.dpi + 4;
        let row = self.rect(x, y, w, 24);
        let box_rect = self.rect(x, y + 4, 16, 16);
        if let Some(hdc) = self.hdc {
            if value {
                let fill = if enabled { self.t.accent } else { self.t.thumb };
                paint::round_rect(hdc, box_rect, self.sc(4), fill, None, self.t.bg);
                let mut cr = box_rect;
                text(hdc, "\u{E73E}", &mut cr, self.t.accent_fg, self.u.fonts.check, DT_CENTER | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX);
            } else {
                let fill = if enabled { self.t.bg } else { self.t.alt_bg };
                let edge = if enabled && self.u.hover == Some(hit) { self.t.accent } else { self.t.thumb };
                paint::round_rect(hdc, box_rect, self.sc(4), fill, Some(edge), self.t.bg);
            }
        }
        let color = if enabled { self.t.fg } else { self.t.path_fg };
        self.text(s, self.rect(x + 22, y, w - 22, 24), color, font, DT_LEFT | DT_END_ELLIPSIS);
        if enabled {
            self.hits.push((row, hit));
        }
        w
    }

    /// 普通按钮。
    unsafe fn button(&mut self, s: &str, x: i32, y: i32, hit: Hit) {
        let font = self.u.fonts.small;
        let w = measure(font, s) * 96 / self.u.dpi + 24;
        let r = self.rect(x, y, w, 26);
        if let Some(hdc) = self.hdc {
            let fill = if self.u.hover == Some(hit) { self.t.alt_bg } else { self.t.bg };
            paint::round_rect(hdc, r, self.sc(RADIUS), fill, Some(self.t.border), self.t.bg);
        }
        self.text(s, r, self.t.fg, font, DT_CENTER);
        self.hits.push((r, hit));
    }

    /// 输入框的圆角外框。文字由真实的 EDIT 控件显示。
    unsafe fn input(&mut self, index: usize, x: i32, y: i32, w: i32, invalid: bool) {
        let r = self.rect(x, y, w, INPUT_H);
        if let Some(hdc) = self.hdc {
            let edge = if invalid { self.t.err } else { self.t.border };
            paint::round_rect(hdc, r, self.sc(RADIUS), self.t.bg, Some(edge), self.t.bg);
        }
        let inner_h = self.sc(19);
        let top = r.top + (r.bottom - r.top - inner_h) / 2;
        self.edit_rects[index] = RECT { left: r.left + self.sc(9), top, right: r.right - self.sc(8), bottom: top + inner_h };
    }

    /// 带图标按钮的路径行。`icons` 是（字形，动作）。
    unsafe fn path_row(&mut self, s: &str, x: i32, y: i32, w: i32, icons: &[(&str, Hit)]) {
        let r = self.rect(x, y, w, ROW_H - 4);
        if let Some(hdc) = self.hdc {
            paint::round_rect(hdc, r, self.sc(RADIUS), self.t.bg, Some(self.t.border), self.t.bg);
        }
        let icon_w = 24;
        let reserve = 8 + icons.len() as i32 * icon_w;
        self.text(s, self.rect(x + 10, y, w - 10 - reserve, ROW_H - 4), self.t.fg, self.u.fonts.small, DT_LEFT | DT_PATH_ELLIPSIS);
        let mut ix = x + w - 4 - icons.len() as i32 * icon_w;
        for (glyph, hit) in icons {
            let b = self.rect(ix, y + 3, icon_w - 2, ROW_H - 10);
            let hovered = self.u.hover == Some(*hit);
            if let (Some(hdc), true) = (self.hdc, hovered) {
                paint::round_rect(hdc, b, self.sc(4), self.t.sel_bg, None, self.t.bg);
            }
            self.text(glyph, b, if hovered { self.t.fg } else { self.t.path_fg }, self.u.fonts.icon, DT_CENTER);
            self.hits.push((b, *hit));
            ix += icon_w;
        }
    }

    /// 排除目录或预设文件夹的列表，返回下一段的 y。
    unsafe fn folder_list(&mut self, which: Folders, items: &[String], x: i32, mut y: i32, w: i32) -> i32 {
        for (i, path) in items.iter().enumerate() {
            self.path_row(path, x, y, w, &[("\u{E711}", Hit::Remove(which, i))]);
            y += ROW_H;
        }
        self.button("添加…", x, y, Hit::Add(which));
        y + 36
    }

    /// 磁盘表格，返回下一段的 y。
    unsafe fn drive_table(&mut self, y: i32) -> i32 {
        let t = self.t;
        let w = self.logical_width() - SIDE * 2 - 8;
        let (head_h, row_h) = (28, 34);
        let rows = self.u.drives.len() as i32;
        let frame = self.rect(SIDE, y, w, head_h + row_h * rows);
        if let Some(hdc) = self.hdc {
            paint::round_rect(hdc, frame, self.sc(RADIUS + 1), t.bg, Some(t.border), t.bg);
            // 表头：上方两角圆，下方直
            let head = RECT { left: frame.left + 1, top: frame.top + 1, right: frame.right - 1, bottom: frame.top + self.sc(head_h) };
            paint::round_rect(hdc, head, self.sc(RADIUS), t.alt_bg, None, t.bg);
            let lower = RECT { top: head.top + (head.bottom - head.top) / 2, ..head };
            FillRect(hdc, &lower, theme::solid_brush(t.alt_bg));
            for i in 0..rows {
                let ly = self.rect(SIDE, y + head_h + row_h * i, w, 1);
                let line = RECT { left: frame.left + 1, right: frame.right - 1, bottom: ly.top + 1, ..ly };
                FillRect(hdc, &line, theme::solid_brush(t.border));
            }
        }
        let cols = [SIDE + 12, SIDE + 186, SIDE + 262, SIDE + 322];
        let col_w = [170, 72, 56, w - 322 - 8];
        for (i, title) in ["盘", "文件系统", "文件数", "状态"].iter().enumerate() {
            self.text(title, self.rect(cols[i], y, col_w[i], head_h), t.path_fg, self.u.fonts.small, DT_LEFT);
        }
        let drives = &self.u.drives;
        for (i, d) in drives.iter().enumerate() {
            let ry = y + head_h + row_h * i as i32;
            self.check(&d.title, cols[0], ry + 5, d.checked, d.indexable, Hit::Drive(i));
            let small = self.u.fonts.small;
            self.text(&d.file_system, self.rect(cols[1], ry, col_w[1], row_h), t.fg, small, DT_LEFT);
            self.text(&d.files, self.rect(cols[2], ry, col_w[2], row_h), t.fg, small, DT_LEFT);
            let color = if d.healthy { t.ok } else { t.err };
            self.text(&d.status, self.rect(cols[3], ry, col_w[3], row_h), color, small, DT_LEFT | DT_END_ELLIPSIS);
        }
        y + head_h + row_h * rows
    }

    /// 走一遍全部内容，返回总高度（96 DPI）。
    unsafe fn run(&mut self) -> i32 {
        let t = self.t;
        let u = self.u;
        let field_w = self.logical_width() - FIELD_X - SIDE - 8;
        let mut y = 16;

        self.heading("索引的磁盘", y);
        y += 28;
        y = self.drive_table(y) + 10;
        let full_w = self.logical_width() - SIDE * 2 - 8;
        self.text(&u.stats, self.rect(SIDE, y, full_w - 90, 26), t.path_fg, u.fonts.small, DT_LEFT | DT_END_ELLIPSIS);
        self.button("重建索引", SIDE + full_w - 80, y, Hit::Rebuild);
        y += 44;

        self.heading("搜索", y);
        y += 30;
        self.label("呼出热键", SIDE, y, 100, INPUT_H);
        self.input(0, FIELD_X, y, 240, u.hotkey_hint.is_some());
        y += INPUT_H + 3;
        if let Some(h) = &u.hotkey_hint {
            self.hint(h, FIELD_X, y);
            y += 20;
        }
        y += 5;
        self.label("结果上限", SIDE, y, 100, INPUT_H);
        self.input(1, FIELD_X, y, 240, u.limit_hint.is_some());
        y += INPUT_H + 3;
        if let Some(h) = &u.limit_hint {
            self.hint(h, FIELD_X, y);
            y += 20;
        }
        y += 5;
        self.label("排除目录", SIDE, y, 100, ROW_H - 4);
        y = self.folder_list(Folders::Exclude, &u.exclude, FIELD_X, y, field_w);
        self.label("预设文件夹", SIDE, y, 100, ROW_H - 4);
        y = self.folder_list(Folders::Preset, &u.presets, FIELD_X, y, field_w);
        self.label("隐藏系统文件", SIDE, y, 100, 24);
        self.check("不显示带隐藏或系统属性的文件", FIELD_X, y, u.hide_system, true, Hit::Flag(Flag::HideSystem));
        y += 30;
        self.label("开机自启", SIDE, y, 100, 24);
        self.check("登录 Windows 时启动（计划任务，免 UAC）", FIELD_X, y, u.autostart, true, Hit::Flag(Flag::Autostart));
        y += 44;

        self.heading("快选浮窗", y);
        y += 30;
        self.label("启用", SIDE, y, 100, 24);
        self.check("在保存和打开对话框下方显示", FIELD_X, y, u.popup, true, Hit::Flag(Flag::Popup));
        y += 30;
        self.label("显示内容", SIDE, y, 100, 24);
        let mut x = FIELD_X;
        x += self.check("收藏", x, y, u.show_favorites, u.popup, Hit::Flag(Flag::ShowFavorites)) + 14;
        x += self.check("已打开", x, y, u.show_open, u.popup, Hit::Flag(Flag::ShowOpen)) + 14;
        self.check("最近", x, y, u.show_recent, u.popup, Hit::Flag(Flag::ShowRecent));
        y += 32;
        self.label("最近条数", SIDE, y, 100, INPUT_H);
        self.input(2, FIELD_X, y, 240, u.recent_hint.is_some());
        y += INPUT_H + 3;
        if let Some(h) = &u.recent_hint {
            self.hint(h, FIELD_X, y);
            y += 20;
        }
        y += 11;
        self.label("收藏夹", SIDE, y, 100, ROW_H - 4);
        if u.favorites.is_empty() {
            self.text("还没有收藏。在快选浮窗里右键文件夹可以收藏。", self.rect(FIELD_X, y, field_w, ROW_H - 4), t.path_fg, u.fonts.small, DT_LEFT);
            y += ROW_H;
        }
        for (i, path) in u.favorites.iter().enumerate() {
            self.path_row(path, FIELD_X, y, field_w, &[
                ("\u{E74A}", Hit::FavUp(i)),
                ("\u{E74B}", Hit::FavDown(i)),
                ("\u{E711}", Hit::FavRemove(i)),
            ]);
            y += ROW_H;
        }
        y + 14
    }
}

/// 重新计算内容区：可点击区域、总高度和输入框位置。
fn relayout() {
    let Some(content) = SUI.with(|c| c.borrow().as_ref().map(|u| u.content)) else {
        return;
    };
    let mut rc = RECT::default();
    let _ = unsafe { GetClientRect(content, &mut rc) };
    // 先按最新状态算出总高度，把滚动位置钳到有效范围，再算一次位置
    let total = SUI.with(|c| {
        let b = c.borrow();
        let u = b.as_ref()?;
        let mut walk = Walk { u, hdc: None, t: theme::current(), width: rc.right, hits: Vec::new(), edit_rects: [RECT::default(); 3] };
        Some(unsafe { walk.run() } * u.dpi / 96)
    });
    let Some(total_h) = total else {
        return;
    };
    SUI.with(|c| {
        if let Some(u) = c.borrow_mut().as_mut() {
            u.total_h = total_h;
            u.scroll = u.scroll.clamp(0, (total_h - rc.bottom).max(0));
        }
    });
    let placed = SUI.with(|c| {
        let b = c.borrow();
        let u = b.as_ref()?;
        let mut walk = Walk { u, hdc: None, t: theme::current(), width: rc.right, hits: Vec::new(), edit_rects: [RECT::default(); 3] };
        unsafe { walk.run() };
        Some((walk.hits, walk.edit_rects, u.edits))
    });
    let Some((hits, rects, edits)) = placed else {
        return;
    };
    SUI.with(|c| {
        if let Some(u) = c.borrow_mut().as_mut() {
            u.hits = hits;
        }
    });
    for (edit, r) in edits.iter().zip(rects) {
        let _ = unsafe { MoveWindow(*edit, r.left, r.top, r.right - r.left, r.bottom - r.top, true) };
    }
}

fn invalidate_content() {
    relayout();
    if let Some(content) = SUI.with(|c| c.borrow().as_ref().map(|u| u.content)) {
        let _ = unsafe { InvalidateRect(Some(content), None, false) };
    }
}

fn scroll_by(delta: i32) {
    SUI.with(|c| {
        if let Some(u) = c.borrow_mut().as_mut() {
            u.scroll += delta;
        }
    });
    invalidate_content();
}

unsafe fn paint_content(hwnd: HWND) {
    let mut ps = PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut ps);
    let mut rc = RECT::default();
    let _ = GetClientRect(hwnd, &mut rc);
    let t = theme::current();
    paint::buffered(hdc, rc, |mem, lrc| unsafe {
        FillRect(mem, &lrc, theme::solid_brush(t.bg));
        SUI.with(|c| {
            let b = c.borrow();
            let Some(u) = b.as_ref() else {
                return;
            };
            let mut walk = Walk { u, hdc: Some(mem), t, width: lrc.right, hits: Vec::new(), edit_rects: [RECT::default(); 3] };
            walk.run();
            let sc = |v: i32| v * u.dpi / 96;
            if let Some((top, h)) = scroll_thumb(u.total_h, lrc.bottom, u.scroll as f32, sc(28)) {
                let w = sc(5);
                let x = lrc.right - w - sc(3);
                let thumb = RECT { left: x, top: top + sc(2), right: x + w, bottom: top + h - sc(2) };
                paint::round_rect(mem, thumb, w, t.thumb, None, t.bg);
            }
        });
    });
    let _ = EndPaint(hwnd, &ps);
}

/// 底部按钮区。
unsafe fn paint_foot(hwnd: HWND) {
    let mut ps = PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut ps);
    let mut rc = RECT::default();
    let _ = GetClientRect(hwnd, &mut rc);
    let t = theme::current();
    let state = SUI.with(|c| {
        c.borrow().as_ref().map(|u| (u.dpi, u.fonts, u.foot_hint.clone(), u.hover))
    });
    if let Some((dpi, fonts, hint, hover)) = state {
        let sc = |v: i32| v * dpi / 96;
        let foot_h = sc(FOOT_H);
        let area = RECT { top: rc.bottom - foot_h, ..rc };
        paint::buffered(hdc, area, |mem, lrc| unsafe {
            FillRect(mem, &lrc, theme::solid_brush(t.alt_bg));
            let line = RECT { bottom: lrc.top + 1, ..lrc };
            FillRect(mem, &line, theme::solid_brush(t.border));
            let flags = DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX;
            let (bw, bh) = (sc(84), sc(32));
            let by = (foot_h - bh) / 2;
            let cancel = RECT { left: lrc.right - sc(SIDE) - bw, top: by, right: lrc.right - sc(SIDE), bottom: by + bh };
            let save = RECT { left: cancel.left - sc(8) - bw, right: cancel.left - sc(8), ..cancel };
            let cancel_fill = if hover == Some(Hit::Cancel) { t.sel_bg } else { t.bg };
            paint::round_rect(mem, cancel, sc(RADIUS), cancel_fill, Some(t.border), t.alt_bg);
            let mut r = cancel;
            text(mem, "取消", &mut r, t.fg, fonts.body, flags | DT_CENTER);
            paint::round_rect(mem, save, sc(RADIUS), t.accent, None, t.alt_bg);
            let mut r = save;
            text(mem, "保存", &mut r, t.accent_fg, fonts.body, flags | DT_CENTER);
            let mut r = RECT { left: sc(SIDE), top: 0, right: save.left - sc(12), bottom: foot_h };
            match &hint {
                Some(h) => text(mem, h, &mut r, t.err, fonts.small, flags | DT_LEFT | DT_END_ELLIPSIS),
                None => text(mem, concat!("SaveSearch v", env!("CARGO_PKG_VERSION")), &mut r, t.path_fg, fonts.small, flags | DT_LEFT),
            }
            // 命中区域要用窗口坐标
            let to_window = |r: RECT| RECT { top: r.top + area.top, bottom: r.bottom + area.top, ..r };
            let hits = vec![(to_window(save), Hit::Save), (to_window(cancel), Hit::Cancel)];
            SUI.with(|c| {
                if let Some(u) = c.borrow_mut().as_mut() {
                    u.foot_hits = hits;
                }
            });
        });
    }
    let _ = EndPaint(hwnd, &ps);
}

fn set_foot_hint(s: Option<String>) {
    let hwnd = SUI.with(|c| {
        c.borrow_mut().as_mut().map(|u| {
            u.foot_hint = s;
            u.hwnd
        })
    });
    if let Some(hwnd) = hwnd {
        let _ = unsafe { InvalidateRect(Some(hwnd), None, false) };
    }
}

/// 校验并保存。有错误时在对应输入框下方显示提示，窗口保持打开。
fn apply_and_close() {
    let Some((hwnd, edits, s)) = SUI.with(|c| {
        let b = c.borrow();
        let u = b.as_ref()?;
        let indexable = u.drives.iter().filter(|d| d.indexable).count();
        let mut drives: Vec<char> = u.drives.iter().filter(|d| d.checked).map(|d| d.letter).collect();
        // 全选 = 视为全部（空）
        if drives.len() == indexable {
            drives.clear();
        }
        let s = ss_config::Settings {
            indexed_drives: drives,
            preset_folders: u.presets.clone(),
            exclude_dirs: u.exclude.clone(),
            hide_system: u.hide_system,
            autostart: u.autostart,
            popup_enabled: u.popup,
            popup_show_favorites: u.show_favorites,
            popup_show_recent: u.show_recent,
            popup_show_open: u.show_open,
            // 设置页不编辑的字段保持原值
            ..ss_config::load_settings()
        };
        Some((u.hwnd, u.edits, s))
    }) else {
        return;
    };
    let hotkey_text = get_text(edits[0]).trim().to_string();
    let limit = parse_result_limit(&get_text(edits[1]));
    let recent = parse_recent_max(&get_text(edits[2]));
    let hotkey = parse_hotkey(&hotkey_text);
    let mut hotkey_hint = hotkey.is_none().then(|| "格式不对，例如 Alt+Space、Ctrl+Shift+K 或 F12。".to_string());
    // 只有其他项都合法时才真正切换热键，避免保存失败却已经换了键
    if let (Some(h), Ok(_), Ok(_)) = (hotkey, &limit, &recent) {
        if !crate::apply_hotkey(h) {
            hotkey_hint = Some("这个组合已被其他程序占用，请换一个。".to_string());
        }
    }
    let invalid = hotkey_hint.is_some() || limit.is_err() || recent.is_err();
    SUI.with(|c| {
        if let Some(u) = c.borrow_mut().as_mut() {
            u.hotkey_hint = hotkey_hint;
            u.limit_hint = limit.as_ref().err().cloned();
            u.recent_hint = recent.as_ref().err().cloned();
        }
    });
    if invalid {
        invalidate_content();
        return;
    }
    let (Ok(result_limit), Ok(recent_max)) = (limit, recent) else {
        return;
    };
    let s = ss_config::Settings { result_limit, recent_max, hotkey: hotkey_text, ..s };
    let autostart_changed = ss_config::load_settings().autostart != s.autostart;
    let saved = ss_config::try_save_settings(&s);
    crate::searchwin::refresh_scopes(); // 预设文件夹和排除目录变更后刷新
    // schtasks 要起子进程、数百毫秒；未改动时不跑，避免每次保存都卡一下
    if autostart_changed {
        crate::set_autostart(s.autostart);
    }
    if let Err(e) = saved {
        set_foot_hint(Some(format!("设置已生效，但写入文件失败：{e}")));
        return;
    }
    let _ = unsafe { DestroyWindow(hwnd) };
}

/// 调整收藏夹后立即写入并刷新列表。
fn after_favorite_change(result: std::io::Result<bool>) {
    if let Err(e) = result {
        set_foot_hint(Some(format!("收藏夹写入文件失败：{e}")));
    }
    let now = ss_config::favorites();
    SUI.with(|c| {
        if let Some(u) = c.borrow_mut().as_mut() {
            u.favorites = now;
        }
    });
    invalidate_content();
}

fn favorite_at(i: usize) -> Option<String> {
    SUI.with(|c| c.borrow().as_ref().and_then(|u| u.favorites.get(i).cloned()))
}

/// 响应点击。
unsafe fn run_hit(hit: Hit) {
    match hit {
        Hit::Flag(f) => {
            SUI.with(|c| {
                if let Some(u) = c.borrow_mut().as_mut() {
                    let v = u.flag_mut(f);
                    *v = !*v;
                }
            });
            invalidate_content();
        }
        Hit::Drive(i) => {
            SUI.with(|c| {
                if let Some(d) = c.borrow_mut().as_mut().and_then(|u| u.drives.get_mut(i)) {
                    d.checked = !d.checked;
                }
            });
            invalidate_content();
        }
        Hit::Rebuild => {
            let letters = crate::CATALOG
                .get()
                .and_then(|cat| cat.read().as_ref().map(|c| c.drive_letters()))
                .unwrap_or_default();
            let msg = if letters.is_empty() {
                "索引还没有建立完成。"
            } else if crate::rebuild_drives(letters) {
                "已开始重建索引，搜索不受影响。"
            } else {
                "正在重建索引，请稍候。"
            };
            set_foot_hint(Some(msg.to_string()));
        }
        Hit::Add(which) => {
            let Some(hwnd) = SUI.with(|c| c.borrow().as_ref().map(|u| u.hwnd)) else {
                return;
            };
            if let Some(path) = pick_folder(hwnd) {
                SUI.with(|c| {
                    if let Some(u) = c.borrow_mut().as_mut() {
                        let list = u.folders_mut(which);
                        if !list.iter().any(|p| p.eq_ignore_ascii_case(&path)) {
                            list.push(path);
                        }
                    }
                });
                invalidate_content();
            }
        }
        Hit::Remove(which, i) => {
            SUI.with(|c| {
                if let Some(u) = c.borrow_mut().as_mut() {
                    let list = u.folders_mut(which);
                    if i < list.len() {
                        list.remove(i);
                    }
                }
            });
            invalidate_content();
        }
        Hit::FavUp(i) => {
            if let Some(p) = favorite_at(i) {
                after_favorite_change(ss_config::move_favorite(&p, -1));
            }
        }
        Hit::FavDown(i) => {
            if let Some(p) = favorite_at(i) {
                after_favorite_change(ss_config::move_favorite(&p, 1));
            }
        }
        Hit::FavRemove(i) => {
            if let Some(p) = favorite_at(i) {
                ss_config::remove_favorite(&p);
                after_favorite_change(Ok(true));
            }
        }
        Hit::Save => apply_and_close(),
        Hit::Cancel => {
            if let Some(h) = SUI.with(|c| c.borrow().as_ref().map(|u| u.hwnd)) {
                let _ = DestroyWindow(h);
            }
        }
    }
}

fn hit_at(hits: &[(RECT, Hit)], x: i32, y: i32) -> Option<Hit> {
    hits.iter()
        .find(|(r, _)| x >= r.left && x < r.right && y >= r.top && y < r.bottom)
        .map(|(_, h)| *h)
}

fn xy(lp: LPARAM) -> (i32, i32) {
    ((lp.0 & 0xFFFF) as i16 as i32, ((lp.0 >> 16) & 0xFFFF) as i16 as i32)
}

/// 更新鼠标悬停的控件，有变化时重绘。
unsafe fn update_hover(hwnd: HWND, hit: Option<Hit>) {
    let changed = SUI.with(|c| {
        c.borrow_mut().as_mut().is_some_and(|u| {
            let changed = u.hover != hit;
            u.hover = hit;
            changed
        })
    });
    if changed {
        let mut tme = TRACKMOUSEEVENT {
            cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
            dwFlags: TME_LEAVE,
            hwndTrack: hwnd,
            dwHoverTime: 0,
        };
        let _ = TrackMouseEvent(&mut tme);
        let _ = InvalidateRect(Some(hwnd), None, false);
    }
}

unsafe fn on_dpi_changed(hwnd: HWND, wp: WPARAM, lp: LPARAM) {
    let new_dpi = ((wp.0 & 0xFFFF) as i32).max(96);
    let fonts = make_fonts(new_dpi);
    let old = SUI.with(|c| {
        c.borrow_mut().as_mut().map(|u| {
            let old = u.fonts;
            u.scroll = u.scroll * new_dpi / u.dpi.max(1);
            u.fonts = fonts;
            u.dpi = new_dpi;
            (old, u.edits)
        })
    });
    let Some((old, edits)) = old else {
        return;
    };
    for e in edits {
        send(e, WM_SETFONT, fonts.body.0 as usize, 1);
    }
    delete_fonts(old);
    let r = &*(lp.0 as *const RECT);
    let _ = MoveWindow(hwnd, r.left, r.top, r.right - r.left, r.bottom - r.top, true);
    layout_content();
    invalidate_content();
    let _ = InvalidateRect(Some(hwnd), None, false);
}

/// 滚动条拖动：把滑块移到鼠标处。
unsafe fn drag_to(y: i32, grab: i32) {
    SUI.with(|c| {
        if let Some(u) = c.borrow_mut().as_mut() {
            let mut rc = RECT::default();
            let _ = GetClientRect(u.content, &mut rc);
            let min_h = 28 * u.dpi / 96;
            if let Some((_, h)) = scroll_thumb(u.total_h, rc.bottom, 0.0, min_h) {
                u.scroll = scroll_from_thumb(u.total_h, rc.bottom, h, y - grab) as i32;
            }
        }
    });
    invalidate_content();
}

unsafe extern "system" fn content_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => {
            paint_content(hwnd);
            LRESULT(0)
        }
        WM_CTLCOLOREDIT => {
            let t = theme::current();
            let hdc = HDC(wp.0 as *mut c_void);
            SetTextColor(hdc, COLORREF(t.fg));
            SetBkColor(hdc, COLORREF(t.bg));
            LRESULT(theme::solid_brush(t.bg).0 as isize)
        }
        WM_MOUSEWHEEL => {
            let delta = ((wp.0 >> 16) & 0xFFFF) as i16 as i32;
            let dpi = SUI.with(|c| c.borrow().as_ref().map_or(96, |u| u.dpi));
            scroll_by(-delta * 72 * dpi / 96 / 120);
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            let (x, y) = xy(lp);
            let _ = SetFocus(Some(hwnd));
            let mut rc = RECT::default();
            let _ = GetClientRect(hwnd, &mut rc);
            let thumb = SUI.with(|c| {
                c.borrow().as_ref().and_then(|u| {
                    let in_bar = x >= rc.right - 12 * u.dpi / 96;
                    in_bar.then(|| scroll_thumb(u.total_h, rc.bottom, u.scroll as f32, 28 * u.dpi / 96)).flatten()
                })
            });
            if let Some((top, h)) = thumb {
                let grab = if y >= top && y < top + h { y - top } else { h / 2 };
                SUI.with(|c| {
                    if let Some(u) = c.borrow_mut().as_mut() {
                        u.drag_grab = Some(grab);
                    }
                });
                SetCapture(hwnd);
                drag_to(y, grab);
                return LRESULT(0);
            }
            let hit = SUI.with(|c| c.borrow().as_ref().and_then(|u| hit_at(&u.hits, x, y)));
            if let Some(hit) = hit {
                run_hit(hit);
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let dragging = SUI.with(|c| c.borrow_mut().as_mut().is_some_and(|u| u.drag_grab.take().is_some()));
            if dragging {
                let _ = ReleaseCapture();
            }
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let (x, y) = xy(lp);
            if let Some(grab) = SUI.with(|c| c.borrow().as_ref().and_then(|u| u.drag_grab)) {
                drag_to(y, grab);
                return LRESULT(0);
            }
            let hit = SUI.with(|c| c.borrow().as_ref().and_then(|u| hit_at(&u.hits, x, y)));
            update_hover(hwnd, hit);
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            update_hover(hwnd, None);
            LRESULT(0)
        }
        WM_SIZE => {
            relayout();
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => {
            paint_foot(hwnd);
            LRESULT(0)
        }
        WM_MOUSEWHEEL => {
            if let Some(content) = SUI.with(|c| c.borrow().as_ref().map(|u| u.content)) {
                send(content, msg, wp.0, lp.0);
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let (x, y) = xy(lp);
            if let Some(hit) = SUI.with(|c| c.borrow().as_ref().and_then(|u| hit_at(&u.foot_hits, x, y))) {
                run_hit(hit);
            }
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let (x, y) = xy(lp);
            let hit = SUI.with(|c| c.borrow().as_ref().and_then(|u| hit_at(&u.foot_hits, x, y)));
            update_hover(hwnd, hit);
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            update_hover(hwnd, None);
            LRESULT(0)
        }
        WM_DPICHANGED => {
            on_dpi_changed(hwnd, wp, lp);
            LRESULT(0)
        }
        WM_CLOSE => {
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            // 回收窗口字体：每次打开设置都会新建，不删会泄漏 GDI 句柄
            if let Some(u) = SUI.with(|c| c.borrow_mut().take()) {
                delete_fonts(u.fonts);
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

/// 主消息循环里的键盘和滚轮处理。返回 true 表示消息已处理。
pub fn pretranslate(msg: &MSG) -> bool {
    let Some((hwnd, content, edits)) = SUI.with(|c| c.borrow().as_ref().map(|u| (u.hwnd, u.content, u.edits))) else {
        return false;
    };
    unsafe {
        if msg.message == WM_MOUSEWHEEL {
            // 滚轮消息可能发给有焦点的输入框，这里按鼠标位置转给内容区
            let under = WindowFromPoint(msg.pt);
            if under == content || edits.contains(&under) {
                send(content, WM_MOUSEWHEEL, msg.wParam.0, msg.lParam.0);
                return true;
            }
            return false;
        }
        if msg.message != WM_KEYDOWN {
            return false;
        }
        let focus = GetFocus();
        if focus != content && !edits.contains(&focus) && focus != hwnd {
            return false;
        }
        let vk = msg.wParam.0 as u16;
        if vk == VK_ESCAPE.0 {
            let _ = DestroyWindow(hwnd);
            return true;
        }
        if vk == VK_RETURN.0 {
            apply_and_close();
            return true;
        }
        if vk == VK_TAB.0 {
            let shift = GetKeyState(VK_SHIFT.0 as i32) < 0;
            let current = edits.iter().position(|e| *e == focus);
            let next = match (current, shift) {
                (Some(i), false) => (i + 1) % edits.len(),
                (Some(i), true) => (i + edits.len() - 1) % edits.len(),
                (None, _) => 0,
            };
            let _ = SetFocus(Some(edits[next]));
            send(edits[next], EM_SETSEL, 0, -1);
            return true;
        }
    }
    false
}
