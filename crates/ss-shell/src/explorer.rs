//! 枚举当前所有已打开的资源管理器窗口及其当前文件夹路径。
//!
//! COM 调用链：IShellWindows → (每个窗口的 IDispatch) → IServiceProvider →
//! QueryService(SID_STopLevelBrowser, IShellBrowser) → QueryActiveShellView →
//! IFolderView → GetFolder(IPersistFolder2) → GetCurFolder → PIDL → 路径。
//!
//! 调用前需在本线程初始化 COM（STA）。

use std::ffi::c_void;

use windows::core::Interface;
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, IServiceProvider, CLSCTX_ALL};
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::Shell::{
    IFolderView, IPersistFolder2, IShellBrowser, IShellWindows, SHGetPathFromIDListEx, ShellWindows,
    GPFIDL_DEFAULT, SID_STopLevelBrowser,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetAncestor, GetTopWindow, GetWindow, GA_ROOT, GW_HWNDNEXT,
};

/// 路径缓冲区大小：NTFS 路径上限，足够容纳超过 `MAX_PATH` 的路径。
const PATH_BUF_LEN: usize = 32_768;

/// 一个已打开的资源管理器窗口。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenFolder {
    /// 顶层窗口句柄（`HWND` 不能跨线程传递，这里保存为整数；用 `HWND(hwnd as _)` 还原）。
    pub hwnd: isize,
    pub path: String,
}

/// 返回去重后的已打开资源管理器文件夹路径列表。失败返回空。
pub fn enumerate_open_folders() -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for folder in collect_open_folders() {
        if !out.iter().any(|p| p.eq_ignore_ascii_case(&folder.path)) {
            out.push(folder.path);
        }
    }
    out
}

/// 返回已打开的资源管理器文件夹，最上层的窗口在前，相同路径只保留最上层的。失败返回空。
pub fn enumerate_open_folders_by_z_order() -> Vec<OpenFolder> {
    let folders = collect_open_folders();
    let wanted: Vec<isize> = folders.iter().map(|f| f.hwnd).collect();
    sort_and_dedup_by_z_order(folders, &top_level_z_order(&wanted))
}

/// 按 Z 序排序（不在 `z_order` 里的放最后）并按路径去重。
fn sort_and_dedup_by_z_order(mut folders: Vec<OpenFolder>, z_order: &[isize]) -> Vec<OpenFolder> {
    let rank = |hwnd: isize| z_order.iter().position(|&h| h == hwnd).unwrap_or(usize::MAX);
    folders.sort_by_key(|f| rank(f.hwnd));
    let mut out: Vec<OpenFolder> = Vec::with_capacity(folders.len());
    for folder in folders {
        if !out.iter().any(|f| f.path.eq_ignore_ascii_case(&folder.path)) {
            out.push(folder);
        }
    }
    out
}

/// 从最上层往下遍历顶层窗口，找齐 `wanted` 后停止。
fn top_level_z_order(wanted: &[isize]) -> Vec<isize> {
    let mut order = Vec::with_capacity(wanted.len());
    unsafe {
        let mut cur = GetTopWindow(None).ok();
        // 遍历期间窗口可能变化，加上限防止死循环
        for _ in 0..65_536 {
            let Some(hwnd) = cur else {
                break;
            };
            let raw = hwnd.0 as isize;
            if wanted.contains(&raw) && !order.contains(&raw) {
                order.push(raw);
                if order.len() == wanted.len() {
                    break;
                }
            }
            cur = GetWindow(hwnd, GW_HWNDNEXT).ok();
        }
    }
    order
}

/// 枚举所有资源管理器窗口，不排序、不去重。
fn collect_open_folders() -> Vec<OpenFolder> {
    let mut out: Vec<OpenFolder> = Vec::new();
    unsafe {
        let shell_windows: IShellWindows =
            match CoCreateInstance(&ShellWindows, None, CLSCTX_ALL) {
                Ok(s) => s,
                Err(_) => return out,
            };
        let mut buf = vec![0u16; PATH_BUF_LEN];
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

            let ok = SHGetPathFromIDListEx(pidl, &mut buf, GPFIDL_DEFAULT).as_bool();
            CoTaskMemFree(Some(pidl as *const c_void));
            if !ok {
                continue;
            }
            let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
            let path = String::from_utf16_lossy(&buf[..len]);
            if path.is_empty() {
                continue;
            }
            let hwnd = browser
                .GetWindow()
                .map(|h| GetAncestor(h, GA_ROOT).0 as isize)
                .unwrap_or(0);
            out.push(OpenFolder { hwnd, path });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder(hwnd: isize, path: &str) -> OpenFolder {
        OpenFolder {
            hwnd,
            path: path.to_string(),
        }
    }

    #[test]
    fn sorts_top_window_first_and_keeps_topmost_duplicate() {
        let folders = vec![
            folder(1, "C:\\a"),
            folder(2, "C:\\b"),
            folder(3, "c:\\A"),
            folder(4, "C:\\unknown"),
        ];
        let sorted = sort_and_dedup_by_z_order(folders, &[3, 2, 1]);
        assert_eq!(
            sorted,
            [folder(3, "c:\\A"), folder(2, "C:\\b"), folder(4, "C:\\unknown")]
        );
    }
}
