//! 枚举当前所有已打开的资源管理器窗口及其当前文件夹路径。
//!
//! COM 调用链：IShellWindows → (每个窗口的 IDispatch) → IServiceProvider →
//! QueryService(SID_STopLevelBrowser, IShellBrowser) → QueryActiveShellView →
//! IFolderView → GetFolder(IPersistFolder2) → GetCurFolder → PIDL → 路径。
//!
//! 调用前需在本线程初始化 COM（STA）。

use std::ffi::c_void;

use windows::core::Interface;
use windows::Win32::Foundation::MAX_PATH;
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, IServiceProvider, CLSCTX_ALL};
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::Shell::{
    IFolderView, IPersistFolder2, IShellBrowser, IShellWindows, SHGetPathFromIDListW, ShellWindows,
    SID_STopLevelBrowser,
};

/// 返回去重后的已打开资源管理器文件夹路径列表。失败返回空。
pub fn enumerate_open_folders() -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    unsafe {
        let shell_windows: IShellWindows =
            match CoCreateInstance(&ShellWindows, None, CLSCTX_ALL) {
                Ok(s) => s,
                Err(_) => return out,
            };
        let count = shell_windows.Count().unwrap_or(0);
        for i in 0..count {
            let var = VARIANT::from(i);
            let Ok(disp) = shell_windows.Item(&var) else {
                continue;
            };
            let Ok(sp) = disp.cast::<IServiceProvider>() else {
                continue;
            };
            let browser: IShellBrowser =
                match sp.QueryService(&SID_STopLevelBrowser) {
                    Ok(b) => b,
                    Err(_) => continue,
                };
            let Ok(view) = browser.QueryActiveShellView() else {
                continue;
            };
            let Ok(fv) = view.cast::<IFolderView>() else {
                continue;
            };
            let pf2: IPersistFolder2 = match fv.GetFolder() {
                Ok(p) => p,
                Err(_) => continue,
            };
            let Ok(pidl) = pf2.GetCurFolder() else {
                continue;
            };

            let mut buf = [0u16; MAX_PATH as usize];
            let ok = SHGetPathFromIDListW(pidl, &mut buf).as_bool();
            CoTaskMemFree(Some(pidl as *const c_void));
            if ok {
                let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
                let s = String::from_utf16_lossy(&buf[..len]);
                if !s.is_empty() && !out.iter().any(|p| p.eq_ignore_ascii_case(&s)) {
                    out.push(s);
                }
            }
        }
    }
    out
}
