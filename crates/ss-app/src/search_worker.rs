//! 搜索工作线程：只保留最新的请求，新请求到达时取消正在进行的扫描。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};

use ss_core::{Category, Scope, SearchRequest, SearchResponse};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

use crate::searchwin::{SharedCatalog, WM_APP_SEARCH_DONE};

pub struct Job {
    pub seq: u64,
    pub query: String,
    pub scope: Scope,
    pub category: Category,
    pub limit: usize,
    pub exclude_dirs: Vec<String>,
    pub hide_system: bool,
    /// 后台刷新：保持选中项和滚动位置。
    pub keep_position: bool,
}

pub struct Done {
    pub seq: u64,
    pub query: String,
    pub keep_position: bool,
    pub response: SearchResponse,
}

struct State {
    pending: Option<Job>,
    /// 正在执行的扫描的取消标志。
    cancel: Arc<AtomicBool>,
}

struct Shared {
    state: Mutex<State>,
    wake: Condvar,
}

static SHARED: OnceLock<Shared> = OnceLock::new();

/// 启动工作线程。结果通过 `WM_APP_SEARCH_DONE` 发给 `hwnd`，`LPARAM` 是 `Box<Done>` 的裸指针。
pub fn start(catalog: SharedCatalog, hwnd: isize) {
    let shared = SHARED.get_or_init(|| Shared {
        state: Mutex::new(State { pending: None, cancel: Arc::new(AtomicBool::new(false)) }),
        wake: Condvar::new(),
    });
    std::thread::spawn(move || loop {
        let (job, cancel) = {
            let mut st = shared.state.lock().unwrap_or_else(|p| p.into_inner());
            loop {
                if let Some(job) = st.pending.take() {
                    st.cancel = Arc::new(AtomicBool::new(false));
                    break (job, st.cancel.clone());
                }
                st = shared.wake.wait(st).unwrap_or_else(|p| p.into_inner());
            }
        };
        let response = catalog.read().as_ref().map(|cat| {
            cat.query(&SearchRequest {
                query: &job.query,
                scope: job.scope.clone(),
                category: job.category,
                limit: job.limit,
                cancel: Some(&cancel),
                exclude_dirs: &job.exclude_dirs,
                hide_system: job.hide_system,
            })
        });
        let Some(response) = response else {
            continue;
        };
        if response.cancelled || cancel.load(Ordering::Relaxed) {
            continue;
        }
        let done = Box::into_raw(Box::new(Done {
            seq: job.seq,
            query: job.query,
            keep_position: job.keep_position,
            response,
        }));
        let target = HWND(hwnd as *mut core::ffi::c_void);
        let posted = unsafe {
            PostMessageW(Some(target), WM_APP_SEARCH_DONE, WPARAM(0), LPARAM(done as isize))
        };
        if posted.is_err() {
            // 消息没有送达，结果由这里回收
            drop(unsafe { Box::from_raw(done) });
        }
    });
}

/// 提交新请求，替换还没开始的请求，并取消正在进行的扫描。
pub fn submit(job: Option<Job>) {
    let Some(shared) = SHARED.get() else {
        return;
    };
    let mut st = shared.state.lock().unwrap_or_else(|p| p.into_inner());
    st.cancel.store(true, Ordering::Relaxed);
    st.pending = job;
    shared.wake.notify_one();
}
