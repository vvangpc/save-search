//! 配置数据：收藏夹与最近保存位置（JSON，存 `%APPDATA%\SaveSearch\`）。
//!
//! 均为简单的路径字符串列表，原子写（临时文件 + rename），最近列表 LRU 截断。
//! 读取走进程内缓存，写入时更新缓存。

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock, RwLock};
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

/// 带进程内缓存的 JSON 数据。读取不碰磁盘；写入时先在写锁内修改缓存，再写盘。
///
/// `writer` 让整个「修改 + 写盘」串行，保证磁盘上留下的是最后一次修改。`value` 的写锁
/// 只在修改时持有，写盘（含 fsync）期间读取不被阻塞。
/// 写盘失败时缓存仍保留新值：进程内以缓存为准，错误返回给调用方。
struct Store<T> {
    path: PathBuf,
    value: RwLock<T>,
    /// 串行写入，并记录是否还有上次失败后尚未落盘的内容。
    writer: Mutex<bool>,
}

impl<T: Clone + PartialEq + Serialize> Store<T> {
    fn new(path: PathBuf, value: T) -> Self {
        Store {
            path,
            value: RwLock::new(value),
            writer: Mutex::new(false),
        }
    }

    fn read<R>(&self, f: impl FnOnce(&T) -> R) -> R {
        // release 为 panic=abort，锁毒化实际不可达；仍兜底以保 debug 健壮
        f(&self.value.read().unwrap_or_else(|p| p.into_inner()))
    }

    /// 修改并保存。内容没有变化且没有待落盘内容时不写盘。
    fn update<R>(&self, f: impl FnOnce(&mut T) -> R) -> io::Result<R> {
        let mut dirty = self.writer.lock().unwrap_or_else(|p| p.into_inner());
        let (snapshot, result) = {
            let mut value = self.value.write().unwrap_or_else(|p| p.into_inner());
            let before = value.clone();
            let result = f(&mut value);
            *dirty |= *value != before;
            let snapshot = (*dirty).then(|| value.clone());
            (snapshot, result)
        };
        if let Some(snapshot) = snapshot {
            let json = serde_json::to_vec_pretty(&snapshot).map_err(io::Error::other)?;
            atomic_write_json(&self.path, &json)?;
            *dirty = false;
        }
        Ok(result)
    }
}

fn same_path(a: &str, b: &str) -> bool {
    a.trim_end_matches('\\')
        .eq_ignore_ascii_case(b.trim_end_matches('\\'))
}

fn favorites_store() -> &'static Store<Vec<String>> {
    static STORE: OnceLock<Store<Vec<String>>> = OnceLock::new();
    STORE.get_or_init(|| {
        let path = favorites_path();
        let list = load_list(&path);
        Store::new(path, list)
    })
}

fn recent_store() -> &'static Store<Vec<String>> {
    static STORE: OnceLock<Store<Vec<String>>> = OnceLock::new();
    STORE.get_or_init(|| {
        let path = recent_path();
        let list = load_list(&path);
        Store::new(path, list)
    })
}

fn log_write_error(_what: &str, _result: io::Result<()>) {
    #[cfg(debug_assertions)]
    if let Err(e) = _result {
        eprintln!("SaveSearch 写入 {_what} 失败: {e}");
    }
}

/// 收藏夹（手动置顶的常用文件夹）。
pub fn favorites() -> Vec<String> {
    favorites_store().read(Clone::clone)
}

/// 最近用过的保存/打开位置（最新在前）。
pub fn recent() -> Vec<String> {
    recent_store().read(Clone::clone)
}

pub fn is_favorite(path: &str) -> bool {
    favorites_store().read(|f| f.iter().any(|p| same_path(p, path)))
}

fn add_to_list(list: &mut Vec<String>, path: &str) {
    if !list.iter().any(|p| same_path(p, path)) {
        list.push(path.to_string());
    }
}

pub fn add_favorite(path: &str) {
    log_write_error("favorites.json", favorites_store().update(|f| add_to_list(f, path)));
}

pub fn remove_favorite(path: &str) {
    log_write_error(
        "favorites.json",
        favorites_store().update(|f| f.retain(|p| !same_path(p, path))),
    );
}

/// 切换后，`path` 在列表中则返回 true。
fn toggle_in_list(list: &mut Vec<String>, path: &str) -> bool {
    let before = list.len();
    list.retain(|p| !same_path(p, path));
    if list.len() != before {
        return false;
    }
    list.push(path.to_string());
    true
}

/// 切换收藏状态，返回切换后是否已收藏。
pub fn toggle_favorite(path: &str) -> bool {
    let store = favorites_store();
    match store.update(|f| toggle_in_list(f, path)) {
        Ok(now_favorite) => now_favorite,
        Err(e) => {
            log_write_error("favorites.json", Err(e));
            store.read(|f| f.iter().any(|p| same_path(p, path)))
        }
    }
}

/// 把 `path` 移动 `delta` 位（负数向前），超出范围时停在两端。返回位置是否变化。
fn move_in_list(list: &mut [String], path: &str, delta: isize) -> bool {
    let Some(from) = list.iter().position(|p| same_path(p, path)) else {
        return false;
    };
    let to = from
        .saturating_add_signed(delta)
        .min(list.len().saturating_sub(1));
    if to < from {
        list[to..=from].rotate_right(1);
    } else {
        list[from..=to].rotate_left(1);
    }
    to != from
}

/// 调整收藏顺序：把 `path` 移动 `delta` 位（负数向前），超出范围时停在两端。
/// 返回位置是否变化；写盘失败时返回错误，此时进程内的顺序已经更新。
pub fn move_favorite(path: &str, delta: isize) -> io::Result<bool> {
    favorites_store().update(|f| move_in_list(f, path, delta))
}

/// 记录一次「最近位置」（去重置顶 + LRU 截断，上限取自设置）。
pub fn record_recent(path: &str) {
    let max = load_settings().recent_max;
    log_write_error(
        "recent.json",
        recent_store().update(|r| {
            r.retain(|p| !same_path(p, path));
            r.insert(0, path.to_string());
            r.truncate(max);
        }),
    );
}

// ---- 设置 ----

/// 全局设置（settings.json）。`#[serde(default)]` 保证旧文件缺字段时用默认值。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// 主题。只保留字段以便读取旧配置文件，任何旧值都能正常加载。
    pub theme: String,
    /// 索引的盘符（大写）；空 = 全部固定 NTFS 盘
    pub indexed_drives: Vec<char>,
    /// 预设文件夹（完整路径）：出现在搜索范围下拉框中，选中后只在该文件夹子树内搜索
    pub preset_folders: Vec<String>,
    /// 搜索结果上限（读取时限制在 20 到 5000）
    pub result_limit: usize,
    /// 浮窗「最近位置」保留条数（读取时限制在 0 到 50）
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
    /// 呼出搜索窗的全局热键，如 `Alt+Space`
    pub hotkey: String,
    /// 上次的搜索范围：空 = 全部；`C` = 某个盘；完整路径 = 某个文件夹
    pub last_scope: String,
    /// 上次的分类标签：空 = 全部，其他值由界面定义
    pub last_category: String,
    /// 不搜索这些目录的子树（完整路径）
    pub exclude_dirs: Vec<String>,
    /// 不显示带隐藏或系统属性的文件
    pub hide_system: bool,
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
            hotkey: DEFAULT_HOTKEY.into(),
            last_scope: String::new(),
            last_category: String::new(),
            exclude_dirs: Vec::new(),
            hide_system: false,
        }
    }
}

const DEFAULT_HOTKEY: &str = "Alt+Space";

impl Settings {
    /// 把超出范围或无法识别的值改成有效值。手改的配置文件不会让程序出错。
    fn normalize(&mut self) {
        self.result_limit = self.result_limit.clamp(20, 5000);
        self.recent_max = self.recent_max.min(50);
        if self.hotkey.trim().is_empty() {
            self.hotkey = DEFAULT_HOTKEY.into();
        }
    }
}

fn parse_settings(bytes: &[u8]) -> serde_json::Result<Settings> {
    let mut s: Settings = serde_json::from_slice(bytes)?;
    s.normalize();
    Ok(s)
}

/// 进程内 Settings 缓存：热路径（每次击键 / 每次对话框出现 / 每次导航）不再读盘。
/// 代价：运行期外部手改 settings.json 不生效（设置页是唯一预期编辑入口）。
fn settings_store() -> &'static Store<Settings> {
    static STORE: OnceLock<Store<Settings>> = OnceLock::new();
    STORE.get_or_init(|| {
        let path = settings_path();
        let settings = load_settings_from_disk(&path);
        Store::new(path, settings)
    })
}

/// 真正读盘 + 解析（含损坏隔离）。仅缓存首次初始化时调用。
fn load_settings_from_disk(path: &Path) -> Settings {
    match std::fs::read(path) {
        Ok(bytes) => parse_settings(&bytes).unwrap_or_else(|_| {
            quarantine_corrupt(path);
            Settings::default()
        }),
        Err(_) => Settings::default(),
    }
}

pub fn load_settings() -> Settings {
    settings_store().read(Clone::clone)
}

/// 在同一把写锁内修改设置并保存，并发修改不会互相覆盖。内容没有变化时不写盘。
/// 写盘失败时返回错误，此时进程内的设置已经更新。
pub fn update_settings(f: impl FnOnce(&mut Settings)) -> io::Result<()> {
    settings_store().update(|s| {
        f(s);
        s.normalize();
    })
}

/// 保存整份设置，写盘失败时返回错误，界面可以据此提示用户。
pub fn try_save_settings(s: &Settings) -> io::Result<()> {
    update_settings(|current| *current = s.clone())
}

/// 读取上次搜索词（跨程序重启）。
pub fn last_query() -> String {
    settings_store().read(|s| s.last_query.clone())
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

    #[test]
    fn old_settings_files_still_load() {
        // 只有部分旧字段，theme 是已删除的主题
        let s = parse_settings(
            br#"{"theme":"business_dark","result_limit":500,"last_query":"abc","removed":1}"#,
        )
        .unwrap();
        assert_eq!(s.theme, "business_dark");
        assert_eq!(s.result_limit, 500);
        assert_eq!(s.last_query, "abc");
        // 新字段取默认值
        assert_eq!(s.hotkey, "Alt+Space");
        assert!(s.exclude_dirs.is_empty());
        assert!(!s.hide_system);
        assert!(s.popup_enabled);

        let empty = parse_settings(b"{}").unwrap();
        assert_eq!(empty, Settings::default());
    }

    #[test]
    fn settings_values_are_limited_on_read() {
        let low = parse_settings(br#"{"result_limit":1,"recent_max":0}"#).unwrap();
        assert_eq!((low.result_limit, low.recent_max), (20, 0));
        let high = parse_settings(br#"{"result_limit":100000,"recent_max":999}"#).unwrap();
        assert_eq!((high.result_limit, high.recent_max), (5000, 50));
        let bad = parse_settings(br#"{"hotkey":"  ","enter_action":"explode"}"#).unwrap();
        assert_eq!(bad.hotkey, "Alt+Space");
        // 废弃的 enter_action 被忽略，旧配置不能反转固定的定位手势。
        for value in ["open", "reveal", "explode"] {
            let legacy = format!(r#"{{"enter_action":"{value}"}}"#);
            assert_eq!(parse_settings(legacy.as_bytes()).unwrap(), Settings::default());
        }
    }

    #[test]
    fn corrupt_settings_are_quarantined() {
        let dir = temp_dir("settings-corrupt");
        let p = dir.join("settings.json");
        fs::write(&p, b"{oops").unwrap();
        assert_eq!(load_settings_from_disk(&p), Settings::default());
        assert!(!p.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn store_updates_cache_and_disk_without_lost_writes() {
        let dir = temp_dir("store-race");
        let p = dir.join("v.json");
        let store = Store::new(p.clone(), Vec::<String>::new());
        std::thread::scope(|s| {
            for t in 0..4 {
                let store = &store;
                s.spawn(move || {
                    for i in 0..10 {
                        store.update(|v| v.push(format!("{t}-{i}"))).unwrap();
                    }
                });
            }
        });
        assert_eq!(store.read(Vec::len), 40);
        let on_disk: Vec<String> = serde_json::from_slice(&fs::read(&p).unwrap()).unwrap();
        assert_eq!(on_disk, store.read(Clone::clone));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn store_skips_disk_when_unchanged_and_reports_write_failure() {
        let dir = temp_dir("store-fail");
        // 父路径是文件，写入必然失败
        let blocker = dir.join("blocker");
        fs::write(&blocker, b"x").unwrap();
        let store = Store::new(blocker.join("v.json"), vec!["a".to_string()]);
        // 内容不变：不写盘，也就不会失败
        assert!(store.update(|_| ()).is_ok());
        // 内容变化：返回错误，但进程内已更新
        assert!(store.update(|v| v.push("b".to_string())).is_err());
        assert_eq!(store.read(Vec::len), 2);
        // 同样的内容重试必须继续报错，不能把缓存命中当作保存成功。
        assert!(store.update(|_| ()).is_err());
        fs::remove_file(&blocker).unwrap();
        fs::create_dir(&blocker).unwrap();
        store.update(|_| ()).unwrap();
        let persisted: Vec<String> =
            serde_json::from_slice(&fs::read(blocker.join("v.json")).unwrap()).unwrap();
        assert_eq!(persisted, ["a", "b"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn toggle_returns_new_state() {
        let mut list = vec!["C:\\a".to_string()];
        assert!(!toggle_in_list(&mut list, "c:\\A\\"));
        assert!(list.is_empty());
        assert!(toggle_in_list(&mut list, "C:\\b"));
        assert_eq!(list, ["C:\\b"]);
        add_to_list(&mut list, "c:\\B");
        assert_eq!(list.len(), 1);
    }

    #[test]
    fn move_keeps_order_and_stops_at_ends() {
        let mut list: Vec<String> = ["a", "b", "c", "d"].map(String::from).to_vec();
        assert!(move_in_list(&mut list, "c", -1));
        assert_eq!(list, ["a", "c", "b", "d"]);
        assert!(move_in_list(&mut list, "a", 2));
        assert_eq!(list, ["c", "b", "a", "d"]);
        assert!(move_in_list(&mut list, "b", -99));
        assert_eq!(list, ["b", "c", "a", "d"]);
        assert!(move_in_list(&mut list, "b", 99));
        assert_eq!(list, ["c", "a", "d", "b"]);
        // 已在两端或不存在：位置不变
        assert!(!move_in_list(&mut list, "b", 1));
        assert!(!move_in_list(&mut list, "c", -1));
        assert!(!move_in_list(&mut list, "zzz", 1));
        assert!(!move_in_list(&mut [], "a", 1));
    }
}
