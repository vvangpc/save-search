//! 人工验证盘管理：列出卷，并加载索引、打印每个盘的状态变化。
//!
//! 用法：`volumes.exe [缓存目录]`。不提供缓存目录时只列出卷，不需要管理员权限；
//! 提供后会加载或构建索引，需要管理员权限。

use std::sync::atomic::{AtomicUsize, Ordering};

use ss_core::{Catalog, DriveState};

fn main() {
    for v in ss_core::list_volumes() {
        println!(
            "{}: 卷标={:?} 文件系统={:?} 类型={:?} 可索引={} 原因={:?}",
            v.letter, v.label, v.file_system, v.kind, v.indexable, v.reason
        );
    }

    let Some(cache_dir) = std::env::args().nth(1) else {
        return;
    };
    let building_events = AtomicUsize::new(0);
    let cat = Catalog::build_or_load_with_progress(cache_dir.as_ref(), &[], &|letter, state| {
        // 进度事件很多，只打印每 50 个中的一个
        if matches!(state, DriveState::Building { .. })
            && !building_events.fetch_add(1, Ordering::Relaxed).is_multiple_of(50)
        {
            return;
        }
        println!("{letter}: {state:?}");
    });
    println!("最终状态：");
    for s in cat.drive_statuses() {
        println!("{}: {:?} 最近错误={:?}", s.letter, s.state, s.last_error);
    }
    println!("{} 节点，{:.1} MB", cat.total_nodes(), cat.memory_bytes() as f64 / 1_000_000.0);
}
