//! 搜索范围和设置里保存的文字之间的转换。

/// 保存到设置的范围：空 = 全部；`C` = 某个盘；完整路径 = 某个文件夹。
pub fn scope_key(drive: Option<char>, folder: Option<&str>) -> String {
    match (folder, drive) {
        (Some(f), _) => f.to_string(),
        (None, Some(d)) => d.to_string(),
        (None, None) => String::new(),
    }
}

/// 范围下拉框里的序号：0 = 全部盘，之后是各盘，再之后是预设文件夹。
/// 保存的范围已经不在列表中时回到全部盘。
pub fn scope_index(key: &str, drives: &[char], presets: &[String]) -> usize {
    if key.is_empty() {
        return 0;
    }
    let mut chars = key.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        return drives.iter().position(|d| *d == c).map_or(0, |i| i + 1);
    }
    presets
        .iter()
        .position(|p| p == key)
        .map_or(0, |i| i + 1 + drives.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_round_trips_through_settings() {
        let drives = ['C', 'D'];
        let presets = vec!["D:\\工作".to_string()];
        assert_eq!(scope_index(&scope_key(None, None), &drives, &presets), 0);
        assert_eq!(scope_index(&scope_key(Some('D'), None), &drives, &presets), 2);
        assert_eq!(scope_index(&scope_key(None, Some("D:\\工作")), &drives, &presets), 3);
    }

    #[test]
    fn missing_scope_falls_back_to_all() {
        let drives = ['C'];
        assert_eq!(scope_index("E", &drives, &[]), 0);
        assert_eq!(scope_index("D:\\旧项目", &drives, &[]), 0);
    }
}
