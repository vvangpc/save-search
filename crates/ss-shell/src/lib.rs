//! 功能2：保存/打开对话框增强（进程外，不注入 DLL）。
//!
//! - `explorer`：枚举已打开的资源管理器文件夹路径。
//! - 后续：对话框检测（WinEvent）、UIA 导航。

mod dialog;
mod explorer;

pub use dialog::{is_file_dialog, navigate_dialog};
pub use explorer::enumerate_open_folders;
