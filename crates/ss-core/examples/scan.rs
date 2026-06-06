//! 手动验证：扫描一个盘符建索引，打印节点数 / 内存 / 扫描耗时，并做一次搜索计时。
//!
//! 用法（需管理员）：`scan.exe C explorer [输出文件]`
//! 若给出第 3 个参数，则把结果写入该文件（提权运行时方便回读，绕开 cmd 重定向）。

use std::fmt::Write as _;
use std::time::Instant;

fn main() {
    let letter = std::env::args()
        .nth(1)
        .and_then(|s| s.chars().next())
        .unwrap_or('C');
    let query = std::env::args().nth(2).unwrap_or_else(|| "explorer".into());
    let out_path = std::env::args().nth(3);

    let mut buf = String::new();
    let code = run(letter, &query, &mut buf);

    print!("{buf}");
    if let Some(p) = out_path {
        let _ = std::fs::write(&p, buf.as_bytes());
    }
    std::process::exit(code);
}

fn run(letter: char, query: &str, buf: &mut String) -> i32 {
    let t = Instant::now();
    match ss_core::build_index_for_drive(letter) {
        Ok(idx) => {
            let scan = t.elapsed();
            let _ = writeln!(
                buf,
                "盘 {}: {} 个节点, {:.1} MB, 扫描耗时 {:.2?}",
                letter,
                idx.len(),
                idx.memory_bytes() as f64 / 1_000_000.0,
                scan
            );

            let t2 = Instant::now();
            let res = idx.search(query, ss_core::Category::All, 100);
            let search = t2.elapsed();
            let _ = writeln!(buf, "搜索 '{}': {} 个命中, 耗时 {:.2?}", query, res.len(), search);
            for r in res.iter().take(15) {
                let _ = writeln!(buf, "  [{}] {}", if r.is_dir { "D" } else { "F" }, r.path);
            }
            0
        }
        Err(e) => {
            let _ = writeln!(buf, "错误: {e} (code={:?})", e.code());
            let _ = writeln!(buf, "提示: 需要管理员权限，且目标盘必须是 NTFS。");
            1
        }
    }
}
