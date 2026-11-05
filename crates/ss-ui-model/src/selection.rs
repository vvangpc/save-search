//! 选中项规则。

/// 键盘移动选中项的方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelMove {
    Up,
    Down,
    PageUp,
    PageDown,
    Home,
    End,
}

/// 移动后的选中项，钳到有效范围。列表为空时返回 `None`。`current` 为 -1 表示没有选中。
pub fn moved(current: i32, how: SelMove, count: i32, page: i32) -> Option<i32> {
    if count <= 0 {
        return None;
    }
    let next = match how {
        SelMove::Up => current - 1,
        SelMove::Down => current + 1,
        SelMove::PageUp => current - page,
        SelMove::PageDown => current + page,
        SelMove::Home => 0,
        SelMove::End => count - 1,
    };
    Some(next.clamp(0, count - 1))
}

/// 后台刷新后的选中项：优先找同一路径，找不到时留在原来的位置（钳到有效范围）。
pub fn reselect<'a>(
    old_path: Option<&str>,
    old_sel: i32,
    new_paths: impl ExactSizeIterator<Item = &'a str>,
) -> i32 {
    let count = new_paths.len() as i32;
    if count == 0 {
        return -1;
    }
    if let Some(old) = old_path {
        let mut paths = new_paths;
        if let Some(i) = paths.position(|p| p == old) {
            return i as i32;
        }
    }
    old_sel.clamp(0, count - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reselect_follows_path_then_falls_back_to_position() {
        let new = ["a", "b", "c"];
        assert_eq!(reselect(Some("c"), 0, new.iter().copied()), 2);
        assert_eq!(reselect(Some("gone"), 1, new.iter().copied()), 1);
        assert_eq!(reselect(Some("gone"), 9, new.iter().copied()), 2);
        assert_eq!(reselect(None, -1, new.iter().copied()), 0);
        assert_eq!(reselect(Some("a"), 0, [].iter().copied()), -1);
    }

    #[test]
    fn moves_are_clamped() {
        assert_eq!(moved(0, SelMove::Up, 5, 2), Some(0));
        assert_eq!(moved(-1, SelMove::Down, 5, 2), Some(0));
        assert_eq!(moved(4, SelMove::Down, 5, 2), Some(4));
        assert_eq!(moved(1, SelMove::PageDown, 5, 2), Some(3));
        assert_eq!(moved(1, SelMove::PageUp, 5, 2), Some(0));
        assert_eq!(moved(2, SelMove::Home, 5, 2), Some(0));
        assert_eq!(moved(2, SelMove::End, 5, 2), Some(4));
        assert_eq!(moved(0, SelMove::Down, 0, 2), None);
    }
}
