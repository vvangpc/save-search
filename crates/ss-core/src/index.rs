//! 紧凑文件索引：SoA + 字符串池 + 父引用编号 + FRN→id 映射（供 USN 增量更新）。
//!
//! 每个文件/目录是一个节点（id 为 `u32` 下标）。名字存共享字节池，节点只存
//! 「偏移 + 长度 + 父 id + 标志位」。完整路径沿父链反查。大小写不敏感（ASCII）
//! 通过查询时折叠实现，不再存小写副本池以省内存。

use std::collections::HashMap;

use crate::category::Category;

/// 节点标志位。
pub const IS_DIR: u16 = 1 << 0;
/// 已删除（USN 删除事件后置位，搜索时跳过；全量重建时清理）。
pub const IS_DELETED: u16 = 1 << 1;

/// 合成根节点 id（代表盘符根，如 `C:\`）。
pub const ROOT_ID: u32 = 0;

/// NTFS 根目录的文件记录号（FRN 低 48 位）。
const NTFS_ROOT_FILE_NUMBER: u64 = 5;
const FRN_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;

const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;

/// 一条搜索结果。
#[derive(Debug, Clone)]
pub struct SearchResult {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
}

/// USN 增量变更。
#[derive(Debug, Clone)]
pub enum Change {
    Upsert {
        frn: u64,
        parent_frn: u64,
        attrs: u32,
        name: String,
    },
    Delete {
        frn: u64,
    },
}

/// ASCII 大小写不敏感子串匹配。`needle` 必须已小写。
fn contains_ci(hay: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    if hay.len() < needle.len() {
        return false;
    }
    let first = needle[0];
    let last_start = hay.len() - needle.len();
    for i in 0..=last_start {
        if hay[i].to_ascii_lowercase() == first {
            let mut ok = true;
            for j in 1..needle.len() {
                if hay[i + j].to_ascii_lowercase() != needle[j] {
                    ok = false;
                    break;
                }
            }
            if ok {
                return true;
            }
        }
    }
    false
}

/// 一个查询词：已折叠小写的字节 + 是否含通配符（`*`/`?`）。
struct CompiledTerm {
    lower: Vec<u8>,
    has_wild: bool,
}

/// 把原始查询编译为词列表：按空白分词，每词预折叠小写并标记是否含通配符。
/// 空白被 `split_ascii_whitespace` 吃掉，不产生空词；空查询返回空 Vec。
fn compile_query(query: &str) -> Vec<CompiledTerm> {
    query
        .split_ascii_whitespace()
        .map(|w| {
            let lower: Vec<u8> = w.bytes().map(|b| b.to_ascii_lowercase()).collect();
            let has_wild = lower.iter().any(|&b| b == b'*' || b == b'?');
            CompiledTerm { lower, has_wild }
        })
        .collect()
}

/// 单个词是否命中文件名：无通配=子串匹配（复用 `contains_ci`）；含通配=子串锚定 glob。
fn term_matches(hay: &[u8], t: &CompiledTerm) -> bool {
    if t.has_wild {
        wildcard_contains_ci(hay, &t.lower)
    } else {
        contains_ci(hay, &t.lower)
    }
}

/// 所有词都命中才算匹配（AND）；任一不命中即短路。
fn all_terms_match(hay: &[u8], terms: &[CompiledTerm]) -> bool {
    terms.iter().all(|t| term_matches(hay, t))
}

/// 子串锚定的通配匹配：`pat` 能在 `hay` 任意起始位匹配即命中。`pat` 须已小写。
/// `*`=任意多字符（含 0），`?`=任意一字符，其余按 ASCII 折叠大小写比较。
fn wildcard_contains_ci(hay: &[u8], pat: &[u8]) -> bool {
    for start in 0..=hay.len() {
        if glob_at(&hay[start..], pat) {
            return true;
        }
    }
    false
}

/// 从 `h` 开头匹配 glob `pat`；`pat` 消费完即成功（尾部自由 = 子串语义）。
/// 经典「贪心 + 星号回溯」，无递归。
fn glob_at(h: &[u8], pat: &[u8]) -> bool {
    let (mut hi, mut pi) = (0usize, 0usize);
    let mut star_pi: Option<usize> = None;
    let mut star_hi = 0usize;
    loop {
        if pi < pat.len() {
            match pat[pi] {
                b'*' => {
                    star_pi = Some(pi);
                    star_hi = hi;
                    pi += 1;
                    continue;
                }
                b'?' => {
                    if hi < h.len() {
                        hi += 1;
                        pi += 1;
                        continue;
                    }
                }
                c => {
                    if hi < h.len() && h[hi].to_ascii_lowercase() == c {
                        hi += 1;
                        pi += 1;
                        continue;
                    }
                }
            }
        } else {
            return true; // pat 用尽即命中（不要求消费完 h）
        }
        // 失配：回溯到最近的 '*'，多吞一个字符再试
        if let Some(sp) = star_pi {
            star_hi += 1;
            if star_hi > h.len() {
                return false;
            }
            hi = star_hi;
            pi = sp + 1;
        } else {
            return false;
        }
    }
}

/// 搜索结果排序键。派生 `Ord` 按字段先后、各字段升序比较，`bool` 的 `false < true`，
/// 故命中项一律用否定语义（`not_*`，命中 = `false` 排在前）。
/// 顺序：完全匹配 > 前缀匹配 > 目录 > 浅路径 > 名称字母序。
#[derive(PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct ScoreKey {
    not_exact: bool,
    not_prefix: bool,
    not_dir: bool,
    depth: u16,
    name_lower: Vec<u8>,
}

/// 候选收集上限：兜底极端泛查询（如单字符），限制内存/CPU。
const CAND_CAP_MIN: usize = 2000;
fn cand_cap(limit: usize) -> usize {
    limit.saturating_mul(20).max(CAND_CAP_MIN)
}

/// 增量构建器：枚举 MFT 时逐条 `push`，最后 `finish` 得到 `Index`。
pub struct IndexBuilder {
    names_off: Vec<u32>,
    names_len: Vec<u16>,
    parent_frn: Vec<u64>, // 临时：完成时解析为父 id
    flags: Vec<u16>,
    name_pool: Vec<u8>,
    frn_to_id: HashMap<u64, u32>,
    drive_letter: u8,
}

impl IndexBuilder {
    pub fn new(drive_letter: u8) -> Self {
        let mut b = IndexBuilder {
            names_off: Vec::new(),
            names_len: Vec::new(),
            parent_frn: Vec::new(),
            flags: Vec::new(),
            name_pool: Vec::new(),
            frn_to_id: HashMap::new(),
            drive_letter,
        };
        // 合成根节点（id 0）
        b.names_off.push(0);
        b.names_len.push(0);
        b.parent_frn.push(0);
        b.flags.push(IS_DIR);
        b
    }

    pub fn reserve(&mut self, n: usize) {
        self.names_off.reserve(n);
        self.names_len.reserve(n);
        self.parent_frn.reserve(n);
        self.flags.reserve(n);
        self.frn_to_id.reserve(n);
    }

    #[inline]
    pub fn is_root_frn(frn: u64) -> bool {
        (frn & FRN_MASK) == NTFS_ROOT_FILE_NUMBER
    }

    pub fn push(&mut self, frn: u64, parent_frn: u64, attrs: u32, name: &str) {
        let id = self.names_len.len() as u32;
        let bytes = name.as_bytes();
        let off = self.name_pool.len() as u32;
        self.name_pool.extend_from_slice(bytes);
        self.names_off.push(off);
        self.names_len.push(bytes.len().min(u16::MAX as usize) as u16);
        let mut f = 0u16;
        if attrs & FILE_ATTRIBUTE_DIRECTORY != 0 {
            f |= IS_DIR;
        }
        self.flags.push(f);
        self.parent_frn.push(parent_frn);
        self.frn_to_id.insert(frn, id);
    }

    pub fn finish(self) -> Index {
        let n = self.names_len.len();
        let mut parent = Vec::with_capacity(n);
        for (i, &pf) in self.parent_frn.iter().enumerate() {
            if i as u32 == ROOT_ID {
                parent.push(u32::MAX);
            } else {
                parent.push(self.frn_to_id.get(&pf).copied().unwrap_or(ROOT_ID));
            }
        }
        Index {
            names_off: self.names_off,
            names_len: self.names_len,
            parent,
            flags: self.flags,
            name_pool: self.name_pool,
            frn_to_id: self.frn_to_id,
            drive_letter: self.drive_letter,
            volume_serial: 0,
            usn_journal_id: 0,
            next_usn: 0,
            tombstones: 0,
        }
    }
}

/// 只读 + 可增量更新的紧凑索引。
pub struct Index {
    pub(crate) names_off: Vec<u32>,
    pub(crate) names_len: Vec<u16>,
    pub(crate) parent: Vec<u32>,
    pub(crate) flags: Vec<u16>,
    pub(crate) name_pool: Vec<u8>,
    pub(crate) frn_to_id: HashMap<u64, u32>,
    pub(crate) drive_letter: u8,
    pub(crate) volume_serial: u64,
    pub(crate) usn_journal_id: u64,
    pub(crate) next_usn: i64,
    /// 墓碑数：被 USN Delete 置 IS_DELETED 但仍占据 SoA 数组的节点数。
    /// 不落盘（persist 加载时按 flags 重算）。只增不减，全量重建后归零。
    pub(crate) tombstones: u32,
}

impl Index {
    pub fn len(&self) -> usize {
        self.names_len.len()
    }
    pub fn is_empty(&self) -> bool {
        self.len() <= 1
    }

    pub fn memory_bytes(&self) -> usize {
        self.names_off.len() * 4
            + self.names_len.len() * 2
            + self.parent.len() * 4
            + self.flags.len() * 2
            + self.name_pool.len()
            + self.frn_to_id.capacity() * (8 + 4)
    }

    /// 墓碑绝对量下限：低于此值不值得全量重建（重建本身要读整卷 MFT）。
    pub(crate) const REBUILD_MIN_TOMBSTONES: usize = 100_000;

    /// 墓碑占比过高（> 活跃节点的 25% 且绝对量 > REBUILD_MIN_TOMBSTONES）
    /// → 建议全量重建回收内存。t > (len - t)/4 ⇔ 5t > len。
    /// 已知局限：重命名同样向 name_pool 追加旧字节但不产生墓碑，
    /// 不计入本阈值；全量重建会一并回收。
    pub fn needs_rebuild(&self) -> bool {
        let t = self.tombstones as usize;
        t > Self::REBUILD_MIN_TOMBSTONES && t.saturating_mul(5) > self.len()
    }

    pub fn drive_letter(&self) -> char {
        self.drive_letter as char
    }
    pub fn volume_serial(&self) -> u64 {
        self.volume_serial
    }
    pub fn usn_journal_id(&self) -> u64 {
        self.usn_journal_id
    }
    pub fn next_usn(&self) -> i64 {
        self.next_usn
    }
    pub fn set_usn_state(&mut self, volume_serial: u64, usn_journal_id: u64, next_usn: i64) {
        self.volume_serial = volume_serial;
        self.usn_journal_id = usn_journal_id;
        self.next_usn = next_usn;
    }
    pub fn set_next_usn(&mut self, next_usn: i64) {
        self.next_usn = next_usn;
    }

    #[inline]
    fn name_bytes(&self, id: u32) -> &[u8] {
        let off = self.names_off[id as usize] as usize;
        let len = self.names_len[id as usize] as usize;
        &self.name_pool[off..off + len]
    }

    pub fn name(&self, id: u32) -> String {
        String::from_utf8_lossy(self.name_bytes(id)).into_owned()
    }
    pub fn is_dir(&self, id: u32) -> bool {
        self.flags[id as usize] & IS_DIR != 0
    }
    #[inline]
    fn is_deleted(&self, id: u32) -> bool {
        self.flags[id as usize] & IS_DELETED != 0
    }

    pub fn path(&self, id: u32) -> String {
        let mut parts: Vec<&[u8]> = Vec::new();
        let mut cur = id;
        let mut guard = 0;
        while cur != u32::MAX && cur != ROOT_ID && guard < 4096 {
            parts.push(self.name_bytes(cur));
            cur = self.parent[cur as usize];
            guard += 1;
        }
        let mut s = String::with_capacity(parts.iter().map(|p| p.len() + 1).sum::<usize>() + 3);
        s.push(self.drive_letter as char);
        s.push(':');
        s.push('\\');
        for (i, part) in parts.iter().rev().enumerate() {
            if i > 0 {
                s.push('\\');
            }
            s.push_str(&String::from_utf8_lossy(part));
        }
        s
    }

    /// 沿父链数层级（不构建路径字符串）。带 4096 步护栏防环。
    fn depth_of(&self, id: u32) -> u16 {
        let mut d = 0u16;
        let mut cur = id;
        let mut guard = 0;
        while cur != u32::MAX && cur != ROOT_ID && guard < 4096 {
            d = d.saturating_add(1);
            cur = self.parent[cur as usize];
            guard += 1;
        }
        d
    }

    /// 收集命中候选（仅 id + 打分键，不建路径字符串）。命中数达 `cap` 即停、返回 `truncated=true`。
    /// `terms` 须非空（由调用方保证）。`ancestor=Some` 时只取该子树后代。
    fn collect_scored(
        &self,
        terms: &[CompiledTerm],
        category: Category,
        ancestor: Option<u32>,
        cap: usize,
    ) -> (Vec<(ScoreKey, u32)>, bool) {
        let mut out: Vec<(ScoreKey, u32)> = Vec::new();
        let mut truncated = false;
        // 只有「单词且无通配」才可能构成完全/前缀匹配（多词或含通配恒不算）。
        let single_plain = terms.len() == 1 && !terms[0].has_wild;
        for id in 1..self.names_len.len() as u32 {
            if self.is_deleted(id) {
                continue;
            }
            let nb = self.name_bytes(id);
            if !all_terms_match(nb, terms) {
                continue;
            }
            let is_dir = self.is_dir(id);
            if !crate::category::matches(category, nb, is_dir) {
                continue;
            }
            if let Some(anc) = ancestor {
                if !self.is_descendant(id, anc) {
                    continue;
                }
            }
            let name_lower: Vec<u8> = nb.iter().map(|b| b.to_ascii_lowercase()).collect();
            let not_exact = !(single_plain && name_lower == terms[0].lower);
            let not_prefix = if terms[0].has_wild {
                true
            } else {
                !name_lower.starts_with(&terms[0].lower)
            };
            out.push((
                ScoreKey {
                    not_exact,
                    not_prefix,
                    not_dir: !is_dir,
                    depth: self.depth_of(id),
                    name_lower,
                },
                id,
            ));
            if out.len() >= cap {
                truncated = true;
                #[cfg(debug_assertions)]
                eprintln!(
                    "SaveSearch 搜索候选达上限 {}（盘 {}），尾部未排序候选被丢弃",
                    cap,
                    self.drive_letter()
                );
                break;
            }
        }
        (out, truncated)
    }

    /// 由 id 还原一条搜索结果（建 name/path 字符串）。
    pub(crate) fn materialize(&self, id: u32) -> SearchResult {
        SearchResult {
            name: self.name(id),
            path: self.path(id),
            is_dir: self.is_dir(id),
        }
    }

    /// 收集 → 按相关性排序 → 取前 `limit` → 仅对前 `limit` 个还原字符串。
    /// `ancestor=None` 全盘，`Some` 限子树。空查询返回空。
    pub fn search_scored(
        &self,
        query: &str,
        category: Category,
        ancestor: Option<u32>,
        limit: usize,
    ) -> Vec<SearchResult> {
        let terms = compile_query(query);
        if terms.is_empty() {
            return Vec::new();
        }
        let (mut c, _) = self.collect_scored(&terms, category, ancestor, cand_cap(limit));
        c.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        c.truncate(limit);
        c.into_iter().map(|(_, id)| self.materialize(id)).collect()
    }

    /// 供多盘合并：返回单盘 cap 内、已排序并截到 `limit` 的 `(键, id)`，不还原字符串。
    /// 全局 top-limit ⊆ 各盘 top-limit 的并集，故先截可减少合并量。
    pub(crate) fn collect_sorted(
        &self,
        query: &str,
        category: Category,
        ancestor: Option<u32>,
        limit: usize,
    ) -> Vec<(ScoreKey, u32)> {
        let terms = compile_query(query);
        if terms.is_empty() {
            return Vec::new();
        }
        let (mut c, _) = self.collect_scored(&terms, category, ancestor, cand_cap(limit));
        c.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        c.truncate(limit);
        c
    }

    pub fn search(&self, query: &str, category: Category, limit: usize) -> Vec<SearchResult> {
        self.search_scored(query, category, None, limit)
    }

    /// 节点是否为 `ancestor` 的后代（含自身）。
    fn is_descendant(&self, node: u32, ancestor: u32) -> bool {
        let mut cur = node;
        let mut guard = 0;
        while cur != u32::MAX && guard < 4096 {
            if cur == ancestor {
                return true;
            }
            cur = self.parent[cur as usize];
            guard += 1;
        }
        false
    }

    /// 在 `parent_id` 的直接子目录中按名字（大小写不敏感）查找。
    fn find_child_dir(&self, parent_id: u32, name: &str) -> Option<u32> {
        let target = name.as_bytes();
        for id in 1..self.names_len.len() as u32 {
            if self.parent[id as usize] == parent_id
                && self.is_dir(id)
                && self.name_bytes(id).eq_ignore_ascii_case(target)
            {
                return Some(id);
            }
        }
        None
    }

    /// 把完整路径（如 `D:\AI\pr-re`）解析为该盘索引内的目录节点 id。
    pub fn find_dir_by_path(&self, path: &str) -> Option<u32> {
        let p = path.trim_end_matches('\\');
        let mut parts = p.split('\\');
        let _drive = parts.next()?; // 跳过盘符 "D:"
        let mut cur = ROOT_ID;
        for comp in parts {
            if comp.is_empty() {
                continue;
            }
            cur = self.find_child_dir(cur, comp)?;
        }
        Some(cur)
    }

    /// 只在 `ancestor` 子树内搜索。
    /// 只在 `ancestor` 子树内搜索（带相关性排序）。
    pub fn search_under(
        &self,
        query: &str,
        category: Category,
        ancestor: u32,
        limit: usize,
    ) -> Vec<SearchResult> {
        self.search_scored(query, category, Some(ancestor), limit)
    }

    // ---- USN 增量 ----

    fn set_name(&mut self, id: u32, name: &str) {
        let bytes = name.as_bytes();
        let off = self.name_pool.len() as u32;
        self.name_pool.extend_from_slice(bytes);
        self.names_off[id as usize] = off;
        self.names_len[id as usize] = bytes.len().min(u16::MAX as usize) as u16;
    }

    fn resolve_parent(&self, parent_frn: u64) -> u32 {
        self.frn_to_id.get(&parent_frn).copied().unwrap_or(ROOT_ID)
    }

    /// 应用一条增量变更。
    pub fn apply(&mut self, change: &Change) {
        match change {
            Change::Delete { frn } => {
                if let Some(&id) = self.frn_to_id.get(frn) {
                    if self.flags[id as usize] & IS_DELETED == 0 {
                        self.tombstones += 1; // 仅首次置位计数，重复 Delete 不重计
                    }
                    self.flags[id as usize] |= IS_DELETED;
                    self.frn_to_id.remove(frn);
                }
            }
            Change::Upsert {
                frn,
                parent_frn,
                attrs,
                name,
            } => {
                if IndexBuilder::is_root_frn(*frn) {
                    return;
                }
                let parent_id = self.resolve_parent(*parent_frn);
                let is_dir = attrs & FILE_ATTRIBUTE_DIRECTORY != 0;
                if let Some(&id) = self.frn_to_id.get(frn) {
                    self.set_name(id, name);
                    self.parent[id as usize] = parent_id;
                    let mut f = if is_dir { IS_DIR } else { 0 };
                    // 复活（清除删除位）
                    f &= !IS_DELETED;
                    self.flags[id as usize] = f;
                } else {
                    let id = self.names_len.len() as u32;
                    let bytes = name.as_bytes();
                    let off = self.name_pool.len() as u32;
                    self.name_pool.extend_from_slice(bytes);
                    self.names_off.push(off);
                    self.names_len.push(bytes.len().min(u16::MAX as usize) as u16);
                    self.parent.push(parent_id);
                    self.flags.push(if is_dir { IS_DIR } else { 0 });
                    self.frn_to_id.insert(*frn, id);
                }
            }
        }
    }

    pub fn apply_all(&mut self, changes: &[Change]) {
        for c in changes {
            self.apply(c);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_search_path_and_increment() {
        let mut b = IndexBuilder::new(b'C');
        b.push(100, 5, FILE_ATTRIBUTE_DIRECTORY, "Users");
        b.push(200, 100, 0, "report.txt");
        let mut idx = b.finish();

        let r = idx.search("report", Category::All, 10);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].path, "C:\\Users\\report.txt");
        assert_eq!(idx.search("USERS", Category::All, 10).len(), 1);

        // 分类过滤：report.txt 是文档，不是图片/文件夹
        assert_eq!(idx.search("report", Category::Document, 10).len(), 1);
        assert_eq!(idx.search("report", Category::Image, 10).len(), 0);
        assert_eq!(idx.search("Users", Category::Folder, 10).len(), 1);
        assert_eq!(idx.search("Users", Category::Document, 10).len(), 0);

        // 增量：新增文件、删除文件
        idx.apply(&Change::Upsert {
            frn: 300,
            parent_frn: 100,
            attrs: 0,
            name: "notes.md".into(),
        });
        assert_eq!(idx.search("notes", Category::All, 10)[0].path, "C:\\Users\\notes.md");

        idx.apply(&Change::Delete { frn: 200 });
        assert_eq!(idx.search("report", Category::All, 10).len(), 0);

        // 重命名（同 frn upsert 新名字）
        idx.apply(&Change::Upsert {
            frn: 300,
            parent_frn: 100,
            attrs: 0,
            name: "todo.md".into(),
        });
        assert_eq!(idx.search("notes", Category::All, 10).len(), 0);
        assert_eq!(idx.search("todo", Category::All, 10).len(), 1);
    }

    #[test]
    fn tokenize_and_wildcard() {
        let mut b = IndexBuilder::new(b'C');
        b.push(100, 5, FILE_ATTRIBUTE_DIRECTORY, "Users");
        b.push(200, 100, 0, "report final.txt");
        b.push(300, 100, 0, "summary.txt");
        let idx = b.finish();

        // 空格分词 AND：所有词都要命中
        assert_eq!(idx.search("report txt", Category::All, 10).len(), 1);
        assert_eq!(idx.search("report zzz", Category::All, 10).len(), 0);
        // 大小写不敏感
        assert_eq!(idx.search("REPORT", Category::All, 10).len(), 1);
        // 通配符
        assert_eq!(idx.search("*.txt", Category::All, 10).len(), 2);
        assert_eq!(idx.search("re?ort*", Category::All, 10).len(), 1);
        assert_eq!(idx.search("rep*final", Category::All, 10).len(), 1);
    }

    #[test]
    fn relevance_sort() {
        let mut b = IndexBuilder::new(b'C');
        b.push(100, 5, FILE_ATTRIBUTE_DIRECTORY, "deep"); // id1 dir depth1
        b.push(101, 100, FILE_ATTRIBUTE_DIRECTORY, "sub"); // id2 dir depth2
        b.push(200, 5, 0, "reporting.txt"); // 前缀匹配，非完全
        b.push(201, 5, 0, "report"); // 完全匹配，文件，depth1
        b.push(202, 5, FILE_ATTRIBUTE_DIRECTORY, "report"); // 完全匹配，目录，depth1
        b.push(203, 101, 0, "report"); // 完全匹配，文件，depth3
        let idx = b.finish();

        let r = idx.search("report", Category::All, 10);
        // 完全匹配 + 目录优先 → 排第一
        assert_eq!(r[0].name, "report");
        assert!(r[0].is_dir);
        // 完全匹配文件，浅路径在前
        assert_eq!(r[1].path, "C:\\report");
        assert!(!r[1].is_dir);
        assert_eq!(r[2].path, "C:\\deep\\sub\\report");
        // 前缀匹配（reporting.txt）排在所有完全匹配之后
        assert_eq!(r.last().unwrap().name, "reporting.txt");
    }

    #[test]
    fn tombstone_counting() {
        let mut b = IndexBuilder::new(b'C');
        b.push(100, 5, FILE_ATTRIBUTE_DIRECTORY, "Users");
        b.push(200, 100, 0, "a.txt");
        b.push(300, 100, 0, "b.txt");
        let mut idx = b.finish();
        assert_eq!(idx.tombstones, 0);

        idx.apply(&Change::Delete { frn: 200 });
        assert_eq!(idx.tombstones, 1);
        // 重复 Delete（frn 已移出 map）不重计
        idx.apply(&Change::Delete { frn: 200 });
        assert_eq!(idx.tombstones, 1);
        // 不存在的 frn 不计
        idx.apply(&Change::Delete { frn: 999 });
        assert_eq!(idx.tombstones, 1);

        idx.apply(&Change::Delete { frn: 300 });
        assert_eq!(idx.tombstones, 2);

        // 同路径「重建」走新增分支，墓碑保持
        idx.apply(&Change::Upsert {
            frn: 301,
            parent_frn: 100,
            attrs: 0,
            name: "b.txt".into(),
        });
        assert_eq!(idx.tombstones, 2);
    }

    #[test]
    fn needs_rebuild_threshold() {
        let mut b = IndexBuilder::new(b'C');
        b.push(100, 5, 0, "a");
        let mut idx = b.finish();
        // 比例满足（墓碑 1 : 总 2）但绝对量远低于下限 → 不重建
        idx.apply(&Change::Delete { frn: 100 });
        assert!(!idx.needs_rebuild());
        // 人为满足绝对量但比例不足 → 不重建；两者都满足 → 重建
        idx.tombstones = Index::REBUILD_MIN_TOMBSTONES as u32 + 1;
        assert!(idx.needs_rebuild()); // len=2，5t > len 必然成立
        // 构造比例不足：t*5 <= len
        let t = idx.tombstones as usize;
        idx.names_len.resize(t * 5 + 1, 0);
        assert!(!idx.needs_rebuild());
    }

    #[test]
    fn cap_truncates_candidates() {
        let mut b = IndexBuilder::new(b'C');
        for i in 0..30u64 {
            b.push(1000 + i, 5, 0, "x.txt");
        }
        let idx = b.finish();
        let terms = compile_query("x");
        let (cands, trunc) = idx.collect_scored(&terms, Category::All, None, 10);
        assert_eq!(cands.len(), 10);
        assert!(trunc);
        let (cands2, trunc2) = idx.collect_scored(&terms, Category::All, None, 100);
        assert_eq!(cands2.len(), 30);
        assert!(!trunc2);
    }
}
