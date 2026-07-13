//! SaveSearch 主程序：常驻后台托盘应用。
//!
//! 阶段0 骨架：DPI 感知 + 隐藏窗口 + 系统托盘图标 + 右键菜单（退出）+ 消息循环。
//! 后续阶段在此编排索引线程、热键、Shell 钩子、搜索窗与浮窗。

#![windows_subsystem = "windows"]

mod dlgpopup;
mod icons;
mod paint;
mod searchwin;
mod settings_win;
mod theme;

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use parking_lot::RwLock;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{
    GetLastError, ERROR_ALREADY_EXISTS, HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM,
};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::Accessibility::{SetWinEventHook, HWINEVENTHOOK};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, MOD_ALT, MOD_NOREPEAT, VK_SPACE,
};
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
/// 菜单项：设置。
const IDM_SETTINGS: u32 = 40003;
/// 菜单项：退出。
const IDM_EXIT: u32 = 40001;

/// "TaskbarCreated" 注册消息 id（资源管理器重启后需重建托盘图标）。
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);

/// 全局目录/缓存目录句柄：供 wndproc 在关机/注销（WM_QUERYENDSESSION）时抢救性保存索引。
static CATALOG: OnceLock<searchwin::SharedCatalog> = OnceLock::new();
static CACHE_DIR: OnceLock<PathBuf> = OnceLock::new();

/// 保存索引缓存（正常退出与会话结束共用）。
fn save_catalog_cache() {
    if let (Some(cat), Some(dir)) = (CATALOG.get(), CACHE_DIR.get()) {
        if let Some(c) = cat.read().as_ref() {
            c.save_all(dir);
        }
    }
}

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

/// 从 exe 资源（app.rc 的 `1 ICON "app.ico"`）加载应用图标。
/// `cx==0 && cy==0` 取默认大图标尺寸；否则按指定像素尺寸。
/// `LR_SHARED` → 句柄由系统管理，免 `DestroyIcon`，重复加载不泄漏。
pub(crate) fn load_app_icon(cx: i32, cy: i32) -> Option<HICON> {
    unsafe {
        let hmod = GetModuleHandleW(None).ok()?;
        let flags = if cx == 0 && cy == 0 {
            LR_DEFAULTSIZE | LR_SHARED
        } else {
            LR_SHARED
        };
        // windows-rs 0.62 无 MAKEINTRESOURCEW：数字资源 id 用伪指针 PCWSTR(id as _)。
        let h = LoadImageW(
            Some(HINSTANCE(hmod.0)),
            PCWSTR(1usize as *const u16),
            IMAGE_ICON,
            cx,
            cy,
            flags,
        )
        .ok()?; // LoadImageW 返回 HANDLE
        Some(HICON(h.0)) // HANDLE → HICON（同构，取裸指针）
    }
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
    // 托盘按系统小图标尺寸加载自有图标（替代系统默认灰齿轮）。
    if let Some(icon) = load_app_icon(GetSystemMetrics(SM_CXSMICON), GetSystemMetrics(SM_CYSMICON)) {
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
    let set_text = wide("设置…");
    let exit_text = wide("退出 SaveSearch");
    let _ = AppendMenuW(menu, MF_STRING, IDM_OPEN as usize, PCWSTR(open_text.as_ptr()));
    let _ = AppendMenuW(menu, MF_STRING, IDM_SETTINGS as usize, PCWSTR(set_text.as_ptr()));
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
    // KB135788：隐藏窗口上的弹出菜单需要事后 Post 一条消息，否则下次点击外部不收起
    let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
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
                IDM_SETTINGS => settings_win::open(),
                IDM_EXIT => {
                    let _ = DestroyWindow(hwnd);
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_QUERYENDSESSION => {
            // 关机/注销不会走消息循环的正常退出路径，这里抢救性保存索引缓存
            save_catalog_cache();
            LRESULT(1) // 允许会话结束
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
        // 防多开：同一会话只允许一个实例。已有实例则唤起其搜索框后退出。
        // 句柄绑定到 main 作用域，存活整个进程期间 → Mutex 持续存在作为锁。
        let _instance_mutex = CreateMutexW(None, false, w!("SaveSearch.SingleInstance.Mutex")).ok();
        // GetLastError 必须紧跟 CreateMutexW，中间不能插入其他 Win32 调用（.ok() 是纯 Rust）。
        if GetLastError() == ERROR_ALREADY_EXISTS {
            // 找到已有实例那个隐藏托盘窗口（类名 SaveSearchTrayWnd），让它打开搜索框。
            if let Ok(existing) = FindWindowW(w!("SaveSearchTrayWnd"), PCWSTR::null()) {
                let _ = PostMessageW(
                    Some(existing),
                    WM_COMMAND,
                    WPARAM(IDM_OPEN as usize),
                    LPARAM(0),
                );
            }
            return Ok(());
        }

        // PerMonitorV2 DPI（清单里也声明了，这里兜底）
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);

        // COM STA（IShellWindows 枚举 / UI Automation 导航需要）
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);

        // 读取设置并应用主题
        let settings = ss_config::load_settings();
        theme::set_current(theme::theme_by_name(&settings.theme));

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
        let _ = CATALOG.set(catalog.clone());
        let _ = CACHE_DIR.set(cache_dir.clone());
        let search_hwnd = searchwin::init(catalog.clone())?;
        {
            let catalog2 = catalog.clone();
            let cache2 = cache_dir.clone();
            let drives = settings.indexed_drives.clone();
            let hwnd_raw = search_hwnd.0 as isize;
            std::thread::spawn(move || {
                let cat = ss_core::Catalog::build_or_load(&cache2, &drives);
                *catalog2.write() = Some(cat);
                let h = HWND(hwnd_raw as *mut core::ffi::c_void);
                let _ = PostMessageW(Some(h), searchwin::WM_APP_INDEX_READY, WPARAM(0), LPARAM(0));
            });
        }

        // 后台定时增量（每 2 秒读 USN 日志，文件增删改几秒内反映）。
        // 读日志的磁盘 I/O 在锁外做，只在应用变更的瞬间短暂持写锁——
        // 否则 I/O 慢时写锁长占，UI 线程每个按键的搜索（读锁）会被卡住。
        {
            let catalog3 = catalog.clone();
            std::thread::spawn(move || loop {
                std::thread::sleep(Duration::from_secs(2));
                let positions = match catalog3.read().as_ref() {
                    Some(c) => c.usn_positions(),
                    None => continue,
                };
                for (letter, jid, from) in positions {
                    if let Ok((changes, new_next)) = ss_core::read_changes(letter, jid, from) {
                        if changes.is_empty() && new_next == from {
                            continue;
                        }
                        if let Some(c) = catalog3.write().as_mut() {
                            c.apply_drive_changes(letter, &changes, new_next);
                        }
                    }
                }
            });
        }

        // 注册全局热键 Alt+Space（MOD_NOREPEAT：长按不重复触发 toggle）
        let _ = RegisterHotKey(None, 1, MOD_ALT | MOD_NOREPEAT, VK_SPACE.0 as u32);

        // 功能2：保存/打开对话框浮窗 + WinEvent 钩子（监听对话框出现/移动/关闭）。
        // 钩子范围收窄成两段：0x8001(DESTROY)-0x8003(HIDE) 含 SHOW=0x8002，
        // 加单独的 0x800B(LOCATIONCHANGE)——避开中间 0x8004-0x800A 的
        // FOCUS/SELECTION/STATECHANGE 全系统高频事件反复唤醒主线程。
        let _ = dlgpopup::init();
        let _ = SetWinEventHook(
            EVENT_OBJECT_DESTROY,
            EVENT_OBJECT_HIDE,
            None,
            Some(win_event_proc),
            0,
            0,
            WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
        );
        let _ = SetWinEventHook(
            EVENT_OBJECT_LOCATIONCHANGE,
            EVENT_OBJECT_LOCATIONCHANGE,
            None,
            Some(win_event_proc),
            0,
            0,
            WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
        );
        // 前台变化：对话框失焦时隐藏浮窗
        let _ = SetWinEventHook(
            EVENT_SYSTEM_FOREGROUND,
            EVENT_SYSTEM_FOREGROUND,
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
        save_catalog_cache();
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
            if ss_shell::is_file_dialog(hwnd) && ss_config::load_settings().popup_enabled {
                let entries = build_popup_entries();
                dlgpopup::show_for(hwnd, entries);
            }
        }
        EVENT_OBJECT_LOCATIONCHANGE => {
            dlgpopup::on_dialog_moved(hwnd);
        }
        EVENT_SYSTEM_FOREGROUND => {
            // 对话框重新回到前台：重新枚举条目——用户可能在切走期间
            // 新开/关闭了资源管理器文件夹（见 build_popup_entries）。
            // 自进程窗口（浮窗）的 FOREGROUND 已被 WINEVENT_SKIPOWNPROCESS 过滤，
            // 右键菜单 SetForegroundWindow(popup) 不会进到这里。
            if dlgpopup::current_dialog() == Some(hwnd) {
                // 复核仍是文件对话框：HWND 可能已被系统回收复用给无关窗口
                if ss_shell::is_file_dialog(hwnd) {
                    let entries = build_popup_entries();
                    dlgpopup::show_for(hwnd, entries);
                } else {
                    dlgpopup::hide();
                }
            } else {
                dlgpopup::on_foreground(hwnd);
            }
        }
        EVENT_OBJECT_HIDE | EVENT_OBJECT_DESTROY => {
            if dlgpopup::current_dialog() == Some(hwnd) {
                dlgpopup::hide();
            }
        }
        _ => {}
    }
}

/// 组装浮窗条目：收藏 + 已打开资源管理器 + 最近位置（按路径去重，遵循设置显示项）。
fn build_popup_entries() -> Vec<(dlgpopup::EntryKind, String)> {
    use dlgpopup::EntryKind;
    let s = ss_config::load_settings();
    // 顺序即优先级：收藏 > 已打开 > 最近。seen 去重为 first-seen-wins，
    // 同一路径只保留最高优先级来源。务必保持下面三个采集块的先后顺序。
    let mut out: Vec<(EntryKind, String)> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    let mut add = |kind: EntryKind, path: String| {
        if path.is_empty() || seen.iter().any(|p| same_path(p, &path)) {
            return;
        }
        seen.push(path.clone());
        out.push((kind, path));
    };
    if s.popup_show_favorites {
        for f in ss_config::favorites() {
            add(EntryKind::Favorite, f);
        }
    }
    if s.popup_show_open {
        for f in ss_shell::enumerate_open_folders() {
            add(EntryKind::Open, f);
        }
    }
    if s.popup_show_recent {
        for f in ss_config::recent() {
            add(EntryKind::Recent, f);
        }
    }
    out
}

fn same_path(a: &str, b: &str) -> bool {
    a.trim_end_matches('\\')
        .eq_ignore_ascii_case(b.trim_end_matches('\\'))
}

/// 开机自启：建/删「最高权限」登录计划任务（免 UAC）。需管理员权限（本程序已提权）。
pub(crate) fn set_autostart(enable: bool) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let exe = std::env::current_exe()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    // /TR 的值需内嵌引号：否则 exe 路径含空格时，任务动作会在首个空格处被截断
    let exe_quoted = format!("\"{exe}\"");
    let mut cmd = std::process::Command::new("schtasks");
    if enable {
        cmd.args([
            "/Create", "/F", "/RL", "HIGHEST", "/SC", "ONLOGON", "/TN", "SaveSearch", "/TR",
            &exe_quoted,
        ]);
    } else {
        cmd.args(["/Delete", "/F", "/TN", "SaveSearch"]);
    }
    cmd.creation_flags(CREATE_NO_WINDOW);
    let _ = cmd.output();
}

/// 索引缓存目录：`%LOCALAPPDATA%\SaveSearch\cache`。
fn cache_dir() -> PathBuf {
    let base = std::env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".into());
    PathBuf::from(base).join("SaveSearch").join("cache")
}
