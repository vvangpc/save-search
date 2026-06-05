# SaveSearch

Windows 10/11 常驻后台小工具，两大功能：

1. **指定盘符快速搜索** —— 基于 NTFS MFT 的毫秒级文件/文件夹搜索（类似 Everything）。
2. **保存位置快速选择** —— 任何程序弹出"保存/打开/上传文件"对话框时，在对话框**下方**显示浮窗，
   列出「当前已打开的资源管理器文件夹 + 收藏夹 + 最近位置」，单击即跳转。

特点：常驻后台、内存占用小（单 exe ~270KB，静态链接无依赖）、进程外实现（不注入 DLL）。

## 功能细节

**快速搜索**
- 多盘索引，毫秒级大小写不敏感子串搜索
- 盘符过滤：搜全部盘或指定某个盘
- 索引磁盘缓存，二次启动秒开（无需重新全盘扫描）
- USN 日志增量更新，文件增删改几秒内反映
- 全局热键 `Alt+Space` 弹出搜索框；回车 / 双击在资源管理器中定位

**保存位置快速选择**
- `SetWinEventHook` 检测系统文件对话框（`#32770`）
- `IShellWindows` 枚举已打开的资源管理器文件夹
- 浮窗列出 收藏 + 已打开 + 最近，单击经 UI Automation 让对话框切换到该文件夹（不误保存）
- 右键浮窗项可收藏 / 取消收藏；浮窗随对话框移动、对话框关闭即消失

## 架构

Rust + [windows-rs](https://github.com/microsoft/windows-rs)，Cargo workspace 多 crate：

| crate | 职责 |
|---|---|
| `ss-platform` | Win32 薄封装：宽字符串、COM guard |
| `ss-core` | 索引引擎：MFT 枚举、USN 增量、紧凑索引、搜索、磁盘缓存 |
| `ss-shell` | 功能2：WinEvent 钩子、IShellWindows 枚举、UI Automation 对话框导航 |
| `ss-config` | 收藏夹 / 最近位置（JSON） |
| `ss-app` | 可执行文件：托盘、热键、消息主循环、线程编排、保存对话框浮窗 |

## 构建

需要 Rust（`stable-x86_64-pc-windows-msvc`）与 VS Build Tools。

```powershell
cargo build --release
```

产物：`target/release/ss-app.exe`（带 `requireAdministrator` 清单——读取 NTFS MFT 需要管理员权限，
启动时会弹 UAC）。

数据目录：索引缓存 `%LOCALAPPDATA%\SaveSearch\cache\`，收藏/最近 `%APPDATA%\SaveSearch\`。

## 状态

两大核心功能已完成可用。计划中的后续项：开机自启（计划任务）、Inno Setup 安装包、代码签名、
非 NTFS 盘（FAT32/exFAT/U 盘）支持。

## License

MIT
