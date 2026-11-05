//! 设置窗口。
//!
//! 收藏夹的排序和删除立即生效，其余内容在点“保存”时校验并写入。
//! 盘符改动重启后生效；开机自启和热键保存时立即应用。

use std::cell::RefCell;
use std::ffi::c_void;

use ss_core::{DriveState, DriveStatus, VolumeInfo};
use ss_ui_model::{index_stats, parse_hotkey, parse_recent_max, parse_result_limit};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateFontW, DeleteObject, FillRect, MapWindowPoints, SetBkMode, SetTextColor,
    CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, HDC, HFONT, HGDIOBJ,
    OUT_DEFAULT_PRECIS, TRANSPARENT,
};
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_INPROC_SERVER};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
use windows::Win32::UI::Shell::{FileOpenDialog, IFileOpenDialog, FOS_PICKFOLDERS, SIGDN_FILESYSPATH};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::theme;

const ID_AUTOSTART: u32 = 1002;
const ID_LIMIT: u32 = 1003;
const ID_RECENT: u32 = 1004;
const ID_POPUP: u32 = 1005;
const ID_FAV: u32 = 1006;
const ID_RECENTSHOW: u32 = 1007;
const ID_OPEN: u32 = 1008;
const ID_PRESET_LIST: u32 = 1009;
const ID_PRESET_ADD: u32 = 1010;
const ID_PRESET_DEL: u32 = 1011;
const ID_HOTKEY: u32 = 1012;
const ID_HIDE_SYSTEM: u32 = 1013;
const ID_EXCLUDE_LIST: u32 = 1014;
const ID_EXCLUDE_ADD: u32 = 1015;
const ID_EXCLUDE_DEL: u32 = 1016;
const ID_FAV_LIST: u32 = 1017;
const ID_FAV_UP: u32 = 1018;
const ID_FAV_DOWN: u32 = 1019;
const ID_FAV_DEL: u32 = 1020;
const ID_REBUILD: u32 = 1021;
const ID_DRIVE_BASE: u32 = 1100;
const IDOK: u32 = 1;
const IDCANCEL: u32 = 2;

const BM_GETCHECK: u32 = 0x00F0;
const BM_SETCHECK: u32 = 0x00F1;

const WINDOW_W: i32 = 760;
const WINDOW_H: i32 = 600;
/// 左右两栏的起点和宽度（96 DPI）。
const LEFT_X: i32 = 16;
const RIGHT_X: i32 = 388;
const COL_W: i32 = 340;

mod st {
    pub const WS_CHILD: u32 = 0x4000_0000;
    pub const WS_VISIBLE: u32 = 0x1000_0000;
    pub const WS_TABSTOP: u32 = 0x0001_0000;
    pub const WS_VSCROLL: u32 = 0x0020_0000;
    pub const BS_AUTOCHECKBOX: u32 = 0x0003;
    pub const BS_DEFPUSHBUTTON: u32 = 0x0001;
    pub const LBS_NOTIFY: u32 = 0x0001;
    pub const LBS_HASSTRINGS: u32 = 0x0040;
    pub const LBS_NOINTEGRALHEIGHT: u32 = 0x0100;
    pub const ES_NUMBER: u32 = 0x2000;
    pub const ES_AUTOHSCROLL: u32 = 0x0080;
    pub const SS_ENDELLIPSIS: u32 = 0x4000;
    pub const SS_NOPREFIX: u32 = 0x0080;
    pub const WS_EX_CLIENTEDGE: u32 = 0x0000_0200;
    pub const WS_OVERLAPPED_DLG: u32 = 0x00CC_0000; // caption | sysmenu | border
}

struct SUi {
    hwnd: HWND,
    autostart: HWND,
    limit: HWND,
    recent: HWND,
    popup: HWND,
    fav: HWND,
    recent_show: HWND,
    open: HWND,
    hide_system: HWND,
    hotkey: HWND,
    preset_list: HWND,
    exclude_list: HWND,
    fav_list: HWND,
    hint: HWND,
    stats: HWND,
    drives: Vec<(char, HWND)>,
    font: HFONT,
    dpi: i32,
}

thread_local! {
    static SUI: RefCell<Option<SUi>> = const { RefCell::new(None) };
}

fn send(hwnd: HWND, msg: u32, w: usize, l: isize) -> isize {
    unsafe { SendMessageW(hwnd, msg, Some(WPARAM(w)), Some(LPARAM(l))).0 }
}

unsafe fn make_font(px: i32) -> HFONT {
    CreateFontW(
        -px, 0, 0, 0, 400, 0, 0, 0, DEFAULT_CHARSET, OUT_DEFAULT_PRECIS, CLIP_DEFAULT_PRECIS,
        CLEARTYPE_QUALITY, 0, w!("Segoe UI"),
    )
}

fn checked(hwnd: HWND) -> bool {
    send(hwnd, BM_GETCHECK, 0, 0) == 1
}
fn set_checked(hwnd: HWND, v: bool) {
    send(hwnd, BM_SETCHECK, usize::from(v), 0);
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
fn set_text(hwnd: HWND, s: &str) {
    let w = crate::wide(s);
    unsafe {
        let _ = SetWindowTextW(hwnd, PCWSTR(w.as_ptr()));
    }
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

/// 读取列表框全部条目。
fn listbox_items(list: HWND) -> Vec<String> {
    let mut out = Vec::new();
    let count = send(list, LB_GETCOUNT, 0, 0);
    for i in 0..count.max(0) {
        let len = send(list, LB_GETTEXTLEN, i as usize, 0);
        if len <= 0 {
            continue;
        }
        let mut buf = vec![0u16; len as usize + 1];
        let n = send(list, LB_GETTEXT, i as usize, buf.as_mut_ptr() as isize);
        if n > 0 {
            out.push(String::from_utf16_lossy(&buf[..(n as usize).min(buf.len())]));
        }
    }
    out
}

fn fill_list(list: HWND, items: &[String]) {
    send(list, LB_RESETCONTENT, 0, 0);
    for item in items {
        let w = crate::wide(item);
        send(list, LB_ADDSTRING, 0, w.as_ptr() as isize);
    }
}

/// 打开设置窗口（若已开则前置）。
pub fn open() {
    if let Some(h) = SUI.with(|c| c.borrow().as_ref().map(|u| u.hwnd)) {
        unsafe {
            let _ = ShowWindow(h, SW_SHOW);
            let _ = SetForegroundWindow(h);
        }
        refresh_stats();
        return;
    }
    let _ = unsafe { create() };
}

/// 一个盘的状态文字。
fn drive_status_text(v: &VolumeInfo, statuses: Option<&[DriveStatus]>) -> String {
    if !v.indexable {
        let reason = v.reason.as_deref().unwrap_or("不支持");
        return format!("不能索引：{reason}");
    }
    let state = match statuses {
        None => "索引中".to_string(),
        Some(all) => match all.iter().find(|s| s.letter == v.letter) {
            None => "未索引，重启后生效".to_string(),
            Some(s) => match (&s.state, &s.last_error) {
                (DriveState::Failed { reason }, _) => format!("失败：{reason}"),
                (DriveState::Ready, Some(e)) => format!("已就绪，{e}"),
                (DriveState::Ready, None) => "已就绪".to_string(),
                _ => "索引中".to_string(),
            },
        },
    };
    format!("{} · {state}", v.file_system)
}

/// 重新显示索引统计。
fn refresh_stats() {
    let Some(stats) = SUI.with(|c| c.borrow().as_ref().map(|u| u.stats)) else {
        return;
    };
    let text = match crate::CATALOG.get().and_then(|cat| {
        cat.read().as_ref().map(|c| (c.total_nodes(), c.memory_bytes()))
    }) {
        Some((nodes, memory)) => index_stats(nodes, memory, crate::synced_secs_ago()),
        None => "索引建立中".to_string(),
    };
    set_text(stats, &text);
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

    let hwnd = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        class,
        w!("SaveSearch 设置"),
        WINDOW_STYLE(st::WS_OVERLAPPED_DLG),
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
    let font = make_font(sc(14));

    // 窗口按 DPI 放大，并居中。盘多于 3 个时加高，避免压到底部按钮。
    let volumes = ss_core::list_volumes();
    let window_h = WINDOW_H + (volumes.len() as i32 - 3).max(0) * 24;
    let (ww, wh) = (sc(WINDOW_W), sc(window_h));
    let sw = GetSystemMetrics(SM_CXSCREEN);
    let sh = GetSystemMetrics(SM_CYSCREEN);
    let _ = MoveWindow(hwnd, (sw - ww) / 2, ((sh - wh) / 3).max(0), ww, wh, false);

    let s = ss_config::load_settings();

    let mk = |class: PCWSTR, text: &str, style: u32, ex: u32, id: u32, x: i32, y: i32, w: i32, h: i32| -> HWND {
        let wl = crate::wide(text);
        let handle = CreateWindowExW(
            WINDOW_EX_STYLE(ex),
            class,
            PCWSTR(wl.as_ptr()),
            WINDOW_STYLE(st::WS_CHILD | st::WS_VISIBLE | style),
            sc(x),
            sc(y),
            sc(w),
            sc(h),
            Some(hwnd),
            Some(HMENU(id as usize as *mut c_void)),
            Some(hinstance),
            None,
        )
        .unwrap_or(HWND(std::ptr::null_mut()));
        send(handle, WM_SETFONT, font.0 as usize, 1);
        handle
    };
    let label = |text: &str, x: i32, y: i32, w: i32| -> HWND {
        mk(w!("STATIC"), text, st::SS_ENDELLIPSIS | st::SS_NOPREFIX, 0, 0, x, y, w, 20)
    };
    let check = |text: &str, id: u32, x: i32, y: i32, w: i32, value: bool| -> HWND {
        let h = mk(w!("BUTTON"), text, st::WS_TABSTOP | st::BS_AUTOCHECKBOX, 0, id, x, y, w, 22);
        set_checked(h, value);
        h
    };
    let button = |text: &str, id: u32, x: i32, y: i32, w: i32| -> HWND {
        mk(w!("BUTTON"), text, st::WS_TABSTOP, 0, id, x, y, w, 26)
    };
    let edit = |text: &str, style: u32, id: u32, x: i32, y: i32, w: i32| -> HWND {
        mk(w!("EDIT"), text, st::WS_TABSTOP | st::ES_AUTOHSCROLL | style, st::WS_EX_CLIENTEDGE, id, x, y, w, 24)
    };
    let listbox = |id: u32, x: i32, y: i32, w: i32, h: i32, items: &[String]| -> HWND {
        let style = st::WS_TABSTOP
            | st::WS_VSCROLL
            | st::LBS_NOTIFY
            | st::LBS_HASSTRINGS
            | st::LBS_NOINTEGRALHEIGHT;
        let list = mk(w!("LISTBOX"), "", style, st::WS_EX_CLIENTEDGE, id, x, y, w, h);
        fill_list(list, items);
        list
    };

    // ---- 左栏：索引 ----
    let mut y = 12;
    label("索引的磁盘（改动重启后生效）", LEFT_X, y, COL_W);
    y += 22;
    let statuses = crate::CATALOG
        .get()
        .and_then(|cat| cat.read().as_ref().map(|c| c.drive_statuses()));
    let mut drives = Vec::new();
    for (i, v) in volumes.iter().enumerate() {
        let title = if v.label.is_empty() {
            format!("{}:", v.letter)
        } else {
            format!("{}: {}", v.letter, v.label)
        };
        let selected = v.indexable && (s.indexed_drives.is_empty() || s.indexed_drives.contains(&v.letter));
        let cb = check(&title, ID_DRIVE_BASE + i as u32, LEFT_X, y, 130, selected);
        if v.indexable {
            drives.push((v.letter, cb));
        } else {
            let _ = EnableWindow(cb, false);
        }
        label(&drive_status_text(v, statuses.as_deref()), LEFT_X + 134, y + 2, COL_W - 134);
        y += 24;
    }
    y += 4;
    let stats = label("", LEFT_X, y + 4, COL_W - 92);
    button("重建索引", ID_REBUILD, LEFT_X + COL_W - 86, y, 86);
    y += 36;

    label("排除目录（不搜索这些目录）", LEFT_X, y, COL_W);
    y += 22;
    let exclude_list = listbox(ID_EXCLUDE_LIST, LEFT_X, y, COL_W - 92, 84, &s.exclude_dirs);
    button("添加…", ID_EXCLUDE_ADD, LEFT_X + COL_W - 86, y, 86);
    button("删除", ID_EXCLUDE_DEL, LEFT_X + COL_W - 86, y + 30, 86);
    y += 90;
    let hide_system = check("不显示隐藏和系统文件", ID_HIDE_SYSTEM, LEFT_X, y, COL_W, s.hide_system);
    y += 34;

    label("预设文件夹（搜索范围可选）", LEFT_X, y, COL_W);
    y += 22;
    let preset_list = listbox(ID_PRESET_LIST, LEFT_X, y, COL_W - 92, 84, &s.preset_folders);
    button("添加…", ID_PRESET_ADD, LEFT_X + COL_W - 86, y, 86);
    button("删除", ID_PRESET_DEL, LEFT_X + COL_W - 86, y + 30, 86);

    // ---- 右栏：搜索和浮窗 ----
    let mut y = 12;
    label("呼出热键", RIGHT_X, y + 3, 80);
    let hotkey = edit(&s.hotkey, 0, ID_HOTKEY, RIGHT_X + 84, y, COL_W - 84);
    y += 32;
    label("结果上限", RIGHT_X, y + 3, 80);
    let limit = edit(&s.result_limit.to_string(), st::ES_NUMBER, ID_LIMIT, RIGHT_X + 84, y, 70);
    label("最近条数", RIGHT_X + 180, y + 3, 80);
    let recent = edit(&s.recent_max.to_string(), st::ES_NUMBER, ID_RECENT, RIGHT_X + 264, y, 70);
    y += 34;
    let autostart = check("开机自启（计划任务，免 UAC）", ID_AUTOSTART, RIGHT_X, y, COL_W, s.autostart);
    y += 32;

    let popup = check("启用保存对话框浮窗", ID_POPUP, RIGHT_X, y, COL_W, s.popup_enabled);
    y += 24;
    let fav = check("显示收藏", ID_FAV, RIGHT_X + 18, y, 90, s.popup_show_favorites);
    let recent_show = check("显示最近", ID_RECENTSHOW, RIGHT_X + 112, y, 90, s.popup_show_recent);
    let open = check("显示已打开", ID_OPEN, RIGHT_X + 206, y, 110, s.popup_show_open);
    y += 34;

    label("收藏夹（立即生效）", RIGHT_X, y, COL_W);
    y += 22;
    let fav_list = listbox(ID_FAV_LIST, RIGHT_X, y, COL_W - 92, 170, &ss_config::favorites());
    button("上移", ID_FAV_UP, RIGHT_X + COL_W - 86, y, 86);
    button("下移", ID_FAV_DOWN, RIGHT_X + COL_W - 86, y + 30, 86);
    button("删除", ID_FAV_DEL, RIGHT_X + COL_W - 86, y + 60, 86);

    // ---- 底部 ----
    let bottom = window_h - 84;
    let hint = label("", LEFT_X, bottom - 2, COL_W * 2 - 150);
    label(concat!("SaveSearch v", env!("CARGO_PKG_VERSION")), LEFT_X, bottom + 18, 200);
    mk(w!("BUTTON"), "保存", st::WS_TABSTOP | st::BS_DEFPUSHBUTTON, 0, IDOK, RIGHT_X + 150, bottom + 8, 90, 30);
    button("取消", IDCANCEL, RIGHT_X + 248, bottom + 8, 90);

    SUI.with(|c| {
        *c.borrow_mut() = Some(SUi {
            hwnd,
            autostart,
            limit,
            recent,
            popup,
            fav,
            recent_show,
            open,
            hide_system,
            hotkey,
            preset_list,
            exclude_list,
            fav_list,
            hint,
            stats,
            drives,
            font,
            dpi,
        });
    });
    refresh_stats();
    if crate::hotkey_failed() {
        set_hint(&format!("热键 {} 已被其他程序占用，请换一个。", s.hotkey));
    }

    theme::apply_window(hwnd);
    let _ = ShowWindow(hwnd, SW_SHOW);
    let _ = SetForegroundWindow(hwnd);
    Ok(())
}

fn set_hint(text: &str) {
    if let Some(h) = SUI.with(|c| c.borrow().as_ref().map(|u| u.hint)) {
        set_text(h, text);
    }
}

/// 校验并保存。有错误时显示提示并返回，窗口保持打开。
fn apply_and_close() {
    let Some((hwnd, hotkey_edit, limit, recent, s)) = SUI.with(|c| {
        let b = c.borrow();
        let u = b.as_ref()?;
        let mut drives: Vec<char> = u.drives.iter().filter(|(_, h)| checked(*h)).map(|(d, _)| *d).collect();
        // 全选 = 视为全部（空）
        if drives.len() == u.drives.len() {
            drives.clear();
        }
        let s = ss_config::Settings {
            indexed_drives: drives,
            preset_folders: listbox_items(u.preset_list),
            exclude_dirs: listbox_items(u.exclude_list),
            hide_system: checked(u.hide_system),
            autostart: checked(u.autostart),
            popup_enabled: checked(u.popup),
            popup_show_favorites: checked(u.fav),
            popup_show_recent: checked(u.recent_show),
            popup_show_open: checked(u.open),
            // 设置页不编辑的字段保持原值
            ..ss_config::load_settings()
        };
        Some((u.hwnd, get_text(u.hotkey), get_text(u.limit), get_text(u.recent), s))
    }) else {
        return;
    };
    let result_limit = match parse_result_limit(&limit) {
        Ok(n) => n,
        Err(e) => return set_hint(&e),
    };
    let recent_max = match parse_recent_max(&recent) {
        Ok(n) => n,
        Err(e) => return set_hint(&e),
    };
    let hotkey_text = hotkey_edit.trim().to_string();
    let Some(hotkey) = parse_hotkey(&hotkey_text) else {
        return set_hint("热键格式不对，例如 Alt+Space、Ctrl+Shift+K 或 F12。");
    };
    if !crate::apply_hotkey(hotkey) {
        return set_hint("这个组合已被其他程序占用，请换一个。");
    }
    let s = ss_config::Settings { result_limit, recent_max, hotkey: hotkey_text, ..s };
    let autostart_changed = ss_config::load_settings().autostart != s.autostart;
    let saved = ss_config::try_save_settings(&s);
    crate::searchwin::refresh_scopes(); // 预设文件夹和排除目录变更后刷新
    // schtasks 要起子进程、数百毫秒；未改动时不跑，避免每次保存都卡一下
    if autostart_changed {
        crate::set_autostart(s.autostart);
    }
    if let Err(e) = saved {
        return set_hint(&format!("设置已生效，但写入文件失败：{e}"));
    }
    let _ = unsafe { DestroyWindow(hwnd) };
}

/// 向列表框添加用户选择的文件夹（去重）。
unsafe fn add_folder(hwnd: HWND, list: HWND) {
    if let Some(path) = pick_folder(hwnd) {
        if !listbox_items(list).iter().any(|p| p.eq_ignore_ascii_case(&path)) {
            let w = crate::wide(&path);
            send(list, LB_ADDSTRING, 0, w.as_ptr() as isize);
        }
    }
}

fn delete_selected(list: HWND) {
    let sel = send(list, LB_GETCURSEL, 0, 0);
    if sel >= 0 {
        send(list, LB_DELETESTRING, sel as usize, 0);
    }
}

/// 调整收藏夹：移动 `delta` 位，或在 `delta == 0` 时删除。
fn change_favorite(delta: isize) {
    let Some(list) = SUI.with(|c| c.borrow().as_ref().map(|u| u.fav_list)) else {
        return;
    };
    let sel = send(list, LB_GETCURSEL, 0, 0);
    let items = listbox_items(list);
    let Some(path) = usize::try_from(sel).ok().and_then(|i| items.get(i)) else {
        return;
    };
    let mut new_sel = sel;
    if delta == 0 {
        ss_config::remove_favorite(path);
    } else {
        match ss_config::move_favorite(path, delta) {
            Ok(true) => new_sel += delta,
            Ok(false) => return,
            Err(e) => set_hint(&format!("收藏夹写入文件失败：{e}")),
        }
    }
    let now = ss_config::favorites();
    fill_list(list, &now);
    if !now.is_empty() {
        send(list, LB_SETCURSEL, new_sel.clamp(0, now.len() as isize - 1) as usize, 0);
    }
}

/// 按新 DPI 缩放所有子控件。
unsafe extern "system" fn scale_child(child: HWND, lp: LPARAM) -> windows::core::BOOL {
    let (new, old, font) = *(lp.0 as *const (i32, i32, HFONT));
    let parent = GetParent(child).unwrap_or_default();
    let mut r = RECT::default();
    if GetWindowRect(child, &mut r).is_ok() {
        let mut pts = [POINT { x: r.left, y: r.top }, POINT { x: r.right, y: r.bottom }];
        MapWindowPoints(None, Some(parent), &mut pts);
        let _ = MoveWindow(
            child,
            pts[0].x * new / old,
            pts[0].y * new / old,
            (pts[1].x - pts[0].x) * new / old,
            (pts[1].y - pts[0].y) * new / old,
            true,
        );
    }
    send(child, WM_SETFONT, font.0 as usize, 1);
    true.into()
}

unsafe fn on_dpi_changed(hwnd: HWND, wp: WPARAM, lp: LPARAM) {
    let new_dpi = ((wp.0 & 0xFFFF) as i32).max(96);
    let Some((old_dpi, old_font)) = SUI.with(|c| c.borrow().as_ref().map(|u| (u.dpi, u.font))) else {
        return;
    };
    let font = make_font(14 * new_dpi / 96);
    let args = (new_dpi, old_dpi, font);
    let _ = EnumChildWindows(Some(hwnd), Some(scale_child), LPARAM(&args as *const _ as isize));
    let r = &*(lp.0 as *const RECT);
    let _ = MoveWindow(hwnd, r.left, r.top, r.right - r.left, r.bottom - r.top, true);
    let _ = DeleteObject(HGDIOBJ(old_font.0));
    SUI.with(|c| {
        if let Some(u) = c.borrow_mut().as_mut() {
            u.dpi = new_dpi;
            u.font = font;
        }
    });
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_CTLCOLORSTATIC | WM_CTLCOLORBTN | WM_CTLCOLOREDIT | WM_CTLCOLORLISTBOX => {
            let t = theme::current();
            let hdc = HDC(wp.0 as *mut c_void);
            let is_hint = SUI.with(|c| c.borrow().as_ref().is_some_and(|u| u.hint.0 as isize == lp.0));
            SetTextColor(hdc, COLORREF(if is_hint { t.err } else { t.fg }));
            SetBkMode(hdc, TRANSPARENT);
            LRESULT(theme::solid_brush(t.bg).0 as isize)
        }
        WM_ERASEBKGND => {
            let t = theme::current();
            let mut rc = RECT::default();
            let _ = GetClientRect(hwnd, &mut rc);
            FillRect(HDC(wp.0 as *mut c_void), &rc, theme::solid_brush(t.bg));
            LRESULT(1)
        }
        WM_DPICHANGED => {
            on_dpi_changed(hwnd, wp, lp);
            LRESULT(0)
        }
        WM_COMMAND => {
            let id = (wp.0 as u32) & 0xFFFF;
            let lists = SUI.with(|c| c.borrow().as_ref().map(|u| (u.preset_list, u.exclude_list)));
            match (id, lists) {
                (IDOK, _) => apply_and_close(),
                (IDCANCEL, _) => {
                    let _ = DestroyWindow(hwnd);
                }
                (ID_PRESET_ADD, Some((list, _))) | (ID_EXCLUDE_ADD, Some((_, list))) => add_folder(hwnd, list),
                (ID_PRESET_DEL, Some((list, _))) | (ID_EXCLUDE_DEL, Some((_, list))) => delete_selected(list),
                (ID_FAV_UP, _) => change_favorite(-1),
                (ID_FAV_DOWN, _) => change_favorite(1),
                (ID_FAV_DEL, _) => change_favorite(0),
                (ID_REBUILD, _) => {
                    let letters = crate::CATALOG
                        .get()
                        .and_then(|cat| cat.read().as_ref().map(|c| c.drive_letters()))
                        .unwrap_or_default();
                    if letters.is_empty() {
                        set_hint("索引还没有建立完成。");
                    } else if crate::rebuild_drives(letters) {
                        set_hint("已开始重建索引，搜索不受影响。");
                    } else {
                        set_hint("正在重建索引，请稍候。");
                    }
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            // 回收窗口字体：每次打开设置都会新建一只 HFONT，不删会泄漏 GDI 句柄
            if let Some(u) = SUI.with(|c| c.borrow_mut().take()) {
                let _ = DeleteObject(HGDIOBJ(u.font.0));
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}
