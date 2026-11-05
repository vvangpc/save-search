//! 对搜索结果的操作：打开、定位、复制路径、属性、系统右键菜单。
//!
//! 除 [`copy_path`] 和 [`copy_text`] 外都要求调用线程已初始化 COM（STA）。
//!
//! 本程序以管理员权限运行，子进程默认继承这个权限。所以打开文件必须走
//! [`open_deelevated`]：让 Explorer 进程去启动目标，目标得到的是 Explorer 的普通权限。

use std::ffi::c_void;
use std::fmt;
use std::path::Path;

use ss_platform::wide;
use windows::core::{Interface, BSTR, PCSTR, PCWSTR, PSTR};
use windows::Win32::Foundation::{
    GlobalFree, HANDLE, HWND, LPARAM, LRESULT, POINT, WPARAM,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoTaskMemFree, IDispatch, IServiceProvider, CLSCTX_ALL,
};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Ole::CF_UNICODETEXT;
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    DefSubclassProc, IContextMenu, IContextMenu2, IContextMenu3, IShellBrowser, IShellDispatch2,
    IShellFolder, IShellFolderViewDual, IShellWindows, RemoveWindowSubclass, SHBindToParent,
    SHObjectProperties, SHOpenFolderAndSelectItems, SHParseDisplayName, SetWindowSubclass,
    ShellWindows, CMF_NORMAL, CMINVOKECOMMANDINFO, CMINVOKECOMMANDINFOEX, CSIDL_DESKTOP, GCS_VERBW,
    SHOP_FILEPATH, SID_STopLevelBrowser, SVGIO_BACKGROUND, SWC_DESKTOP, SWFO_NEEDDISPATCH,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreatePopupMenu, DestroyMenu, PostMessageW, SetForegroundWindow, TrackPopupMenuEx,
    SW_SHOWNORMAL, TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_DRAWITEM, WM_INITMENUPOPUP, WM_MEASUREITEM,
    WM_MENUCHAR, WM_NULL,
};

/// 结果操作失败的原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionError {
    /// 路径已不存在。
    NotFound(String),
    /// 拿不到 Explorer 的桌面窗口（Explorer 没有运行或正在重启）。
    /// 这时不会退回到直接启动，否则目标会继承管理员权限。
    ExplorerUnavailable(String),
    /// 剪贴板被其他程序占用，或写入失败。
    Clipboard(String),
    /// 其他系统调用失败。
    System {
        action: &'static str,
        message: String,
    },
}

impl fmt::Display for ActionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ActionError::NotFound(path) => write!(f, "路径不存在：{path}"),
            ActionError::ExplorerUnavailable(m) => write!(f, "资源管理器不可用：{m}"),
            ActionError::Clipboard(m) => write!(f, "无法写入剪贴板：{m}"),
            ActionError::System { action, message } => write!(f, "{action}失败：{message}"),
        }
    }
}

impl std::error::Error for ActionError {}

/// 右键菜单的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuOutcome {
    /// 用户没有选择任何命令。
    Dismissed,
    Invoked,
}

fn system_error(action: &'static str) -> impl Fn(windows::core::Error) -> ActionError {
    move |e| ActionError::System {
        action,
        message: e.to_string(),
    }
}

fn ensure_exists(path: &str) -> Result<(), ActionError> {
    match Path::new(path).try_exists() {
        Ok(true) => Ok(()),
        Ok(false) => Err(ActionError::NotFound(path.to_string())),
        Err(e) => Err(ActionError::System {
            action: "检查路径",
            message: e.to_string(),
        }),
    }
}

/// 拥有一个 PIDL，离开作用域时释放。
struct Pidl(*mut ITEMIDLIST);

impl Pidl {
    fn parse(path: &str, action: &'static str) -> Result<Pidl, ActionError> {
        let wpath = wide(path);
        let mut pidl: *mut ITEMIDLIST = std::ptr::null_mut();
        unsafe { SHParseDisplayName(PCWSTR(wpath.as_ptr()), None, &mut pidl, 0, None) }
            .map_err(system_error(action))?;
        Ok(Pidl(pidl))
    }
}

impl Drop for Pidl {
    fn drop(&mut self) {
        unsafe { CoTaskMemFree(Some(self.0 as *const c_void)) };
    }
}

/// 在资源管理器中打开所在文件夹并选中 `path`。
pub fn reveal(path: &str) -> Result<(), ActionError> {
    ensure_exists(path)?;
    let pidl = Pidl::parse(path, "定位文件")?;
    unsafe { SHOpenFolderAndSelectItems(pidl.0, None, 0) }.map_err(system_error("定位文件"))
}

/// 把路径写入剪贴板。`owner` 是本进程的窗口，见 [`copy_text`]。
pub fn copy_path(owner: HWND, path: &str) -> Result<(), ActionError> {
    ensure_exists(path)?;
    copy_text(owner, path)
}

/// 把文本写入剪贴板。必须提供窗口：不提供所有者时 `SetClipboardData` 可能失败。
/// 其他程序短暂占用剪贴板很常见，所以会重试几次。
pub fn copy_text(owner: HWND, text: &str) -> Result<(), ActionError> {
    const ATTEMPTS: u64 = 5;
    let clipboard_error = |e: windows::core::Error| ActionError::Clipboard(e.to_string());
    let mut opened = Ok(());
    for i in 0..ATTEMPTS {
        opened = unsafe { OpenClipboard(Some(owner)) };
        if opened.is_ok() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10 * (i + 1)));
    }
    opened.map_err(clipboard_error)?;

    let result = (|| unsafe {
        EmptyClipboard()?;
        let utf16 = wide(text);
        let bytes = utf16.len() * std::mem::size_of::<u16>();
        let mem = GlobalAlloc(GMEM_MOVEABLE, bytes)?;
        let dst = GlobalLock(mem) as *mut u16;
        if dst.is_null() {
            let _ = GlobalFree(Some(mem));
            return Err(windows::core::Error::from_thread());
        }
        std::ptr::copy_nonoverlapping(utf16.as_ptr(), dst, utf16.len());
        let _ = GlobalUnlock(mem);
        // 成功后内存归剪贴板所有，失败时才由我们释放
        if let Err(e) = SetClipboardData(CF_UNICODETEXT.0 as u32, Some(HANDLE(mem.0))) {
            let _ = GlobalFree(Some(mem));
            return Err(e);
        }
        Ok(())
    })();
    let _ = unsafe { CloseClipboard() };
    result.map_err(clipboard_error)
}

/// 打开系统属性对话框。对话框在系统自己的线程上运行，本函数立即返回。
pub fn show_properties(owner: HWND, path: &str) -> Result<(), ActionError> {
    ensure_exists(path)?;
    let wpath = wide(path);
    let ok = unsafe {
        SHObjectProperties(Some(owner), SHOP_FILEPATH, PCWSTR(wpath.as_ptr()), PCWSTR::null())
    };
    if ok.as_bool() {
        Ok(())
    } else {
        Err(system_error("打开属性")(windows::core::Error::from_thread()))
    }
}

/// 通过 Explorer 进程打开 `path`：文件用默认程序打开，文件夹直接打开。
/// 目标进程得到的是 Explorer 的普通权限，不继承本程序的管理员权限。
pub fn open_deelevated(path: &str) -> Result<(), ActionError> {
    open_deelevated_with_verb(path, "open")
}

fn open_deelevated_with_verb(path: &str, verb: &str) -> Result<(), ActionError> {
    ensure_exists(path)?;
    let shell = desktop_shell_dispatch()
        .map_err(|e| ActionError::ExplorerUnavailable(e.to_string()))?;
    let working_dir = Path::new(path)
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    unsafe {
        shell.ShellExecute(
            &BSTR::from(path),
            &VARIANT::default(),
            &VARIANT::from(BSTR::from(working_dir.as_str())),
            &VARIANT::from(BSTR::from(verb)),
            &VARIANT::from(SW_SHOWNORMAL.0),
        )
    }
    .map_err(system_error("打开文件"))
}

/// 取得运行在 Explorer 进程里的 `Shell.Application` 对象：
/// 桌面窗口 → 活动视图 → 背景对象 → `Application`。
fn desktop_shell_dispatch() -> windows::core::Result<IShellDispatch2> {
    unsafe {
        let windows: IShellWindows = CoCreateInstance(&ShellWindows, None, CLSCTX_ALL)?;
        let location = VARIANT::from(CSIDL_DESKTOP as i32);
        let empty = VARIANT::default();
        let mut hwnd = 0i32;
        let desktop: IDispatch =
            windows.FindWindowSW(&location, &empty, SWC_DESKTOP, &mut hwnd, SWFO_NEEDDISPATCH)?;
        let provider: IServiceProvider = desktop.cast()?;
        let browser: IShellBrowser = provider.QueryService(&SID_STopLevelBrowser)?;
        let view = browser.QueryActiveShellView()?;
        // 背景对象只接受按 IDispatch 请求，之后才能转换成 IShellFolderViewDual
        let background: IDispatch = view.GetItemObject(SVGIO_BACKGROUND)?;
        let folder_view: IShellFolderViewDual = background.cast()?;
        folder_view.Application()?.cast()
    }
}

/// 这些常用命令转交 Explorer 执行，以普通权限启动程序。其他命令（第三方菜单项等）
/// 只能在本进程内执行，如果它们启动程序，程序会继承管理员权限。
const DEELEVATED_VERBS: &[&str] = &["open", "edit", "print"];

/// `CMINVOKECOMMANDINFOEX.fMask`：`lpVerbW` 有效。
const CMIC_MASK_UNICODE: u32 = 0x0000_4000;
/// `CMINVOKECOMMANDINFOEX.fMask`：`ptInvoke` 有效。
const CMIC_MASK_PTINVOKE: u32 = 0x2000_0000;

const FIRST_COMMAND_ID: u32 = 1;
const LAST_COMMAND_ID: u32 = 0x7FFF;
const SUBCLASS_ID: usize = 0x5353;

/// 右键菜单的子菜单（“打开方式”“发送到”）和图标靠这两个接口处理窗口消息。
struct MenuHandlers {
    menu2: Option<IContextMenu2>,
    menu3: Option<IContextMenu3>,
}

unsafe extern "system" fn menu_subclass_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    data: usize,
) -> LRESULT {
    let from_menu = match msg {
        WM_INITMENUPOPUP | WM_MENUCHAR => true,
        // wParam 为 0 表示消息来自菜单，不是窗口自己的自绘控件
        WM_DRAWITEM | WM_MEASUREITEM => wparam.0 == 0,
        _ => false,
    };
    if from_menu {
        let handlers = &*(data as *const MenuHandlers);
        let mut result = LRESULT(0);
        if let Some(menu3) = &handlers.menu3 {
            if menu3.HandleMenuMsg2(msg, wparam, lparam, Some(&mut result)).is_ok() {
                return result;
            }
        } else if let Some(menu2) = &handlers.menu2 {
            if menu2.HandleMenuMsg(msg, wparam, lparam).is_ok() {
                return LRESULT(0);
            }
        }
    }
    DefSubclassProc(hwnd, msg, wparam, lparam)
}

/// 读取命令的标准动词名（如 `open`）。很多菜单项没有动词，返回 `None`。
fn command_verb(menu: &IContextMenu, offset: usize) -> Option<String> {
    let mut buf = [0u16; 64];
    unsafe {
        menu.GetCommandString(
            offset,
            GCS_VERBW,
            None,
            PSTR(buf.as_mut_ptr() as *mut u8),
            buf.len() as u32,
        )
    }
    .ok()?;
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..len]))
}

/// 在屏幕坐标 `(x, y)` 弹出系统右键菜单并执行用户选择的命令。
/// `owner` 必须是调用线程创建的窗口。菜单期间本函数会运行消息循环。
pub fn show_context_menu(
    owner: HWND,
    path: &str,
    x: i32,
    y: i32,
) -> Result<MenuOutcome, ActionError> {
    const ACTION: &str = "弹出右键菜单";
    ensure_exists(path)?;
    let pidl = Pidl::parse(path, ACTION)?;
    unsafe {
        let mut last: *mut ITEMIDLIST = std::ptr::null_mut();
        let parent: IShellFolder =
            SHBindToParent(pidl.0, Some(&mut last)).map_err(system_error(ACTION))?;
        let menu: IContextMenu = parent
            .GetUIObjectOf(owner, &[last as *const ITEMIDLIST], None)
            .map_err(system_error(ACTION))?;

        let popup = CreatePopupMenu().map_err(system_error(ACTION))?;
        let queried = menu
            .QueryContextMenu(popup, 0, FIRST_COMMAND_ID, LAST_COMMAND_ID, CMF_NORMAL)
            .ok();
        if let Err(e) = queried {
            let _ = DestroyMenu(popup);
            return Err(system_error(ACTION)(e));
        }

        let handlers = MenuHandlers {
            menu2: menu.cast().ok(),
            menu3: menu.cast().ok(),
        };
        let subclassed = SetWindowSubclass(
            owner,
            Some(menu_subclass_proc),
            SUBCLASS_ID,
            &handlers as *const MenuHandlers as usize,
        )
        .as_bool();

        // 菜单所有者必须在前台，否则点击菜单外部时菜单不会关闭
        let _ = SetForegroundWindow(owner);
        let chosen = TrackPopupMenuEx(popup, (TPM_RETURNCMD | TPM_RIGHTBUTTON).0, x, y, owner, None);
        let _ = PostMessageW(Some(owner), WM_NULL, WPARAM(0), LPARAM(0));

        if subclassed {
            let _ = RemoveWindowSubclass(owner, Some(menu_subclass_proc), SUBCLASS_ID);
        }

        let chosen = chosen.0 as u32;
        let outcome = if chosen < FIRST_COMMAND_ID {
            Ok(MenuOutcome::Dismissed)
        } else {
            let offset = (chosen - FIRST_COMMAND_ID) as usize;
            let verb = command_verb(&menu, offset).filter(|v| {
                DEELEVATED_VERBS.iter().any(|d| d.eq_ignore_ascii_case(v))
            });
            match verb {
                Some(verb) => open_deelevated_with_verb(path, &verb),
                None => invoke_command(&menu, owner, offset, x, y),
            }
            .map(|()| MenuOutcome::Invoked)
        };
        let _ = DestroyMenu(popup);
        outcome
    }
}

/// 在本进程内执行菜单命令。动词位置放的是命令序号，这是接口规定的用法。
unsafe fn invoke_command(
    menu: &IContextMenu,
    owner: HWND,
    offset: usize,
    x: i32,
    y: i32,
) -> Result<(), ActionError> {
    let info = CMINVOKECOMMANDINFOEX {
        cbSize: std::mem::size_of::<CMINVOKECOMMANDINFOEX>() as u32,
        fMask: CMIC_MASK_UNICODE | CMIC_MASK_PTINVOKE,
        hwnd: owner,
        lpVerb: PCSTR(offset as *const u8),
        lpVerbW: PCWSTR(offset as *const u16),
        nShow: SW_SHOWNORMAL.0,
        ptInvoke: POINT { x, y },
        ..Default::default()
    };
    menu.InvokeCommand(&info as *const CMINVOKECOMMANDINFOEX as *const CMINVOKECOMMANDINFO)
        .map_err(system_error("执行菜单命令"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MISSING: &str = "Z:\\ss-shell-test\\does-not-exist\\file.txt";

    #[test]
    fn missing_path_reports_not_found_for_every_action() {
        let not_found = Err(ActionError::NotFound(MISSING.to_string()));
        let no_window = HWND(std::ptr::null_mut());
        assert_eq!(reveal(MISSING), not_found);
        assert_eq!(copy_path(no_window, MISSING), not_found);
        assert_eq!(show_properties(no_window, MISSING), not_found);
        assert_eq!(open_deelevated(MISSING), not_found);
        assert_eq!(
            show_context_menu(no_window, MISSING, 0, 0),
            Err(ActionError::NotFound(MISSING.to_string()))
        );
    }

    #[test]
    fn errors_explain_the_reason() {
        assert_eq!(
            ActionError::NotFound("C:\\a".into()).to_string(),
            "路径不存在：C:\\a"
        );
        let e = ActionError::System {
            action: "打开文件",
            message: "拒绝访问".into(),
        };
        assert_eq!(e.to_string(), "打开文件失败：拒绝访问");
    }
}
