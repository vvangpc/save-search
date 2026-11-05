//! 快选浮窗的条目：收藏和最近先显示，已打开的文件夹异步补在后面，已有条目的位置不变。

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
    for p in recent {
        push_unique(&mut out, EntrySource::Recent, p);
    }
    for p in open {
        push_unique(&mut out, EntrySource::Open, p);
    }
    if loading {
        out.push(PopupEntry { source: EntrySource::Loading, path: String::new(), dead: false });
    }
    out
}

/// 用最新读到的已打开文件夹更新条目：去掉已关闭的，新的补在最后，其余顺序不变。
pub fn merge_open(entries: &[PopupEntry], open: &[String]) -> Vec<PopupEntry> {
    let mut out: Vec<PopupEntry> = entries
        .iter()
        .filter(|e| match e.source {
            EntrySource::Loading => false,
            EntrySource::Open => open.iter().any(|p| same_path(p, &e.path)),
            _ => true,
        })
        .cloned()
        .collect();
    for p in open {
        push_unique(&mut out, EntrySource::Open, p);
    }
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
        assert_eq!(e[1].source, EntrySource::Recent);
        assert_eq!(e[2].source, EntrySource::Open);
        assert_eq!(e[3].source, EntrySource::Loading);
    }

    #[test]
    fn merge_keeps_positions_and_appends_new_folders() {
        let e = initial_entries(&s(&["C:\\fav"]), &s(&["C:\\rec"]), &s(&["C:\\o1", "C:\\o2"]), true);
        let m = merge_open(&e, &s(&["C:\\o3", "C:\\o2", "C:\\REC"]));
        // o1 已关闭；o2 留在原位；o3 补在最后；已存在的最近位置不重复
        assert_eq!(paths(&m), ["C:\\fav", "C:\\rec", "C:\\o2", "C:\\o3"]);
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
