# 执行进度

## 批次一：后端

### 步骤 1：记录性能基线

- 结果：exe 大小、测试结果和合成性能已写入 `audit.md`。新增 `examples/synth.rs`，用于比较改动前后的搜索耗时和索引内存，不需要管理员权限。
- 验证：`cargo build --release --locked`、`cargo test --release --locked` 通过。
- 需要用户手动验证：在管理员终端运行 `target\baseline\cat.exe explorer` 和 `target\baseline\cache.exe`，把结果回填到 `audit.md`。计划要求在修改代码前运行；用户要求整批完成后再告知，所以预先保存了基线版程序。
