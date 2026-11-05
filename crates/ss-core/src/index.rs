//! 紧凑文件索引：SoA + 字符串池 + 父引用编号 + MFT 记录号→id 稠密表（供 USN 增量更新）。
//!
//! 每个文件/目录是一个节点（id 为 `u32` 下标）。名字存共享字节池，节点只存
//! 「偏移 + 长度 + 父 id + 标志位 + FRN 序列号」。完整路径沿父链反查。大小写不敏感（ASCII）
//! 通过查询时折叠实现，不再存小写副本池以省内存。
//!
//! FRN（64 位文件引用号）= 高 16 位序列号 + 低 48 位 MFT 记录号。记录号即 MFT 槽位下标，
//! 天然稠密、上界 ≈ 卷历史峰值文件数，故用 `Vec<u32>` 直接按记录号索引（4 B/槽）替代
//! `HashMap<u64,u32>`（≈17 B/桶且按 2 的幂膨胀），序列号另存每节点 2 B 用于校验记录被复用。

use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};

use crate::category::{classify, ext_of, Category, CategoryCounts};

/// 节点标志位。
pub const IS_DIR: u16 = 1 << 0;
/// 已删除（USN 删除事件后置位，搜索时跳过；全量重建时清理）。
pub const IS_DELETED: u16 = 1 << 1;

/// 合成根节点 id（代表盘符根，如 `C:\`）。
pub const ROOT_ID: u32 = 0;

/// NTFS 根目录的文件记录号（FRN 低 48 位）。
const NTFS_ROOT_FILE_NUMBER: u64 = 5;
const FRN_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;

/// 记录号表空槽标记。
pub(crate) const NO_ID: u32 = u32::MAX;
/// 记录号表槽位上限（2^28 槽 = 1 GiB）：MFT 记录号来自内核、实际稠密，此处仅防
/// 损坏数据触发天量分配；超限的节点直接忽略。
pub(crate) const MAX_RECORDS: usize = 1 << 28;

const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;

#[inline]
fn frn_rec(frn: u64) -> usize {
    (frn & FRN_MASK) as usize
}
#[inline]
fn frn_seq(frn: u64) -> u16 {
    (frn >> 48) as u16
}

/// 一条搜索结果。
#[derive(Debug, Clone)]
pub struct SearchResult {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    /// 命中部分在 `name` 中的字节区间，已排序且互不重叠。
    pub match_ranges: Vec<(u16, u16)>,
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
#[inline]
fn contains_ci(hay: &[u8], needle: &[u8]) -> bool {
    find_ci(hay, needle).is_some()
}

/// 返回 `needle` 首次出现的位置。`needle` 必须已小写。
/// 首字节用 memchr（SIMD）同时找大小写两种形态，命中后再逐字节核对余下部分。
fn find_ci(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    if hay.len() < needle.len() {
        return None;
    }
    let lower = needle[0];
    let upper = lower.to_ascii_uppercase();
    let rest = &needle[1..];
    let last_start = hay.len() - needle.len();
    let mut pos = 0usize;
    while pos <= last_start {
        let window = &hay[pos..=last_start];
        let hit = if lower == upper {
            memchr::memchr(lower, window)
        } else {
            memchr::memchr2(lower, upper, window)
        };
        let i = hit?;
        let s = pos + i;
        if hay[s + 1..s + needle.len()].eq_ignore_ascii_case(rest) {
            return Some(s);
        }
        pos = s + 1;
    }
    None
}

/// 一个查询词：已折叠小写的字节 + 是否含通配符（`*`/`?`）。
pub(crate) struct CompiledTerm {
    lower: Vec<u8>,
    has_wild: bool,
    /// 以 `*` 开头且不以 `*` 结尾：要求匹配到名字结尾（`*.txt` 不匹配 `a.txt.bak`）。
    anchor_end: bool,
}

/// 编译后的查询。
#[derive(Default)]
pub(crate) struct CompiledQuery {
    /// 匹配文件名的词，全部命中才算匹配（AND）。
    pub terms: Vec<CompiledTerm>,
    /// 含 `\\` 的词，匹配完整路径。只有这时才需要拼路径，普通搜索不受影响。
    pub path_terms: Vec<CompiledTerm>,
    /// `ext:pdf,docx`：扩展名白名单（已小写，不含点）。
    pub exts: Vec<Vec<u8>>,
    /// `dir:`：只要文件夹。
    pub dirs_only: bool,
}

impl CompiledQuery {
    /// 没有任何条件。这样的查询没有结果，而不是匹配全部节点。
    pub fn is_empty(&self) -> bool {
        self.terms.is_empty() && self.path_terms.is_empty() && self.exts.is_empty() && !self.dirs_only
    }
}

fn compile_term(word: &str) -> CompiledTerm {
    let lower = word.as_bytes().to_ascii_lowercase();
    let has_wild = lower.iter().any(|&b| b == b'*' || b == b'?');
    let anchor_end = lower.first() == Some(&b'*') && lower.last() != Some(&b'*');
    CompiledTerm {
        lower,
        has_wild,
        anchor_end,
    }
}

/// 解析查询：按 Unicode 空白分词（含全角空格 U+3000）。`ext:a,b` 和单独的 `dir:` 是筛选
/// 条件，含 `\\` 的词匹配完整路径，其余的词匹配文件名。
pub(crate) fn compile_query(query: &str) -> CompiledQuery {
    let mut q = CompiledQuery::default();
    for word in query.split_whitespace() {
        let ext_list = word
            .get(..4)
            .filter(|prefix| prefix.eq_ignore_ascii_case("ext:"))
            .map(|_| &word[4..])
            .filter(|list| !list.is_empty());
        if let Some(list) = ext_list {
            q.exts.extend(
                list.split(',')
                    .map(|e| e.trim_start_matches('.').as_bytes().to_ascii_lowercase())
                    .filter(|e| !e.is_empty()),
            );
        } else if word.eq_ignore_ascii_case("dir:") {
            q.dirs_only = true;
        } else if word.contains('\\') {
            q.path_terms.push(compile_term(word));
        } else {
            q.terms.push(compile_term(word));
        }
    }
    q
}

/// 单个词是否命中文件名：无通配=子串匹配（复用 `contains_ci`）；含通配=子串锚定 glob。
fn term_matches(hay: &[u8], t: &CompiledTerm) -> bool {
    if t.has_wild {
        wildcard_contains_ci(hay, &t.lower, t.anchor_end)
    } else {
        contains_ci(hay, &t.lower)
    }
}

/// 所有词都命中才算匹配（AND）；任一不命中即短路。
fn all_terms_match(hay: &[u8], terms: &[CompiledTerm]) -> bool {
    terms.iter().all(|t| term_matches(hay, t))
}

/// 子串锚定的通配匹配：`pat` 能在 `hay` 任意起始位匹配即命中。`pat` 须已小写。
/// `*`=任意多字符（含 0），`?`=任意一个完整字符，其余按 ASCII 折叠大小写比较。
/// `anchor_end` 见 [`CompiledTerm::anchor_end`]。
fn wildcard_contains_ci(hay: &[u8], pat: &[u8], anchor_end: bool) -> bool {
    // 开头的 `*` 已覆盖所有起点，只需从 0 试一次。
    if pat.first() == Some(&b'*') {
        return glob_at(hay, pat, anchor_end);
    }
    // 只从字符边界开始，否则 `?` 会把半个多字节字符算成一个字符。
    (0..=hay.len())
        .filter(|&start| start == hay.len() || !is_utf8_continuation(hay[start]))
        .any(|start| glob_at(&hay[start..], pat, anchor_end))
}

#[inline]
fn is_utf8_continuation(b: u8) -> bool {
    b & 0xC0 == 0x80
}

/// 由 UTF-8 首字节得到字符的字节数。
#[inline]
fn utf8_char_len(first: u8) -> usize {
    match first {
        0xF0..=0xFF => 4,
        0xE0..=0xEF => 3,
        0xC0..=0xDF => 2,
        _ => 1,
    }
}

/// 从 `h` 开头匹配 glob `pat`。`anchor_end=false` 时 `pat` 消费完即成功（尾部自由 =
/// 子串语义）；`anchor_end=true` 时还要求 `h` 也用尽，否则回溯到最近的 `*`。
/// 经典「贪心 + 星号回溯」，无递归。`?` 和回溯都按完整 UTF-8 字符前进。
fn glob_at(h: &[u8], pat: &[u8], anchor_end: bool) -> bool {
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
                        hi = (hi + utf8_char_len(h[hi])).min(h.len());
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
        } else if !anchor_end || hi == h.len() {
            return true;
        }
        // 失配：回溯到最近的 '*'，多吞一个字符再试
        if let Some(sp) = star_pi {
            if star_hi >= h.len() {
                return false;
            }
            star_hi = (star_hi + utf8_char_len(h[star_hi])).min(h.len());
            hi = star_hi;
            pi = sp + 1;
        } else {
            return false;
        }
    }
}

/// 一段不含 `*` 的模式是否能从 `h` 开头匹配，返回匹配的字节数。
fn segment_len_at(h: &[u8], seg: &[u8]) -> Option<usize> {
    let mut hi = 0usize;
    for &c in seg {
        if hi >= h.len() {
            return None;
        }
        if c == b'?' {
            hi = (hi + utf8_char_len(h[hi])).min(h.len());
        } else if h[hi].to_ascii_lowercase() == c {
            hi += 1;
        } else {
            return None;
        }
    }
    Some(hi)
}

/// 通配模式的命中区间：把模式按 `*` 拆段，从左到右为每段找最早的位置。这样高亮的是字面
/// 部分（`*.txt` 只高亮 `.txt`），而不是 `*` 吞掉的内容。`anchor_end` 时最后一段对齐结尾。
fn wildcard_ranges(hay: &[u8], t: &CompiledTerm, out: &mut Vec<(u16, u16)>) {
    let segs: Vec<&[u8]> = t.lower.split(|&b| b == b'*').filter(|s| !s.is_empty()).collect();
    let last = segs.len().wrapping_sub(1);
    let mut cursor = 0usize;
    for (i, seg) in segs.iter().enumerate() {
        let found = if t.anchor_end && i == last {
            // 从右往左找能恰好匹配到结尾的起点（段里可能有多字节字符和 `?`）
            (cursor..hay.len())
                .rev()
                .filter(|&s| !is_utf8_continuation(hay[s]))
                .find(|&s| segment_len_at(&hay[s..], seg) == Some(hay.len() - s))
                .map(|s| (s, hay.len()))
        } else {
            (cursor..hay.len())
                .filter(|&s| !is_utf8_continuation(hay[s]))
                .find_map(|s| segment_len_at(&hay[s..], seg).map(|len| (s, s + len)))
        };
        let Some((start, end)) = found else {
            return;
        };
        out.push((start as u16, end as u16));
        cursor = end;
    }
}

/// 计算 `name` 上所有词的命中区间，合并成有序且互不重叠的列表。只为最终结果调用。
fn match_ranges(name: &[u8], terms: &[CompiledTerm]) -> Vec<(u16, u16)> {
    let mut ranges: Vec<(u16, u16)> = Vec::new();
    for t in terms {
        if t.has_wild {
            wildcard_ranges(name, t, &mut ranges);
        } else if let Some(s) = find_ci(name, &t.lower) {
            ranges.push((s as u16, (s + t.lower.len()) as u16));
        }
    }
    ranges.sort_unstable();
    let mut merged: Vec<(u16, u16)> = Vec::with_capacity(ranges.len());
    for (s, e) in ranges {
        match merged.last_mut() {
            Some(last) if s <= last.1 => last.1 = last.1.max(e),
            _ => merged.push((s, e)),
        }
    }
    merged
}

/// 搜索结果排序键。派生 `Ord` 按字段先后、各字段升序比较，`bool` 的 `false < true`，
/// 故命中项一律用否定语义（`not_*`，命中 = `false` 排在前）。
/// 顺序：完全匹配 > 主名完全匹配（`report` 命中 `report.pdf`）> 前缀匹配 > 目录 >
/// 浅路径 > 名称字母序。
#[derive(PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct ScoreKey {
    not_exact: bool,
    not_stem_exact: bool,
    not_prefix: bool,
    not_dir: bool,
    depth: u16,
    name_lower: Vec<u8>,
}

/// 文件主名（去掉最后一个扩展名）。目录没有扩展名的概念，返回完整名字。
#[inline]
fn stem_of(name: &[u8], is_dir: bool) -> &[u8] {
    if is_dir {
        return name;
    }
    match name.iter().rposition(|&b| b == b'.') {
        Some(pos) if pos > 0 => &name[..pos],
        _ => name,
    }
}

impl ScoreKey {
    /// 不需要沿父链或分配内存就能得到的字段。
    #[inline]
    fn cheap(&self) -> (bool, bool, bool, bool) {
        (self.not_exact, self.not_stem_exact, self.not_prefix, self.not_dir)
    }
}

/// 一次扫描的条件。
#[derive(Clone, Copy)]
pub(crate) struct ScanSpec<'a> {
    pub query: &'a CompiledQuery,
    pub category: Category,
    /// 只取该目录子树内的节点。
    pub ancestor: Option<u32>,
    pub limit: usize,
    pub cancel: Option<&'a AtomicBool>,
}

/// 每扫描这么多节点检查一次取消标记。
const CANCEL_CHECK_MASK: u32 = 4096 - 1;

/// 一次扫描的结果：最好的若干条和全部命中数。取消时只包含已扫描到的部分。
#[derive(Default)]
pub(crate) struct Scan {
    pub hits: Vec<(ScoreKey, u32)>,
    /// 符合全部条件的命中数。
    pub total_hits: usize,
    /// 不看分类筛选时，各分类的命中数。
    pub category_counts: CategoryCounts,
    pub cancelled: bool,
}

/// `name` 是否以 `lower`（已小写）开头，ASCII 大小写不敏感。
#[inline]
fn starts_with_ci(name: &[u8], lower: &[u8]) -> bool {
    name.len() >= lower.len() && name[..lower.len()].eq_ignore_ascii_case(lower)
}

/// 按小写形式比较 `name` 和已小写的 `lower`，不分配内存。
#[inline]
fn cmp_name_lower(name: &[u8], lower: &[u8]) -> Ordering {
    name.iter()
        .map(|b| b.to_ascii_lowercase())
        .cmp(lower.iter().copied())
}

/// 节点数达到此值才分线程扫描（小索引单线程更快：省线程创建开销）。
const PAR_MIN_NODES: usize = 200_000;
/// 扫描线程数上限（内存带宽受限，再多无益）。
const PAR_MAX_THREADS: usize = 8;

/// 增量构建器：枚举 MFT 时逐条 `push`，最后 `finish` 得到 `Index`。
pub struct IndexBuilder {
    names_off: Vec<u32>,
    names_len: Vec<u16>,
    parent_frn: Vec<u64>, // 临时：完成时解析为父 id
    flags: Vec<u16>,
    seq: Vec<u16>,
    name_pool: Vec<u8>,
    rec_to_id: Vec<u32>,
    drive_letter: u8,
}

impl IndexBuilder {
    pub fn new(drive_letter: u8) -> Self {
        let mut b = IndexBuilder {
            names_off: Vec::new(),
            names_len: Vec::new(),
            parent_frn: Vec::new(),
            flags: Vec::new(),
            seq: Vec::new(),
            name_pool: Vec::new(),
            rec_to_id: Vec::new(),
            drive_letter,
        };
        // 合成根节点（id 0）
        b.names_off.push(0);
        b.names_len.push(0);
        b.parent_frn.push(0);
        b.flags.push(IS_DIR);
        b.seq.push(0);
        b
    }

    /// 预留 `n` 个节点（及记录号表 `n` 槽）。避免 push 期间反复倍增搬迁、峰值内存翻倍。
    pub fn reserve(&mut self, n: usize) {
        self.names_off.reserve(n);
        self.names_len.reserve(n);
        self.parent_frn.reserve(n);
        self.flags.reserve(n);
        self.seq.reserve(n);
        self.rec_to_id.reserve(n.min(MAX_RECORDS));
        // 名字平均 ~20 字节（UTF-8）
        self.name_pool.reserve(n.saturating_mul(20));
    }

    #[inline]
    pub fn is_root_frn(frn: u64) -> bool {
        (frn & FRN_MASK) == NTFS_ROOT_FILE_NUMBER
    }

    pub fn push(&mut self, frn: u64, parent_frn: u64, attrs: u32, name: &str) {
        let rec = frn_rec(frn);
        if rec >= MAX_RECORDS {
            return;
        }
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
        self.seq.push(frn_seq(frn));
        self.parent_frn.push(parent_frn);
        if rec >= self.rec_to_id.len() {
            self.rec_to_id.resize(rec + 1, NO_ID);
        }
        self.rec_to_id[rec] = id;
    }

    pub fn finish(self) -> Index {
        let n = self.names_len.len();
        let mut parent = Vec::with_capacity(n);
        for (i, &pf) in self.parent_frn.iter().enumerate() {
            if i as u32 == ROOT_ID {
                parent.push(u32::MAX);
            } else {
                parent.push(lookup_rec(&self.rec_to_id, pf));
            }
        }
        let mut idx = Index {
            names_off: self.names_off,
            names_len: self.names_len,
            parent,
            flags: self.flags,
            seq: self.seq,
            name_pool: self.name_pool,
            rec_to_id: self.rec_to_id,
            drive_letter: self.drive_letter,
            volume_serial: 0,
            usn_journal_id: 0,
            next_usn: 0,
            tombstones: 0,
        };
        // 释放 Vec 倍增留下的冗余容量（最坏接近 2 倍），索引常驻内存以实际大小为准
        idx.shrink_to_fit();
        idx
    }
}

/// 按记录号（忽略序列号）查节点 id；无则挂到根。
/// 父引用只按记录号：父目录若被删，其子项必然同时被删，序列号不匹配的情况仅见于损坏数据。
#[inline]
fn lookup_rec(table: &[u32], frn: u64) -> u32 {
    table
        .get(frn_rec(frn))
        .copied()
        .filter(|&id| id != NO_ID)
        .unwrap_or(ROOT_ID)
}

/// 增量追加前按需小步扩容。`Vec::push`/`resize` 在容量用满时按 2 倍增长：索引数组
/// 常驻且加载/构建后容量恰好等于长度，随后第一条 USN 新增就会把每个数组翻倍
///（实测 2 百万节点：86 MB → 163 MB）。这里改为 `reserve_exact` 每次只多要
/// max(len/32, min)，增长开销 ~3%，代价是每 3% 新增触发一次搬迁（毫秒级）。
#[inline]
fn grow_exact<T>(v: &mut Vec<T>, need: usize, min: usize) {
    let spare = v.capacity() - v.len();
    if spare < need {
        v.reserve_exact((v.len() / 32).max(min).max(need));
    }
}
const NODE_GROW_MIN: usize = 4096;
const POOL_GROW_MIN: usize = 1 << 20;

/// 只读 + 可增量更新的紧凑索引。
pub struct Index {
    pub(crate) names_off: Vec<u32>,
    pub(crate) names_len: Vec<u16>,
    pub(crate) parent: Vec<u32>,
    pub(crate) flags: Vec<u16>,
    /// 每节点 FRN 序列号（高 16 位）；根为 0。
    pub(crate) seq: Vec<u16>,
    pub(crate) name_pool: Vec<u8>,
    /// MFT 记录号 → 节点 id（`NO_ID`=空槽）。已删节点的槽在删除时清空。
    pub(crate) rec_to_id: Vec<u32>,
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

    /// 常驻内存估算（按已分配容量，而非元素数）。
    pub fn memory_bytes(&self) -> usize {
        self.names_off.capacity() * 4
            + self.names_len.capacity() * 2
            + self.parent.capacity() * 4
            + self.flags.capacity() * 2
            + self.seq.capacity() * 2
            + self.name_pool.capacity()
            + self.rec_to_id.capacity() * 4
    }

    pub(crate) fn shrink_to_fit(&mut self) {
        self.names_off.shrink_to_fit();
        self.names_len.shrink_to_fit();
        self.parent.shrink_to_fit();
        self.flags.shrink_to_fit();
        self.seq.shrink_to_fit();
        self.name_pool.shrink_to_fit();
        self.rec_to_id.shrink_to_fit();
    }

    /// 墓碑绝对量下限：低于此值不值得全量重建（重建本身要读整卷 MFT）。
    pub(crate) const REBUILD_MIN_TOMBSTONES: usize = 100_000;

    /// 墓碑占比过高（> 活跃节点的 25% 且绝对量 > REBUILD_MIN_TOMBSTONES）
    /// → 建议全量重建回收内存。t > (len - t)/4 ⇔ 5t > len。
    /// 已知局限：改名为更长的名字会向 name_pool 追加新字节、旧字节成死区，
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

    /// 把完整路径的 UTF-8 字节写进 `buf`（复用其容量），用于路径匹配。
    fn write_path(&self, id: u32, buf: &mut Vec<u8>) {
        let mut stack = [ROOT_ID; 256];
        let mut depth = 0usize;
        let mut cur = id;
        while cur != u32::MAX && cur != ROOT_ID && depth < stack.len() {
            stack[depth] = cur;
            depth += 1;
            cur = self.parent[cur as usize];
        }
        buf.clear();
        buf.extend_from_slice(&[self.drive_letter, b':', b'\\']);
        for (i, &node) in stack[..depth].iter().rev().enumerate() {
            if i > 0 {
                buf.push(b'\\');
            }
            buf.extend_from_slice(self.name_bytes(node));
        }
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

    /// `id` 仍是一个未删除的目录，且其完整路径等于 `path`（大小写不敏感、忽略尾部 `\`）。
    /// 供调用方校验缓存的「路径→id」解析结果是否仍有效（目录被删 / 改名 / 重建即失效）。
    pub(crate) fn is_live_dir_at(&self, id: u32, path: &str) -> bool {
        (id as usize) < self.len()
            && id != ROOT_ID
            && self.is_dir(id)
            && !self.is_deleted(id)
            && self
                .path(id)
                .trim_end_matches('\\')
                .eq_ignore_ascii_case(path.trim_end_matches('\\'))
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

    /// 在 id 区间 `[lo, hi)` 内做全量扫描：统计全部命中数，只保留最好的 `limit` 条。
    /// 比堆顶差的命中不计算深度、不分配小写名，所以泛查询的开销主要在名字匹配上。
    fn collect_range(&self, lo: u32, hi: u32, spec: &ScanSpec) -> Scan {
        let ScanSpec {
            query,
            category,
            ancestor,
            limit,
            cancel,
        } = *spec;
        let mut heap: BinaryHeap<(ScoreKey, u32)> = BinaryHeap::new();
        let mut total_hits = 0usize;
        let mut category_counts = CategoryCounts::default();
        let mut cancelled = false;
        let terms = query.terms.as_slice();
        // 只有「单词且无通配」才可能构成完全匹配（多词或含通配恒不算）。
        let single_plain = terms.len() == 1 && !terms[0].has_wild;
        let mut path_buf: Vec<u8> = Vec::new();
        for id in lo..hi {
            if (id - lo) & CANCEL_CHECK_MASK == 0
                && cancel.is_some_and(|c| c.load(AtomicOrdering::Relaxed))
            {
                cancelled = true;
                break;
            }
            if self.is_deleted(id) {
                continue;
            }
            let nb = self.name_bytes(id);
            if !all_terms_match(nb, terms) {
                continue;
            }
            let is_dir = self.is_dir(id);
            if query.dirs_only && !is_dir {
                continue;
            }
            if !query.exts.is_empty() {
                let ext = ext_of(nb);
                if is_dir || !query.exts.iter().any(|e| ext.eq_ignore_ascii_case(e)) {
                    continue;
                }
            }
            if let Some(anc) = ancestor {
                if !self.is_descendant(id, anc) {
                    continue;
                }
            }
            if !query.path_terms.is_empty() {
                self.write_path(id, &mut path_buf);
                if !all_terms_match(&path_buf, &query.path_terms) {
                    continue;
                }
            }
            let item_category = classify(nb, is_dir);
            category_counts.add_hit(item_category);
            if category != Category::All && category != item_category {
                continue;
            }
            total_hits += 1;
            if limit == 0 {
                continue;
            }

            let first = terms.first();
            let not_exact = !(single_plain && nb.eq_ignore_ascii_case(&terms[0].lower));
            let not_stem_exact =
                !(single_plain && stem_of(nb, is_dir).eq_ignore_ascii_case(&terms[0].lower));
            let not_prefix = first.is_none_or(|t| t.has_wild || !starts_with_ci(nb, &t.lower));
            let not_dir = !is_dir;
            let cheap = (not_exact, not_stem_exact, not_prefix, not_dir);

            let mut known_depth = None;
            if heap.len() == limit {
                let (top, _) = heap.peek().expect("limit > 0 时满堆非空");
                let order = cheap.cmp(&top.cheap()).then_with(|| {
                    let depth = self.depth_of(id);
                    known_depth = Some(depth);
                    depth
                        .cmp(&top.depth)
                        .then_with(|| cmp_name_lower(nb, &top.name_lower))
                });
                // 名字也相同时后扫描到的 id 更大，同样排在堆顶之后。
                if order != Ordering::Less {
                    continue;
                }
                heap.pop();
            }
            let depth = known_depth.unwrap_or_else(|| self.depth_of(id));
            let key = ScoreKey {
                not_exact,
                not_stem_exact,
                not_prefix,
                not_dir,
                depth,
                name_lower: nb.to_ascii_lowercase(),
            };
            heap.push((key, id));
        }
        Scan {
            hits: heap.into_vec(),
            total_hits,
            category_counts,
            cancelled,
        }
    }

    /// 全量扫描。大索引按 id 区间切成多段并行（作用域线程，每次搜索建/收线程约几十微秒，
    /// 远小于扫描本身），每段独立保留前 `limit` 条。全局前 `limit` 条一定在各段前
    /// `limit` 条的并集里。查询须非空（由调用方保证）。
    fn collect_scored(&self, spec: &ScanSpec) -> Scan {
        let n = self.names_len.len();
        let threads = if n >= PAR_MIN_NODES {
            std::thread::available_parallelism()
                .map(|x| x.get())
                .unwrap_or(1)
                .min(PAR_MAX_THREADS)
        } else {
            1
        };
        if threads <= 1 || n < 2 {
            return self.collect_range(1, n as u32, spec);
        }
        let chunk = n.div_ceil(threads);
        std::thread::scope(|s| {
            let handles: Vec<_> = (0..threads)
                .map(|t| {
                    let lo = (t * chunk).max(1).min(n) as u32;
                    let hi = ((t + 1) * chunk).min(n) as u32;
                    s.spawn(move || self.collect_range(lo, hi, spec))
                })
                .collect();
            let mut out = Scan::default();
            for h in handles {
                let part = h.join().unwrap_or_default();
                out.hits.extend(part.hits);
                out.total_hits += part.total_hits;
                out.category_counts.merge(&part.category_counts);
                out.cancelled |= part.cancelled;
            }
            out
        })
    }

    /// 由 id 还原一条搜索结果（建 name/path 字符串，计算命中区间）。
    pub(crate) fn materialize(&self, id: u32, query: &CompiledQuery) -> SearchResult {
        SearchResult {
            name: self.name(id),
            path: self.path(id),
            is_dir: self.is_dir(id),
            match_ranges: match_ranges(self.name_bytes(id), &query.terms),
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
        let compiled = compile_query(query);
        let scan = self.scan_sorted(&ScanSpec {
            query: &compiled,
            category,
            ancestor,
            limit,
            cancel: None,
        });
        scan.hits
            .into_iter()
            .map(|(_, id)| self.materialize(id, &compiled))
            .collect()
    }

    /// 供多盘合并：返回单盘已排序并截到 `limit` 的结果，不还原字符串。
    /// 全局 top-limit ⊆ 各盘 top-limit 的并集，故先截可减少合并量。空查询返回空。
    pub(crate) fn scan_sorted(&self, spec: &ScanSpec) -> Scan {
        if spec.query.is_empty() {
            return Scan::default();
        }
        let mut scan = self.collect_scored(spec);
        scan.hits.sort_unstable();
        scan.hits.truncate(spec.limit);
        scan
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

    /// 在 `parent_id` 的直接子目录中按名字（大小写不敏感）查找。跳过墓碑：
    /// 目录「删除后重建」会留下同名同父的已删旧节点且 id 更小，不跳过会解析到旧节点，
    /// 其子树为空 → 预设文件夹搜索永远无结果。
    fn find_child_dir(&self, parent_id: u32, name: &str) -> Option<u32> {
        let target = name.as_bytes();
        (1..self.names_len.len() as u32).find(|&id| {
            self.parent[id as usize] == parent_id
                && self.flags[id as usize] & (IS_DIR | IS_DELETED) == IS_DIR
                && self.name_bytes(id).eq_ignore_ascii_case(target)
        })
    }

    /// 把完整路径（如 `D:\AI\pr-re`）解析为该盘索引内的目录节点 id。
    /// O(节点数 × 路径深度) 的线性扫描；调用方（Catalog）缓存结果并用
    /// [`is_live_dir_at`](Self::is_live_dir_at) 校验，避免每次击键重解析。
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

    /// 改名。名字未变则不动（USN 对同一文件的写入/关闭等会连发多条同名 Upsert，
    /// 原实现每条都向 name_pool 追加一份名字 → 池随文件活动无限增长）；
    /// 新名不长于旧名则原地覆盖（各节点名字区间互不共享）；仅变长时才追加。
    fn set_name(&mut self, id: u32, name: &str) {
        let bytes = name.as_bytes();
        if self.name_bytes(id) == bytes {
            return;
        }
        let i = id as usize;
        let off = self.names_off[i] as usize;
        let old_len = self.names_len[i] as usize;
        let new_len = bytes.len().min(u16::MAX as usize);
        if new_len <= old_len {
            self.name_pool[off..off + new_len].copy_from_slice(&bytes[..new_len]);
            self.names_len[i] = new_len as u16;
            return;
        }
        grow_exact(&mut self.name_pool, new_len, POOL_GROW_MIN);
        let off = self.name_pool.len() as u32;
        self.name_pool.extend_from_slice(&bytes[..new_len]);
        self.names_off[i] = off;
        self.names_len[i] = new_len as u16;
    }

    fn resolve_parent(&self, parent_frn: u64) -> u32 {
        lookup_rec(&self.rec_to_id, parent_frn)
    }

    /// 给节点立墓碑（首次置位才计数）。
    fn tombstone(&mut self, id: u32) {
        let f = &mut self.flags[id as usize];
        if *f & IS_DELETED == 0 {
            self.tombstones += 1;
        }
        *f |= IS_DELETED;
    }

    fn push_node(&mut self, name: &str, parent_id: u32, is_dir: bool, seq: u16) -> u32 {
        let id = self.names_len.len() as u32;
        let bytes = name.as_bytes();
        grow_exact(&mut self.names_off, 1, NODE_GROW_MIN);
        grow_exact(&mut self.names_len, 1, NODE_GROW_MIN);
        grow_exact(&mut self.parent, 1, NODE_GROW_MIN);
        grow_exact(&mut self.flags, 1, NODE_GROW_MIN);
        grow_exact(&mut self.seq, 1, NODE_GROW_MIN);
        grow_exact(&mut self.name_pool, bytes.len(), POOL_GROW_MIN);
        let off = self.name_pool.len() as u32;
        self.name_pool.extend_from_slice(bytes);
        self.names_off.push(off);
        self.names_len.push(bytes.len().min(u16::MAX as usize) as u16);
        self.parent.push(parent_id);
        self.flags.push(if is_dir { IS_DIR } else { 0 });
        self.seq.push(seq);
        id
    }

    /// 应用一条增量变更，返回索引内容是否真的变化。
    pub fn apply(&mut self, change: &Change) -> bool {
        match change {
            Change::Delete { frn } => {
                let rec = frn_rec(*frn);
                if let Some(&id) = self.rec_to_id.get(rec) {
                    // 序列号不符 = 该槽已被更新的文件占用，此 Delete 属于旧文件（已处理过）
                    if id != NO_ID && self.seq[id as usize] == frn_seq(*frn) {
                        self.tombstone(id);
                        self.rec_to_id[rec] = NO_ID;
                        return true;
                    }
                }
                false
            }
            Change::Upsert {
                frn,
                parent_frn,
                attrs,
                name,
            } => {
                if IndexBuilder::is_root_frn(*frn) {
                    return false;
                }
                let rec = frn_rec(*frn);
                if rec >= MAX_RECORDS {
                    return false;
                }
                let seq = frn_seq(*frn);
                let parent_id = self.resolve_parent(*parent_frn);
                let is_dir = attrs & FILE_ATTRIBUTE_DIRECTORY != 0;
                let existing = self.rec_to_id.get(rec).copied().filter(|&id| id != NO_ID);
                match existing {
                    Some(id) if self.seq[id as usize] == seq => {
                        let flags = if is_dir { IS_DIR } else { 0 };
                        let i = id as usize;
                        if self.name_bytes(id) == name.as_bytes()
                            && self.parent[i] == parent_id
                            && self.flags[i] == flags
                        {
                            return false;
                        }
                        self.set_name(id, name);
                        self.parent[i] = parent_id;
                        self.flags[i] = flags;
                        true
                    }
                    other => {
                        // 记录号被新文件复用（序列号不同）= 旧文件的 Delete 事件丢失：
                        // 先给旧节点立墓碑，再建新节点占槽（比 HashMap 版更能自愈）
                        if let Some(old) = other {
                            self.tombstone(old);
                        }
                        let id = self.push_node(name, parent_id, is_dir, seq);
                        if rec >= self.rec_to_id.len() {
                            let need = rec + 1 - self.rec_to_id.len();
                            grow_exact(&mut self.rec_to_id, need, NODE_GROW_MIN);
                            self.rec_to_id.resize(rec + 1, NO_ID);
                        }
                        self.rec_to_id[rec] = id;
                        true
                    }
                }
            }
        }
    }

    /// 应用一批变更，返回真正改变索引内容的条数。
    pub fn apply_all(&mut self, changes: &[Change]) -> usize {
        changes.iter().filter(|c| self.apply(c)).count()
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
    fn contains_ci_matches_original_semantics() {
        assert!(contains_ci(b"Report.TXT", b"report"));
        assert!(contains_ci(b"Report.TXT", b".txt"));
        assert!(contains_ci(b"Report.TXT", b"t.t"));
        assert!(contains_ci(b"aab", b"ab"));
        assert!(contains_ci(b"AAB", b"ab"));
        assert!(!contains_ci(b"ab", b"abc"));
        assert!(!contains_ci(b"xyz", b"a"));
        assert!(contains_ci(b"x", b""));
        // 非 ASCII 字节原样比较
        assert!(contains_ci("报告.txt".as_bytes(), "告".as_bytes()));
        assert!(!contains_ci("报告.txt".as_bytes(), "吿".as_bytes()));
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
    fn suffix_wildcard_anchors_to_name_end() {
        let mut b = IndexBuilder::new(b'C');
        b.push(100, 5, 0, "a.txt");
        b.push(101, 5, 0, "a.txt.bak");
        b.push(102, 5, 0, "txt.txt");
        let idx = b.finish();
        let names = |q: &str| -> Vec<String> {
            let mut v: Vec<String> =
                idx.search(q, Category::All, 10).into_iter().map(|r| r.name).collect();
            v.sort();
            v
        };
        assert_eq!(names("*.txt"), ["a.txt", "txt.txt"]);
        // 回溯：第一个 .txt 处模式已用尽但名字未尽，继续尝试后面的 .txt
        assert!(glob_at(b"a.txt.txt", b"*.txt", true));
        assert_eq!(names("*.TXT"), ["a.txt", "txt.txt"]);
        // 以 * 结尾或不以 * 开头的模式仍是子串语义
        assert_eq!(names("*.txt*"), ["a.txt", "a.txt.bak", "txt.txt"]);
        assert_eq!(names("a.t?t"), ["a.txt", "a.txt.bak"]);
        assert_eq!(names("*.ba?"), ["a.txt.bak"]);
    }

    #[test]
    fn question_mark_matches_whole_utf8_char() {
        let mut b = IndexBuilder::new(b'C');
        b.push(100, 5, 0, "报告.txt");
        b.push(101, 5, 0, "报.txt");
        let idx = b.finish();
        assert_eq!(idx.search("报?.txt", Category::All, 10)[0].name, "报告.txt");
        assert_eq!(idx.search("报?.txt", Category::All, 10).len(), 1);
        assert_eq!(idx.search("*告?txt", Category::All, 10).len(), 1);
        // 一个汉字只算一个 ?，不会因从字符中间开始而多算
        assert!(glob_at("报".as_bytes(), b"?", true));
        assert!(!glob_at("报".as_bytes(), b"??", true));
        assert!(!wildcard_contains_ci("报".as_bytes(), b"??", false));
        assert!(wildcard_contains_ci("报告".as_bytes(), b"??", false));
    }

    #[test]
    fn full_width_space_splits_terms() {
        let mut b = IndexBuilder::new(b'C');
        b.push(100, 5, 0, "报告 2024.txt");
        b.push(101, 5, 0, "报告.txt");
        let idx = b.finish();
        assert_eq!(compile_query("报告\u{3000}2024").terms.len(), 2);
        let r = idx.search("2024\u{3000}报告", Category::All, 10);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].name, "报告 2024.txt");
    }

    #[test]
    fn stem_exact_ranks_before_longer_prefix() {
        let mut b = IndexBuilder::new(b'C');
        b.push(100, 5, 0, "reporting.txt");
        b.push(101, 5, 0, "report.pdf");
        b.push(102, 5, 0, "report");
        b.push(103, 5, FILE_ATTRIBUTE_DIRECTORY, "report.v2");
        let idx = b.finish();
        let r = idx.search("report", Category::All, 10);
        assert_eq!(r[0].name, "report");
        assert_eq!(r[1].name, "report.pdf");
        // 目录名里的点不算扩展名，report.v2 和 reporting.txt 一样只是前缀匹配，目录在前
        assert_eq!(r[2].name, "report.v2");
        assert_eq!(r[3].name, "reporting.txt");
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
        // 重复 Delete（槽已清空）不重计
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
    fn sequence_number_guards_record_reuse() {
        let seq = |s: u64, rec: u64| (s << 48) | rec;
        let mut b = IndexBuilder::new(b'C');
        b.push(seq(1, 100), 5, FILE_ATTRIBUTE_DIRECTORY, "Users");
        b.push(seq(3, 200), seq(1, 100), 0, "old.txt");
        let mut idx = b.finish();

        // 旧序列号的 Delete（属于更早的文件）不影响当前节点
        idx.apply(&Change::Delete { frn: seq(2, 200) });
        assert_eq!(idx.tombstones, 0);
        assert_eq!(idx.search("old", Category::All, 10).len(), 1);

        // 记录号被复用（序列号 4）且 Delete 丢失：旧节点自动立墓碑、新节点占槽
        idx.apply(&Change::Upsert {
            frn: seq(4, 200),
            parent_frn: seq(1, 100),
            attrs: 0,
            name: "new.txt".into(),
        });
        assert_eq!(idx.tombstones, 1);
        assert_eq!(idx.search("old", Category::All, 10).len(), 0);
        assert_eq!(idx.search("new", Category::All, 10)[0].path, "C:\\Users\\new.txt");

        // 同序列号再 Upsert = 改名，走原地更新（节点数不变）
        let n = idx.len();
        idx.apply(&Change::Upsert {
            frn: seq(4, 200),
            parent_frn: seq(1, 100),
            attrs: 0,
            name: "renamed.txt".into(),
        });
        assert_eq!(idx.len(), n);
        assert_eq!(idx.search("renamed", Category::All, 10).len(), 1);
    }

    #[test]
    fn set_name_does_not_grow_pool_for_same_or_shorter_name() {
        let mut b = IndexBuilder::new(b'C');
        b.push(100, 5, 0, "abcdef.txt");
        let mut idx = b.finish();
        let pool = idx.name_pool.len();
        // 同名 Upsert（USN 写入/关闭事件）：池不增长
        idx.apply(&Change::Upsert { frn: 100, parent_frn: 5, attrs: 0, name: "abcdef.txt".into() });
        assert_eq!(idx.name_pool.len(), pool);
        // 更短的名：原地覆盖
        idx.apply(&Change::Upsert { frn: 100, parent_frn: 5, attrs: 0, name: "ab.txt".into() });
        assert_eq!(idx.name_pool.len(), pool);
        assert_eq!(idx.search("ab.txt", Category::All, 10)[0].name, "ab.txt");
        assert_eq!(idx.search("abcdef", Category::All, 10).len(), 0);
        // 更长的名：才追加
        idx.apply(&Change::Upsert { frn: 100, parent_frn: 5, attrs: 0, name: "abcdefghij.txt".into() });
        assert!(idx.name_pool.len() > pool);
        assert_eq!(idx.search("abcdefghij", Category::All, 10).len(), 1);
    }

    #[test]
    fn incremental_growth_is_small_step_not_doubling() {
        let mut b = IndexBuilder::new(b'C');
        b.reserve(400_000);
        for i in 0..400_000u64 {
            b.push(1000 + i, 5, 0, "some_name.txt");
        }
        let mut idx = b.finish(); // shrink_to_fit → 容量 == 长度
        let before = idx.memory_bytes();
        idx.apply(&Change::Upsert { frn: 401_500, parent_frn: 5, attrs: 0, name: "new.txt".into() });
        let after = idx.memory_bytes();
        // 倍增会让 after ≈ 2×before；小步增长（len/32 或最小步长）应在 +25% 以内
        assert!(after < before + before / 4, "before={before} after={after}");
    }

    #[test]
    fn find_dir_by_path_skips_deleted_dirs() {
        let mut b = IndexBuilder::new(b'C');
        b.push(100, 5, FILE_ATTRIBUTE_DIRECTORY, "Proj");
        b.push(200, 100, 0, "a.txt");
        let mut idx = b.finish();
        // 删除目录及其内容，再用新 FRN 重建同名目录
        idx.apply(&Change::Delete { frn: 200 });
        idx.apply(&Change::Delete { frn: 100 });
        idx.apply(&Change::Upsert { frn: 101, parent_frn: 5, attrs: FILE_ATTRIBUTE_DIRECTORY, name: "Proj".into() });
        idx.apply(&Change::Upsert { frn: 201, parent_frn: 101, attrs: 0, name: "b.txt".into() });
        let anc = idx.find_dir_by_path("C:\\Proj").unwrap();
        assert!(idx.is_live_dir_at(anc, "C:\\proj\\"));
        assert_eq!(idx.search_under("b", Category::All, anc, 10).len(), 1);
        // 旧节点 id=1 已是墓碑，校验必须失败
        assert!(!idx.is_live_dir_at(1, "C:\\Proj"));
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

    fn spec(query: &CompiledQuery, limit: usize) -> ScanSpec<'_> {
        ScanSpec {
            query,
            category: Category::All,
            ancestor: None,
            limit,
            cancel: None,
        }
    }

    #[test]
    fn match_ranges_cover_terms_in_name_bytes() {
        let ranges = |name: &str, q: &str| match_ranges(name.as_bytes(), &compile_query(q).terms);
        assert_eq!(ranges("Report.TXT", "report"), [(0, 6)]);
        assert_eq!(ranges("Report.TXT", "txt rep"), [(0, 3), (7, 10)]);
        // 相邻和重叠的区间会合并
        assert_eq!(ranges("abcdef", "abc cde"), [(0, 5)]);
        // 汉字占 3 字节
        assert_eq!(ranges("年度报告.pdf", "报告"), [(6, 12)]);
        // 通配符只高亮字面部分，? 算进区间
        assert_eq!(ranges("a.txt.txt", "*.txt"), [(5, 9)]);
        assert_eq!(ranges("报告.txt", "报?.t*"), [(0, 8)]);
        assert_eq!(ranges("rep_x_final.txt", "rep*final"), [(0, 3), (6, 11)]);

        let mut b = IndexBuilder::new(b'C');
        b.push(100, 5, 0, "报告.txt");
        let idx = b.finish();
        assert_eq!(idx.search("告", Category::All, 1)[0].match_ranges, [(3, 6)]);
    }

    fn syntax_index() -> Index {
        let mut b = IndexBuilder::new(b'C');
        b.push(100, 5, FILE_ATTRIBUTE_DIRECTORY, "工作");
        b.push(101, 100, FILE_ATTRIBUTE_DIRECTORY, "报告");
        b.push(200, 101, 0, "年度.PDF");
        b.push(201, 100, 0, "报告.docx");
        b.push(202, 5, 0, "报告.pdf");
        b.push(203, 5, 0, "song.mp3");
        b.push(204, 5, 0, "setup.exe");
        b.finish()
    }

    fn search_names(idx: &Index, q: &str) -> Vec<String> {
        let mut v: Vec<String> =
            idx.search(q, Category::All, 50).into_iter().map(|r| r.name).collect();
        v.sort();
        v
    }

    #[test]
    fn ext_and_dir_filters() {
        let idx = syntax_index();
        assert_eq!(search_names(&idx, "ext:pdf"), ["年度.PDF", "报告.pdf"]);
        assert_eq!(search_names(&idx, "EXT:PDF,.docx 报告"), ["报告.docx", "报告.pdf"]);
        assert_eq!(search_names(&idx, "dir:"), ["工作", "报告"]);
        assert_eq!(search_names(&idx, "dir: 报告"), ["报告"]);
        // 文件夹没有扩展名，ext: 不选中文件夹
        assert!(search_names(&idx, "dir: ext:pdf").is_empty());
        // 不完整的语法按普通词处理（冒号不能出现在文件名里，所以没有结果）
        assert!(search_names(&idx, "ext:").is_empty());
        assert!(search_names(&idx, "dir:报告").is_empty());
    }

    #[test]
    fn path_terms_match_full_path_only_when_they_contain_backslash() {
        let idx = syntax_index();
        assert_eq!(search_names(&idx, "工作\\ 报告"), ["报告", "报告.docx"]);
        assert_eq!(search_names(&idx, "工作\\报告\\"), ["年度.PDF"]);
        assert_eq!(search_names(&idx, "c:\\song"), ["song.mp3"]);
        assert_eq!(search_names(&idx, "工作\\ ext:pdf"), ["年度.PDF"]);
        // 不含 \\ 的词只匹配文件名，不匹配路径
        assert_eq!(search_names(&idx, "工作"), ["工作"]);
        // 路径词不产生文件名高亮
        let r = idx.search("工作\\ 报告.d", Category::All, 5);
        assert_eq!(r[0].match_ranges, [(0, 8)]);
    }

    #[test]
    fn category_counts_ignore_category_filter() {
        let idx = syntax_index();
        let q = compile_query("报告");
        let mut s = spec(&q, 10);
        s.category = Category::Document;
        let scan = idx.collect_scored(&s);
        assert_eq!(scan.total_hits, 2);
        assert_eq!(scan.category_counts.get(Category::All), 3);
        assert_eq!(scan.category_counts.get(Category::Folder), 1);
        assert_eq!(scan.category_counts.get(Category::Document), 2);

        assert_eq!(idx.search("song", Category::Media, 5).len(), 1);
        assert_eq!(idx.search("song", Category::Other, 5).len(), 0);
        assert_eq!(idx.search("setup", Category::App, 5).len(), 1);
    }

    #[test]
    fn apply_reports_whether_index_changed() {
        let mut b = IndexBuilder::new(b'C');
        b.push(100, 5, 0, "a.txt");
        let mut idx = b.finish();
        let same = Change::Upsert { frn: 100, parent_frn: 5, attrs: 0, name: "a.txt".into() };
        assert!(!idx.apply(&same));
        assert!(idx.apply(&Change::Upsert { frn: 100, parent_frn: 5, attrs: 0, name: "b.txt".into() }));
        assert!(idx.apply(&Change::Upsert { frn: 200, parent_frn: 5, attrs: 0, name: "c.txt".into() }));
        assert!(!idx.apply(&Change::Delete { frn: 999 }));
        assert!(idx.apply(&Change::Delete { frn: 200 }));
        assert!(!idx.apply(&Change::Delete { frn: 200 }));
    }

    #[test]
    fn exact_match_after_many_hits_ranks_first() {
        // 旧实现命中 4000 条就停止扫描，排在后面的完全匹配会丢失。
        let mut b = IndexBuilder::new(b'C');
        for i in 0..50_000u64 {
            b.push(1000 + i, 5, 0, &format!("report_{i}.txt"));
        }
        b.push(60_000, 5, 0, "report");
        let idx = b.finish();
        let r = idx.search("report", Category::All, 10);
        assert_eq!(r.len(), 10);
        assert_eq!(r[0].name, "report");
    }

    #[test]
    fn scan_counts_all_hits_but_keeps_only_limit() {
        let mut b = IndexBuilder::new(b'C');
        for i in 0..30u64 {
            b.push(1000 + i, 5, 0, "x.txt");
        }
        let idx = b.finish();
        let terms = compile_query("x");
        let scan = idx.collect_scored(&spec(&terms, 10));
        assert_eq!(scan.hits.len(), 10);
        assert_eq!(scan.total_hits, 30);
        // 完全相同的键按 id 决定先后，保留最小的 10 个 id
        let mut ids: Vec<u32> = scan.hits.iter().map(|(_, id)| *id).collect();
        ids.sort_unstable();
        assert_eq!(ids, (1..=10).collect::<Vec<u32>>());
        let scan = idx.collect_scored(&spec(&terms, 100));
        assert_eq!(scan.hits.len(), 30);
        assert_eq!(idx.collect_scored(&spec(&terms, 0)).total_hits, 30);
    }

    #[test]
    fn parallel_scan_matches_single_thread() {
        // 超过 PAR_MIN_NODES 触发多线程路径；结果集合应与单线程一致
        let mut b = IndexBuilder::new(b'C');
        let n = PAR_MIN_NODES as u64 + 1000;
        b.reserve(n as usize);
        for i in 0..n {
            let name = if i % 997 == 0 { format!("hit{i}.log") } else { format!("f{i}.dat") };
            b.push(1000 + i, 5, 0, &name);
        }
        let idx = b.finish();
        let terms = compile_query("hit");
        let par = idx.collect_scored(&spec(&terms, 1_000_000));
        let single = idx.collect_range(1, idx.len() as u32, &spec(&terms, 1_000_000));
        assert_eq!(par.total_hits, single.total_hits);
        let mut a: Vec<u32> = par.hits.into_iter().map(|(_, id)| id).collect();
        let mut c: Vec<u32> = single.hits.into_iter().map(|(_, id)| id).collect();
        a.sort_unstable();
        c.sort_unstable();
        assert_eq!(a, c);
        assert_eq!(a.len(), (n as usize).div_ceil(997));

        // 只保留前几条时，多线程和单线程的结果和顺序也一致
        let mut par = idx.collect_scored(&spec(&terms, 50)).hits;
        let mut single = idx.collect_range(1, idx.len() as u32, &spec(&terms, 50)).hits;
        par.sort_unstable();
        par.truncate(50);
        single.sort_unstable();
        assert_eq!(
            par.iter().map(|(_, id)| *id).collect::<Vec<_>>(),
            single.iter().map(|(_, id)| *id).collect::<Vec<_>>()
        );
    }

    /// 合成性能测试：`cargo test --release -p ss-core -- --ignored --nocapture`。
    #[test]
    #[ignore = "合成性能测试，单独运行"]
    fn perf_single_char_query_on_2m_nodes() {
        const WORDS: &[&str] = &["report", "final", "backup", "image", "报告", "项目", "data", "notes"];
        const EXTS: &[&str] = &["txt", "pdf", "docx", "png", "zip", "dll", "exe", "log"];
        let n = 2_000_000u64;
        let mut b = IndexBuilder::new(b'C');
        b.reserve(n as usize);
        b.push(100, 5, FILE_ATTRIBUTE_DIRECTORY, "root");
        for i in 0..n {
            let w1 = WORDS[(i % 8) as usize];
            let w2 = WORDS[((i >> 3) % 8) as usize];
            let ext = EXTS[((i >> 6) % 8) as usize];
            let parent = if i % 20 == 0 { 100 } else { 1000 + (i / 20) * 20 };
            let attrs = if i % 20 == 0 { FILE_ATTRIBUTE_DIRECTORY } else { 0 };
            b.push(1000 + i, parent, attrs, &format!("{w1}_{w2}_{i}.{ext}"));
        }
        let idx = b.finish();
        for q in ["a", "e", "t", "0"] {
            let mut times: Vec<_> = (0..7)
                .map(|_| {
                    let t = std::time::Instant::now();
                    assert_eq!(idx.search(q, Category::All, 200).len(), 200);
                    t.elapsed()
                })
                .collect();
            times.sort_unstable();
            let median = times[times.len() / 2];
            eprintln!("单字符查询 {q:?}：中位 {median:.2?}，最慢 {:.2?}", times[times.len() - 1]);
            assert!(median.as_millis() <= 150, "查询 {q:?} 中位 {median:?} 超过 150 ms");
        }
    }
}
