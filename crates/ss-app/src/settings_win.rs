//! 设置窗口：主题 / 索引盘符 / 开机自启 / 结果上限 / 最近条数 / 浮窗开关与显示项。
//! 保存即写 settings.json 并即时应用（主题立即重绘；上限/最近/浮窗项下次读取生效；
//! 盘符更改重启后生效；开机自启立即建/删计划任务）。

use std::cell::RefCell;
use std::ffi::c_void;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateFontW, DeleteObject, FillRect, SetBkMode, SetTextColor, CLEARTYPE_QUALITY,
    CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, HDC, HFONT, HGDIOBJ, OUT_DEFAULT_PRECIS, TRANSPARENT,
};
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_INPROC_SERVER};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Shell::{FileOpenDialog, IFileOpenDialog, FOS_PICKFOLDERS, SIGDN_FILESYSPATH};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::theme;

const ID_THEME: u32 = 1001;
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
const ID_DRIVE_BASE: u32 = 1100;
const IDOK: u32 = 1;
const IDCANCEL: u32 = 2;

const BM_GETCHECK: u32 = 0x00F0;
const BM_SETCHECK: u32 = 0x00F1;
const CB_ADDSTRING: u32 = 0x0143;
const CB_SETCURSEL: u32 = 0x014E;
const CB_GETCURSEL: u32 = 0x0147;

mod st {
    pub const WS_CHILD: u32 = 0x4000_0000;
    pub const WS_VISIBLE: u32 = 0x1000_0000;
    pub const WS_TABSTOP: u32 = 0x0001_0000;
    pub const WS_VSCROLL: u32 = 0x0020_0000;
    pub const BS_AUTOCHECKBOX: u32 = 0x0003;
    pub const BS_DEFPUSHBUTTON: u32 = 0x0001;
    pub const CBS_DROPDOWNLIST: u32 = 0x0003;
    pub const WS_BORDER: u32 = 0x0080_0000;
    pub const LBS_NOTIFY: u32 = 0x0001;
    pub const LBS_HASSTRINGS: u32 = 0x0040;
    pub const LBS_NOINTEGRALHEIGHT: u32 = 0x0100;
    pub const ES_NUMBER: u32 = 0x2000;
    pub const ES_AUTOHSCROLL: u32 = 0x0080;
    pub const SS_LEFT: u32 = 0x0000;
    pub const WS_EX_CLIENTEDGE: u32 = 0x0000_0200;
    pub const WS_OVERLAPPED_DLG: u32 = 0x00CC_0000; // caption | sysmenu | border
}

const THEMES: [(&str, &str); 3] = [
    ("business_light", "商务浅色"),
    ("business_dark", "商务深色"),
    ("solarized", "暖阳浅色"),
];

struct SUi {
    hwnd: HWND,
    theme_combo: HWND,
    autostart: HWND,
    limit: HWND,
    recent: HWND,
    popup: HWND,
    fav: HWND,
    recent_show: HWND,
    open: HWND,
    preset_list: HWND,
    drives: Vec<(char, HWND)>,
    font: HFONT,
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
    send(hwnd, BM_SETCHECK, if v { 1 } else { 0 }, 0);
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

/// 打开设置窗口（若已开则前置）。
pub fn open() {
    if let Some(h) = SUI.with(|c| c.borrow().as_ref().map(|u| u.hwnd)) {
        unsafe {
            let _ = ShowWindow(h, SW_SHOW);
            let _ = SetForegroundWindow(h);
        }
        return;
    }
    let _ = unsafe { create() };
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

    let sw = GetSystemMetrics(SM_CXSCREEN);
    let sh = GetSystemMetrics(SM_CYSCREEN);
    let (ww, wh) = (460, 660);
    let hwnd = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        class,
        w!("SaveSearch 设置"),
        WINDOW_STYLE(st::WS_OVERLAPPED_DLG),
        (sw - ww) / 2,
        (sh - wh) / 3,
        ww,
        wh,
        None,
        None,
        Some(hinstance),
        None,
    )?;

    let dpi = GetDpiForWindow(hwnd).max(96) as i32;
    let sc = |v: i32| v * dpi / 96;
    let font = make_font(sc(15));

    let s = ss_config::load_settings();

    let mklabel = |text: &str, x: i32, y: i32, w: i32| -> HWND {
        let wl = crate::wide(text);
        let h = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("STATIC"),
            PCWSTR(wl.as_ptr()),
            WINDOW_STYLE(st::WS_CHILD | st::WS_VISIBLE | st::SS_LEFT),
            sc(x),
            sc(y),
            sc(w),
            sc(20),
            Some(hwnd),
            None,
            Some(hinstance),
            None,
        )
        .unwrap_or(HWND(std::ptr::null_mut()));
        send(h, WM_SETFONT, font.0 as usize, 1);
        h
    };
    let mkcheck = |text: &str, id: u32, x: i32, y: i32, w: i32| -> HWND {
        let wl = crate::wide(text);
        let h = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("BUTTON"),
            PCWSTR(wl.as_ptr()),
            WINDOW_STYLE(st::WS_CHILD | st::WS_VISIBLE | st::WS_TABSTOP | st::BS_AUTOCHECKBOX),
            sc(x),
            sc(y),
            sc(w),
            sc(22),
            Some(hwnd),
            Some(HMENU(id as usize as *mut c_void)),
            Some(hinstance),
            None,
        )
        .unwrap_or(HWND(std::ptr::null_mut()));
        send(h, WM_SETFONT, font.0 as usize, 1);
        h
    };

    let mut y = 14;
    mklabel("主题", 16, y + 3, 60);
    let theme_combo = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        w!("COMBOBOX"),
        PCWSTR::null(),
        WINDOW_STYLE(st::WS_CHILD | st::WS_VISIBLE | st::WS_TABSTOP | st::WS_VSCROLL | st::CBS_DROPDOWNLIST),
        sc(90),
        sc(y),
        sc(330),
        sc(200),
        Some(hwnd),
        Some(HMENU(ID_THEME as usize as *mut c_void)),
        Some(hinstance),
        None,
    )?;
    send(theme_combo, WM_SETFONT, font.0 as usize, 1);
    for (i, (name, label)) in THEMES.iter().enumerate() {
        let wl = crate::wide(label);
        send(theme_combo, CB_ADDSTRING, 0, wl.as_ptr() as isize);
        if *name == s.theme {
            send(theme_combo, CB_SETCURSEL, i, 0);
        }
    }
    if send(theme_combo, CB_GETCURSEL, 0, 0) < 0 {
        send(theme_combo, CB_SETCURSEL, 0, 0);
    }

    y += 40;
    mklabel("索引盘符（不勾=全部 NTFS，改动重启生效）", 16, y, 400);
    y += 24;
    let mut drives = Vec::new();
    let mut x = 16;
    for (i, d) in ss_core::ntfs_fixed_drives().into_iter().enumerate() {
        let cb = mkcheck(&format!("{d}:"), ID_DRIVE_BASE + i as u32, x, y, 56);
        set_checked(cb, s.indexed_drives.is_empty() || s.indexed_drives.contains(&d));
        drives.push((d, cb));
        x += 64;
    }

    y += 34;
    let autostart = mkcheck("开机自启（计划任务，免 UAC）", ID_AUTOSTART, 16, y, 400);
    set_checked(autostart, s.autostart);

    y += 30;
    mklabel("结果上限", 16, y + 3, 70);
    let limit = CreateWindowExW(
        WINDOW_EX_STYLE(st::WS_EX_CLIENTEDGE),
        w!("EDIT"),
        PCWSTR::null(),
        WINDOW_STYLE(st::WS_CHILD | st::WS_VISIBLE | st::WS_TABSTOP | st::ES_NUMBER | st::ES_AUTOHSCROLL),
        sc(90),
        sc(y),
        sc(80),
        sc(24),
        Some(hwnd),
        Some(HMENU(ID_LIMIT as usize as *mut c_void)),
        Some(hinstance),
        None,
    )?;
    send(limit, WM_SETFONT, font.0 as usize, 1);
    set_text(limit, &s.result_limit.to_string());

    mklabel("最近条数", 220, y + 3, 70);
    let recent = CreateWindowExW(
        WINDOW_EX_STYLE(st::WS_EX_CLIENTEDGE),
        w!("EDIT"),
        PCWSTR::null(),
        WINDOW_STYLE(st::WS_CHILD | st::WS_VISIBLE | st::WS_TABSTOP | st::ES_NUMBER | st::ES_AUTOHSCROLL),
        sc(300),
        sc(y),
        sc(80),
        sc(24),
        Some(hwnd),
        Some(HMENU(ID_RECENT as usize as *mut c_void)),
        Some(hinstance),
        None,
    )?;
    send(recent, WM_SETFONT, font.0 as usize, 1);
    set_text(recent, &s.recent_max.to_string());

    y += 36;
    let popup = mkcheck("启用保存对话框浮窗", ID_POPUP, 16, y, 400);
    set_checked(popup, s.popup_enabled);
    y += 26;
    let fav = mkcheck("显示 收藏", ID_FAV, 36, y, 110);
    set_checked(fav, s.popup_show_favorites);
    let recent_show = mkcheck("显示 最近", ID_RECENTSHOW, 156, y, 110);
    set_checked(recent_show, s.popup_show_recent);
    let open = mkcheck("显示 已打开", ID_OPEN, 276, y, 130);
    set_checked(open, s.popup_show_open);

    // 预设文件夹
    y += 36;
    mklabel("预设文件夹（搜索范围下拉可选，仅搜该文件夹）", 16, y, 420);
    y += 22;
    let preset_list = CreateWindowExW(
        WINDOW_EX_STYLE(st::WS_EX_CLIENTEDGE),
        w!("LISTBOX"),
        PCWSTR::null(),
        WINDOW_STYLE(
            st::WS_CHILD
                | st::WS_VISIBLE
                | st::WS_TABSTOP
                | st::WS_VSCROLL
                | st::LBS_NOTIFY
                | st::LBS_HASSTRINGS
                | st::LBS_NOINTEGRALHEIGHT,
        ),
        sc(16),
        sc(y),
        sc(296),
        sc(96),
        Some(hwnd),
        Some(HMENU(ID_PRESET_LIST as usize as *mut c_void)),
        Some(hinstance),
        None,
    )?;
    send(preset_list, WM_SETFONT, font.0 as usize, 1);
    for f in &s.preset_folders {
        let wf = crate::wide(f);
        send(preset_list, LB_ADDSTRING, 0, wf.as_ptr() as isize);
    }
    let mkbtn = |text: &str, id: u32, bx: i32, by: i32| {
        let wl = crate::wide(text);
        let h = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("BUTTON"),
            PCWSTR(wl.as_ptr()),
            WINDOW_STYLE(st::WS_CHILD | st::WS_VISIBLE | st::WS_TABSTOP),
            sc(bx),
            sc(by),
            sc(118),
            sc(28),
            Some(hwnd),
            Some(HMENU(id as usize as *mut c_void)),
            Some(hinstance),
            None,
        )
        .unwrap_or(HWND(std::ptr::null_mut()));
        send(h, WM_SETFONT, font.0 as usize, 1);
    };
    mkbtn("添加文件夹…", ID_PRESET_ADD, 320, y);
    mkbtn("删除", ID_PRESET_DEL, 320, y + 34);
    y += 96 + 14;

    y += 44;
    // 版本号（与保存/取消同行左侧；编译期取自 workspace 版本）
    mklabel(concat!("SaveSearch v", env!("CARGO_PKG_VERSION")), 16, y + 8, 200);
    let wl_ok = crate::wide("保存");
    let ok = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        w!("BUTTON"),
        PCWSTR(wl_ok.as_ptr()),
        WINDOW_STYLE(st::WS_CHILD | st::WS_VISIBLE | st::WS_TABSTOP | st::BS_DEFPUSHBUTTON),
        sc(230),
        sc(y),
        sc(90),
        sc(30),
        Some(hwnd),
        Some(HMENU(IDOK as usize as *mut c_void)),
        Some(hinstance),
        None,
    )?;
    send(ok, WM_SETFONT, font.0 as usize, 1);
    let wl_cancel = crate::wide("取消");
    let cancel = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        w!("BUTTON"),
        PCWSTR(wl_cancel.as_ptr()),
        WINDOW_STYLE(st::WS_CHILD | st::WS_VISIBLE | st::WS_TABSTOP),
        sc(330),
        sc(y),
        sc(90),
        sc(30),
        Some(hwnd),
        Some(HMENU(IDCANCEL as usize as *mut c_void)),
        Some(hinstance),
        None,
    )?;
    send(cancel, WM_SETFONT, font.0 as usize, 1);

    SUI.with(|c| {
        *c.borrow_mut() = Some(SUi {
            hwnd,
            theme_combo,
            autostart,
            limit,
            recent,
            popup,
            fav,
            recent_show,
            open,
            preset_list,
            drives,
            font,
        });
    });

    theme::apply_window(hwnd, &theme::current());
    let _ = ShowWindow(hwnd, SW_SHOW);
    let _ = SetForegroundWindow(hwnd);
    Ok(())
}

fn apply_and_close() {
    let s = SUI.with(|c| {
        let b = c.borrow();
        let u = b.as_ref()?;
        let theme_idx = send(u.theme_combo, CB_GETCURSEL, 0, 0).max(0) as usize;
        let theme = THEMES.get(theme_idx).map(|t| t.0).unwrap_or("business_light");
        let drives: Vec<char> = u
            .drives
            .iter()
            .filter(|(_, h)| checked(*h))
            .map(|(d, _)| *d)
            .collect();
        // 全选 = 视为全部（空）
        let drives = if drives.len() == u.drives.len() {
            Vec::new()
        } else {
            drives
        };
        let limit = get_text(u.limit).trim().parse::<usize>().unwrap_or(200).clamp(1, 100_000);
        let recent_max = get_text(u.recent).trim().parse::<usize>().unwrap_or(2).min(100);
        Some(ss_config::Settings {
            theme: theme.to_string(),
            indexed_drives: drives,
            preset_folders: listbox_items(u.preset_list),
            result_limit: limit,
            recent_max,
            autostart: checked(u.autostart),
            popup_enabled: checked(u.popup),
            popup_show_favorites: checked(u.fav),
            popup_show_recent: checked(u.recent_show),
            popup_show_open: checked(u.open),
            last_query: ss_config::last_query(), // 保留已记忆的上次搜索词，设置页不编辑此项
        })
    });
    if let Some(s) = s {
        ss_config::save_settings(&s);
        theme::set_current(theme::theme_by_name(&s.theme));
        crate::searchwin::refresh_theme();
        crate::searchwin::refresh_scopes(); // 预设文件夹变更后刷新下拉
        crate::set_autostart(s.autostart);
    }
    if let Some(h) = SUI.with(|c| c.borrow().as_ref().map(|u| u.hwnd)) {
        unsafe {
            let _ = DestroyWindow(h);
        }
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_CTLCOLORSTATIC | WM_CTLCOLORBTN | WM_CTLCOLOREDIT | WM_CTLCOLORLISTBOX => {
            let t = theme::current();
            let hdc = HDC(wp.0 as *mut c_void);
            SetTextColor(hdc, COLORREF(t.fg));
            SetBkMode(hdc, TRANSPARENT);
            let bg = if msg == WM_CTLCOLOREDIT || msg == WM_CTLCOLORLISTBOX {
                t.alt_bg
            } else {
                t.bg
            };
            return LRESULT(theme::solid_brush(bg).0 as isize);
        }
        WM_ERASEBKGND => {
            let t = theme::current();
            let mut rc = RECT::default();
            let _ = GetClientRect(hwnd, &mut rc);
            FillRect(HDC(wp.0 as *mut c_void), &rc, theme::solid_brush(t.bg));
            return LRESULT(1);
        }
        WM_COMMAND => {
            let id = (wp.0 as u32) & 0xFFFF;
            match id {
                IDOK => apply_and_close(),
                IDCANCEL => {
                    let _ = DestroyWindow(hwnd);
                }
                ID_PRESET_ADD => {
                    if let Some(list) = SUI.with(|c| c.borrow().as_ref().map(|u| u.preset_list)) {
                        if let Some(path) = pick_folder(hwnd) {
                            let w = crate::wide(&path);
                            send(list, LB_ADDSTRING, 0, w.as_ptr() as isize);
                        }
                    }
                }
                ID_PRESET_DEL => {
                    if let Some(list) = SUI.with(|c| c.borrow().as_ref().map(|u| u.preset_list)) {
                        let sel = send(list, LB_GETCURSEL, 0, 0);
                        if sel >= 0 {
                            send(list, LB_DELETESTRING, sel as usize, 0);
                        }
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
