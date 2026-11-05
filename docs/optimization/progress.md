# 执行进度

## 批次一：后端

### 步骤 1：记录性能基线

- 结果：exe 大小、测试结果和合成性能已写入 `audit.md`。新增 `examples/synth.rs`，用于比较改动前后的搜索耗时和索引内存，不需要管理员权限。
- 验证：`cargo build --release --locked`、`cargo test --release --locked` 通过。
- 需要用户手动验证：在管理员终端运行 `target\baseline\cat.exe explorer` 和 `target\baseline\cache.exe`，把结果回填到 `audit.md`。计划要求在修改代码前运行；用户要求整批完成后再告知，所以预先保存了基线版程序。

### 步骤 2：搜索语义修复

- 提交：`f395da7`
- 结果：全角空格分词、`?` 按完整字符匹配、`*.txt` 结尾对齐、主名完全匹配优先都已生效。
- 验证：构建和测试通过，新增 4 个测试。

### 步骤 3：全量扫描与 Top-K 排序

- 提交：`37dacf7`
- 结果：不再提前退出，并统计 `total_hits`。完全相同的键按 id 决定先后，结果与线程数无关。
- 验证：构建和测试通过。`--ignored` 合成测试单字符查询中位 6.9 到 9.4 ms，低于 150 ms 上限。

### 步骤 4：新搜索接口

- 提交：`aafdc07`
- 结果：新增 `Catalog::query`、命中区间、取消和 generation。旧 `Catalog::search` 改为调用它。
- 计划外补充：`Index::apply` 返回是否有变化，`apply_all` 返回变化条数。否则同内容 Upsert 也会让 generation 递增。这两个函数只是增加了返回值，旧调用方不受影响。
- 验证：构建和测试通过。

### 步骤 5：分类与搜索语法

- 提交：`25643e3`
- 结果：新增 Media、App 分类，支持 `ext:`、`dir:` 和路径片段，新增各分类命中数。
- 与计划的差异：计划没有定义 `dir:`，按 `ui-preview.html` 的原型实现：单独的 `dir:` 表示只要文件夹，`dir:abc` 按普通词处理。
- 分类归属：`dll` 留在“其他”，因为用户不会直接运行它。
- 用户可见的过渡问题：前端批次完成前，音视频和程序文件不再出现在旧界面的“其他”标签中，只能在“全部”下看到。
- 验证：构建和测试通过。合成测试单字符查询中位 9.1 到 12.6 ms，exe 665,600 字节。

### 步骤 6：索引状态与盘管理

- 提交：`24ef0ef`
- 结果：新增盘状态、进度回调、`drive_statuses`、`list_volumes`、`remove_drive`。原来吞掉的错误现在保存在 `DriveStatus` 中。
- 与计划的差异：实盘部分只写成示例程序 `examples/volumes.rs`，没有写 `#[ignore]` 测试。否则步骤 3 的 `cargo test --release -p ss-core -- --ignored` 在没有管理员权限时会失败。
- 验证：构建和测试通过。无管理员权限运行示例：卷列表正确（C、D 盘，NTFS），`Failed` 事件带“拒绝访问”原因。
- 需要用户手动验证：在管理员终端运行 `target\release\examples\volumes.exe <缓存目录>`，确认能看到 Loading、Building、Ready 事件。

### 步骤 7：配置层加固与新设置项

- 提交：`cf8d320`
- 结果：按计划完成。`theme` 字段保留，任何旧值都能读取；旧界面暂时仍在使用它，等前端批次步骤 2 删除。
- 对 ss-app 的最小修改：`settings_win.rs` 的 `Settings` 结构体字面量因新增字段无法编译，改为 `..ss_config::load_settings()`，这样也不会清空新设置。
- 行为变化：旧设置页允许的 `result_limit` 1 到 100000 现在会被限制到 20 到 5000，`recent_max` 上限从 100 变为 50。
- 取值约定：`last_scope` 和 `last_category` 用字符串保存，因为 ss-config 不依赖 ss-core。`enter_action` 用字符串而不是枚举，这样无法识别的值只会恢复默认，不会让整个配置文件被判为损坏。
- 验证：构建和测试通过，ss-config 测试从 3 个增至 10 个。测试使用临时目录，不碰真实配置。
