//! 验证：缓存加载（秒开）+ USN 实时增量（建/删文件后增量反映）。
//! 用法（需管理员）：`cache.exe [输出文件]`

use std::fmt::Write as _;
use std::time::{Duration, Instant};

fn main() {
    let out_path = std::env::args().nth(1);
    let cache = std::path::Path::new("D:\\AI\\save-search\\target\\ss-cache");
    let mut buf = String::new();

    let t = Instant::now();
    let mut cat = ss_core::Catalog::build_or_load(cache);
    let load = t.elapsed();
    let _ = writeln!(
        buf,
        "build_or_load: 盘 {:?}, {} 节点, {:.1} MB, 耗时 {:.2?}",
        cat.drive_letters(),
        cat.total_nodes(),
        cat.memory_bytes() as f64 / 1_000_000.0,
        load
    );

    // USN 增量：在 C 盘 temp 建一个唯一名字的文件
    let marker = "sssusnprobe7391";
    let mut p = std::env::temp_dir();
    p.push(format!("{marker}.txt"));
    let _ = std::fs::write(&p, b"x");
    let _ = writeln!(buf, "创建测试文件: {}", p.display());
    std::thread::sleep(Duration::from_millis(600));

    let n1 = cat.catch_up();
    let found1 = cat.search(marker, None, ss_core::Category::All, 10);
    let _ = writeln!(buf, "catch_up 应用 {n1} 变更; 搜到 {} 个 (期望≥1)", found1.len());
    for r in &found1 {
        let _ = writeln!(buf, "  {}", r.path);
    }

    let _ = std::fs::remove_file(&p);
    std::thread::sleep(Duration::from_millis(600));
    let n2 = cat.catch_up();
    let found2 = cat.search(marker, None, ss_core::Category::All, 10);
    let _ = writeln!(buf, "删除后 catch_up 应用 {n2} 变更; 搜到 {} 个 (期望0)", found2.len());

    cat.save_all(cache);
    let _ = writeln!(buf, "已保存缓存");

    print!("{buf}");
    if let Some(op) = out_path {
        let _ = std::fs::write(&op, buf.as_bytes());
    }
}
