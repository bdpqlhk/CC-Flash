//! CC-Flash · Tauri 桌面壳
//! 职责:托盘(关窗收到托盘,服务不停) + 启动时自动拉起局域网文件服务。

use tauri::Manager;
// 托盘为桌面专属能力, 移动端构建不编译这部分
#[cfg(desktop)]
use tauri::{
    menu::{Menu, MenuItem},
    tray::TrayIconBuilder,
};
use tauri_plugin_notification::NotificationExt;

mod config;
mod server;

#[cfg(desktop)]
static TRAY_TIP_SHOWN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 唤出主窗口的统一实现(托盘菜单 / 单实例重复启动 / macOS 点程序坞图标 三条路径共用):
/// 必须先 unminimize 再 show —— show() 只置可见, 不会把最小化的窗口还原。
#[cfg(desktop)]
fn show_main_window(app: &tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
    }
}

/// 关窗 = 收到托盘(服务继续运行);托盘菜单「退出」才真正结束进程。
#[cfg(desktop)]
fn main_window_event(window: &tauri::WebviewWindow, event: &tauri::WindowEvent) {
    if let tauri::WindowEvent::CloseRequested { api, .. } = event {
        api.prevent_close();
        let _ = window.hide();
        // 首次收起托盘: 系统通知提醒
        if !TRAY_TIP_SHOWN.swap(true, std::sync::atomic::Ordering::Relaxed) {
            let _ = window
                .app_handle()
                .notification()
                .builder()
                .title(format!("{} 仍在运行", config::APP_NAME))
                .body("窗口已收到托盘，服务保持运行。点托盘图标，或再次从程序坞(macOS)/桌面与开始菜单(Windows)打开均可重新显示；退出请用托盘菜单中的「退出」。")
                .show();
        }
    }
}

/// 移动端入口: tauri 需要该宏生成 JNI 运行时符号(Android/iOS 胶水层调用)
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let builder = tauri::Builder::default();
    // 单实例守卫(桌面专属, 且必须注册在所有插件之前): 关窗后应用仍在托盘运行,
    // 再从桌面/任务栏图标启动会拉出新进程 —— 每重开一次就多一个托盘图标。
    // 有该守卫后重复启动改为唤起已有实例的主窗口。
    #[cfg(desktop)]
    let builder = builder.plugin(tauri_plugin_single_instance::init(
        |app, _args, _cwd| show_main_window(app),
    ));
    let builder = builder
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            server::control_status_cmd,
            server::control_start_cmd,
            server::control_stop_cmd,
            server::control_clear_logs_cmd,
            server::set_share_dir_cmd,
            server::open_path_cmd,
            server::open_url_cmd,
            server::copy_file_cmd
        ]);
    // 开机自启插件(桌面专属; Cargo.toml 中按目标平台条件引入, 移动端无此插件)
    // 自启启动带 --hidden 参数: 登录后收进托盘后台运行, 不弹主窗口(与设置页文案一致)
    #[cfg(desktop)]
    let builder = builder.plugin(tauri_plugin_autostart::init(
        tauri_plugin_autostart::MacosLauncher::LaunchAgent,
        Some(vec!["--hidden"]),
    ));
    builder
        .setup(|app: &mut tauri::App| {
            // 移动端托盘/窗口能力被门控, app 不被直接使用;
            // 显式标注参数类型, 避免移动端编译时推断失败(E0282)
            #[cfg(not(desktop))]
            let _ = app;
            // Android: 注入 app 专属目录作默认共享目录 —— 应用进程 $HOME 无效,
            // 走 $HOME/CC-Flash 会落到 /CC-Flash 直接 EACCES, 服务启动即失败;
            // app 专属外部存储免权限可读写, 且经 HTTP 对外提供不受影响
            #[cfg(target_os = "android")]
            {
                use tauri::Manager;
                if let Ok(dir) = app.path().app_data_dir() {
                    let share = dir.join(config::SHARE_DIR_NAME);
                    let _ = std::fs::create_dir_all(&share);
                    server::set_platform_default_dir(share.to_string_lossy().to_string());
                }
            }
            // ---- 托盘 + 关窗收纳(桌面专属; 移动端无托盘概念) ----
            #[cfg(desktop)]
            {
            // ---- 托盘:显示主窗口 / 启停服务 / 打开网页 / 退出 ----
            let show = MenuItem::with_id(app, "show", "显示主窗口", true, None::<&str>)?;
            let toggle = MenuItem::with_id(app, "toggle", "启动/停止服务", true, None::<&str>)?;
            let web = MenuItem::with_id(app, "web", "打开网页", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show, &toggle, &web, &quit])?;
            let mut tray = TrayIconBuilder::with_id("main-tray")
                .tooltip(config::APP_NAME)
                .menu(&menu)
                // 关掉"左键弹菜单": macOS 上挂了 menu 后左键会直接进入菜单追踪, Click 事件
                // 到不了 handler; 关掉后两平台统一为 左键=唤窗 / 右键=菜单
                .show_menu_on_left_click(false)
                // 左键点击托盘图标 → 唤出主窗口(右键为菜单); 关窗通知文案承诺了这条路径
                .on_tray_icon_event(|tray, event| {
                    if let tauri::tray::TrayIconEvent::Click {
                        button: tauri::tray::MouseButton::Left,
                        button_state: tauri::tray::MouseButtonState::Up,
                        ..
                    } = event
                    {
                        show_main_window(tray.app_handle());
                    }
                })
                .on_menu_event(|app, ev| match ev.id().as_ref() {
                    "quit" => app.exit(0),
                    "show" => show_main_window(app),
                    "toggle" => {
                        if server::is_running() {
                            server::stop();
                        } else {
                            let _ = server::start(None);
                        }
                    }
                    "web" => {
                        let (running, port, ip) = server::status();
                        if running {
                            let _ = server::open_with_system(&format!("http://{ip}:{port}"));
                        }
                    }
                    _ => {}
                });
            // 有配置图标就用(未生成 icons 时托盘无图标,不影响功能)
            if let Some(icon) = app.default_window_icon() {
                tray = tray.icon(icon.clone());
            }
            tray.build(app)?;

            // ---- 主窗口:关窗收到托盘 ----
            if let Some(w) = app.get_webview_window("main") {
                let w2 = w.clone();
                w.on_window_event(move |event| main_window_event(&w2, event));
                // 窗口在 tauri.conf 中默认隐藏(visible:false), 防止自启时先创建可见窗口再隐藏的闪屏;
                // 手动启动(无 --hidden 参数)在这里显式展示, 自启则保持隐藏收进托盘
                if !std::env::args().any(|a| a == "--hidden") {
                    let _ = w.show();
                }
            }
            } // #[cfg(desktop)]

            // ---- 自动拉起局域网服务(端口被占用自动 +1 重试) ----
            match server::autostart() {
                Ok(port) => eprintln!("[wft] 服务已启动: http://{}:{port}", server::lan_ip()),
                Err(e) => eprintln!("[wft] 服务启动失败: {e}"),
            }
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app, event| {
            // macOS: 关窗收到托盘后点程序坞图标 → applicationShouldHandleReopen 触发,
            // 重新显示主窗口(否则窗口已隐藏时 Dock 点击无反应)
            #[cfg(target_os = "macos")]
            if let tauri::RunEvent::Reopen { .. } = event {
                show_main_window(app);
            }
            #[cfg(not(target_os = "macos"))]
            let _ = (&app, &event);
        });
}
