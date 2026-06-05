//! 打印当前所有已打开的资源管理器窗口的文件夹路径。
//! 用法：`folders.exe [输出文件]`（无需管理员；需要先打开几个资源管理器窗口）

use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};

fn main() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
    let folders = ss_shell::enumerate_open_folders();
    let out = format!(
        "已打开的资源管理器文件夹 ({}):\n{}\n",
        folders.len(),
        folders.join("\n")
    );
    print!("{out}");
    if let Some(p) = std::env::args().nth(1) {
        let _ = std::fs::write(p, out);
    }
}
