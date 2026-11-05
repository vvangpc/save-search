//! 虚拟列表的滚动计算。

/// 列表的尺寸（像素）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListGeometry {
    pub count: i32,
    pub item_h: i32,
    pub view_h: i32,
    /// 行之后的固定内容高度（例如语法提示）。
    pub extra_h: i32,
}

impl ListGeometry {
    pub fn total(&self) -> i32 {
        self.count.max(0) * self.item_h + self.extra_h
    }

    pub fn max_scroll(&self) -> f32 {
        (self.total() - self.view_h).max(0) as f32
    }

    /// 一页的行数，至少为 1。
    pub fn page(&self) -> i32 {
        (self.view_h / self.item_h.max(1)).max(1)
    }

    /// 视图内 `y` 处的行号。空白处返回 `None`。
    pub fn index_at(&self, y: i32, scroll: f32) -> Option<i32> {
        if self.item_h <= 0 {
            return None;
        }
        let pos = y + scroll as i32;
        if pos < 0 {
            return None;
        }
        let idx = pos / self.item_h;
        (idx < self.count).then_some(idx)
    }

    /// 需要绘制的行（闭区间）。没有可见行时返回 `None`。
    pub fn visible(&self, scroll: f32) -> Option<(i32, i32)> {
        if self.count <= 0 || self.item_h <= 0 {
            return None;
        }
        let sy = scroll as i32;
        let first = (sy / self.item_h).max(0);
        let last = ((sy + self.view_h) / self.item_h).min(self.count - 1);
        (first <= last).then_some((first, last))
    }

    /// 让第 `idx` 行完全可见所需的滚动目标。已经可见时返回 `target`。
    pub fn ensure_visible(&self, idx: i32, target: f32) -> f32 {
        let top = (idx * self.item_h) as f32;
        let bottom = top + self.item_h as f32;
        if top < target {
            top
        } else if bottom > target + self.view_h as f32 {
            bottom - self.view_h as f32
        } else {
            target
        }
    }
}

/// 缓动一帧，返回（新位置，是否到达）。
pub fn ease_toward(current: f32, target: f32) -> (f32, bool) {
    let d = target - current;
    if d.abs() < 0.5 {
        (target, true)
    } else {
        (current + d * 0.30, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const G: ListGeometry = ListGeometry { count: 10, item_h: 40, view_h: 100, extra_h: 0 };

    #[test]
    fn max_scroll_and_page() {
        assert_eq!(G.max_scroll(), 300.0);
        assert_eq!(G.page(), 2);
        let short = ListGeometry { count: 1, ..G };
        assert_eq!(short.max_scroll(), 0.0);
        let extra = ListGeometry { count: 1, extra_h: 200, ..G };
        assert_eq!(extra.max_scroll(), 140.0);
        assert_eq!(ListGeometry { view_h: 10, ..G }.page(), 1);
    }

    #[test]
    fn index_at_accounts_for_scroll_and_blank_space() {
        assert_eq!(G.index_at(0, 0.0), Some(0));
        assert_eq!(G.index_at(39, 0.0), Some(0));
        assert_eq!(G.index_at(40, 0.0), Some(1));
        assert_eq!(G.index_at(0, 45.0), Some(1));
        assert_eq!(G.index_at(400, 0.0), None);
        assert_eq!(G.index_at(-5, 0.0), None);
    }

    #[test]
    fn visible_range_is_clamped() {
        assert_eq!(G.visible(0.0), Some((0, 2)));
        assert_eq!(G.visible(300.0), Some((7, 9)));
        assert_eq!(ListGeometry { count: 0, ..G }.visible(0.0), None);
    }

    #[test]
    fn ensure_visible_scrolls_only_when_needed() {
        assert_eq!(G.ensure_visible(0, 0.0), 0.0);
        assert_eq!(G.ensure_visible(5, 0.0), 140.0);
        assert_eq!(G.ensure_visible(1, 120.0), 40.0);
        assert_eq!(G.ensure_visible(3, 100.0), 100.0);
    }

    #[test]
    fn easing_reaches_target() {
        let (mut pos, mut done) = (0.0, false);
        for _ in 0..100 {
            (pos, done) = ease_toward(pos, 100.0);
            if done {
                break;
            }
        }
        assert!(done);
        assert_eq!(pos, 100.0);
    }
}
