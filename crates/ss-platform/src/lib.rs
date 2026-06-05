//! 跨 crate 共享的 Win32 薄封装：宽字符串、COM 套间 guard、错误类型。

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;

/// 把字符串转成以 NUL 结尾的 UTF-16，用于传给 Win32 `*W` API。
pub fn wide(s: impl AsRef<OsStr>) -> Vec<u16> {
    s.as_ref().encode_wide().chain(std::iter::once(0)).collect()
}

/// COM STA 套间初始化 guard。Drop 时调用 `CoUninitialize`。
///
/// `SetWinEventHook` / `IShellWindows` / UI Automation 都偏好 STA。
pub struct ComScope;

impl ComScope {
    /// 在当前线程初始化单线程套间（STA）。
    pub fn init_sta() -> windows::core::Result<Self> {
        use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
        unsafe {
            CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;
        }
        Ok(ComScope)
    }
}

impl Drop for ComScope {
    fn drop(&mut self) {
        use windows::Win32::System::Com::CoUninitialize;
        unsafe {
            CoUninitialize();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_is_nul_terminated() {
        let w = wide("ab");
        assert_eq!(w, vec![0x61, 0x62, 0x00]);
    }
}
