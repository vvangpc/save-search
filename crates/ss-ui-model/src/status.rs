//! 搜索窗显示什么：主体内容、横幅和状态栏文字。

/// 加载失败的盘。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriveFailure {
    pub letter: char,
    pub reason: String,
}

/// 正在首次建立索引的盘。`estimated` 取不到时为 0。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuildProgress {
    pub letter: char,
    pub done: usize,
    pub estimated: usize,
}

/// 搜索范围的问题。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeProblem {
    FolderMissing,
    DriveNotIndexed,
}

/// 决定界面状态需要的全部输入。
#[derive(Debug, Clone)]
pub struct ViewInput<'a> {
    pub query_empty: bool,
    pub index_ready: bool,
    pub scope_problem: Option<ScopeProblem>,
    pub shown: usize,
    pub total_hits: usize,
    pub scope_is_all: bool,
    pub category_is_all: bool,
    pub failed: &'a [DriveFailure],
    pub building: &'a [BuildProgress],
    /// 当前范围的文字，如 `C、D 盘`、`D:\工作`。
    pub scope_label: &'a str,
    pub elapsed_ms: u32,
    pub total_files: usize,
    /// 距离上次读取文件变更的秒数。
    pub synced_secs_ago: Option<u64>,
}

/// 列表区域显示的内容。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Body {
    /// 空输入：最近位置、收藏和语法提示。
    Home,
    Indexing,
    ScopeInvalid,
    NoResults { offer_all_scopes: bool, offer_clear_category: bool },
    Results,
}

pub fn body(v: &ViewInput) -> Body {
    if v.query_empty {
        Body::Home
    } else if !v.index_ready {
        Body::Indexing
    } else if v.scope_problem.is_some() {
        Body::ScopeInvalid
    } else if v.shown == 0 {
        Body::NoResults {
            offer_all_scopes: !v.scope_is_all,
            offer_clear_category: !v.category_is_all,
        }
    } else {
        Body::Results
    }
}

/// 列表上方的横幅，同一时间只显示最重要的一条。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Banner {
    /// `permille` 取不到总量时为 `None`。
    Indexing { text: String, permille: Option<u16> },
    ScopeInvalid { text: String },
    DriveFailed { text: String },
    Truncated { text: String },
}

pub fn banner(v: &ViewInput) -> Option<Banner> {
    if !v.index_ready {
        return Some(indexing_banner(v.building));
    }
    if !v.query_empty {
        if let Some(problem) = v.scope_problem {
            let text = match problem {
                ScopeProblem::FolderMissing => format!("{} 已不存在或已改名。", v.scope_label),
                ScopeProblem::DriveNotIndexed => format!("{} 没有索引。", v.scope_label),
            };
            return Some(Banner::ScopeInvalid { text });
        }
    }
    if let Some(first) = v.failed.first() {
        let letters: Vec<String> = v.failed.iter().map(|f| f.letter.to_string()).collect();
        let text = format!(
            "{} 盘加载失败：{}。结果不包含这些盘。",
            letters.join("、"),
            first.reason
        );
        return Some(Banner::DriveFailed { text });
    }
    if !v.query_empty && v.total_hits > v.shown && v.shown > 0 {
        let text = format!(
            "匹配太多，只显示最相关的前 {} 条，共 {} 条。多输入一些字可以缩小范围。",
            grouped(v.shown),
            grouped(v.total_hits)
        );
        return Some(Banner::Truncated { text });
    }
    None
}

fn indexing_banner(building: &[BuildProgress]) -> Banner {
    let (done, estimated) = building
        .iter()
        .fold((0, 0), |(d, e), p| (d + p.done, e + p.estimated));
    let letters: Vec<String> = building.iter().map(|p| p.letter.to_string()).collect();
    if letters.is_empty() {
        return Banner::Indexing { text: "正在读取索引…".to_string(), permille: None };
    }
    let who = letters.join("、");
    if estimated == 0 {
        return Banner::Indexing {
            text: format!("{who} 盘首次建立索引中，已处理 {} 个文件。", grouped(done)),
            permille: None,
        };
    }
    // 估算值来自 MFT 大小，完成前通常到不了 100%，封顶避免显示已完成
    let permille = (done.saturating_mul(1000) / estimated).min(990) as u16;
    Banner::Indexing {
        text: format!("{who} 盘首次建立索引中，已处理 {}%。", permille / 10),
        permille: Some(permille),
    }
}

/// 状态栏左侧的文字。
pub fn status_line(v: &ViewInput) -> String {
    let synced = v.synced_secs_ago.map(synced_text);
    if !v.index_ready {
        return "索引建立中".to_string();
    }
    if v.query_empty {
        let mut s = format!("共 {} 个文件", compact(v.total_files));
        if let Some(t) = synced {
            s.push_str(" · ");
            s.push_str(&t);
        }
        return s;
    }
    if v.scope_problem.is_some() {
        return "范围不可用".to_string();
    }
    let count = if v.total_hits > v.shown {
        format!("{} / {} 条", grouped(v.shown), grouped(v.total_hits))
    } else {
        format!("{} 条", grouped(v.shown))
    };
    let mut s = format!("{count} · {} ms · {}", v.elapsed_ms, v.scope_label);
    if let Some(t) = synced {
        s.push_str(" · ");
        s.push_str(&t);
    }
    s
}

/// 范围文字：全部盘时列出盘符。
pub fn all_drives_label(letters: &[char]) -> String {
    if letters.is_empty() {
        return "全部盘".to_string();
    }
    let names: Vec<String> = letters.iter().map(char::to_string).collect();
    format!("{} 盘", names.join("、"))
}

pub(crate) fn synced_text(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs} 秒前更新"),
        60..=3599 => format!("{} 分钟前更新", secs / 60),
        _ => format!("{} 小时前更新", secs / 3600),
    }
}

/// `48213` → `48,213`。
pub fn grouped(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// `2500884` → `250 万`，`3200` → `3,200`。
pub fn compact(n: usize) -> String {
    if n >= 10_000 {
        format!("{} 万", n / 10_000)
    } else {
        grouped(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base<'a>() -> ViewInput<'a> {
        ViewInput {
            query_empty: false,
            index_ready: true,
            scope_problem: None,
            shown: 3,
            total_hits: 3,
            scope_is_all: true,
            category_is_all: true,
            failed: &[],
            building: &[],
            scope_label: "C、D 盘",
            elapsed_ms: 9,
            total_files: 2_500_000,
            synced_secs_ago: Some(2),
        }
    }

    #[test]
    fn body_states() {
        assert_eq!(body(&base()), Body::Results);
        assert_eq!(body(&ViewInput { query_empty: true, index_ready: false, ..base() }), Body::Home);
        assert_eq!(body(&ViewInput { index_ready: false, ..base() }), Body::Indexing);
        assert_eq!(
            body(&ViewInput { scope_problem: Some(ScopeProblem::FolderMissing), ..base() }),
            Body::ScopeInvalid
        );
        assert_eq!(
            body(&ViewInput { shown: 0, total_hits: 0, scope_is_all: false, ..base() }),
            Body::NoResults { offer_all_scopes: true, offer_clear_category: false }
        );
        assert_eq!(
            body(&ViewInput { shown: 0, total_hits: 0, category_is_all: false, ..base() }),
            Body::NoResults { offer_all_scopes: false, offer_clear_category: true }
        );
    }

    #[test]
    fn no_banner_when_everything_is_fine() {
        assert_eq!(banner(&base()), None);
    }

    #[test]
    fn indexing_banner_reports_progress() {
        let building = [BuildProgress { letter: 'D', done: 62, estimated: 100 }];
        let v = ViewInput { index_ready: false, building: &building, ..base() };
        match banner(&v) {
            Some(Banner::Indexing { text, permille }) => {
                assert_eq!(permille, Some(620));
                assert!(text.contains("D 盘") && text.contains("62%"));
            }
            other => panic!("{other:?}"),
        }
        let unknown = [BuildProgress { letter: 'D', done: 1500, estimated: 0 }];
        let v = ViewInput { index_ready: false, building: &unknown, ..base() };
        match banner(&v) {
            Some(Banner::Indexing { text, permille }) => {
                assert_eq!(permille, None);
                assert!(text.contains("1,500"));
            }
            other => panic!("{other:?}"),
        }
        // 估算偏小时不显示 100%
        let over = [BuildProgress { letter: 'C', done: 200, estimated: 100 }];
        let v = ViewInput { index_ready: false, building: &over, ..base() };
        assert!(matches!(banner(&v), Some(Banner::Indexing { permille: Some(990), .. })));
        let v = ViewInput { index_ready: false, ..base() };
        assert!(matches!(banner(&v), Some(Banner::Indexing { permille: None, .. })));
    }

    #[test]
    fn banner_priority_and_text() {
        let failed = [DriveFailure { letter: 'D', reason: "拒绝访问".into() }];
        let v = ViewInput { failed: &failed, shown: 2, total_hits: 9, ..base() };
        match banner(&v) {
            Some(Banner::DriveFailed { text }) => {
                assert!(text.contains("D 盘") && text.contains("拒绝访问"))
            }
            other => panic!("{other:?}"),
        }
        let v = ViewInput { shown: 200, total_hits: 48_213, ..base() };
        match banner(&v) {
            Some(Banner::Truncated { text }) => assert!(text.contains("48,213")),
            other => panic!("{other:?}"),
        }
        let v = ViewInput {
            scope_problem: Some(ScopeProblem::FolderMissing),
            scope_label: "D:\\旧项目",
            failed: &failed,
            ..base()
        };
        assert!(matches!(banner(&v), Some(Banner::ScopeInvalid { .. })));
        // 空输入不提示范围问题
        let v = ViewInput {
            query_empty: true,
            scope_problem: Some(ScopeProblem::FolderMissing),
            ..base()
        };
        assert_eq!(banner(&v), None);
    }

    #[test]
    fn status_line_text() {
        assert_eq!(status_line(&base()), "3 条 · 9 ms · C、D 盘 · 2 秒前更新");
        let v = ViewInput { shown: 200, total_hits: 48_213, synced_secs_ago: None, ..base() };
        assert_eq!(status_line(&v), "200 / 48,213 条 · 9 ms · C、D 盘");
        let v = ViewInput { query_empty: true, synced_secs_ago: Some(120), ..base() };
        assert_eq!(status_line(&v), "共 250 万 个文件 · 2 分钟前更新");
        assert_eq!(status_line(&ViewInput { index_ready: false, ..base() }), "索引建立中");
        let v = ViewInput { scope_problem: Some(ScopeProblem::DriveNotIndexed), ..base() };
        assert_eq!(status_line(&v), "范围不可用");
    }

    #[test]
    fn number_formats() {
        assert_eq!(grouped(0), "0");
        assert_eq!(grouped(999), "999");
        assert_eq!(grouped(1000), "1,000");
        assert_eq!(grouped(1_234_567), "1,234,567");
        assert_eq!(compact(9_999), "9,999");
        assert_eq!(compact(2_500_884), "250 万");
        assert_eq!(all_drives_label(&['C', 'D']), "C、D 盘");
        assert_eq!(all_drives_label(&[]), "全部盘");
    }
}
