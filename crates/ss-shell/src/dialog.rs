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
    EnumChildWindows, GetClassNameW, PostMessageW, SendMessageW, WM_CHAR, WM_KEYDOWN, WM_KEYUP,
};

const CT_EDIT: i32 = 50004;
const VK_RETURN: usize = 0x0D;
const EM_SETSEL: u32 = 0x00B1;

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

        // 保存原文件名，导航后恢复（避免动到用户已输入的名字）
        let original: Vec<u16> = ss_platform::wide(&get_window_text(eh));

        // 模拟真实键入：选中全部 → 逐字符 WM_CHAR 打入目录路径 → 回车。
        // 真实输入会更新对话框内部模型，回车把「目录路径」识别为进入该目录（而非保存）。
        let _ = edit.SetFocus();
        let _ = SendMessageW(eh, EM_SETSEL, Some(WPARAM(0)), Some(LPARAM(-1)));
        for u in path.encode_utf16() {
            let _ = PostMessageW(Some(eh), WM_CHAR, WPARAM(u as usize), LPARAM(0));
        }
        let _ = PostMessageW(Some(eh), WM_KEYDOWN, WPARAM(VK_RETURN), LPARAM(0));
        let _ = PostMessageW(Some(eh), WM_KEYUP, WPARAM(VK_RETURN), LPARAM(0));

        restore_filename_later(eh, original);
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

fn get_window_text(hwnd: HWND) -> String {
    use windows::Win32::UI::WindowsAndMessaging::{WM_GETTEXT, WM_GETTEXTLENGTH};
    unsafe {
        let len = SendMessageW(hwnd, WM_GETTEXTLENGTH, None, None).0 as usize;
        if len == 0 {
            return String::new();
        }
        let mut buf = vec![0u16; len + 1];
        let n = SendMessageW(
            hwnd,
            WM_GETTEXT,
            Some(WPARAM(buf.len())),
            Some(LPARAM(buf.as_mut_ptr() as isize)),
        )
        .0 as usize;
        String::from_utf16_lossy(&buf[..n.min(buf.len())])
    }
}

/// 导航完成后恢复原文件名（后台线程延时执行）。
fn restore_filename_later(hwnd: HWND, original: Vec<u16>) {
    // original 含结尾 NUL；为空（仅 [0]）则不恢复
    if original.len() <= 1 {
        return;
    }
    let raw = hwnd.0 as isize;
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(250));
        use windows::Win32::UI::WindowsAndMessaging::SetWindowTextW;
        use windows::core::PCWSTR;
        let h = HWND(raw as *mut std::ffi::c_void);
        unsafe {
            let _ = SetWindowTextW(h, PCWSTR(original.as_ptr()));
        }
    });
}
