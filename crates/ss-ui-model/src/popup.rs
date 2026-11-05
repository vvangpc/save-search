//! 快选浮窗的条目，顺序是收藏、已打开、最近。
//! 已打开的文件夹异步读取，读到后插到最近位置之前，同组内已有条目的相对顺序不变。

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntrySource {
    Favorite,
    Recent,
    Open,
    /// “正在读取”占位行，没有路径。
    Loading,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PopupEntry {
    pub source: EntrySource,
    pub path: String,
    /// 路径已失效，不可点击。
    pub dead: bool,
}

fn same_path(a: &str, b: &str) -> bool {
    a.trim_end_matches('\\')
        .eq_ignore_ascii_case(b.trim_end_matches('\\'))
}

fn push_unique(out: &mut Vec<PopupEntry>, source: EntrySource, path: &str) {
    if !path.is_empty() && !out.iter().any(|e| same_path(&e.path, path)) {
        out.push(PopupEntry { source, path: path.to_string(), dead: false });
    }
}

/// 立即可用的条目。`open` 是上次读到的已打开文件夹。
pub fn initial_entries(
    favorites: &[String],
    recent: &[String],
    open: &[String],
    loading: bool,
) -> Vec<PopupEntry> {
    let mut out = Vec::new();
    for p in favorites {
        push_unique(&mut out, EntrySource::Favorite, p);
    }
    for p in open {
        push_unique(&mut out, EntrySource::Open, p);
    }
    for p in recent {
        push_unique(&mut out, EntrySource::Recent, p);
    }
    if loading {
        out.push(PopupEntry { source: EntrySource::Loading, path: String::new(), dead: false });
    }
    out
}

/// 用最新读到的已打开文件夹更新条目：去掉已关闭的，新的排在已打开组的最后。
/// 已经在收藏里的路径不变；出现在最近里的路径改归已打开组。
pub fn merge_open(entries: &[PopupEntry], open: &[String]) -> Vec<PopupEntry> {
    let is_open = |path: &str| open.iter().any(|p| same_path(p, path));
    let mut out: Vec<PopupEntry> =
        entries.iter().filter(|e| e.source == EntrySource::Favorite).cloned().collect();
    out.extend(entries.iter().filter(|e| e.source == EntrySource::Open && is_open(&e.path)).cloned());
    for p in open {
        push_unique(&mut out, EntrySource::Open, p);
    }
    let dead = |path: &str| entries.iter().any(|e| e.dead && same_path(&e.path, path));
    for e in out.iter_mut() {
        e.dead = dead(&e.path);
    }
    out.extend(entries.iter().filter(|e| e.source == EntrySource::Recent && !is_open(&e.path)).cloned());
    out
}

/// 去掉“正在读取”占位行。
pub fn without_loading(entries: &[PopupEntry]) -> Vec<PopupEntry> {
    entries.iter().filter(|e| e.source != EntrySource::Loading).cloned().collect()
}

/// 按后台检查的结果标记失效路径。
pub fn mark_dead(entries: &mut [PopupEntry], dead: &[String]) {
    for e in entries.iter_mut() {
        e.dead = dead.iter().any(|p| same_path(p, &e.path));
    }
}

/// 路径的最后一段。`D:\a\b` → `b`，`D:\` → `D:`。
pub fn leaf_of(path: &str) -> &str {
    let p = path.trim_end_matches('\\');
    match p.rfind('\\') {
        Some(i) => &p[i + 1..],
        None => p,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    fn paths(v: &[PopupEntry]) -> Vec<&str> {
        v.iter().map(|e| e.path.as_str()).collect()
    }

    #[test]
    fn initial_order_and_dedup() {
        let e = initial_entries(&s(&["C:\\a"]), &s(&["c:\\A\\", "C:\\b"]), &s(&["C:\\b", "C:\\c"]), true);
        assert_eq!(paths(&e), ["C:\\a", "C:\\b", "C:\\c", ""]);
        assert_eq!(e[0].source, EntrySource::Favorite);
        assert_eq!(e[1].source, EntrySource::Open);
        assert_eq!(e[2].source, EntrySource::Open);
        assert_eq!(e[3].source, EntrySource::Loading);
    }

    #[test]
    fn merge_keeps_groups_and_order() {
        let e = initial_entries(&s(&["C:\\fav"]), &s(&["C:\\rec", "C:\\r2"]), &s(&["C:\\o1", "C:\\o2"]), true);
        let m = merge_open(&e, &s(&["C:\\o3", "C:\\o2", "C:\\REC"]));
        // o1 已关闭；o2 排在前面；新读到的 o3 和 rec 排在已打开组后面；r2 仍在最近组
        assert_eq!(paths(&m), ["C:\\fav", "C:\\o2", "C:\\o3", "C:\\REC", "C:\\r2"]);
        assert_eq!(m[3].source, EntrySource::Open);
        assert_eq!(m[4].source, EntrySource::Recent);
    }

    #[test]
    fn merge_is_stable_when_nothing_changed() {
        let e = initial_entries(&[], &s(&["C:\\r"]), &s(&["C:\\o"]), false);
        assert_eq!(merge_open(&e, &s(&["C:\\o"])), e);
    }

    #[test]
    fn dead_paths_and_loading_row() {
        let mut e = initial_entries(&s(&["C:\\a"]), &s(&["C:\\b"]), &[], true);
        mark_dead(&mut e, &s(&["c:\\b\\"]));
        assert!(!e[0].dead && e[1].dead && !e[2].dead);
        assert_eq!(without_loading(&e).len(), 2);
    }

    #[test]
    fn leaf_names() {
        assert_eq!(leaf_of("D:\\a\\b"), "b");
        assert_eq!(leaf_of("D:\\a\\b\\"), "b");
        assert_eq!(leaf_of("D:\\"), "D:");
    }
}
