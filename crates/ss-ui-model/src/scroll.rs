//! 虚拟列表的滚动计算。

/// 列表的尺寸（像素）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListGeometry {
    pub count: i32,
    pub item_h: i32,
    pub view_h: i32,
    /// 行之后的固定内容高度（例如语法提示）。
    pub extra_h: i32,
    /// 分组标题的高度。
    pub header_h: i32,
    /// 标题出现在这些行之前，-1 表示没有。
    pub heads: [i32; 2],
}

impl ListGeometry {
    /// 没有分组标题和附加内容的列表。
    pub fn plain(count: i32, item_h: i32, view_h: i32) -> Self {
        ListGeometry { count, item_h, view_h, extra_h: 0, header_h: 0, heads: [-1, -1] }
    }

    /// 第 `idx` 行顶部的位置。`idx == count` 时是最后一行的底部。
    pub fn row_top(&self, idx: i32) -> i32 {
        let headers = self.heads.iter().filter(|h| **h >= 0 && **h <= idx).count() as i32;
        idx * self.item_h + headers * self.header_h
    }

    pub fn total(&self) -> i32 {
        self.row_top(self.count.max(0)) + self.extra_h
    }

    pub fn max_scroll(&self) -> f32 {
        (self.total() - self.view_h).max(0) as f32
    }

    /// 一页的行数，至少为 1。
    pub fn page(&self) -> i32 {
        (self.view_h / self.item_h.max(1)).max(1)
    }

    /// 视图内 `y` 处的行号。空白处和分组标题上返回 `None`。
    pub fn index_at(&self, y: i32, scroll: f32) -> Option<i32> {
        if self.item_h <= 0 {
            return None;
        }
        let pos = y + scroll as i32;
        if pos < 0 {
            return None;
        }
        if self.header_h == 0 {
            let idx = pos / self.item_h;
            return (idx < self.count).then_some(idx);
        }
        (0..self.count).find(|&i| {
            let top = self.row_top(i);
            pos >= top && pos < top + self.item_h
        })
    }

    /// 需要绘制的行（闭区间）。没有可见行时返回 `None`。
    pub fn visible(&self, scroll: f32) -> Option<(i32, i32)> {
        if self.count <= 0 || self.item_h <= 0 {
            return None;
        }
        let sy = scroll as i32;
        if self.header_h == 0 {
            let first = (sy / self.item_h).max(0);
            let last = ((sy + self.view_h) / self.item_h).min(self.count - 1);
            return (first <= last).then_some((first, last));
        }
        let first = (0..self.count).find(|&i| self.row_top(i) + self.item_h > sy)?;
        let last = (first..self.count).rev().find(|&i| self.row_top(i) < sy + self.view_h)?;
        Some((first, last))
    }

    /// 让第 `idx` 行完全可见所需的滚动目标。已经可见时返回 `target`。
    pub fn ensure_visible(&self, idx: i32, target: f32) -> f32 {
        let top = self.row_top(idx) as f32;
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

/// 自绘滚动条的滑块：（顶部位置，高度）。内容不超过视图时返回 `None`。
pub fn scroll_thumb(total: i32, view: i32, scroll: f32, min_h: i32) -> Option<(i32, i32)> {
    if total <= view || view <= 0 {
        return None;
    }
    let h = (view as i64 * view as i64 / total as i64) as i32;
    let h = h.max(min_h).min(view);
    let max_scroll = (total - view) as f32;
    let top = ((view - h) as f32 * (scroll / max_scroll).clamp(0.0, 1.0)) as i32;
    Some((top, h))
}

/// 把滑块拖到 `thumb_top` 时对应的滚动位置。
pub fn scroll_from_thumb(total: i32, view: i32, thumb_h: i32, thumb_top: i32) -> f32 {
    let room = view - thumb_h;
    if room <= 0 || total <= view {
        return 0.0;
    }
    let ratio = thumb_top.clamp(0, room) as f32 / room as f32;
    ratio * (total - view) as f32
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

    const G: ListGeometry =
        ListGeometry { count: 10, item_h: 40, view_h: 100, extra_h: 0, header_h: 0, heads: [-1, -1] };
    /// 两组：第 0 行和第 2 行前各有 20 像素的标题。
    const H: ListGeometry =
        ListGeometry { count: 4, item_h: 40, view_h: 100, extra_h: 30, header_h: 20, heads: [0, 2] };

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
    fn headers_shift_rows() {
        assert_eq!(H.row_top(0), 20);
        assert_eq!(H.row_top(1), 60);
        assert_eq!(H.row_top(2), 120);
        assert_eq!(H.row_top(4), 200);
        assert_eq!(H.total(), 230);
        // 标题上不算任何一行
        assert_eq!(H.index_at(10, 0.0), None);
        assert_eq!(H.index_at(20, 0.0), Some(0));
        assert_eq!(H.index_at(105, 0.0), None);
        assert_eq!(H.index_at(120, 0.0), Some(2));
        assert_eq!(H.visible(0.0), Some((0, 1)));
        assert_eq!(H.visible(130.0), Some((2, 3)));
        assert_eq!(H.ensure_visible(3, 0.0), 100.0);
    }

    #[test]
    fn thumb_position_round_trips() {
        assert_eq!(scroll_thumb(100, 100, 0.0, 10), None);
        assert_eq!(scroll_thumb(400, 100, 0.0, 10), Some((0, 25)));
        assert_eq!(scroll_thumb(400, 100, 300.0, 10), Some((75, 25)));
        // 内容很长时滑块不小于最小高度
        assert_eq!(scroll_thumb(100_000, 100, 0.0, 20), Some((0, 20)));
        assert_eq!(scroll_from_thumb(400, 100, 25, 75), 300.0);
        assert_eq!(scroll_from_thumb(400, 100, 25, 999), 300.0);
        assert_eq!(scroll_from_thumb(400, 100, 25, -5), 0.0);
        assert_eq!(scroll_from_thumb(50, 100, 100, 0), 0.0);
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
