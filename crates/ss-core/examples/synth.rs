//! 合成索引性能测量：不碰实盘、无需管理员，用于比较改动前后的搜索耗时和索引内存。
//!
//! 用法：`cargo run --release -p ss-core --example synth -- [节点数] [每个查询重复次数]`

use std::time::{Duration, Instant};

use ss_core::{Category, IndexBuilder};

const WORDS: &[&str] = &[
    "report", "final", "backup", "project", "image", "photo", "readme", "config", "module",
    "system", "data", "notes", "draft", "setup", "client", "server", "报告", "项目", "备份", "照片",
];
const EXTS: &[&str] = &[
    "txt", "pdf", "docx", "png", "jpg", "zip", "dll", "exe", "rs", "json", "mp4", "log", "xml",
];

fn main() {
    let nodes: u64 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(2_000_000);
    let repeat: usize = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(9);

    let t = Instant::now();
    let mut b = IndexBuilder::new(b'C');
    b.reserve(nodes as usize);
    let mut rng = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = move || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };
    let mut dirs: Vec<u64> = vec![5];
    let mut name = String::with_capacity(64);
    for i in 0..nodes {
        let frn = 1000 + i;
        let parent = dirs[(next() % dirs.len() as u64) as usize];
        name.clear();
        name.push_str(WORDS[(next() % WORDS.len() as u64) as usize]);
        name.push('_');
        name.push_str(WORDS[(next() % WORDS.len() as u64) as usize]);
        name.push_str(&format!("_{}", next() % 100_000));
        if next() % 20 == 0 {
            dirs.push(frn);
            b.push(frn, parent, 0x10, &name);
        } else {
            name.push('.');
            name.push_str(EXTS[(next() % EXTS.len() as u64) as usize]);
            b.push(frn, parent, 0, &name);
        }
    }
    let idx = b.finish();
    println!(
        "节点 {} | 索引内存 {:.1} MB | 构建 {:.2?} | 线程 {}",
        idx.len(),
        idx.memory_bytes() as f64 / 1_000_000.0,
        t.elapsed(),
        std::thread::available_parallelism().map(|x| x.get()).unwrap_or(1),
    );

    for query in ["a", "e", "report", "报告", "*.txt", "report final", "zzzznotfound"] {
        let mut times: Vec<Duration> = Vec::with_capacity(repeat);
        let mut hits = 0;
        for _ in 0..repeat {
            let t = Instant::now();
            hits = idx.search(query, Category::All, 200).len();
            times.push(t.elapsed());
        }
        times.sort_unstable();
        println!(
            "{:<14} 返回 {:>4} 条 | 中位 {:>10.2?} | 最慢 {:>10.2?}",
            query,
            hits,
            times[times.len() / 2],
            times[times.len() - 1],
        );
    }
}
