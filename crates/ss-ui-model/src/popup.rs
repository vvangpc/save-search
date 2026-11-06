//! 快选浮窗的条目，顺序是已打开、收藏、最近打开。
//! 已打开的文件夹异步读取，同组内已有条目的相对顺序不变。

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
    for p in open {
        push_unique(&mut out, EntrySource::Open, p);
    }
    if loading {
        out.push(PopupEntry { source: EntrySource::Loading, path: String::new(), dead: false });
    }
    for p in favorites {
        push_unique(&mut out, EntrySource::Favorite, p);
    }
    for p in recent {
        push_unique(&mut out, EntrySource::Recent, p);
    }
    out
}

/// 用最新读到的已打开文件夹更新条目：去掉已关闭的，新的排在已打开组的最后。
/// 同一路径优先归已打开组；已关闭的路径按完整的收藏和最近列表恢复归属。
pub fn merge_open(
    entries: &[PopupEntry], open: &[String], favorites: &[String], recent: &[String],
) -> Vec<PopupEntry> {
    let is_open = |path: &str| open.iter().any(|p| same_path(p, path));
    let mut stable_open: Vec<String> = entries.iter()
        .filter(|e| e.source == EntrySource::Open && is_open(&e.path))
        .map(|e| e.path.clone()).collect();
    for p in open {
        if !stable_open.iter().any(|old| same_path(old, p)) {
            stable_open.push(p.clone());
        }
    }
    let mut out = initial_entries(favorites, recent, &stable_open, false);
    let dead: Vec<String> = entries.iter().filter(|e| e.dead).map(|e| e.path.clone()).collect();
    mark_dead(&mut out, &dead);
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
        assert_eq!(paths(&e), ["C:\\b", "C:\\c", "", "C:\\a"]);
        assert_eq!(e[0].source, EntrySource::Open);
        assert_eq!(e[1].source, EntrySource::Open);
        assert_eq!(e[2].source, EntrySource::Loading);
        assert_eq!(e[3].source, EntrySource::Favorite);
    }

    #[test]
    fn merge_keeps_groups_and_order() {
        let e = initial_entries(&s(&["C:\\fav"]), &s(&["C:\\rec", "C:\\r2"]), &s(&["C:\\o1", "C:\\o2"]), true);
        let m = merge_open(&e, &s(&["C:\\o3", "C:\\o2", "C:\\REC"]), &s(&["C:\\fav"]), &s(&["C:\\rec", "C:\\r2"]));
        // o1 已关闭；o2 排在前面；新读到的 o3 和 rec 排在已打开组后面；r2 仍在最近组
        assert_eq!(paths(&m), ["C:\\o2", "C:\\o3", "C:\\REC", "C:\\fav", "C:\\r2"]);
        assert_eq!(m[2].source, EntrySource::Open);
        assert_eq!(m[3].source, EntrySource::Favorite);
        assert_eq!(m[4].source, EntrySource::Recent);
    }

    #[test]
    fn merge_is_stable_when_nothing_changed() {
        let e = initial_entries(&[], &s(&["C:\\r"]), &s(&["C:\\o"]), false);
        assert_eq!(merge_open(&e, &s(&["C:\\o"]), &[], &s(&["C:\\r"])), e);
    }

    #[test]
    fn open_paths_take_priority_and_restore_their_source_when_closed() {
        let favorites = s(&["C:\\shared", "C:\\fav"]);
        let recent = s(&["C:\\SHARED\\", "C:\\rec"]);
        let mut e = initial_entries(&favorites, &recent, &[], false);
        mark_dead(&mut e, &s(&["C:\\shared"]));
        let opened = merge_open(&e, &s(&["c:\\shared\\", "C:\\rec"]), &favorites, &recent);
        assert_eq!(paths(&opened), ["c:\\shared\\", "C:\\rec", "C:\\fav"]);
        assert_eq!(opened[0].source, EntrySource::Open);
        assert!(opened[0].dead);
        let closed = merge_open(&opened, &[], &favorites, &recent);
        assert_eq!(closed, e);
        assert_eq!(closed[0].source, EntrySource::Favorite);
        assert_eq!(closed[2].source, EntrySource::Recent);
    }

    #[test]
    fn dead_paths_and_loading_row() {
        let mut e = initial_entries(&s(&["C:\\a"]), &s(&["C:\\b"]), &[], true);
        mark_dead(&mut e, &s(&["c:\\b\\"]));
        assert!(!e[0].dead && !e[1].dead && e[2].dead);
        assert_eq!(without_loading(&e).len(), 2);
    }

    #[test]
    fn leaf_names() {
        assert_eq!(leaf_of("D:\\a\\b"), "b");
        assert_eq!(leaf_of("D:\\a\\b\\"), "b");
        assert_eq!(leaf_of("D:\\"), "D:");
    }
}
