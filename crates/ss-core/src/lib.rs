//! 索引引擎：MFT 枚举、紧凑索引、子串搜索、多盘目录。
//!
//! 阶段1：单盘全量枚举 + 内存索引 + 大小写不敏感子串搜索。
//! 阶段2：多盘（指定盘符过滤）。后续：USN 增量、rkyv 持久化、非 NTFS 降级。

mod catalog;
mod category;
mod drives;
mod index;
mod mft;
mod persist;

pub use catalog::Catalog;
pub use category::{classify, Category};
pub use drives::ntfs_fixed_drives;
pub use index::{Change, Index, IndexBuilder, SearchResult, IS_DIR, ROOT_ID};
pub use mft::{build_index_for_drive, current_usn_state, read_changes};
