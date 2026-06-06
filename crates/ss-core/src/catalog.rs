//! 多盘目录：每个盘一个 [`Index`]，统一搜索、可按盘符过滤；
//! 支持磁盘缓存（秒开）与 USN 增量更新。

use std::path::{Path, PathBuf};

use crate::category::Category;
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
    /// `only` 非空时只索引其中的盘符（大写）；空 = 全部固定 NTFS 盘。
    pub fn build_or_load(cache_dir: &Path, only: &[char]) -> Catalog {
        let _ = std::fs::create_dir_all(cache_dir);
        let only_up: Vec<char> = only.iter().map(|c| c.to_ascii_uppercase()).collect();
        let mut indexes = Vec::new();
        for letter in ntfs_fixed_drives() {
            if !only_up.is_empty() && !only_up.contains(&letter) {
                continue;
            }
            if let Some(idx) = Self::try_load(cache_dir, letter) {
                indexes.push(idx);
            } else if let Ok(idx) = build_index_for_drive(letter) {
                if let Err(_e) = persist::save_index(&idx, &cache_path(cache_dir, letter)) {
                    #[cfg(debug_assertions)]
                    eprintln!("SaveSearch 保存索引({letter})失败: {}", _e);
                }
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
            if let Err(_e) = persist::save_index(idx, &cache_path(cache_dir, idx.drive_letter())) {
                #[cfg(debug_assertions)]
                eprintln!("SaveSearch 保存索引({})失败: {}", idx.drive_letter(), _e);
            }
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

    /// 搜索。`folder=Some(路径)` 只在该文件夹子树内搜（优先于 drive）；
    /// 否则 `drive=None` 搜全部盘、`Some(letter)` 仅该盘。`category` 按类型过滤。
    pub fn search(
        &self,
        query: &str,
        drive: Option<char>,
        folder: Option<&str>,
        category: Category,
        limit: usize,
    ) -> Vec<SearchResult> {
        // 文件夹子树范围
        if let Some(folder) = folder {
            let dl = folder
                .chars()
                .next()
                .map(|c| c.to_ascii_uppercase());
            for idx in &self.indexes {
                if Some(idx.drive_letter()) == dl {
                    return match idx.find_dir_by_path(folder) {
                        Some(anc) => idx.search_under(query, category, anc, limit),
                        None => Vec::new(),
                    };
                }
            }
            return Vec::new();
        }

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
            out.extend(idx.search(query, category, remaining));
        }
        out
    }
}
