<img src="assets/generated/savesearch-app-icon-concept.png" width="120" align="right" alt="SaveSearch icon"/>

# SaveSearch

[![CI](https://github.com/vvangpc/save-search/actions/workflows/ci.yml/badge.svg)](https://github.com/vvangpc/save-search/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/vvangpc/save-search)](https://github.com/vvangpc/save-search/releases/latest)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

> Windows 10/11 常驻托盘小工具：NTFS 毫秒级文件搜索 + 「保存 / 打开」对话框位置快选。

单个 exe（约 0.6 MB，静态链接、无运行时依赖），用 **Rust + [windows-rs](https://github.com/microsoft/windows-rs)** 编写，
原生 Win32 自绘界面，200 万文件常驻内存不到 90 MB。

- **🔍 搜索**：`Alt + Space` 呼出，输入即搜，全盘 200 万文件 10 ms 内出结果。
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
- **即时搜索**：每键即搜，大小写不敏感；空格分词取交集（`report 2024`）；支持 `*` / `?` 通配（`*.pdf`、`re?ort*`）。
- **相关性排序**：完全匹配 > 前缀匹配 > 文件夹 > 浅路径 > 字母序。
- **搜索范围**：全部盘 / 单个盘 / **预设文件夹**（设置里添加，仅搜该文件夹子树）。
- **分类筛选**：全部 / 文件夹 / 文档 / 图片 / 压缩包 / 其他。
- **秒开缓存**：索引落盘，二次启动多盘并行加载，200 万节点约 30 ms。
- **实时增量**：USN 日志每 2 秒增量更新，只订阅创建 / 删除 / 改名 / 硬链接四类记录；
  文件增删改几秒内反映到结果，长期运行自动回收已删节点，内存不随时间增长。
- **自绘结果列表**：真实文件类型图标 + 文件名 + 灰色路径，逐像素平滑滚动，方向键 / 回车 / 双击 / Esc。
- **记住上次搜索词**，跨重启预填。

### 保存位置快选
- 进程外检测系统文件对话框（`SetWinEventHook`，**不注入 DLL**）。
- 浮窗按 **收藏 → 已打开 → 最近** 排列并去重，带来源标记（★ 收藏 / 最近）。
- 对话框开着时去资源管理器新开一个文件夹，切回对话框浮窗即自动出现该文件夹。
- 单击通过 UI Automation 让对话框切换目录：模拟真实键入 + 回车，**不会误改文件名**，
  跨进程消息全部带超时，目标程序卡死不影响本工具。
- 右键浮窗项收藏 / 取消收藏；浮窗随对话框移动，失焦或关闭即隐藏；贴近屏底时自动改显示在上方。

### 设置与外观
- 托盘「设置…」：主题、索引盘符、预设文件夹、开机自启（计划任务，免 UAC）、结果上限、
  最近条数、浮窗总开关与三类显示项。
- 三套主题：商务浅色 / 商务深色 / 暖阳浅色，Windows 11 下标题栏随主题着色。
- 搜索窗在鼠标所在显示器居中弹出；多显示器友好。

## 下载与安装

**系统要求**：Windows 10（1607+）/ Windows 11，x64，NTFS 固定盘。
需要**管理员权限**（读取 MFT / USN 日志必须打开卷句柄），启动时会弹 UAC。

前往 [Releases](https://github.com/vvangpc/save-search/releases/latest) 下载 `ss-app.exe`
（或 `SaveSearch-vX.Y.Z.zip`），放到任意目录双击运行即可，无需安装。程序驻留系统托盘。

> 每次 push 到 `main` 的 CI 构建产物也可在 [Actions](https://github.com/vvangpc/save-search/actions) 页面下载（需登录 GitHub）。

## 使用

| 操作 | 说明 |
|---|---|
| `Alt + Space` | 打开 / 隐藏搜索窗 |
| 输入关键字 | 即时搜索；空格分词；`*` `?` 通配 |
| `↑` `↓` `PgUp` `PgDn` `Home` `End` | 在结果中移动选中项（焦点始终在输入框，可继续打字） |
| `Enter` / 双击 | 在资源管理器中定位（文件夹直接打开） |
| `Esc` | 隐藏搜索窗 |
| 顶部标签 | 切换分类 |
| 右侧下拉 | 切换范围：全部盘 / 盘符 / 📁 预设文件夹 |
| 托盘左键 | 打开搜索 |
| 托盘右键 | 打开搜索 / 设置… / 退出 |

在任意程序中触发「保存 / 打开 / 上传」对话框后，对话框**下方**出现浮窗：单击某项跳转到该目录，右键收藏 / 取消收藏。

## 性能

release 构建，本机 C + D 两盘共 202 万节点（v0.3.0 与 v0.2.3 对比）：

| 指标 | v0.2.3 | v0.3.0 |
|---|---|---|
| 搜索 `explorer`（C 盘 135 万节点，取前 100） | 61.5 ms | **9.7 ms** |
| 缓存加载（两盘） | 169 ms | **32 ms** |
| 全量构建（两盘） | 3.23 s | **2.03 s** |
| 常驻内存 | ≈ 126 MB | **86 MB** |

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
| `ss-app` | 可执行文件：托盘、热键、消息循环、线程编排、搜索窗、浮窗、设置窗、主题 |

## 数据目录

| 内容 | 路径 |
|---|---|
| 索引缓存 | `%LOCALAPPDATA%\SaveSearch\cache\index_*.ssidx` |
| 收藏夹 / 最近位置 / 设置 | `%APPDATA%\SaveSearch\{favorites,recent,settings}.json` |

- 所有写入都是**原子写**（临时文件 → `fsync` → 替换），崩溃 / 断电不会留下半截文件。
- JSON 若意外损坏会改名保留为 `*.json.corrupt-<时间戳>`，不会被默认值覆盖。
- 索引缓存格式为 v3；从 v0.2.x 升级后首次启动会自动重建一次索引（约 2 s）。
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
- 不同缩放比例的多显示器之间移动窗口时，字体与布局暂不重算（功能正常，仅尺寸略偏）。
- 计划中：Inno Setup 安装包、代码签名（需证书）、非 NTFS 盘降级方案。

## 许可证

[MIT](LICENSE)
