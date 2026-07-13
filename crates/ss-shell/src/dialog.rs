//! 文件对话框检测与导航（进程外，UI Automation）。
//!
//! 通过模拟键入目录路径 + 回车，让对话框「进入该目录」而非保存；导航后恢复原文件名。

use windows::core::BOOL;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationElement, TreeScope_Subtree,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumChildWindows, GetClassNameW, PostMessageW, SendMessageTimeoutW, SMTO_ABORTIFHUNG,
    WM_CHAR, WM_KEYDOWN, WM_KEYUP,
};

const CT_EDIT: i32 = 50004;
const VK_RETURN: usize = 0x0D;
const EM_SETSEL: u32 = 0x00B1;

/// 跨进程同步消息的超时上限。目标是另一个进程的对话框：它若挂死/忙于模态，
/// 无超时的 SendMessageW 会把本进程主线程一起冻住（热键/搜索/浮窗全失灵）。
const SEND_TIMEOUT_MS: u32 = 500;

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

/// 让文件对话框导航到目录 `path`（仅切换文件夹，不保存，保留原文件名）。
pub fn navigate_dialog(hwnd: HWND, path: &str) -> bool {
    unsafe {
        let uia: IUIAutomation = match CoCreateInstance(&CUIAutomation, None, CLSCTX_ALL) {
            Ok(u) => u,
            Err(_) => return false,
        };
        let root = match uia.ElementFromHandle(hwnd) {
            Ok(r) => r,
            Err(_) => return false,
        };
        let Some(edit) = find_filename_edit(&uia, &root) else {
            return false;
        };
        let eh = edit
            .CurrentNativeWindowHandle()
            .unwrap_or(HWND(std::ptr::null_mut()));
        if eh.0.is_null() {
            return false;
        }

        // 保存原文件名，导航后恢复（避免动到用户已输入的名字）。
        // 读不到（目标挂起/超时）→ 放弃导航：否则导航后无法恢复，用户会丢文件名
        let Some(orig_text) = get_window_text(eh) else {
            return false;
        };
        let original: Vec<u16> = ss_platform::wide(&orig_text);

        // 模拟真实键入：选中全部 → 逐字符 WM_CHAR 打入目录路径 → 回车。
        // 真实输入会更新对话框内部模型，回车把「目录路径」识别为进入该目录（而非保存）。
        let _ = edit.SetFocus();
        // 全选失败必须放弃：WM_CHAR 会插到现有文本中间/尾部，
        // 回车时「原文件名+目录路径」的混合串可能被对话框当保存名执行
        if send_timeout(eh, EM_SETSEL, WPARAM(0), LPARAM(-1)).is_none() {
            return false;
        }
        for u in path.encode_utf16() {
            let _ = PostMessageW(Some(eh), WM_CHAR, WPARAM(u as usize), LPARAM(0));
        }
        let _ = PostMessageW(Some(eh), WM_KEYDOWN, WPARAM(VK_RETURN), LPARAM(0));
        let _ = PostMessageW(Some(eh), WM_KEYUP, WPARAM(VK_RETURN), LPARAM(0));

        restore_filename_later(eh, original, path.to_string());
        true
    }
}

/// 定位「文件名」编辑框（保存/打开对话框通用）。一次遍历，按优先级取：
/// 1) 名称含「文件名」/「file name」的 Edit（最稳，跨语言/版本）；
/// 2) AutomationId 为 "1001"/"1148" 的 Edit；
/// 3) 第一个有真实窗口句柄的 Edit（文件列表列的 Edit 句柄为 0，自动排除）。
unsafe fn find_filename_edit(
    uia: &IUIAutomation,
    root: &IUIAutomationElement,
) -> Option<IUIAutomationElement> {
    let cond = uia.CreateTrueCondition().ok()?;
    let arr = root.FindAll(TreeScope_Subtree, &cond).ok()?;
    let n = arr.Length().unwrap_or(0);
    let mut by_name: Option<IUIAutomationElement> = None;
    let mut by_id: Option<IUIAutomationElement> = None;
    let mut first: Option<IUIAutomationElement> = None;
    for i in 0..n {
        let Ok(e) = arr.GetElement(i) else { continue };
        if e.CurrentControlType().map(|c| c.0).unwrap_or(0) != CT_EDIT {
            continue;
        }
        if has_native_hwnd(&e).0.is_null() {
            continue;
        }
        let name = e
            .CurrentName()
            .map(|b| b.to_string())
            .unwrap_or_default()
            .to_lowercase();
        if by_name.is_none() && (name.contains("文件名") || name.contains("file name")) {
            by_name = Some(e);
            continue;
        }
        let aid = e.CurrentAutomationId().map(|b| b.to_string()).unwrap_or_default();
        if by_id.is_none() && (aid == "1001" || aid == "1148") {
            by_id = Some(e.clone());
        }
        if first.is_none() {
            first = Some(e);
        }
    }
    by_name.or(by_id).or(first)
}

unsafe fn has_native_hwnd(e: &IUIAutomationElement) -> HWND {
    e.CurrentNativeWindowHandle()
        .unwrap_or(HWND(std::ptr::null_mut()))
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

/// 导航完成后恢复原文件名（后台线程延时执行）。
/// `typed_path` 为刚键入的目录路径：恢复前先校验编辑框内容仍是它（或已被对话框清空），
/// 用户若在延时窗口内已开始输入新文件名，绝不覆盖。
fn restore_filename_later(hwnd: HWND, original: Vec<u16>, typed_path: String) {
    // original 含结尾 NUL；为空（仅 [0]）则不恢复
    if original.len() <= 1 {
        return;
    }
    let raw = hwnd.0 as isize;
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(250));
        use windows::Win32::UI::WindowsAndMessaging::{IsWindow, WM_SETTEXT};
        let h = HWND(raw as *mut std::ffi::c_void);
        unsafe {
            // HWND 失效/被系统回收复用的缓解：还活着且类名仍是 Edit 才动它（非完美，显著降概率）
            if !IsWindow(Some(h)).as_bool() {
                return;
            }
        }
        if class_name(h) != "Edit" {
            return;
        }
        // 读当前内容（带超时，后台线程调用安全）；读不到 → 宁可不恢复也不盲写
        let Some(cur) = get_window_text(h) else {
            return;
        };
        let cur = cur.trim();
        let ours = cur.is_empty()
            || cur
                .trim_end_matches('\\')
                .eq_ignore_ascii_case(typed_path.trim_end_matches('\\'));
        if !ours {
            return; // 编辑框已是别的内容 = 用户已开始输入，跳过恢复
        }
        // SetWindowTextW 对跨进程窗口内部同步发 WM_SETTEXT，同样可能挂死 → 用带超时版本。
        // original 尾带 NUL，作为 WM_SETTEXT 的 lparam 恰好合法。
        let _ = send_timeout(h, WM_SETTEXT, WPARAM(0), LPARAM(original.as_ptr() as isize));
    });
}
