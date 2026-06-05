//! 多盘目录：每个盘一个 [`Index`]，统一搜索、可按盘符过滤；
//! 支持磁盘缓存（秒开）与 USN 增量更新。

use std::path::{Path, PathBuf};

use crate::drives::ntfs_fixed_drives;
use crate::index::{Index, SearchResult};
use crate::mft::{build_index_for_drive, current_usn_state, read_changes};
use crate::persist;

/// 多个盘的索引集合。
pub struct Catalog {
    indexes: Vec<Index>,
}

fn cache_path(dir: &Path, letter: char) -> PathBuf {
    dir.join(format!("index_{}.ssidx", letter.to_ascii_uppercase()))
}

impl Catalog {
    /// 仅全量构建（不读写缓存），用于测试/示例。
    pub fn build_all() -> Catalog {
        let mut indexes = Vec::new();
        for letter in ntfs_fixed_drives() {
            if let Ok(idx) = build_index_for_drive(letter) {
                indexes.push(idx);
            }
        }
        Catalog { indexes }
    }

    /// 优先从缓存加载并做 USN 追赶；缓存无效则全量重建并写缓存。
    pub fn build_or_load(cache_dir: &Path) -> Catalog {
        let _ = std::fs::create_dir_all(cache_dir);
        let mut indexes = Vec::new();
        for letter in ntfs_fixed_drives() {
            if let Some(idx) = Self::try_load(cache_dir, letter) {
                indexes.push(idx);
            } else if let Ok(idx) = build_index_for_drive(letter) {
                let _ = persist::save_index(&idx, &cache_path(cache_dir, letter));
                indexes.push(idx);
            }
        }
        Catalog { indexes }
    }

    /// 尝试加载某盘缓存并校验 + 追赶。失败返回 None（触发全量重建）。
    fn try_load(cache_dir: &Path, letter: char) -> Option<Index> {
        let mut idx = persist::load_index(&cache_path(cache_dir, letter)).ok()?;
        let (serial, journal_id, first_usn, _next) = current_usn_state(letter).ok()?;
        // 卷变化 / 日志重建 / 缓存太旧（日志已截断）→ 全量重建
        if idx.volume_serial() != serial || idx.usn_journal_id() != journal_id {
            return None;
        }
        if idx.next_usn() < first_usn {
            return None;
        }
        if let Ok((changes, new_next)) = read_changes(letter, journal_id, idx.next_usn()) {
            idx.apply_all(&changes);
            idx.set_next_usn(new_next);
        }
        Some(idx)
    }

    /// 增量追赶（供定时器周期调用），返回本次应用的变更总数。
    pub fn catch_up(&mut self) -> usize {
        let mut total = 0;
        for idx in &mut self.indexes {
            let letter = idx.drive_letter();
            let jid = idx.usn_journal_id();
            let from = idx.next_usn();
            if let Ok((changes, new_next)) = read_changes(letter, jid, from) {
                total += changes.len();
                if !changes.is_empty() {
                    idx.apply_all(&changes);
                }
                idx.set_next_usn(new_next);
            }
        }
        total
    }

    /// 保存所有盘索引到缓存。
    pub fn save_all(&self, cache_dir: &Path) {
        let _ = std::fs::create_dir_all(cache_dir);
        for idx in &self.indexes {
            let _ = persist::save_index(idx, &cache_path(cache_dir, idx.drive_letter()));
        }
    }

    pub fn is_empty(&self) -> bool {
        self.indexes.is_empty()
    }
    pub fn drive_letters(&self) -> Vec<char> {
        self.indexes.iter().map(|i| i.drive_letter()).collect()
    }
    pub fn total_nodes(&self) -> usize {
        self.indexes.iter().map(|i| i.len()).sum()
    }
    pub fn memory_bytes(&self) -> usize {
        self.indexes.iter().map(|i| i.memory_bytes()).sum()
    }

    /// 搜索。`drive=None` 搜索全部盘；`Some(letter)` 仅该盘。结果总数不超过 `limit`。
    pub fn search(&self, query: &str, drive: Option<char>, limit: usize) -> Vec<SearchResult> {
        let mut out = Vec::new();
        let want = drive.map(|d| d.to_ascii_uppercase());
        for idx in &self.indexes {
            if let Some(d) = want {
                if idx.drive_letter() != d {
                    continue;
                }
            }
            let remaining = limit.saturating_sub(out.len());
            if remaining == 0 {
                break;
            }
            out.extend(idx.search(query, remaining));
        }
        out
    }
}
