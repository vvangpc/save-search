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
    println!("按 Z 序（最上层在前）：");
    for f in ss_shell::enumerate_open_folders_by_z_order() {
        println!("{:#x} {}", f.hwnd, f.path);
    }
    if let Some(p) = std::env::args().nth(1) {
        let _ = std::fs::write(p, out);
    }
}
