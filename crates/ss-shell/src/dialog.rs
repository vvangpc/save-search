//! 文件对话框检测与导航（进程外，UI Automation）。
//!
//! 通过模拟键入目录路径 + 回车，让对话框「进入该目录」而非保存；导航后恢复原文件名。

use std::fmt;
use std::time::{Duration, Instant};

use windows::core::{Interface, BOOL, HRESULT};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::Accessibility::{
    CUIAutomation, CUIAutomation8, IUIAutomation, IUIAutomation2, IUIAutomationElement,
    TreeScope_Descendants,
    UIA_AutomationIdPropertyId, UIA_ControlTypePropertyId, UIA_EditControlTypeId,
    UIA_NamePropertyId, UIA_NativeWindowHandlePropertyId,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumChildWindows, GetClassNameW, PostMessageW, SendMessageTimeoutW, SMTO_ABORTIFHUNG,
    WM_CHAR, WM_KEYDOWN, WM_KEYUP,
};

const VK_RETURN: usize = 0x0D;
const EM_SETSEL: u32 = 0x00B1;

/// 跨进程同步消息的超时上限。目标是另一个进程的对话框：它若挂死/忙于模态，
/// 无超时的 SendMessageW 会把本进程主线程一起冻住（热键/搜索/浮窗全失灵）。
const SEND_TIMEOUT_MS: u32 = 500;

/// UIA 建立连接和单次调用的超时（默认都是 20 秒）。目标挂起时，导航很快失败而不是卡住。
const UIA_CONNECTION_TIMEOUT_MS: u32 = 1500;
const UIA_TRANSACTION_TIMEOUT_MS: u32 = 2000;

/// UIA 调用超时。
const UIA_E_TIMEOUT: HRESULT = HRESULT(0x8013_1505_u32 as i32);
/// `ERROR_TIMEOUT` 对应的 HRESULT。
const E_WIN32_TIMEOUT: HRESULT = HRESULT(0x8007_05B4_u32 as i32);

/// 对话框导航失败的原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NavError {
    /// 对话框里找不到文件名框，无法输入目标路径。
    FilenameBoxNotFound,
    /// 目标程序没有响应。这时不会改动文件名框的内容。
    TargetNotResponding,
    /// UI Automation 调用失败。
    Automation(String),
}

impl fmt::Display for NavError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NavError::FilenameBoxNotFound => f.write_str("找不到文件名框"),
            NavError::TargetNotResponding => f.write_str("目标程序没有响应"),
            NavError::Automation(m) => write!(f, "界面自动化调用失败：{m}"),
        }
    }
}

impl std::error::Error for NavError {}

fn nav_error_from(e: windows::core::Error) -> NavError {
    let code = e.code();
    if code == UIA_E_TIMEOUT || code == E_WIN32_TIMEOUT {
        NavError::TargetNotResponding
    } else {
        NavError::Automation(e.to_string())
    }
}

/// 创建 UIA 对象并设置超时。系统不支持 `IUIAutomation2` 时退回无超时的版本。
fn create_uia() -> windows::core::Result<IUIAutomation> {
    unsafe {
        if let Ok(uia) = CoCreateInstance::<_, IUIAutomation2>(&CUIAutomation8, None, CLSCTX_ALL) {
            let _ = uia.SetConnectionTimeout(UIA_CONNECTION_TIMEOUT_MS);
            let _ = uia.SetTransactionTimeout(UIA_TRANSACTION_TIMEOUT_MS);
            return uia.cast();
        }
        CoCreateInstance(&CUIAutomation, None, CLSCTX_ALL)
    }
}

/// 带超时的跨进程 SendMessage；目标挂起或超时返回 None。
fn send_timeout(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<usize> {
    let mut result = 0usize;
    let r = unsafe {
        SendMessageTimeoutW(
            hwnd,
            msg,
            wp,
            lp,
            SMTO_ABORTIFHUNG,
            SEND_TIMEOUT_MS,
            Some(&mut result),
        )
    };
    if r.0 == 0 {
        None
    } else {
        Some(result)
    }
}

fn class_name(hwnd: HWND) -> String {
    let mut buf = [0u16; 64];
    let n = unsafe { GetClassNameW(hwnd, &mut buf) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

struct FindCtx {
    targets: &'static [&'static str],
    found: bool,
}

unsafe extern "system" fn enum_child_cb(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let ctx = &mut *(lparam.0 as *mut FindCtx);
    let cls = class_name(hwnd);
    if ctx.targets.iter().any(|t| cls == *t) {
        ctx.found = true;
        return BOOL(0);
    }
    BOOL(1)
}

fn has_child_class(hwnd: HWND, targets: &'static [&'static str]) -> bool {
    let mut ctx = FindCtx {
        targets,
        found: false,
    };
    unsafe {
        let _ =
            EnumChildWindows(Some(hwnd), Some(enum_child_cb), LPARAM(&mut ctx as *mut _ as isize));
    }
    ctx.found
}

/// 是否为系统「保存/打开文件」对话框（顶层 `#32770` + 文件名相关子控件）。
pub fn is_file_dialog(hwnd: HWND) -> bool {
    if class_name(hwnd) != "#32770" {
        return false;
    }
    has_child_class(hwnd, &["ComboBoxEx32", "DUIViewWndClassName"])
}

/// 让文件对话框导航到目录 `path`（仅切换文件夹，不保存，保留原文件名）。失败时说明原因。
pub fn navigate_dialog_checked(hwnd: HWND, path: &str) -> Result<(), NavError> {
    unsafe {
        let uia = create_uia().map_err(nav_error_from)?;
        let root = uia.ElementFromHandle(hwnd).map_err(nav_error_from)?;
        let (edit, eh) = find_filename_edit(&uia, &root)?;

        // 保存原文件名，导航后恢复（避免动到用户已输入的名字）。
        // 读不到（目标挂起/超时）→ 放弃导航：否则导航后无法恢复，用户会丢文件名
        let original = get_window_text(eh).ok_or(NavError::TargetNotResponding)?;

        // 模拟真实键入：选中全部 → 逐字符 WM_CHAR 打入目录路径 → 回车。
        // 真实输入会更新对话框内部模型，回车把「目录路径」识别为进入该目录（而非保存）。
        let _ = edit.SetFocus();
        // 全选失败必须放弃：WM_CHAR 会插到现有文本中间/尾部，
        // 回车时「原文件名+目录路径」的混合串可能被对话框当保存名执行
        if send_timeout(eh, EM_SETSEL, WPARAM(0), LPARAM(-1)).is_none() {
            return Err(NavError::TargetNotResponding);
        }
        for u in path.encode_utf16() {
            let _ = PostMessageW(Some(eh), WM_CHAR, WPARAM(u as usize), LPARAM(0));
        }
        let _ = PostMessageW(Some(eh), WM_KEYDOWN, WPARAM(VK_RETURN), LPARAM(0));
        let _ = PostMessageW(Some(eh), WM_KEYUP, WPARAM(VK_RETURN), LPARAM(0));

        restore_filename_later(eh, original, path.to_string());
        Ok(())
    }
}

/// 定位「文件名」编辑框（保存/打开对话框通用），返回 (元素, 其原生窗口句柄)。
/// 一次查询，按优先级取：
/// 1) 名称含「文件名」/「file name」的 Edit（最稳，跨语言/版本）；
/// 2) AutomationId 为 "1001"/"1148" 的 Edit；
/// 3) 第一个有真实窗口句柄的 Edit（文件列表列的 Edit 句柄为 0，自动排除）。
///
/// 性能：用「ControlType == Edit」属性条件让 UIA 在遍历时就过滤（对话框里文件列表的
/// 每个条目都是 UIA 元素，原先 TrueCondition 全取再逐个查类型，几千个文件的目录会
/// 触发上千次跨进程调用、单击浮窗卡顿数秒）；再用缓存请求把 Name/AutomationId/句柄
/// 随查询一次带回，之后读 `Cached*` 零往返。
unsafe fn find_filename_edit(
    uia: &IUIAutomation,
    root: &IUIAutomationElement,
) -> Result<(IUIAutomationElement, HWND), NavError> {
    let cond = uia
        .CreatePropertyCondition(UIA_ControlTypePropertyId, &VARIANT::from(UIA_EditControlTypeId.0))
        .map_err(nav_error_from)?;
    let cache = uia.CreateCacheRequest().map_err(nav_error_from)?;
    cache.AddProperty(UIA_NamePropertyId).map_err(nav_error_from)?;
    cache.AddProperty(UIA_AutomationIdPropertyId).map_err(nav_error_from)?;
    cache
        .AddProperty(UIA_NativeWindowHandlePropertyId)
        .map_err(nav_error_from)?;
    let arr = root
        .FindAllBuildCache(TreeScope_Descendants, &cond, &cache)
        .map_err(nav_error_from)?;
    let n = arr.Length().unwrap_or(0);
    let mut by_name: Option<(IUIAutomationElement, HWND)> = None;
    let mut by_id: Option<(IUIAutomationElement, HWND)> = None;
    let mut first: Option<(IUIAutomationElement, HWND)> = None;
    for i in 0..n {
        let Ok(e) = arr.GetElement(i) else { continue };
        let hwnd = e
            .CachedNativeWindowHandle()
            .unwrap_or(HWND(std::ptr::null_mut()));
        if hwnd.0.is_null() {
            continue;
        }
        let name = e
            .CachedName()
            .map(|b| b.to_string())
            .unwrap_or_default()
            .to_lowercase();
        if by_name.is_none() && (name.contains("文件名") || name.contains("file name")) {
            by_name = Some((e, hwnd));
            continue;
        }
        let aid = e.CachedAutomationId().map(|b| b.to_string()).unwrap_or_default();
        if by_id.is_none() && (aid == "1001" || aid == "1148") {
            by_id = Some((e.clone(), hwnd));
        }
        if first.is_none() {
            first = Some((e, hwnd));
        }
    }
    by_name.or(by_id).or(first).ok_or(NavError::FilenameBoxNotFound)
}

/// 读窗口文本（带超时）。超时/目标挂起返回 None；空文本是合法值，返回 Some("")。
fn get_window_text(hwnd: HWND) -> Option<String> {
    use windows::Win32::UI::WindowsAndMessaging::{WM_GETTEXT, WM_GETTEXTLENGTH};
    let len = send_timeout(hwnd, WM_GETTEXTLENGTH, WPARAM(0), LPARAM(0))?;
    if len == 0 {
        return Some(String::new());
    }
    let mut buf = vec![0u16; len + 1];
    let n = send_timeout(
        hwnd,
        WM_GETTEXT,
        WPARAM(buf.len()),
        LPARAM(buf.as_mut_ptr() as isize),
    )?;
    Some(String::from_utf16_lossy(&buf[..n.min(buf.len())]))
}

/// 恢复文件名时每次轮询的决定。
#[derive(Debug, PartialEq, Eq)]
enum RestoreStep {
    Wait,
    Restore,
    /// 编辑框已是别的内容 = 用户已开始输入，不再恢复。
    Stop,
}

/// 轮询间隔和总上限。
const RESTORE_POLL: Duration = Duration::from_millis(50);
const RESTORE_DEADLINE: Duration = Duration::from_millis(1500);
/// 路径完整出现后再等这么久才恢复。回车紧跟在路径字符后面排队，马上写回原文件名的话，
/// 回车可能把它当作要保存的名字。
const RESTORE_SETTLE: Duration = Duration::from_millis(250);

/// 根据编辑框当前内容决定下一步。`cur=None` 表示目标忙、读不到。
/// `typed_stable` 是路径已完整出现的时长。
fn restore_step(
    cur: Option<&str>,
    typed_path: &str,
    original: &str,
    typed_stable: Option<Duration>,
) -> RestoreStep {
    let Some(cur) = cur.map(str::trim) else {
        return RestoreStep::Wait;
    };
    // 对话框已处理回车并清空编辑框
    if cur.is_empty() {
        return RestoreStep::Restore;
    }
    let typed = typed_path.trim_end_matches('\\');
    if cur.trim_end_matches('\\').eq_ignore_ascii_case(typed) {
        return match typed_stable {
            Some(t) if t >= RESTORE_SETTLE => RestoreStep::Restore,
            _ => RestoreStep::Wait,
        };
    }
    // 还没开始处理，或者路径字符正在一个个进入编辑框
    let typing = typed_path
        .get(..cur.len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(cur));
    if cur == original.trim() || typing {
        return RestoreStep::Wait;
    }
    RestoreStep::Stop
}

/// 导航完成后恢复原文件名（后台线程短轮询，上限约 1.5 秒）。
/// `typed_path` 为刚键入的目录路径。用户若已开始输入新文件名，绝不覆盖；
/// 到达上限仍未完成时也不写入，宁可不恢复也不盲写。
fn restore_filename_later(hwnd: HWND, original: String, typed_path: String) {
    if original.is_empty() {
        return;
    }
    let raw = hwnd.0 as isize;
    std::thread::spawn(move || {
        use windows::Win32::UI::WindowsAndMessaging::{IsWindow, WM_SETTEXT};
        let h = HWND(raw as *mut std::ffi::c_void);
        let started = Instant::now();
        let mut typed_since: Option<Instant> = None;
        while started.elapsed() < RESTORE_DEADLINE {
            std::thread::sleep(RESTORE_POLL);
            // HWND 失效/被系统回收复用的缓解：还活着且类名仍是 Edit 才动它（非完美，显著降概率）
            if !unsafe { IsWindow(Some(h)) }.as_bool() || class_name(h) != "Edit" {
                return;
            }
            let cur = get_window_text(h);
            let is_full_path = cur.as_deref().is_some_and(|c| {
                c.trim()
                    .trim_end_matches('\\')
                    .eq_ignore_ascii_case(typed_path.trim_end_matches('\\'))
            });
            if !is_full_path {
                typed_since = None;
            } else if typed_since.is_none() {
                typed_since = Some(Instant::now());
            }
            let stable = typed_since.map(|t| t.elapsed());
            match restore_step(cur.as_deref(), &typed_path, &original, stable) {
                RestoreStep::Wait => continue,
                RestoreStep::Stop => return,
                RestoreStep::Restore => {
                    // SetWindowTextW 对跨进程窗口内部同步发 WM_SETTEXT，同样可能挂死 → 用带超时版本。
                    let text = ss_platform::wide(&original);
                    let _ = send_timeout(h, WM_SETTEXT, WPARAM(0), LPARAM(text.as_ptr() as isize));
                    return;
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const TYPED: &str = "D:\\工作\\报告\\";
    const ORIGINAL: &str = "draft.docx";
    const SETTLED: Option<Duration> = Some(RESTORE_SETTLE);

    fn step(cur: Option<&str>, stable: Option<Duration>) -> RestoreStep {
        restore_step(cur, TYPED, ORIGINAL, stable)
    }

    #[test]
    fn waits_while_target_is_busy_or_typing() {
        assert_eq!(step(None, None), RestoreStep::Wait);
        assert_eq!(step(Some(ORIGINAL), None), RestoreStep::Wait);
        assert_eq!(step(Some("D:\\"), None), RestoreStep::Wait);
        assert_eq!(step(Some("d:\\工作"), None), RestoreStep::Wait);
        assert_eq!(step(Some(TYPED), None), RestoreStep::Wait);
        assert_eq!(step(Some(TYPED), Some(Duration::from_millis(100))), RestoreStep::Wait);
    }

    #[test]
    fn restores_after_dialog_consumed_the_path() {
        assert_eq!(step(Some(""), None), RestoreStep::Restore);
        assert_eq!(step(Some("   "), None), RestoreStep::Restore);
        // 有些对话框导航后保留路径，等路径稳定后才恢复
        assert_eq!(step(Some(TYPED), SETTLED), RestoreStep::Restore);
        assert_eq!(step(Some("d:\\工作\\报告"), SETTLED), RestoreStep::Restore);
    }

    #[test]
    fn never_overwrites_user_input() {
        assert_eq!(step(Some("new-name.docx"), None), RestoreStep::Stop);
        assert_eq!(step(Some("D:\\其他"), SETTLED), RestoreStep::Stop);
        // 名字与字符边界不对齐时不能 panic
        assert_eq!(step(Some("D:\\工"), None), RestoreStep::Wait);
        assert_eq!(step(Some("D:\\工x"), None), RestoreStep::Stop);
    }

    #[test]
    fn nav_errors_map_timeouts_to_not_responding() {
        let timeout = windows::core::Error::from_hresult(UIA_E_TIMEOUT);
        assert_eq!(nav_error_from(timeout), NavError::TargetNotResponding);
        let win32 = windows::core::Error::from_hresult(E_WIN32_TIMEOUT);
        assert_eq!(nav_error_from(win32), NavError::TargetNotResponding);
        let other = windows::core::Error::from_hresult(HRESULT(0x8000_4005_u32 as i32));
        assert!(matches!(nav_error_from(other), NavError::Automation(_)));
        assert_eq!(NavError::FilenameBoxNotFound.to_string(), "找不到文件名框");
        assert_eq!(NavError::TargetNotResponding.to_string(), "目标程序没有响应");
    }

    #[test]
    fn uia_object_is_created_with_timeouts() {
        let _com = ss_platform::ComScope::init_sta().unwrap();
        let uia = create_uia().unwrap();
        let uia2: IUIAutomation2 = uia.cast().unwrap();
        unsafe {
            assert_eq!(uia2.ConnectionTimeout().unwrap(), UIA_CONNECTION_TIMEOUT_MS);
            assert_eq!(uia2.TransactionTimeout().unwrap(), UIA_TRANSACTION_TIMEOUT_MS);
        }
    }
}
