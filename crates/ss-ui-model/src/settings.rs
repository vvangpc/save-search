//! 设置页的输入校验和文字。

use crate::status::{compact, synced_text};

/// 校验结果限制在 `min..=max` 的数字输入。
fn parse_range(text: &str, min: usize, max: usize, what: &str) -> Result<usize, String> {
    match text.trim().parse::<usize>() {
        Ok(n) if (min..=max).contains(&n) => Ok(n),
        _ => Err(format!("{what}要填 {min} 到 {max} 之间的数字。")),
    }
}

pub fn parse_result_limit(text: &str) -> Result<usize, String> {
    parse_range(text, 20, 5000, "结果上限")
}

pub fn parse_recent_max(text: &str) -> Result<usize, String> {
    parse_range(text, 0, 50, "最近条数")
}

/// 索引管理一行的文字。
pub fn index_stats(nodes: usize, memory_bytes: usize, synced_secs_ago: Option<u64>) -> String {
    let mut s = format!("共 {} 个文件，占用内存 {} MB", compact(nodes), memory_bytes / 1_000_000);
    if let Some(secs) = synced_secs_ago {
        s.push('，');
        s.push_str(&synced_text(secs));
    }
    s
}

/// 全局热键。`modifiers` 和 `key` 就是 `RegisterHotKey` 的参数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hotkey {
    pub modifiers: u32,
    pub key: u32,
}

const MOD_ALT: u32 = 1;
const MOD_CONTROL: u32 = 2;
const MOD_SHIFT: u32 = 4;
const MOD_WIN: u32 = 8;
const VK_SPACE: u32 = 0x20;
const VK_F1: u32 = 0x70;

fn parse_key(name: &str) -> Option<u32> {
    if name.eq_ignore_ascii_case("space") {
        return Some(VK_SPACE);
    }
    let upper = name.to_ascii_uppercase();
    let bytes = upper.as_bytes();
    if bytes.len() == 1 && bytes[0].is_ascii_alphanumeric() {
        return Some(u32::from(bytes[0]));
    }
    let number = upper.strip_prefix('F')?.parse::<u32>().ok()?;
    (1..=24).contains(&number).then(|| VK_F1 + number - 1)
}

/// 解析 `Ctrl+Alt+K` 这样的文字。不区分大小写，允许空格。
/// 除 F1 到 F24 外必须带修饰键，否则会占用普通输入。
pub fn parse_hotkey(text: &str) -> Option<Hotkey> {
    let mut modifiers = 0;
    let mut key = None;
    for part in text.split('+') {
        let part = part.trim();
        match part.to_ascii_lowercase().as_str() {
            "alt" => modifiers |= MOD_ALT,
            "ctrl" | "control" => modifiers |= MOD_CONTROL,
            "shift" => modifiers |= MOD_SHIFT,
            "win" => modifiers |= MOD_WIN,
            _ if key.is_none() => key = Some(parse_key(part)?),
            _ => return None,
        }
    }
    let key = key?;
    let is_function_key = (VK_F1..VK_F1 + 24).contains(&key);
    (modifiers != 0 || is_function_key).then_some(Hotkey { modifiers, key })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn number_ranges() {
        assert_eq!(parse_result_limit(" 200 "), Ok(200));
        assert_eq!(parse_result_limit("20"), Ok(20));
        assert_eq!(parse_result_limit("5000"), Ok(5000));
        assert!(parse_result_limit("19").is_err());
        assert!(parse_result_limit("5001").is_err());
        assert!(parse_result_limit("").is_err());
        assert!(parse_result_limit("abc").is_err());
        assert_eq!(parse_recent_max("0"), Ok(0));
        assert!(parse_recent_max("51").is_err());
    }

    #[test]
    fn hotkeys() {
        assert_eq!(parse_hotkey("Alt+Space"), Some(Hotkey { modifiers: 1, key: 0x20 }));
        assert_eq!(parse_hotkey("ctrl + shift + k"), Some(Hotkey { modifiers: 6, key: u32::from(b'K') }));
        assert_eq!(parse_hotkey("Win+1"), Some(Hotkey { modifiers: 8, key: u32::from(b'1') }));
        assert_eq!(parse_hotkey("F12"), Some(Hotkey { modifiers: 0, key: 0x7B }));
        assert_eq!(parse_hotkey("Alt+F1"), Some(Hotkey { modifiers: 1, key: 0x70 }));
        // 没有修饰键、没有主键、两个主键、未知的键
        assert_eq!(parse_hotkey("K"), None);
        assert_eq!(parse_hotkey("Alt"), None);
        assert_eq!(parse_hotkey("Alt+K+L"), None);
        assert_eq!(parse_hotkey("Alt+F25"), None);
        assert_eq!(parse_hotkey("Alt+Foo"), None);
        assert_eq!(parse_hotkey(""), None);
    }

    #[test]
    fn stats_text() {
        assert_eq!(index_stats(2_500_000, 102_300_000, Some(2)), "共 250 万 个文件，占用内存 102 MB，2 秒前更新");
        assert_eq!(index_stats(10, 0, None), "共 10 个文件，占用内存 0 MB");
    }
}
