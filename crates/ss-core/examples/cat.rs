//! 验证多盘目录：构建所有 NTFS 盘索引，打印每盘统计，并演示「全部 / 指定盘符」搜索。
//!
//! 用法（需管理员）：`cat.exe explorer [输出文件]`

use std::fmt::Write as _;
use std::time::Instant;

fn main() {
    let query = std::env::args().nth(1).unwrap_or_else(|| "explorer".into());
    let out_path = std::env::args().nth(2);

    let mut buf = String::new();
    let t = Instant::now();
    let cat = ss_core::Catalog::build_all();
    let build = t.elapsed();

    let letters = cat.drive_letters();
    let _ = writeln!(
        buf,
        "已索引盘符: {:?}, 合计 {} 节点, {:.1} MB, 构建耗时 {:.2?}",
        letters,
        cat.total_nodes(),
        cat.memory_bytes() as f64 / 1_000_000.0,
        build
    );

    // 全部盘搜索
    let t2 = Instant::now();
    let all = cat.search(&query, None, 50);
    let _ = writeln!(buf, "\n[全部盘] '{}': {} 命中, {:.2?}", query, all.len(), t2.elapsed());
    for r in all.iter().take(5) {
        let _ = writeln!(buf, "  {}", r.path);
    }

    // 逐盘过滤搜索
    for d in &letters {
        let t3 = Instant::now();
        let res = cat.search(&query, Some(*d), 50);
        let _ = writeln!(buf, "\n[仅 {}:] '{}': {} 命中, {:.2?}", d, query, res.len(), t3.elapsed());
        for r in res.iter().take(3) {
            let _ = writeln!(buf, "  {}", r.path);
        }
    }

    print!("{buf}");
    if let Some(p) = out_path {
        let _ = std::fs::write(&p, buf.as_bytes());
    }
}
