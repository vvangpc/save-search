//! 后台线程的任务队列。比 `std::sync::mpsc` 小得多，这里也用不到它的其他能力。

use std::collections::VecDeque;
use std::sync::{Condvar, Mutex};

pub struct WorkQueue<T> {
    items: Mutex<VecDeque<T>>,
    wake: Condvar,
}

impl<T> WorkQueue<T> {
    pub const fn new() -> Self {
        WorkQueue { items: Mutex::new(VecDeque::new()), wake: Condvar::new() }
    }

    pub fn push(&self, item: T) {
        self.items.lock().unwrap_or_else(|p| p.into_inner()).push_back(item);
        self.wake.notify_one();
    }

    /// 取下一个任务，队列为空时等待。
    pub fn pop(&self) -> T {
        let mut items = self.items.lock().unwrap_or_else(|p| p.into_inner());
        loop {
            if let Some(item) = items.pop_front() {
                return item;
            }
            items = self.wake.wait(items).unwrap_or_else(|p| p.into_inner());
        }
    }
}
