//! 显示文字：命中高亮分段、路径拆分、大小和日期格式、Enter 的行为。

/// 把文件名按命中区间（字节）分段，返回（文字，是否命中）。
/// 区间不在字符边界上时整体不高亮，不会出错。
pub fn highlight_segments<'a>(name: &'a str, ranges: &[(u16, u16)]) -> Vec<(&'a str, bool)> {
    let mut out = Vec::with_capacity(ranges.len() * 2 + 1);
    let mut pos = 0usize;
    for &(start, end) in ranges {
        let (start, end) = (start as usize, end as usize);
        let (Some(before), Some(hit)) = (name.get(pos..start), name.get(start..end)) else {
            return vec![(name, false)];
        };
        if !before.is_empty() {
            out.push((before, false));
        }
        if !hit.is_empty() {
            out.push((hit, true));
        }
        pos = end;
    }
    match name.get(pos..) {
        Some(rest) => {
            if !rest.is_empty() {
                out.push((rest, false));
            }
            out
        }
        None => vec![(name, false)],
    }
}

/// 路径的父目录。`D:\a\b` → `D:\a`，`D:\a` → `D:\`，`D:\` → `D:\`。
pub fn parent_of(path: &str) -> &str {
    let trimmed = path.trim_end_matches('\\');
    match trimmed.rfind('\\') {
        Some(i) if trimmed[..i].ends_with(':') => &path[..=i],
        Some(i) => &path[..i],
        None => path,
    }
}

/// `2_400_000` → `2.3 MB`。
pub fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    if bytes < KB {
        format!("{bytes} B")
    } else if bytes < MB {
        format!("{} KB", (bytes + KB / 2) / KB)
    } else if bytes < GB {
        with_tenth(bytes, MB, "MB")
    } else {
        with_tenth(bytes, GB, "GB")
    }
}

/// 保留一位小数，四舍五入。
fn with_tenth(bytes: u64, unit: u64, name: &str) -> String {
    let tenths = (u128::from(bytes) * 10 + u128::from(unit) / 2) / u128::from(unit);
    format!("{}.{} {name}", tenths / 10, tenths % 10)
}

/// 本地日期和时间。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalTime {
    pub year: i32,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
}

fn day_number(t: LocalTime) -> i64 {
    // 公历日期转连续天数
    let y = i64::from(t.year) - i64::from(t.month <= 2);
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = (i64::from(t.month) + 9) % 12;
    let doy = (153 * mp + 2) / 5 + i64::from(t.day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe
}

/// 今天 `今天 09:41`，昨天 `昨天 18:20`，今年 `10月 28日`，更早 `2025年 1月`。
pub fn format_date(now: LocalTime, t: LocalTime) -> String {
    match day_number(now) - day_number(t) {
        0 => format!("今天 {:02}:{:02}", t.hour, t.minute),
        1 => format!("昨天 {:02}:{:02}", t.hour, t.minute),
        _ if t.year == now.year => format!("{}月 {}日", t.month, t.day),
        _ => format!("{}年 {}月", t.year, t.month),
    }
}

/// 对结果执行的操作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenOp {
    Open,
    Reveal,
}

/// Enter 或 Ctrl+Enter 对应的操作。`enter_reveals` 来自设置，Ctrl+Enter 总是另一个操作。
pub fn enter_op(enter_reveals: bool, ctrl: bool) -> OpenOp {
    if enter_reveals != ctrl {
        OpenOp::Reveal
    } else {
        OpenOp::Open
    }
}

/// 输入框的选区是否只选中了部分文字。这时 Ctrl+C 复制文字，否则复制结果路径。
pub fn is_partial_selection(start: u32, end: u32, len: u32) -> bool {
    start != end && !(start == 0 && end >= len)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segments_cover_whole_name() {
        assert_eq!(
            highlight_segments("report.pdf", &[(0, 6)]),
            vec![("report", true), (".pdf", false)]
        );
        assert_eq!(
            highlight_segments("a报告b", &[(1, 7)]),
            vec![("a", false), ("报告", true), ("b", false)]
        );
        assert_eq!(highlight_segments("abc", &[]), vec![("abc", false)]);
        assert_eq!(
            highlight_segments("abcd", &[(0, 1), (2, 3)]),
            vec![("a", true), ("b", false), ("c", true), ("d", false)]
        );
        // 区间不在字符边界上或超出范围：不高亮
        assert_eq!(highlight_segments("报告", &[(1, 2)]), vec![("报告", false)]);
        assert_eq!(highlight_segments("ab", &[(0, 9)]), vec![("ab", false)]);
    }

    #[test]
    fn parent_paths() {
        assert_eq!(parent_of("D:\\a\\b.txt"), "D:\\a");
        assert_eq!(parent_of("D:\\a"), "D:\\");
        assert_eq!(parent_of("D:\\"), "D:\\");
        assert_eq!(parent_of("D:\\a\\b\\"), "D:\\a");
    }

    #[test]
    fn sizes() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(1023), "1023 B");
        assert_eq!(format_size(1536), "2 KB");
        assert_eq!(format_size(2_516_582), "2.4 MB");
        assert_eq!(format_size(1_932_735_284), "1.8 GB");
    }

    #[test]
    fn dates() {
        let t = |year, month, day, hour, minute| LocalTime { year, month, day, hour, minute };
        let now = t(2026, 11, 5, 21, 0);
        assert_eq!(format_date(now, t(2026, 11, 5, 9, 41)), "今天 09:41");
        assert_eq!(format_date(now, t(2026, 11, 4, 18, 20)), "昨天 18:20");
        assert_eq!(format_date(now, t(2026, 10, 28, 1, 0)), "10月 28日");
        assert_eq!(format_date(now, t(2025, 1, 2, 1, 0)), "2025年 1月");
        // 跨月和跨年的“昨天”
        assert_eq!(format_date(t(2026, 3, 1, 0, 0), t(2026, 2, 28, 8, 5)), "昨天 08:05");
        assert_eq!(format_date(t(2026, 1, 1, 0, 0), t(2025, 12, 31, 23, 59)), "昨天 23:59");
    }

    #[test]
    fn enter_behaviour_follows_decision() {
        assert_eq!(enter_op(false, false), OpenOp::Open);
        assert_eq!(enter_op(false, true), OpenOp::Reveal);
        assert_eq!(enter_op(true, false), OpenOp::Reveal);
        assert_eq!(enter_op(true, true), OpenOp::Open);
    }

    #[test]
    fn partial_selection() {
        assert!(!is_partial_selection(0, 0, 3));
        assert!(!is_partial_selection(0, 3, 3));
        assert!(is_partial_selection(0, 2, 3));
        assert!(is_partial_selection(1, 3, 3));
    }
}
