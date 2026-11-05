# 现状审计

## 性能基线（批次一开始前）

基线提交：`7c18043`（`main`）。测量日期：2026-11-05。工具链：rustc 1.96.0，16 线程。

### exe 大小与测试

| 项目 | 结果 |
|---|---|
| `target/release/ss-app.exe` | 653,824 字节 |
| 体积上限（累计增长不超过 20%） | 784,588 字节 |
| `cargo build --release --locked` | 通过（`ss-app` 有 2 条已存在的警告） |
| `cargo test --release --locked` | 通过：ss-config 3，ss-core 15，ss-platform 1，ss-shell 0 |

### 合成索引性能

命令：`target/release/examples/synth.exe`（2,000,001 节点，`limit=200`，每个查询 9 次）。基线版会在命中 4000 条后提前停止，所以泛词的耗时不能和全量扫描直接比较。

索引内存 80.1 MB（新增内存上限 88.1 MB）。

| 查询 | 中位耗时 | 最慢 |
|---|---|---|
| `a` | 6.16 ms | 7.11 ms |
| `e` | 5.60 ms | 6.98 ms |
| `report` | 7.52 ms | 9.18 ms |
| `报告` | 6.42 ms | 6.91 ms |
| `*.txt` | 37.73 ms | 40.21 ms |
| `report final` | 11.35 ms | 12.01 ms |
| `zzzznotfound` | 7.11 ms | 7.17 ms |

### 实盘基线（待用户回填）

`cat` 和 `cache` 示例需要管理员权限，当前环境没有。修改代码前已把基线版程序保存到 `target/baseline/`，所以之后补测仍然反映基线。在管理员终端运行：

```
target\baseline\cat.exe explorer
target\baseline\cache.exe
```

| 项目 | 结果 |
|---|---|
| `cat`：节点数、内存、构建耗时、搜索耗时 | 待回填 |
| `cache`：加载耗时、USN 增量结果 | 待回填 |
