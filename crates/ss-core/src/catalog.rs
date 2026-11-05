//! 多盘目录：每个盘一个 [`Index`]，统一搜索、可按盘符过滤；
//! 支持磁盘缓存（秒开）与 USN 增量更新。

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Mutex;
use std::time::Instant;

use crate::category::{Category, CategoryCounts};
use crate::drives::ntfs_fixed_drives;
use crate::index::{compile_query, Change, Index, ScanSpec, ScoreKey, SearchResult};
use crate::mft::{build_index_for_drive, current_usn_state, read_changes};
use crate::persist;

/// 搜索范围。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    All,
    Drive(char),
    /// 只搜索该文件夹子树（完整路径）。
    Folder(String),
}

/// 搜索范围是否有效。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeStatus {
    Ok,
    /// 文件夹已不存在（被删除、改名，或不在索引中）。
    FolderMissing,
    DriveNotIndexed,
}

/// 一次搜索请求。
pub struct SearchRequest<'a> {
    pub query: &'a str,
    pub scope: Scope,
    pub category: Category,
    pub limit: usize,
    /// 置为 true 后，扫描线程会尽快停止。
    pub cancel: Option<&'a AtomicBool>,
}

/// 一次搜索的结果。
#[derive(Debug)]
pub struct SearchResponse {
    pub items: Vec<SearchResult>,
    /// 全部命中数，不受 `limit` 影响。取消时只统计已扫描的部分。
    pub total_hits: usize,
    /// `total_hits > items.len()`。
    pub truncated: bool,
    /// 不看分类筛选时各分类的命中数，用于在分类标签上显示。
    pub category_counts: CategoryCounts,
    pub scope_status: ScopeStatus,
    pub cancelled: bool,
    pub elapsed_ms: u32,
    /// 产生这次结果时的索引版本，见 [`Catalog::generation`]。
    pub generation: u64,
}

/// 多个盘的索引集合。
pub struct Catalog {
    indexes: Vec<Index>,
    /// 「预设文件夹路径 → (盘下标, 目录节点 id)」单条解析缓存。
    /// `find_dir_by_path` 是 O(节点数 × 深度) 线性扫描，不缓存则每次击键都重跑；
    /// 命中时用 `Index::is_live_dir_at` 反查路径校验（目录被删/改名/重建即失效重解析）。
    folder_cache: Mutex<Option<(String, usize, u32)>>,
    generation: u64,
}

fn cache_path(dir: &Path, letter: char) -> PathBuf {
    dir.join(format!("index_{}.ssidx", letter.to_ascii_uppercase()))
}

impl Catalog {
    fn from_indexes(indexes: Vec<Index>) -> Catalog {
        Catalog {
            indexes,
            folder_cache: Mutex::new(None),
            generation: 0,
        }
    }

    /// 仅全量构建（不读写缓存），用于测试/示例。
    pub fn build_all() -> Catalog {
        let mut indexes = Vec::new();
        for letter in ntfs_fixed_drives() {
            if let Ok(idx) = build_index_for_drive(letter) {
                indexes.push(idx);
            }
        }
        Self::from_indexes(indexes)
    }

    /// 优先从缓存加载并做 USN 追赶；缓存无效则全量重建并写缓存。
    /// `only` 非空时只索引其中的盘符（大写）；空 = 全部固定 NTFS 盘。
    /// 各盘并行加载/构建（缓存读取与 MFT 枚举都是 I/O 为主，多盘可叠加）。
    pub fn build_or_load(cache_dir: &Path, only: &[char]) -> Catalog {
        let _ = std::fs::create_dir_all(cache_dir);
        let only_up: Vec<char> = only.iter().map(|c| c.to_ascii_uppercase()).collect();
        let letters: Vec<char> = ntfs_fixed_drives()
            .into_iter()
            .filter(|l| only_up.is_empty() || only_up.contains(l))
            .collect();
        let indexes: Vec<Index> = std::thread::scope(|s| {
            let handles: Vec<_> = letters
                .iter()
                .map(|&letter| s.spawn(move || Self::load_or_build(cache_dir, letter)))
                .collect();
            handles
                .into_iter()
                .filter_map(|h| h.join().ok().flatten())
                .collect()
        });
        Self::from_indexes(indexes)
    }

    fn load_or_build(cache_dir: &Path, letter: char) -> Option<Index> {
        if let Some(idx) = Self::try_load(cache_dir, letter) {
            return Some(idx);
        }
        let idx = build_index_for_drive(letter).ok()?;
        if let Err(_e) = persist::save_index(&idx, &cache_path(cache_dir, letter)) {
            #[cfg(debug_assertions)]
            eprintln!("SaveSearch 保存索引({letter})失败: {}", _e);
        }
        Some(idx)
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

    /// 各盘当前 USN 读取位置：(盘符, 日志ID, 起始USN)。
    /// 供调用方在锁外做 `read_changes` 磁盘 I/O，再用 [`apply_drive_changes`] 短暂持锁应用，
    /// 避免共享锁在 I/O 期间被长占（卡 UI 搜索）。
    pub fn usn_positions(&self) -> Vec<(char, u64, i64)> {
        self.indexes
            .iter()
            .map(|i| (i.drive_letter(), i.usn_journal_id(), i.next_usn()))
            .collect()
    }

    /// 应用某盘的一批增量变更并推进该盘 USN 位点。盘符不存在时忽略。
    pub fn apply_drive_changes(&mut self, letter: char, changes: &[Change], new_next: i64) {
        for idx in &mut self.indexes {
            if idx.drive_letter() == letter {
                if idx.apply_all(changes) > 0 {
                    self.generation += 1;
                }
                idx.set_next_usn(new_next);
                return;
            }
        }
    }

    /// 增量追赶（供定时器周期调用），返回本次应用的变更总数。
    pub fn catch_up(&mut self) -> usize {
        let mut total = 0;
        let mut changed = false;
        for idx in &mut self.indexes {
            let letter = idx.drive_letter();
            let jid = idx.usn_journal_id();
            let from = idx.next_usn();
            if let Ok((changes, new_next)) = read_changes(letter, jid, from) {
                total += changes.len();
                changed |= idx.apply_all(&changes) > 0;
                idx.set_next_usn(new_next);
            }
        }
        if changed {
            self.generation += 1;
        }
        total
    }

    /// 墓碑超阈值、建议全量重建的盘符列表（供 USN 轮询线程在读锁下快速查询）。
    pub fn drives_needing_rebuild(&self) -> Vec<char> {
        self.indexes
            .iter()
            .filter(|i| i.needs_rebuild())
            .map(|i| i.drive_letter())
            .collect()
    }

    /// 用新建索引替换同盘符旧索引（盘符不存在则追加）。
    /// 调用方持写锁；本函数只做换指针，旧索引在写锁内 drop。
    pub fn replace_index(&mut self, new_idx: Index) {
        match self
            .indexes
            .iter_mut()
            .find(|i| i.drive_letter() == new_idx.drive_letter())
        {
            Some(slot) => *slot = new_idx,
            None => self.indexes.push(new_idx),
        }
        self.generation += 1;
        // 节点 id 已全部变化，文件夹解析缓存作废（校验也会拦住，这里直接清掉更省）
        *self.folder_cache.lock().unwrap_or_else(|p| p.into_inner()) = None;
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

    /// 把预设文件夹路径解析为 (盘下标, 目录节点 id)，带单条缓存 + 有效性校验。
    fn resolve_folder(&self, folder: &str) -> Option<(usize, u32)> {
        let mut cache = self.folder_cache.lock().unwrap_or_else(|p| p.into_inner());
        if let Some((f, di, id)) = cache.as_ref() {
            if f == folder
                && self
                    .indexes
                    .get(*di)
                    .is_some_and(|idx| idx.is_live_dir_at(*id, folder))
            {
                return Some((*di, *id));
            }
        }
        let dl = folder.chars().next()?.to_ascii_uppercase();
        let di = self.indexes.iter().position(|i| i.drive_letter() == dl)?;
        let id = self.indexes[di].find_dir_by_path(folder)?;
        *cache = Some((folder.to_string(), di, id));
        Some((di, id))
    }

    /// 索引内容的版本号。只在实际应用变更或替换索引后递增，空轮询不递增。
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// 把搜索范围解析成要扫描的盘和子树根。
    fn resolve_scope(&self, scope: &Scope) -> (ScopeStatus, Vec<(usize, Option<u32>)>) {
        match scope {
            Scope::All => (
                ScopeStatus::Ok,
                (0..self.indexes.len()).map(|di| (di, None)).collect(),
            ),
            Scope::Drive(letter) => match self.drive_position(*letter) {
                Some(di) => (ScopeStatus::Ok, vec![(di, None)]),
                None => (ScopeStatus::DriveNotIndexed, Vec::new()),
            },
            Scope::Folder(folder) => {
                let indexed = folder.chars().next().and_then(|c| self.drive_position(c));
                if indexed.is_none() {
                    return (ScopeStatus::DriveNotIndexed, Vec::new());
                }
                match self.resolve_folder(folder) {
                    Some((di, anc)) => (ScopeStatus::Ok, vec![(di, Some(anc))]),
                    None => (ScopeStatus::FolderMissing, Vec::new()),
                }
            }
        }
    }

    fn drive_position(&self, letter: char) -> Option<usize> {
        let letter = letter.to_ascii_uppercase();
        self.indexes.iter().position(|i| i.drive_letter() == letter)
    }

    /// 执行一次搜索：各盘全量扫描 → 全局排序 → 取前 `limit` → 只为前 `limit` 条还原字符串。
    pub fn query(&self, req: &SearchRequest) -> SearchResponse {
        let started = Instant::now();
        let compiled = compile_query(req.query);
        let (scope_status, targets) = self.resolve_scope(&req.scope);

        let mut merged: Vec<(ScoreKey, usize, u32)> = Vec::new();
        let mut total_hits = 0usize;
        let mut category_counts = CategoryCounts::default();
        let mut cancelled = false;
        for (di, ancestor) in targets {
            let scan = self.indexes[di].scan_sorted(&ScanSpec {
                query: &compiled,
                category: req.category,
                ancestor,
                limit: req.limit,
                cancel: req.cancel,
            });
            total_hits += scan.total_hits;
            category_counts.merge(&scan.category_counts);
            merged.extend(scan.hits.into_iter().map(|(k, id)| (k, di, id)));
            if scan.cancelled {
                cancelled = true;
                break;
            }
        }
        merged.sort_unstable();
        merged.truncate(req.limit);
        let items: Vec<SearchResult> = merged
            .into_iter()
            .map(|(_, di, id)| self.indexes[di].materialize(id, &compiled))
            .collect();

        SearchResponse {
            truncated: total_hits > items.len(),
            category_counts,
            items,
            total_hits,
            scope_status,
            cancelled,
            elapsed_ms: started.elapsed().as_millis().min(u32::MAX as u128) as u32,
            generation: self.generation,
        }
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
        let scope = match (folder, drive) {
            (Some(f), _) => Scope::Folder(f.to_string()),
            (None, Some(d)) => Scope::Drive(d),
            (None, None) => Scope::All,
        };
        self.query(&SearchRequest {
            query,
            scope,
            category,
            limit,
            cancel: None,
        })
        .items
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::IndexBuilder;

    const DIR: u32 = 0x10;

    fn sample_catalog() -> Catalog {
        let mut c = IndexBuilder::new(b'C');
        c.push(100, 5, DIR, "Users");
        c.push(200, 100, 0, "report.txt");
        let mut d = IndexBuilder::new(b'D');
        d.push(100, 5, DIR, "Work");
        d.push(200, 100, 0, "report_d.txt");
        Catalog::from_indexes(vec![c.finish(), d.finish()])
    }

    fn req(query: &str, scope: Scope) -> SearchRequest<'_> {
        SearchRequest {
            query,
            scope,
            category: Category::All,
            limit: 10,
            cancel: None,
        }
    }

    #[test]
    fn query_reports_hits_and_truncation() {
        let cat = sample_catalog();
        let mut r = req("report", Scope::All);
        let all = cat.query(&r);
        assert_eq!((all.total_hits, all.items.len()), (2, 2));
        assert!(!all.truncated);
        assert_eq!(all.scope_status, ScopeStatus::Ok);
        r.limit = 1;
        let one = cat.query(&r);
        assert_eq!((one.total_hits, one.items.len()), (2, 1));
        assert!(one.truncated);

        let d = cat.query(&req("report", Scope::Drive('d')));
        assert_eq!(d.items[0].path, "D:\\Work\\report_d.txt");
        assert_eq!(d.total_hits, 1);
    }

    #[test]
    fn query_merges_category_counts_across_drives() {
        let cat = sample_catalog();
        let mut r = req("report", Scope::All);
        r.category = Category::Image;
        let resp = cat.query(&r);
        assert_eq!(resp.total_hits, 0);
        assert_eq!(resp.category_counts.get(Category::Document), 2);
        assert_eq!(resp.category_counts.get(Category::All), 2);
    }

    #[test]
    fn query_reports_scope_status() {
        let cat = sample_catalog();
        let ok = cat.query(&req("report", Scope::Folder("C:\\Users".into())));
        assert_eq!(ok.scope_status, ScopeStatus::Ok);
        assert_eq!(ok.items.len(), 1);

        let missing = cat.query(&req("report", Scope::Folder("C:\\Gone".into())));
        assert_eq!(missing.scope_status, ScopeStatus::FolderMissing);
        assert!(missing.items.is_empty());

        let drive = cat.query(&req("report", Scope::Drive('E')));
        assert_eq!(drive.scope_status, ScopeStatus::DriveNotIndexed);
        let folder = cat.query(&req("report", Scope::Folder("E:\\x".into())));
        assert_eq!(folder.scope_status, ScopeStatus::DriveNotIndexed);
    }

    #[test]
    fn query_stops_when_cancelled() {
        let cat = sample_catalog();
        let cancel = AtomicBool::new(true);
        let mut r = req("report", Scope::All);
        r.cancel = Some(&cancel);
        let resp = cat.query(&r);
        assert!(resp.cancelled);
        assert!(resp.items.is_empty());

        cancel.store(false, std::sync::atomic::Ordering::Relaxed);
        let resp = cat.query(&r);
        assert!(!resp.cancelled);
        assert_eq!(resp.items.len(), 2);
    }

    #[test]
    fn generation_increases_only_on_real_changes() {
        let mut cat = sample_catalog();
        let g0 = cat.generation();
        // 空轮询和不改变内容的变更都不递增，但 USN 位点仍然前进
        cat.apply_drive_changes('C', &[], 42);
        let same = Change::Upsert { frn: 200, parent_frn: 100, attrs: 0, name: "report.txt".into() };
        cat.apply_drive_changes('C', &[same], 43);
        assert_eq!(cat.generation(), g0);
        assert_eq!(cat.usn_positions()[0].2, 43);

        let new = Change::Upsert { frn: 300, parent_frn: 100, attrs: 0, name: "new.txt".into() };
        cat.apply_drive_changes('C', &[new], 44);
        assert_eq!(cat.generation(), g0 + 1);

        cat.replace_index(IndexBuilder::new(b'D').finish());
        assert_eq!(cat.generation(), g0 + 2);
        assert_eq!(cat.query(&req("new", Scope::All)).generation, g0 + 2);
    }

    #[test]
    fn legacy_search_uses_same_scopes() {
        let cat = sample_catalog();
        assert_eq!(cat.search("report", None, None, Category::All, 10).len(), 2);
        assert_eq!(cat.search("report", Some('C'), None, Category::All, 10).len(), 1);
        // folder 优先于 drive
        let r = cat.search("report", Some('C'), Some("D:\\Work"), Category::All, 10);
        assert_eq!(r[0].path, "D:\\Work\\report_d.txt");
        assert!(cat.search("", None, None, Category::All, 10).is_empty());
    }
}
