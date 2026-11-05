//! 人工验证结果操作。
//!
//! 用法：`actions.exe <open|reveal|copy|props|menu> <路径>`
//!
//! 验证 `open` 不继承管理员权限：在管理员终端运行
//! `actions.exe open C:\Windows\System32\notepad.exe`，
//! 然后在任务管理器“详细信息”页的“提升的”一列确认记事本为“否”。

use ss_shell::{copy_path, open_deelevated, reveal, show_context_menu, show_properties};
use windows::core::w;
use windows::Win32::Foundation::POINT;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, GetCursorPos, WINDOW_EX_STYLE, WS_POPUP,
};

fn main() {
    let mut args = std::env::args().skip(1);
    let (Some(action), Some(path)) = (args.next(), args.next()) else {
        eprintln!("用法：actions.exe <open|reveal|copy|props|menu> <路径>");
        std::process::exit(2);
    };
    let _com = ss_platform::ComScope::init_sta().expect("初始化 COM 失败");

    // 不显示的窗口，用作剪贴板和菜单的所有者
    let owner = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("STATIC"),
            w!("ss-actions"),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            None,
            None,
        )
    }
    .expect("创建窗口失败");

    let result = match action.as_str() {
        "open" => open_deelevated(&path).map(|()| "已通过 Explorer 打开".to_string()),
        "reveal" => reveal(&path).map(|()| "已定位".to_string()),
        "copy" => copy_path(owner, &path).map(|()| "已复制到剪贴板".to_string()),
        "props" => show_properties(owner, &path).map(|()| {
            // 属性对话框属于本进程，退出进程会把它一起关掉
            std::thread::sleep(std::time::Duration::from_secs(30));
            "属性对话框已显示 30 秒".to_string()
        }),
        "menu" => {
            let mut at = POINT::default();
            let _ = unsafe { GetCursorPos(&mut at) };
            show_context_menu(owner, &path, at.x, at.y).map(|o| format!("菜单结果：{o:?}"))
        }
        other => {
            eprintln!("未知操作：{other}");
            std::process::exit(2);
        }
    };
    match result {
        Ok(message) => println!("{message}"),
        Err(e) => {
            eprintln!("失败：{e}");
            std::process::exit(1);
        }
    }
}
