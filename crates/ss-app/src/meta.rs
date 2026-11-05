//! 后台读取可见行的大小和修改时间。索引里没有这两项。

use std::io::ErrorKind;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use ss_ui_model::{format_date, format_size, LocalTime};
use windows::Win32::Foundation::{FILETIME, SYSTEMTIME};
use windows::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};

use crate::queue::WorkQueue;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowMeta {
    Loading,
    /// 路径已不存在。
    Gone,
    Info { size: String, modified: String },
}

static REQUESTS: WorkQueue<Vec<String>> = WorkQueue::new();
static RESULTS: Mutex<Vec<(String, RowMeta)>> = Mutex::new(Vec::new());

/// FILETIME 的起点（1601 年）到 Unix 起点的秒数。
const EPOCH_DIFF_SECS: u64 = 11_644_473_600;

fn to_local(t: SystemTime) -> Option<LocalTime> {
    let secs = t.duration_since(UNIX_EPOCH).ok()?.as_secs();
    let ticks = (secs + EPOCH_DIFF_SECS) * 10_000_000;
    let ft = FILETIME { dwLowDateTime: ticks as u32, dwHighDateTime: (ticks >> 32) as u32 };
    let mut utc = SYSTEMTIME::default();
    let mut local = SYSTEMTIME::default();
    unsafe {
        FileTimeToSystemTime(&ft, &mut utc).ok()?;
        SystemTimeToTzSpecificLocalTime(None, &utc, &mut local).ok()?;
    }
    Some(LocalTime {
        year: i32::from(local.wYear),
        month: u32::from(local.wMonth),
        day: u32::from(local.wDay),
        hour: u32::from(local.wHour),
        minute: u32::from(local.wMinute),
    })
}

fn read_meta(path: &str, now: Option<LocalTime>) -> RowMeta {
    match std::fs::metadata(path) {
        Ok(m) => {
            let modified = m
                .modified()
                .ok()
                .and_then(to_local)
                .zip(now)
                .map(|(t, n)| format_date(n, t))
                .unwrap_or_default();
            let size = if m.is_dir() { String::new() } else { format_size(m.len()) };
            RowMeta::Info { size, modified }
        }
        Err(e) if e.kind() == ErrorKind::NotFound => RowMeta::Gone,
        Err(_) => RowMeta::Info { size: String::new(), modified: String::new() },
    }
}

/// 启动读取线程。`on_ready` 在读取线程上调用。
pub fn start(on_ready: fn()) {
    std::thread::spawn(move || {
        loop {
            let batch = REQUESTS.pop();
            let now = to_local(SystemTime::now());
            let read: Vec<(String, RowMeta)> = batch
                .into_iter()
                .map(|path| {
                    let meta = read_meta(&path, now);
                    (path, meta)
                })
                .collect();
            RESULTS.lock().unwrap_or_else(|p| p.into_inner()).extend(read);
            on_ready();
        }
    });
}

pub fn request(paths: Vec<String>) {
    REQUESTS.push(paths);
}

/// 取走已经读好的结果。
pub fn take_results() -> Vec<(String, RowMeta)> {
    std::mem::take(&mut *RESULTS.lock().unwrap_or_else(|p| p.into_inner()))
}
