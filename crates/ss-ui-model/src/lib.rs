//! 不依赖 Win32 的界面逻辑，可以直接做单元测试。

mod scroll;
mod selection;
mod seq;
mod status;
mod text;

pub use scroll::{ease_toward, ListGeometry};
pub use selection::{moved, reselect, SelMove};
pub use seq::RequestSeq;
pub use text::{
    enter_op, format_date, format_size, highlight_segments, is_partial_selection, parent_of,
    LocalTime, OpenOp,
};
pub use status::{
    all_drives_label, banner, body, compact, grouped, status_line, Banner, Body, BuildProgress,
    DriveFailure, ScopeProblem, ViewInput,
};
