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

    pub fn search_ids(&self, query: &str, category: Category, limit: usize) -> Vec<u32> {
        let mut out = Vec::new();
        if query.is_empty() {
            return out;
        }
        let q: Vec<u8> = query.bytes().map(|b| b.to_ascii_lowercase()).collect();
        for id in 1..self.names_len.len() as u32 {
            if self.is_deleted(id) {
                continue;
            }
            let nb = self.name_bytes(id);
            if !contains_ci(nb, &q) {
                continue;
            }
            if !crate::category::matches(category, nb, self.is_dir(id)) {
                continue;
            }
            out.push(id);
            if out.len() >= limit {
                break;
            }
        }
        out
    }

    pub fn search(&self, query: &str, category: Category, limit: usize) -> Vec<SearchResult> {
        self.search_ids(query, category, limit)
            .into_iter()
            .map(|id| SearchResult {
                name: self.name(id),
                path: self.path(id),
                is_dir: self.is_dir(id),
            })
            .collect()
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
}
