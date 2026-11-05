//! 功能2：保存/打开对话框增强（进程外，不注入 DLL）。
//!
//! - `explorer`：枚举已打开的资源管理器文件夹路径。
//! - 后续：对话框检测（WinEvent）、UIA 导航。

mod actions;
mod dialog;
mod explorer;

pub use actions::{
    copy_path, copy_text, open_deelevated, reveal, show_context_menu, show_properties, ActionError,
    MenuOutcome,
};
pub use dialog::{is_file_dialog, navigate_dialog_checked, NavError};
pub use explorer::{enumerate_open_folders_by_z_order, OpenFolder};
