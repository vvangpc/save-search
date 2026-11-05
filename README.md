<img src="assets/generated/savesearch-app-icon-concept.png" width="120" align="right" alt="SaveSearch icon"/>

# SaveSearch

[![CI](https://github.com/vvangpc/save-search/actions/workflows/ci.yml/badge.svg)](https://github.com/vvangpc/save-search/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/vvangpc/save-search)](https://github.com/vvangpc/save-search/releases/latest)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

> Windows 10/11 常驻托盘小工具：NTFS 毫秒级文件搜索 + 「保存 / 打开」对话框位置快选。

单个 exe（约 0.78 MB，静态链接、无运行时依赖），用 **Rust + [windows-rs](https://github.com/microsoft/windows-rs)** 编写，
原生 Win32 自绘界面，250 万文件常驻索引内存约 102 MB。

- **🔍 搜索**：`Alt + Space` 呼出（可在设置里改键），输入即搜，200 万文件约 15 ms 出结果。
- **📌 快选**：任何程序弹出「保存 / 打开 / 上传」对话框时，对话框下方自动出现浮窗，
  列出已打开的资源管理器文件夹、收藏夹和最近位置，单击即跳转。

---

## 目录

- [功能](#功能)
- [下载与安装](#下载与安装)
- [使用](#使用)
- [性能](#性能)
- [工作原理](#工作原理)
- [数据目录](#数据目录)
- [从源码构建](#从源码构建)
- [已知限制与路线图](#已知限制与路线图)

## 功能

### 文件搜索
- **多盘索引**：自动识别所有固定 NTFS 盘，读取 MFT 建索引，无需遍历目录树。
- **即时搜索**：搜索在工作线程执行，只保留最新请求。大小写不敏感；空格分词取交集（`report 2024`）；支持 `*` / `?` 通配（`*.pdf`、`re?ort*`）。
- **搜索语法**：`ext:pdf,docx` 按扩展名筛选，`dir:` 只看文件夹，含 `\` 的词匹配路径（`工作\ 报告`）。
- **相关性排序**：全量扫描后取前 N 条，完全匹配 > 前缀匹配 > 文件夹 > 浅路径 > 字母序。结果被截断时显示总数。
- **搜索范围**：全部盘 / 单个盘 / **预设文件夹**（设置里添加，仅搜该文件夹子树）。下次启动恢复上次的范围和分类。
- **分类筛选**：全部 / 文件夹 / 文档 / 图片 / 音视频 / 压缩包 / 程序 / 其他，标签上显示命中数，`Alt + 1…8` 切换。
- **排除目录与隐藏文件**：可排除指定目录，可隐藏带隐藏或系统属性的文件。
- **秒开缓存**：索引落盘，二次启动多盘并行加载，200 万节点约 30 ms。
- **实时增量**：USN 日志每 2 秒增量更新，只订阅创建 / 删除 / 改名 / 硬链接四类记录；
  文件增删改几秒内反映到结果，长期运行自动回收已删节点，内存不随时间增长。
- **自绘结果列表**：文件类型图标、高亮命中部分的文件名、父目录（长路径省略中间）、大小和修改时间。逐像素平滑滚动，系统关闭动画时直接跳转。
- **状态提示**：索引进度、部分盘失败、范围失效、无结果时都有说明和下一步操作；底部状态栏显示结果数、耗时、范围和更新时间。
- **自动刷新**：窗口打开时索引有变化就重新搜索，并按路径保持选中项和滚动位置。
- **空输入页**：显示收藏、最近位置和语法提示。
- **记住上次搜索词**，跨重启预填。

### 保存位置快选
- 进程外检测系统文件对话框（`SetWinEventHook`，**不注入 DLL**）。
- 按收藏、已打开、最近排列。收藏和最近位置立即显示，已打开的文件夹在后台读取后插入，Explorer 无响应时浮窗仍能显示。
- 文件夹名为主、父路径为辅，收藏带书签标记，右侧标记来源。失效路径置灰且不可点击。
- 对话框开着时去资源管理器新开一个文件夹，切回对话框浮窗即自动出现该文件夹。
- 单击通过 UI Automation 让对话框切换目录：模拟真实键入 + 回车，**不会误改文件名**，
  跨进程消息全部带超时。跳转失败时在浮窗顶部说明原因。
- 右键浮窗项收藏 / 取消收藏；浮窗随对话框移动，失焦或关闭即隐藏；贴近屏底时自动改显示在上方。

### 设置与外观
- 托盘「设置…」：索引盘符及每个盘的状态、索引统计和重建索引、呼出热键（被占用时提示）、排除目录、
  隐藏系统文件、预设文件夹、开机自启（计划任务，免 UAC）、结果上限、最近条数、浮窗总开关与三类显示项、收藏夹排序和删除。
- 只有一套浅色界面，旧配置里的主题设置会被忽略。
- 搜索窗在鼠标所在显示器居中弹出；窗口在不同缩放比例的显示器之间移动时重算字体和布局。

## 下载与安装

**系统要求**：Windows 10（1607+）/ Windows 11，x64，NTFS 固定盘。
需要**管理员权限**（读取 MFT / USN 日志必须打开卷句柄），启动时会弹 UAC。

前往 [Releases](https://github.com/vvangpc/save-search/releases/latest) 下载 `ss-app.exe`
（或 `SaveSearch-vX.Y.Z.zip`），放到任意目录双击运行即可，无需安装。程序驻留系统托盘。

> 每次 push 到 `main` 的 CI 构建产物也可在 [Actions](https://github.com/vvangpc/save-search/actions) 页面下载（需登录 GitHub）。

## 使用

| 操作 | 说明 |
|---|---|
| `Alt + Space` | 打开 / 隐藏搜索窗（可在设置里改键） |
| 输入关键字 | 即时搜索；空格分词；`*` `?` 通配；`ext:` `dir:` 和路径片段 |
| `↑` `↓` `PgUp` `PgDn` `Home` `End` | 在结果中移动选中项（焦点始终在输入框，可继续打字） |
| `Enter` / 双击 | 打开文件（文件夹直接打开），打开的程序不继承管理员权限 |
| `Ctrl + Enter` | 在资源管理器中定位 |
| `Ctrl + C` | 复制路径（输入框只选中部分文字时复制这部分文字） |
| `Ctrl + Shift + C` | 复制文件名 |
| 右键结果 | 系统右键菜单 |
| `Esc` | 隐藏搜索窗 |
| 标签 / `Alt + 1…8` | 切换分类 |
| 右侧下拉 | 切换范围：全部盘 / 盘符 / 预设文件夹 |
| 托盘左键 | 打开搜索 |
| 托盘右键 | 打开搜索 / 设置… / 退出 |

在任意程序中触发「保存 / 打开 / 上传」对话框后，对话框**下方**出现浮窗：单击某项跳转到该目录，右键收藏 / 取消收藏。

## 性能

release 构建。合成索引 200 万节点，每次全量扫描并统计各分类命中数，`limit=200`，取中位耗时：

| 查询 | 耗时 |
|---|---|
| `a` | 13.7 ms |
| `report` | 11.8 ms |
| `报告` | 7.7 ms |
| `*.txt` | 25.9 ms |

实盘（本机 C + D 两盘，约 250 万节点）：索引内存 102.3 MB。

主要手段：FRN 映射用 MFT 记录号稠密表替代哈希表；搜索扫描多线程分段 + SIMD 首字节定位；
USN 只订阅结构性事件；多盘并行加载；增量追加按小步扩容，避免 `Vec` 倍增让常驻内存翻倍。

## 工作原理

```
┌──────────────┐  FSCTL_ENUM_USN_DATA   ┌──────────────────────────────┐
│  NTFS MFT    │ ─────────────────────▶ │ 紧凑索引（SoA + 共享名字池）  │
│  USN Journal │ ─── 每 2 s 增量 ──────▶ │ 记录号→id 稠密表 + 序列号校验 │
└──────────────┘                        └──────────────┬───────────────┘
                                                       │ 多线程扫描 + 排序
       Alt+Space ─▶ 搜索窗（自绘列表）◀────────────────┘

  WinEvent 钩子 ─▶ 检测 #32770 文件对话框 ─▶ IShellWindows 枚举已开文件夹
                                          ─▶ 浮窗 ─单击─▶ UIA 定位文件名框 ─▶ 键入路径 + 回车
```

Cargo workspace 多 crate：

| crate | 职责 |
|---|---|
| `ss-platform` | Win32 薄封装：宽字符串、COM 初始化 guard |
| `ss-core` | 索引引擎：MFT 枚举、USN 增量、紧凑索引、搜索、分类、子树搜索、磁盘缓存 |
| `ss-shell` | `IShellWindows` 枚举、文件对话框检测、UI Automation 导航 |
| `ss-config` | 设置 / 收藏夹 / 最近位置（JSON，原子写） |
| `ss-ui-model` | 不依赖 Win32 的界面逻辑：选中和滚动规则、状态判断、文字格式、热键解析（有单元测试） |
| `ss-app` | 可执行文件：托盘、热键、消息循环、线程编排、搜索窗、浮窗、设置窗、主题 |

## 数据目录

| 内容 | 路径 |
|---|---|
| 索引缓存 | `%LOCALAPPDATA%\SaveSearch\cache\index_*.ssidx` |
| 收藏夹 / 最近位置 / 设置 | `%APPDATA%\SaveSearch\{favorites,recent,settings}.json` |

- 所有写入都是**原子写**（临时文件 → `fsync` → 替换），崩溃 / 断电不会留下半截文件。
- JSON 若意外损坏会改名保留为 `*.json.corrupt-<时间戳>`，不会被默认值覆盖。
- 索引缓存格式为 v4；从旧版升级后首次启动会自动重建一次索引。
- 数据盘若从未启用 USN 日志，程序会自动创建（64 MiB）。

## 从源码构建

前置：Rust `stable-x86_64-pc-windows-msvc` 工具链 + Visual Studio Build Tools（含 Windows SDK 的 `rc.exe`）。
仓库根 `rust-toolchain.toml` 已固定工具链，首次构建 rustup 会自动准备。

```powershell
cargo build --release
```

产物：`target/release/ss-app.exe`（已内嵌 `requireAdministrator` 清单 + 应用图标）。
release profile 为体积优先：`opt-level="z"` + `lto="fat"` + `codegen-units=1` + `panic="abort"` + `strip`。

开发相关：

```powershell
# 库 crate 单元测试（ss-app 内嵌管理员清单，已设 test = false）
cargo test

# 索引引擎示例（需管理员）：扫描单盘 / 缓存 + USN 增量探测 / 多盘搜索
cargo run --release -p ss-core --example scan -- C explorer
cargo run --release -p ss-core --example cache
cargo run --release -p ss-core --example cat

# 从源图重新生成应用图标（需 Python + Pillow）
python tools/make_icon.py
```

### CI / 发布
- [`ci.yml`](.github/workflows/ci.yml)：每次 push / PR 在 `windows-latest` 上 `cargo build --release` + `cargo test`，上传 `ss-app.exe` 构建产物。
- 推送 `vX.Y.Z` tag 自动创建 **GitHub Release**，附 `ss-app.exe` 与 zip。

```powershell
git tag v0.3.0
git push origin v0.3.0
```

## 已知限制与路线图

- 仅索引 **NTFS 固定盘**；FAT32 / exFAT / U 盘暂不支持。
- 所有盘加载完成后才能搜索，首次建立索引期间只显示进度。
- 单独修改文件的隐藏或系统属性，要等文件改名或索引重建后才生效。
- 右键菜单只有打开、编辑、打印会降权，第三方菜单项启动的程序仍带管理员权限。
- 计划中：Inno Setup 安装包、代码签名（需证书）、非 NTFS 盘降级方案。

## 许可证

[MIT](LICENSE)
