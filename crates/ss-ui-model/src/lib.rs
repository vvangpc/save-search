//! 不依赖 Win32 的界面逻辑，可以直接做单元测试。

mod scroll;
mod selection;
mod seq;
mod status;

pub use scroll::{ease_toward, ListGeometry};
pub use selection::{moved, reselect, SelMove};
pub use seq::RequestSeq;
pub use status::{
    all_drives_label, banner, body, compact, grouped, status_line, Banner, Body, BuildProgress,
    DriveFailure, ScopeProblem, ViewInput,
};
