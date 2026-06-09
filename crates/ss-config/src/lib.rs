//! 配置数据：收藏夹与最近保存位置（JSON，存 `%APPDATA%\SaveSearch\`）。
//!
//! 均为简单的路径字符串列表，原子写（临时文件 + rename），最近列表 LRU 截断。

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

fn config_dir() -> PathBuf {
    let base = std::env::var("APPDATA").unwrap_or_else(|_| ".".into());
    PathBuf::from(base).join("SaveSearch")
}

fn favorites_path() -> PathBuf {
    config_dir().join("favorites.json")
}
fn recent_path() -> PathBuf {
    config_dir().join("recent.json")
}
fn settings_path() -> PathBuf {
    config_dir().join("settings.json")
}

fn load_list(path: &PathBuf) -> Vec<String> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice::<Vec<String>>(&bytes).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

/// 原子写 JSON：写临时文件并 `sync_all` 落盘后再 rename 替换目标。
/// Windows 上 `std::fs::rename` 即 `MoveFileExW(REPLACE_EXISTING|COPY_ALLOWED)`，
/// 目标已存在可原子替换；rename 遇杀软/索引器瞬时占用做短重试。
fn atomic_write_json(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    {
        let mut f = File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?; // fsync：内容落盘后再 rename，防崩溃留下空/半截文件
    } // 关闭句柄后再 rename（Windows 上源句柄未关会触发共享冲突）
    for i in 0..3 {
        match fs::rename(&tmp, path) {
            Ok(()) => return Ok(()),
            Err(e) if i == 2 => {
                let _ = fs::remove_file(&tmp);
                return Err(e);
            }
            Err(_) => std::thread::sleep(Duration::from_millis(20 * (i + 1))),
        }
    }
    unreachable!()
}

fn save_list(path: &PathBuf, list: &[String]) {
    if let Ok(json) = serde_json::to_vec_pretty(list) {
        if let Err(_e) = atomic_write_json(path, &json) {
            #[cfg(debug_assertions)]
            eprintln!("SaveSearch 写入 {:?} 失败: {}", path, _e);
        }
    }
}

fn same_path(a: &str, b: &str) -> bool {
    a.trim_end_matches('\\')
        .eq_ignore_ascii_case(b.trim_end_matches('\\'))
}

/// 收藏夹（手动置顶的常用文件夹）。
pub fn favorites() -> Vec<String> {
    load_list(&favorites_path())
}

/// 最近用过的保存/打开位置（最新在前）。
pub fn recent() -> Vec<String> {
    load_list(&recent_path())
}

pub fn is_favorite(path: &str) -> bool {
    favorites().iter().any(|p| same_path(p, path))
}

pub fn add_favorite(path: &str) {
    let mut f = favorites();
    if !f.iter().any(|p| same_path(p, path)) {
        f.push(path.to_string());
        save_list(&favorites_path(), &f);
    }
}

pub fn remove_favorite(path: &str) {
    let mut f = favorites();
    let before = f.len();
    f.retain(|p| !same_path(p, path));
    if f.len() != before {
        save_list(&favorites_path(), &f);
    }
}

pub fn toggle_favorite(path: &str) {
    if is_favorite(path) {
        remove_favorite(path);
    } else {
        add_favorite(path);
    }
}

/// 记录一次「最近位置」（去重置顶 + LRU 截断，上限取自设置）。
pub fn record_recent(path: &str) {
    let mut r = recent();
    r.retain(|p| !same_path(p, path));
    r.insert(0, path.to_string());
    r.truncate(load_settings().recent_max);
    save_list(&recent_path(), &r);
}

// ---- 设置 ----

/// 全局设置（settings.json）。`#[serde(default)]` 保证旧文件缺字段时用默认值。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// 主题：`business_light` | `business_dark` | `acrylic`
    pub theme: String,
    /// 索引的盘符（大写）；空 = 全部固定 NTFS 盘
    pub indexed_drives: Vec<char>,
    /// 预设文件夹（完整路径）：出现在搜索范围下拉框中，选中后只在该文件夹子树内搜索
    pub preset_folders: Vec<String>,
    /// 搜索结果上限
    pub result_limit: usize,
    /// 浮窗「最近位置」保留条数
    pub recent_max: usize,
    /// 开机自启（计划任务）
    pub autostart: bool,
    /// 是否启用保存对话框浮窗
    pub popup_enabled: bool,
    pub popup_show_favorites: bool,
    pub popup_show_recent: bool,
    pub popup_show_open: bool,
    /// 上次搜索词（跨重启记忆，启动时预填搜索框）
    pub last_query: String,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            theme: "business_light".into(),
            indexed_drives: Vec::new(),
            preset_folders: Vec::new(),
            result_limit: 200,
            recent_max: 2,
            autostart: false,
            popup_enabled: true,
            popup_show_favorites: true,
            popup_show_recent: true,
            popup_show_open: true,
            last_query: String::new(),
        }
    }
}

pub fn load_settings() -> Settings {
    match std::fs::read(settings_path()) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
        Err(_) => Settings::default(),
    }
}

pub fn save_settings(s: &Settings) {
    if let Ok(json) = serde_json::to_vec_pretty(s) {
        if let Err(_e) = atomic_write_json(&settings_path(), &json) {
            #[cfg(debug_assertions)]
            eprintln!("SaveSearch 写入 settings.json 失败: {}", _e);
        }
    }
}

/// 读取上次搜索词（跨程序重启）。
pub fn last_query() -> String {
    load_settings().last_query
}

/// 保存上次搜索词；与当前值相同则不写盘（避免无谓 IO）。
pub fn save_last_query(q: &str) {
    let mut s = load_settings();
    if s.last_query != q {
        s.last_query = q.to_string();
        save_settings(&s);
    }
}
