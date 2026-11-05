//! 不依赖 Win32 的界面逻辑，可以直接做单元测试。

mod scroll;
mod selection;
mod seq;

pub use scroll::{ease_toward, ListGeometry};
pub use selection::{moved, SelMove};
pub use seq::RequestSeq;
