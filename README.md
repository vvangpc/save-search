# SaveSearch

Windows 10/11 常驻后台小工具，两大功能：

1. **指定盘符快速搜索** —— 基于 NTFS MFT 的毫秒级文件/文件夹搜索（类似 Everything）。
2. **保存位置快速选择** —— 任何程序弹出"保存/打开/上传文件"对话框时，在对话框**下方**显示浮窗，
   列出「当前已打开的资源管理器文件夹 + 收藏夹 + 最近位置」，单击即跳转。

特点：常驻后台、内存占用小（单 exe ~0.5MB，静态链接无依赖）、进程外实现（不注入 DLL）、原生 Win32 自绘界面（多主题）。

## 功能细节

**快速搜索**
- 多盘索引，毫秒级大小写不敏感子串搜索
- 盘符过滤：搜全部盘或指定某个盘
- **分类标签筛选**：全部 / 文件夹 / 文档 / 图片 / 视频 / 音频 / 其他
- 索引磁盘缓存，二次启动秒开（无需重新全盘扫描）
- USN 日志增量更新，文件增删改几秒内反映
- 全局热键 `Alt+Space` 弹出搜索框；回车 / 双击在资源管理器中定位
- **自绘双行结果**（真实文件类型图标 + 文件名 + 灰色路径）+ **逐像素平滑滚动**（惯性缓动、双缓冲、悬停即可滚、方向键导航）

**保存位置快速选择**
- `SetWinEventHook` 检测系统文件对话框（`#32770`）
- `IShellWindows` 枚举已打开的资源管理器文件夹
- 浮窗列出 收藏 + 已打开 + 最近，单击经 UI Automation 让对话框切换到该文件夹（不误保存）
- 右键浮窗项可收藏 / 取消收藏；浮窗相对对话框居中、随之移动；对话框失焦/关闭即隐藏

**设置与外观**
- 托盘「设置…」：主题、索引盘符、开机自启（计划任务）、结果上限、最近条数、浮窗开关与显示项
- **主题**：商务浅色 / 商务深色 / 暖阳浅色（Win11 下标题栏随主题着色）

## 架构

Rust + [windows-rs](https://github.com/microsoft/windows-rs)，Cargo workspace 多 crate：

| crate | 职责 |
|---|---|
| `ss-platform` | Win32 薄封装：宽字符串、COM guard |
| `ss-core` | 索引引擎：MFT 枚举、USN 增量、紧凑索引、搜索、磁盘缓存 |
| `ss-shell` | 功能2：WinEvent 钩子、IShellWindows 枚举、UI Automation 对话框导航 |
| `ss-config` | 设置 / 收藏夹 / 最近位置（JSON） |
| `ss-app` | 可执行文件：托盘、热键、消息主循环、线程编排、搜索窗（自绘平滑列表）、保存对话框浮窗、设置窗、主题 |

## 构建

需要 Rust（`stable-x86_64-pc-windows-msvc`）与 VS Build Tools。

```powershell
cargo build --release
```

产物：`target/release/ss-app.exe`（带 `requireAdministrator` 清单——读取 NTFS MFT 需要管理员权限，
启动时会弹 UAC）。

数据目录：索引缓存 `%LOCALAPPDATA%\SaveSearch\cache\`，收藏/最近 `%APPDATA%\SaveSearch\`。

## 状态

两大核心功能 + 分类筛选 + 设置页 + 多主题 + 平滑滚动界面均已完成可用。
计划中的后续项：Inno Setup 安装包、代码签名、非 NTFS 盘（FAT32/exFAT/U 盘）支持。

## License

MIT
