//! 配置数据：收藏夹与最近保存位置（JSON，存 `%APPDATA%\SaveSearch\`）。
//!
//! 均为简单的路径字符串列表，原子写（临时文件 + rename），最近列表 LRU 截断。

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{OnceLock, RwLock};
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

/// 解析失败时把损坏文件改名保留（如 favorites.json → favorites.json.corrupt-<unix秒>），
/// 防止后续 save_* 以「默认值 + 新项」覆盖写回、永久抹掉用户数据。
/// rename 失败则放弃（文件仍在，下次启动再试）。隔离文件不自动清理（损坏罕见，量极小）。
fn quarantine_corrupt(path: &Path) {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // with_extension 会把 ".json" 换成新扩展名 → "favorites.json.corrupt-1752..."
    let mut dst = path.with_extension(format!("json.corrupt-{ts}"));
    for i in 1..10 {
        if !dst.exists() {
            break;
        }
        dst = path.with_extension(format!("json.corrupt-{ts}-{i}"));
    }
    let _ = fs::rename(path, &dst);
}

fn load_list(path: &PathBuf) -> Vec<String> {
    match std::fs::read(path) {
        Ok(bytes) => match serde_json::from_slice::<Vec<String>>(&bytes) {
            Ok(v) => v,
            Err(_) => {
                quarantine_corrupt(path);
                Vec::new()
            }
        },
        Err(_) => Vec::new(), // 文件不存在等：按空列表处理
    }
}

/// 原子写 JSON：写临时文件并 `sync_all` 落盘后再 rename 替换目标。
/// Windows 上 `std::fs::rename` 即 `MoveFileExW(REPLACE_EXISTING|COPY_ALLOWED)`，
/// 目标已存在可原子替换；rename 遇杀软/索引器瞬时占用做短重试。
fn atomic_write_json(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    // tmp 名带进程 id + 序号：防「旧进程正退出、新进程已启动」窗口期的跨进程互踩，
    // 以及同进程理论上的重入。崩溃残留的孤儿 tmp 量极小，不做清理。
    static TMP_SEQ: AtomicU32 = AtomicU32::new(0);
    let tmp = path.with_extension(format!(
        "tmp.{}.{}",
        std::process::id(),
        TMP_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    {
        let mut f = File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?; // fsync：内容落盘后再 rename，防崩溃留下空/半截文件
    } // 关闭句柄后再 rename（Windows 上源句柄未关会触发共享冲突）
    let mut last: Option<io::Error> = None;
    for i in 0..3 {
        match fs::rename(&tmp, path) {
            Ok(()) => return Ok(()),
            Err(e) => {
                last = Some(e);
                if i < 2 {
                    std::thread::sleep(Duration::from_millis(20 * (i + 1)));
                }
            }
        }
    }
    let _ = fs::remove_file(&tmp);
    Err(last.unwrap_or_else(|| io::Error::other("rename failed")))
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

/// 进程内 Settings 缓存：热路径（每次击键 / 每次对话框出现 / 每次导航）不再读盘。
/// 代价：运行期外部手改 settings.json 不生效（设置页是唯一预期编辑入口）。
static SETTINGS_CACHE: OnceLock<RwLock<Settings>> = OnceLock::new();

/// 真正读盘 + 解析（含损坏隔离）。仅缓存首次初始化时调用。
fn load_settings_from_disk() -> Settings {
    let path = settings_path();
    match std::fs::read(&path) {
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(s) => s,
            Err(_) => {
                quarantine_corrupt(&path);
                Settings::default()
            }
        },
        Err(_) => Settings::default(),
    }
}

fn settings_cache() -> &'static RwLock<Settings> {
    SETTINGS_CACHE.get_or_init(|| RwLock::new(load_settings_from_disk()))
}

pub fn load_settings() -> Settings {
    // release 为 panic=abort，锁毒化实际不可达；仍兜底以保 debug 健壮
    settings_cache()
        .read()
        .unwrap_or_else(|p| p.into_inner())
        .clone()
}

pub fn save_settings(s: &Settings) {
    // 先更新缓存：即使写盘失败，内存值也是进程内真相（设置页改完立即全局可见）
    *settings_cache().write().unwrap_or_else(|p| p.into_inner()) = s.clone();
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

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir()
            .join("ss-config-tests")
            .join(format!("{}-{}", name, std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn load_list_corrupt_is_quarantined() {
        let dir = temp_dir("corrupt");
        let p = dir.join("favorites.json");
        fs::write(&p, b"{invalid json").unwrap();
        assert_eq!(load_list(&p), Vec::<String>::new());
        // 原文件已被改名隔离，而非留在原地等着被覆盖
        assert!(!p.exists());
        let quarantined = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| e.file_name().to_string_lossy().contains("corrupt-"));
        assert!(quarantined);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_list_missing_and_valid() {
        let dir = temp_dir("valid");
        let p = dir.join("list.json");
        assert_eq!(load_list(&p), Vec::<String>::new()); // 不存在：空且不产生隔离文件
        fs::write(&p, br#"["C:\\a", "D:\\b"]"#).unwrap();
        assert_eq!(load_list(&p), vec!["C:\\a".to_string(), "D:\\b".to_string()]);
        assert!(p.exists()); // 合法文件不被动
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn atomic_write_roundtrip() {
        let dir = temp_dir("atomic");
        let p = dir.join("out.json");
        atomic_write_json(&p, br#"["x"]"#).unwrap();
        assert_eq!(fs::read(&p).unwrap(), br#"["x"]"#);
        // 覆盖写也成功，且无 tmp 残留
        atomic_write_json(&p, br#"["y"]"#).unwrap();
        assert_eq!(fs::read(&p).unwrap(), br#"["y"]"#);
        let tmp_left = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| e.file_name().to_string_lossy().contains(".tmp."));
        assert!(!tmp_left);
        let _ = fs::remove_dir_all(&dir);
    }
}
