//! SaveSearch 主程序：常驻后台托盘应用。
//!
//! 阶段0 骨架：DPI 感知 + 隐藏窗口 + 系统托盘图标 + 右键菜单（退出）+ 消息循环。
//! 后续阶段在此编排索引线程、热键、Shell 钩子、搜索窗与浮窗。

#![windows_subsystem = "windows"]

mod dlgpopup;
mod searchwin;

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::RwLock;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Accessibility::{SetWinEventHook, HWINEVENTHOOK};
use windows::Win32::UI::Input::KeyboardAndMouse::{RegisterHotKey, MOD_ALT, VK_SPACE};
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::*;

/// 托盘图标回调消息（WM_APP 区间，避免与系统消息冲突）。
const WM_TRAY_CALLBACK: u32 = WM_APP + 1;
/// 托盘图标固定 ID。
const TRAY_UID: u32 = 1;
/// 菜单项：打开搜索。
const IDM_OPEN: u32 = 40002;
/// 菜单项：退出。
const IDM_EXIT: u32 = 40001;

/// "TaskbarCreated" 注册消息 id（资源管理器重启后需重建托盘图标）。
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);

fn lo_word(x: u32) -> u32 {
    x & 0xFFFF
}

/// 把 &str 写入定长 u16 缓冲并保证 NUL 结尾。
fn fill_u16(dst: &mut [u16], s: &str) {
    let src: Vec<u16> = s.encode_utf16().collect();
    let n = src.len().min(dst.len().saturating_sub(1));
    dst[..n].copy_from_slice(&src[..n]);
    dst[n] = 0;
}

/// 以 NUL 结尾的 UTF-16（菜单项 / 控件文本用）。
pub(crate) fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

unsafe fn add_tray(hwnd: HWND) {
    let mut nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_UID,
        uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
        uCallbackMessage: WM_TRAY_CALLBACK,
        ..Default::default()
    };
    if let Ok(icon) = LoadIconW(None, IDI_APPLICATION) {
        nid.hIcon = icon;
    }
    fill_u16(&mut nid.szTip, "SaveSearch — 文件搜索 / 保存位置快选");
    let _ = Shell_NotifyIconW(NIM_ADD, &nid);
}

unsafe fn remove_tray(hwnd: HWND) {
    let nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_UID,
        ..Default::default()
    };
    let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
}

unsafe fn show_context_menu(hwnd: HWND) {
    let Ok(menu) = CreatePopupMenu() else {
        return;
    };
    let open_text = wide("打开搜索\tAlt+Space");
    let exit_text = wide("退出 SaveSearch");
    let _ = AppendMenuW(menu, MF_STRING, IDM_OPEN as usize, PCWSTR(open_text.as_ptr()));
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
    let _ = AppendMenuW(menu, MF_STRING, IDM_EXIT as usize, PCWSTR(exit_text.as_ptr()));
    let _ = SetMenuDefaultItem(menu, IDM_OPEN, 0);

    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);
    // 让菜单点击外部时能正确关闭
    let _ = SetForegroundWindow(hwnd);
    let _ = TrackPopupMenu(
        menu,
        TPM_RIGHTBUTTON | TPM_BOTTOMALIGN,
        pt.x,
        pt.y,
        Some(0),
        hwnd,
        None,
    );
    let _ = DestroyMenu(menu);
}

unsafe extern "system" fn wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_TRAY_CALLBACK => {
            let event = lo_word(lparam.0 as u32);
            match event {
                WM_LBUTTONUP => searchwin::toggle(),
                WM_RBUTTONUP | WM_CONTEXTMENU => show_context_menu(hwnd),
                _ => {}
            }
            LRESULT(0)
        }
        WM_COMMAND => {
            match lo_word(wparam.0 as u32) {
                IDM_OPEN => searchwin::toggle(),
                IDM_EXIT => {
                    let _ = DestroyWindow(hwnd);
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            remove_tray(hwnd);
            PostQuitMessage(0);
            LRESULT(0)
        }
        other => {
            let tc = TASKBAR_CREATED.load(Ordering::Relaxed);
            if tc != 0 && other == tc {
                add_tray(hwnd);
                return LRESULT(0);
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
    }
}

fn main() -> windows::core::Result<()> {
    unsafe {
        // PerMonitorV2 DPI（清单里也声明了，这里兜底）
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);

        // COM STA（IShellWindows 枚举 / UI Automation 导航需要）
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);

        let hmodule = GetModuleHandleW(None)?;
        let hinstance = HINSTANCE(hmodule.0);
        let class_name = w!("SaveSearchTrayWnd");

        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance,
            lpszClassName: class_name,
            ..Default::default()
        };
        if RegisterClassW(&wc) == 0 {
            return Err(windows::core::Error::from_thread());
        }

        // 注册资源管理器重启广播消息
        TASKBAR_CREATED.store(RegisterWindowMessageW(w!("TaskbarCreated")), Ordering::Relaxed);

        // 隐藏窗口（不调用 ShowWindow），仅用于接收托盘回调与广播消息
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class_name,
            w!("SaveSearch"),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(hinstance),
            None,
        )?;

        add_tray(hwnd);

        // 索引缓存目录：%LOCALAPPDATA%\SaveSearch\cache
        let cache_dir = cache_dir();

        // 先创建搜索窗（隐藏），再后台「读缓存/全量构建」；建完 PostMessage 回填盘符下拉
        let catalog: searchwin::SharedCatalog = Arc::new(RwLock::new(None));
        let search_hwnd = searchwin::init(catalog.clone())?;
        {
            let catalog2 = catalog.clone();
            let cache2 = cache_dir.clone();
            let hwnd_raw = search_hwnd.0 as isize;
            std::thread::spawn(move || {
                let cat = ss_core::Catalog::build_or_load(&cache2);
                *catalog2.write() = Some(cat);
                let h = HWND(hwnd_raw as *mut core::ffi::c_void);
                let _ = PostMessageW(Some(h), searchwin::WM_APP_INDEX_READY, WPARAM(0), LPARAM(0));
            });
        }

        // 后台定时增量（每 2 秒读 USN 日志，文件增删改几秒内反映）
        {
            let catalog3 = catalog.clone();
            std::thread::spawn(move || loop {
                std::thread::sleep(Duration::from_secs(2));
                if let Some(c) = catalog3.write().as_mut() {
                    c.catch_up();
                }
            });
        }

        // 注册全局热键 Alt+Space
        let _ = RegisterHotKey(None, 1, MOD_ALT, VK_SPACE.0 as u32);

        // 功能2：保存/打开对话框浮窗 + WinEvent 钩子（监听对话框出现/移动/关闭）
        let _ = dlgpopup::init();
        let _ = SetWinEventHook(
            EVENT_OBJECT_DESTROY,
            EVENT_OBJECT_LOCATIONCHANGE,
            None,
            Some(win_event_proc),
            0,
            0,
            WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
        );

        let mut msg = MSG::default();
        loop {
            let r = GetMessageW(&mut msg, None, 0, 0);
            if r.0 <= 0 {
                break; // 0 = WM_QUIT, -1 = 错误
            }
            if msg.message == WM_HOTKEY {
                searchwin::toggle();
                continue;
            }
            if searchwin::pretranslate(&msg) {
                continue;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        // 退出前保存索引缓存（下次秒开）
        {
            let guard = catalog.read();
            if let Some(c) = guard.as_ref() {
                c.save_all(&cache_dir);
            }
        }
    }
    Ok(())
}

/// WinEvent 回调（进程外，运行于主线程消息循环）：检测文件对话框出现/移动/关闭。
unsafe extern "system" fn win_event_proc(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    id_object: i32,
    id_child: i32,
    _tid: u32,
    _time: u32,
) {
    // 只关心顶层窗口对象本身（OBJID_WINDOW = 0, 非子对象）
    if id_object != 0 || id_child != 0 || hwnd.0.is_null() {
        return;
    }
    match event {
        EVENT_OBJECT_SHOW => {
            if ss_shell::is_file_dialog(hwnd) {
                let entries = build_popup_entries();
                dlgpopup::show_for(hwnd, entries);
            }
        }
        EVENT_OBJECT_LOCATIONCHANGE => {
            dlgpopup::on_dialog_moved(hwnd);
        }
        EVENT_OBJECT_HIDE | EVENT_OBJECT_DESTROY => {
            if dlgpopup::current_dialog() == Some(hwnd) {
                dlgpopup::hide();
            }
        }
        _ => {}
    }
}

/// 组装浮窗条目：收藏 + 已打开资源管理器 + 最近位置（按路径去重）。
fn build_popup_entries() -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    let add =
        |disp: String, path: String, out: &mut Vec<(String, String)>, seen: &mut Vec<String>| {
            if path.is_empty() || seen.iter().any(|s| same_path(s, &path)) {
                return;
            }
            seen.push(path.clone());
            out.push((disp, path));
        };
    for f in ss_config::favorites() {
        add(format!("★ {f}"), f, &mut out, &mut seen);
    }
    for f in ss_shell::enumerate_open_folders() {
        add(f.clone(), f, &mut out, &mut seen);
    }
    for f in ss_config::recent() {
        add(format!("最近  {f}"), f, &mut out, &mut seen);
    }
    out
}

fn same_path(a: &str, b: &str) -> bool {
    a.trim_end_matches('\\')
        .eq_ignore_ascii_case(b.trim_end_matches('\\'))
}

/// 索引缓存目录：`%LOCALAPPDATA%\SaveSearch\cache`。
fn cache_dir() -> PathBuf {
    let base = std::env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".into());
    PathBuf::from(base).join("SaveSearch").join("cache")
}
