//! 多盘目录：每个盘一个 [`Index`]，统一搜索、可按盘符过滤；
//! 支持磁盘缓存（秒开）与 USN 增量更新。

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Mutex;
use std::time::Instant;

use crate::category::{Category, CategoryCounts};
use crate::drives::{list_volumes, ntfs_fixed_drives};
use crate::index::{compile_query, Change, Index, ScanSpec, ScoreKey, SearchResult};
use crate::mft::{
    build_index_for_drive, build_index_for_drive_with_progress, current_usn_state, read_changes,
};
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

/// 一个盘的索引状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriveState {
    /// 正在读取缓存。
    Loading,
    /// 正在枚举 MFT。`estimated` 来自 MFT 大小，取不到时为 0；最终数量通常小于它。
    Building { done: usize, estimated: usize },
    Ready,
    Failed { reason: String },
}

/// 一个盘的状态和最近一次不影响使用的错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriveStatus {
    pub letter: char,
    pub state: DriveState,
    /// 索引仍可搜索，但可能不是最新（例如读取 USN 日志失败）。成功追赶后清除。
    pub last_error: Option<String>,
}

/// 加载结果：索引和不影响使用的错误。
type LoadedDrive = (Index, Option<String>);

/// 一次搜索请求。
pub struct SearchRequest<'a> {
    pub query: &'a str,
    pub scope: Scope,
    pub category: Category,
    pub limit: usize,
    /// 置为 true 后，扫描线程会尽快停止。
    pub cancel: Option<&'a AtomicBool>,
    /// 不搜索这些目录的子树（完整路径）。不存在的路径会被忽略。
    pub exclude_dirs: &'a [String],
    /// 不要带隐藏或系统属性的节点，也不要这类目录下的节点。
    pub hide_system: bool,
}

/// 排除目录的解析结果。`find_dir_by_path` 是线性扫描，不缓存则每次击键都要重跑。
struct ExcludeCache {
    generation: u64,
    /// 每个路径解析到的（盘下标，目录节点 id）。
    entries: Vec<(String, Option<(usize, u32)>)>,
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
    /// 按盘符排序的状态，包含加载失败、没有索引的盘。
    statuses: Vec<DriveStatus>,
    exclude_cache: Mutex<Option<ExcludeCache>>,
}

fn cache_path(dir: &Path, letter: char) -> PathBuf {
    dir.join(format!("index_{}.ssidx", letter.to_ascii_uppercase()))
}

impl Catalog {
    fn from_indexes(indexes: Vec<Index>) -> Catalog {
        let statuses = indexes
            .iter()
            .map(|i| DriveStatus {
                letter: i.drive_letter(),
                state: DriveState::Ready,
                last_error: None,
            })
            .collect();
        Catalog {
            indexes,
            statuses,
            folder_cache: Mutex::new(None),
            generation: 0,
            exclude_cache: Mutex::new(None),
        }
    }

    /// 仅全量构建（不读写缓存），用于测试/示例。
    pub fn build_all() -> Catalog {
        let mut cat = Self::from_indexes(Vec::new());
        for letter in ntfs_fixed_drives() {
            match build_index_for_drive(letter) {
                Ok(idx) => cat.push_loaded(idx, None),
                Err(e) => cat.push_failed(letter, e.to_string()),
            }
        }
        cat
    }

    /// 优先从缓存加载并做 USN 追赶；缓存无效则全量重建并写缓存。
    /// `only` 非空时只索引其中的盘符（大写）；空 = 全部固定 NTFS 盘。
    /// 各盘并行加载/构建（缓存读取与 MFT 枚举都是 I/O 为主，多盘可叠加）。
    pub fn build_or_load(cache_dir: &Path, only: &[char]) -> Catalog {
        Self::build_or_load_with_progress(cache_dir, only, &|_, _| {})
    }

    /// 同 [`build_or_load`](Self::build_or_load)，并通过 `on_event` 报告每个盘的状态变化。
    /// `on_event` 可能在各盘的工作线程上调用。结果也可以之后用
    /// [`drive_statuses`](Self::drive_statuses) 查询。实盘行为用 `examples/volumes.rs` 人工验证。
    pub fn build_or_load_with_progress(
        cache_dir: &Path,
        only: &[char],
        on_event: &(dyn Fn(char, DriveState) + Sync),
    ) -> Catalog {
        let _ = std::fs::create_dir_all(cache_dir);
        let only_up: Vec<char> = only.iter().map(|c| c.to_ascii_uppercase()).collect();
        let letters: Vec<char> = ntfs_fixed_drives()
            .into_iter()
            .filter(|l| only_up.is_empty() || only_up.contains(l))
            .collect();
        let outcomes: Vec<(char, Result<LoadedDrive, String>)> = std::thread::scope(|s| {
            let handles: Vec<_> = letters
                .iter()
                .map(|&letter| {
                    (
                        letter,
                        s.spawn(move || Self::load_or_build(cache_dir, letter, on_event)),
                    )
                })
                .collect();
            handles
                .into_iter()
                .map(|(letter, h)| {
                    let outcome = h
                        .join()
                        .unwrap_or_else(|_| Err("索引线程异常退出".to_string()));
                    (letter, outcome)
                })
                .collect()
        });

        let mut cat = Self::from_indexes(Vec::new());
        for (letter, outcome) in outcomes {
            match outcome {
                Ok((idx, warning)) => cat.push_loaded(idx, warning),
                Err(reason) => {
                    on_event(letter, DriveState::Failed { reason: reason.clone() });
                    cat.push_failed(letter, reason);
                }
            }
        }
        // 用户选了但当前不可索引的盘（已拔出、换了文件系统）也要给出原因。
        for letter in only_up {
            if !letters.contains(&letter) && !cat.statuses.iter().any(|s| s.letter == letter) {
                let reason = list_volumes()
                    .into_iter()
                    .find(|v| v.letter == letter)
                    .and_then(|v| v.reason)
                    .unwrap_or_else(|| "盘不存在".to_string());
                on_event(letter, DriveState::Failed { reason: reason.clone() });
                cat.push_failed(letter, reason);
            }
        }
        cat.statuses.sort_by_key(|s| s.letter);
        cat
    }

    fn push_loaded(&mut self, idx: Index, warning: Option<String>) {
        self.statuses.push(DriveStatus {
            letter: idx.drive_letter(),
            state: DriveState::Ready,
            last_error: warning,
        });
        self.indexes.push(idx);
    }

    fn push_failed(&mut self, letter: char, reason: String) {
        self.statuses.push(DriveStatus {
            letter,
            state: DriveState::Failed { reason },
            last_error: None,
        });
    }

    /// 加载或构建一个盘。成功时附带不影响使用的错误（USN 追赶失败、缓存写入失败）。
    fn load_or_build(
        cache_dir: &Path,
        letter: char,
        on_event: &(dyn Fn(char, DriveState) + Sync),
    ) -> Result<LoadedDrive, String> {
        on_event(letter, DriveState::Loading);
        if let Some(loaded) = Self::try_load(cache_dir, letter) {
            on_event(letter, DriveState::Ready);
            return Ok(loaded);
        }
        let idx = build_index_for_drive_with_progress(letter, &|done, estimated| {
            on_event(letter, DriveState::Building { done, estimated });
        })
        .map_err(|e| e.to_string())?;
        let warning = persist::save_index(&idx, &cache_path(cache_dir, letter))
            .err()
            .map(|e| format!("保存索引缓存失败：{e}"));
        on_event(letter, DriveState::Ready);
        Ok((idx, warning))
    }

    /// 尝试加载某盘缓存并校验 + 追赶。失败返回 None（触发全量重建）。
    fn try_load(cache_dir: &Path, letter: char) -> Option<LoadedDrive> {
        let mut idx = persist::load_index(&cache_path(cache_dir, letter)).ok()?;
        let (serial, journal_id, first_usn, _next) = current_usn_state(letter).ok()?;
        // 卷变化 / 日志重建 / 缓存太旧（日志已截断）→ 全量重建
        if idx.volume_serial() != serial || idx.usn_journal_id() != journal_id {
            return None;
        }
        if idx.next_usn() < first_usn {
            return None;
        }
        let warning = match read_changes(letter, journal_id, idx.next_usn()) {
            Ok((changes, new_next)) => {
                idx.apply_all(&changes);
                idx.set_next_usn(new_next);
                None
            }
            Err(e) => Some(format!("读取文件变更失败：{e}")),
        };
        Some((idx, warning))
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

    fn position_matches(&self, letter: char, journal_id: u64, from: i64) -> bool {
        self.indexes.iter().any(|idx| {
            idx.drive_letter() == letter
                && idx.usn_journal_id() == journal_id
                && idx.next_usn() == from
        })
    }

    /// 应用锁外读取的结果；索引已被替换、移除或推进时丢弃过期结果。
    /// 成功（包括空轮询）清除旧错误，失败保留旧索引并记录原因。
    pub fn apply_drive_update(
        &mut self,
        letter: char,
        journal_id: u64,
        from: i64,
        result: Result<(Vec<Change>, i64), String>,
    ) -> bool {
        if !self.position_matches(letter, journal_id, from) {
            return false;
        }
        let error = match result {
            Ok((changes, next)) => {
                self.apply_drive_changes(letter, &changes, next);
                None
            }
            Err(reason) => Some(reason),
        };
        if let Some(status) = self.statuses.iter_mut().find(|s| s.letter == letter) {
            status.last_error = error;
        }
        true
    }

    /// 恢复期间若该盘已被移除或换入新索引，不覆盖更新的状态。
    pub fn replace_index_if_current(&mut self, idx: Index, journal_id: u64, from: i64) -> bool {
        if !self.position_matches(idx.drive_letter(), journal_id, from) {
            return false;
        }
        self.replace_index(idx);
        true
    }

    /// 增量追赶（供定时器周期调用），返回本次应用的变更总数。
    pub fn catch_up(&mut self) -> usize {
        let mut total = 0;
        for (letter, jid, from) in self.usn_positions() {
            let last_error = match read_changes(letter, jid, from) {
                Ok((changes, new_next)) => {
                    total += changes.len();
                    self.apply_drive_changes(letter, &changes, new_next);
                    None
                }
                Err(e) if crate::mft::usn_requires_rebuild(&e) => {
                    match build_index_for_drive(letter) {
                        Ok(idx) => {
                            self.replace_index(idx);
                            None
                        }
                        Err(rebuild_error) => Some(format!("重建索引失败：{rebuild_error}")),
                    }
                }
                Err(e) => Some(format!("读取文件变更失败：{e}")),
            };
            if let Some(status) = self.statuses.iter_mut().find(|s| s.letter == letter) {
                status.last_error = last_error;
            }
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
        let new_idx_letter = new_idx.drive_letter();
        match self
            .indexes
            .iter_mut()
            .find(|i| i.drive_letter() == new_idx.drive_letter())
        {
            Some(slot) => *slot = new_idx,
            None => self.indexes.push(new_idx),
        }
        self.generation += 1;
        let letter = new_idx_letter;
        self.statuses.retain(|s| s.letter != letter);
        self.statuses.push(DriveStatus {
            letter,
            state: DriveState::Ready,
            last_error: None,
        });
        self.statuses.sort_by_key(|s| s.letter);
        // 节点 id 已全部变化，文件夹解析缓存作废（校验也会拦住，这里直接清掉更省）
        *self.folder_cache.lock().unwrap_or_else(|p| p.into_inner()) = None;
    }

    /// 移除某盘的索引和状态，返回是否有变化。磁盘缓存不动，由调用方决定。
    pub fn remove_drive(&mut self, letter: char) -> bool {
        let letter = letter.to_ascii_uppercase();
        let before = (self.indexes.len(), self.statuses.len());
        self.indexes.retain(|i| i.drive_letter() != letter);
        self.statuses.retain(|s| s.letter != letter);
        if (self.indexes.len(), self.statuses.len()) == before {
            return false;
        }
        self.generation += 1;
        // 缓存里的盘下标已失效
        *self.folder_cache.lock().unwrap_or_else(|p| p.into_inner()) = None;
        true
    }

    /// 各盘的索引状态，包含加载失败的盘，按盘符排序。
    pub fn drive_statuses(&self) -> Vec<DriveStatus> {
        self.statuses.clone()
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
    /// 每个盘的节点数：（盘符，节点数）。
    pub fn drive_node_counts(&self) -> Vec<(char, usize)> {
        self.indexes.iter().map(|i| (i.drive_letter(), i.len())).collect()
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

    /// 把一个路径解析成（盘下标，目录节点 id）。
    fn resolve_dir(&self, path: &str) -> Option<(usize, u32)> {
        let di = path.chars().next().and_then(|c| self.drive_position(c))?;
        let id = self.indexes[di].find_dir_by_path(path)?;
        Some((di, id))
    }

    /// 返回每个盘要排除的目录节点 id。结果带缓存：路径列表和 generation 都没变时直接复用；
    /// 只有 generation 变了时，已解析的条目用 `is_live_dir_at` 校验，其余的才重新解析。
    fn resolve_excludes(&self, dirs: &[String]) -> Vec<Vec<u32>> {
        let mut per_drive = vec![Vec::new(); self.indexes.len()];
        if dirs.is_empty() {
            return per_drive;
        }
        let mut cache = self.exclude_cache.lock().unwrap_or_else(|p| p.into_inner());
        let same_dirs = cache.as_ref().is_some_and(|c| {
            c.entries.len() == dirs.len() && c.entries.iter().zip(dirs).all(|(e, d)| &e.0 == d)
        });
        if !same_dirs {
            *cache = Some(ExcludeCache {
                generation: self.generation,
                entries: dirs.iter().map(|d| (d.clone(), self.resolve_dir(d))).collect(),
            });
        } else if let Some(c) = cache.as_mut().filter(|c| c.generation != self.generation) {
            c.generation = self.generation;
            for (path, resolved) in &mut c.entries {
                let still_valid = resolved.is_some_and(|(di, id)| {
                    self.indexes.get(di).is_some_and(|idx| idx.is_live_dir_at(id, path))
                });
                if !still_valid {
                    *resolved = self.resolve_dir(path);
                }
            }
        }
        if let Some(c) = cache.as_ref() {
            for (di, id) in c.entries.iter().filter_map(|(_, r)| *r) {
                per_drive[di].push(id);
            }
        }
        per_drive
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
        let excluded = self.resolve_excludes(req.exclude_dirs);

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
                excluded: &excluded[di],
                hide_system: req.hide_system,
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
            exclude_dirs: &[],
            hide_system: false,
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
            exclude_dirs: &[],
            hide_system: false,
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
    fn node_counts_are_per_drive() {
        let cat = sample_catalog();
        let counts = cat.drive_node_counts();
        assert_eq!(counts.len(), 2);
        assert_eq!(counts.iter().map(|c| c.1).sum::<usize>(), cat.total_nodes());
        assert_eq!(counts[0].0, 'C');
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
    fn exclude_dirs_hide_subtree_and_survive_index_changes() {
        let mut c = IndexBuilder::new(b'C');
        c.push(100, 5, DIR, "node_modules");
        c.push(101, 100, DIR, "pkg");
        c.push(200, 101, 0, "index_deep.js");
        c.push(201, 5, 0, "index_top.js");
        let mut cat = Catalog::from_indexes(vec![c.finish()]);

        let dirs = vec!["c:\\NODE_MODULES\\".to_string(), "C:\\later".to_string(), "Z:\\x".to_string()];
        let mut r = req("index", Scope::All);
        r.exclude_dirs = &dirs;
        let resp = cat.query(&r);
        assert_eq!(resp.total_hits, 1);
        assert_eq!(resp.items[0].name, "index_top.js");

        // 排除目录中还不存在的路径，出现后 generation 变化，会重新解析
        cat.apply_drive_changes('C', &[
            Change::Upsert { frn: 300, parent_frn: 5, attrs: DIR, name: "later".into() },
            Change::Upsert { frn: 301, parent_frn: 300, attrs: 0, name: "index_later.js".into() },
        ], 1);
        let resp = cat.query(&SearchRequest { exclude_dirs: &dirs, ..req("index", Scope::All) });
        assert_eq!(resp.total_hits, 1);

        // 已解析的目录被删除后重建：旧 id 校验失败，解析到新节点
        cat.apply_drive_changes('C', &[
            Change::Delete { frn: 200 },
            Change::Delete { frn: 101 },
            Change::Delete { frn: 100 },
            Change::Upsert { frn: 400, parent_frn: 5, attrs: DIR, name: "node_modules".into() },
            Change::Upsert { frn: 401, parent_frn: 400, attrs: 0, name: "index_new.js".into() },
        ], 2);
        let resp = cat.query(&SearchRequest { exclude_dirs: &dirs, ..req("index", Scope::All) });
        assert_eq!(resp.total_hits, 1);
        assert_eq!(resp.items[0].name, "index_top.js");

        // 不排除时全部可见
        assert_eq!(cat.query(&req("index", Scope::All)).total_hits, 3);
    }

    #[test]
    fn query_applies_hide_system() {
        let mut c = IndexBuilder::new(b'C');
        c.push(100, 5, 0x02, "x_hidden.txt");
        c.push(101, 5, 0, "x_normal.txt");
        let cat = Catalog::from_indexes(vec![c.finish()]);
        assert_eq!(cat.query(&req("x_", Scope::All)).total_hits, 2);
        let resp = cat.query(&SearchRequest { hide_system: true, ..req("x_", Scope::All) });
        assert_eq!(resp.total_hits, 1);
        assert_eq!(resp.items[0].name, "x_normal.txt");
        // 旧接口不过滤
        assert_eq!(cat.search("x_", None, None, Category::All, 10).len(), 2);
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
    fn drive_sync_records_errors_and_recovers_without_accepting_stale_results() {
        let mut cat = sample_catalog();
        let (_, jid, from) = cat.usn_positions()[0];
        assert!(cat.apply_drive_update('C', jid, from, Err("journal expired".into())));
        assert_eq!(cat.drive_statuses()[0].last_error.as_deref(), Some("journal expired"));
        // 成功的空轮询也必须清除上次错误。
        assert!(cat.apply_drive_update('C', jid, from, Ok((vec![], from))));
        assert!(cat.drive_statuses()[0].last_error.is_none());

        let mut b = IndexBuilder::new(b'C');
        b.push(999, 5, 0, "recovered.txt");
        let mut recovered = b.finish();
        recovered.set_usn_state(1, jid + 1, from + 10);
        assert!(cat.replace_index_if_current(recovered, jid, from));
        assert_eq!(cat.query(&req("recovered", Scope::All)).total_hits, 1);
        assert!(!cat.apply_drive_update('C', jid, from, Err("stale error".into())));
        assert!(!cat.apply_drive_update('C', jid, from, Ok((vec![Change::Delete { frn: 999 }], from + 20))));
        assert_eq!(cat.query(&req("recovered", Scope::All)).total_hits, 1);
        assert!(cat.drive_statuses()[0].last_error.is_none());

        cat.remove_drive('C');
        assert!(!cat.replace_index_if_current(IndexBuilder::new(b'C').finish(), jid + 1, from + 10));
        assert!(!cat.drive_letters().contains(&'C'));
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
    fn remove_drive_updates_scopes_status_and_generation() {
        let mut cat = sample_catalog();
        let g0 = cat.generation();
        // 先让文件夹缓存指向 D 盘（下标 1），移除 C 后下标必须失效
        assert_eq!(cat.query(&req("report", Scope::Folder("D:\\Work".into()))).items.len(), 1);
        assert!(cat.remove_drive('c'));
        assert_eq!(cat.generation(), g0 + 1);
        assert_eq!(cat.drive_letters(), ['D']);
        assert_eq!(cat.drive_statuses().len(), 1);
        assert_eq!(
            cat.query(&req("report", Scope::Drive('C'))).scope_status,
            ScopeStatus::DriveNotIndexed
        );
        assert_eq!(cat.query(&req("report", Scope::Folder("D:\\Work".into()))).items.len(), 1);
        // 再次移除：没有变化，不递增
        assert!(!cat.remove_drive('C'));
        assert_eq!(cat.generation(), g0 + 1);
    }

    #[test]
    fn replace_index_marks_drive_ready() {
        let mut cat = sample_catalog();
        cat.push_failed('E', "拒绝访问".into());
        assert!(matches!(
            cat.drive_statuses().iter().find(|s| s.letter == 'E').unwrap().state,
            DriveState::Failed { .. }
        ));
        cat.replace_index(IndexBuilder::new(b'E').finish());
        let statuses = cat.drive_statuses();
        assert_eq!(statuses.iter().map(|s| s.letter).collect::<Vec<_>>(), ['C', 'D', 'E']);
        assert!(statuses.iter().all(|s| s.state == DriveState::Ready));
    }

    #[test]
    fn selected_drive_that_is_absent_reports_failure() {
        let used: Vec<char> = crate::drives::list_volumes().iter().map(|v| v.letter).collect();
        let absent = ('A'..='Z').rev().find(|c| !used.contains(c)).expect("有未分配的盘符");
        let dir = std::env::temp_dir().join(format!("ss-core-cat-test-{}", std::process::id()));
        let events = Mutex::new(Vec::new());
        let cat = Catalog::build_or_load_with_progress(&dir, &[absent], &|l, s| {
            events.lock().unwrap().push((l, s));
        });
        let _ = std::fs::remove_dir_all(&dir);
        assert!(cat.is_empty());
        let statuses = cat.drive_statuses();
        assert_eq!(statuses.len(), 1);
        let failed = DriveState::Failed { reason: "盘不存在".to_string() };
        assert_eq!(statuses[0].state, failed);
        assert_eq!(events.into_inner().unwrap(), [(absent, failed)]);
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
