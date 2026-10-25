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
                .body("窗口已收到托盘，服务保持运行。点托盘图标可重新打开；退出请用其菜单中的「退出」。")
                .show();
        }
    }
}

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .invoke_handler(tauri::generate_handler![
            server::control_status_cmd,
            server::control_start_cmd,
            server::control_stop_cmd,
            server::control_clear_logs_cmd,
            server::set_share_dir_cmd,
            server::open_path_cmd,
            server::open_url_cmd,
            server::copy_file_cmd
        ])
        .setup(|app| {
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
                .on_menu_event(|app, ev| match ev.id().as_ref() {
                    "quit" => app.exit(0),
                    "show" => {
                        if let Some(w) = app.get_webview_window("main") {
                            let _ = w.show();
                            let _ = w.set_focus();
                        }
                    }
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
            }
            } // #[cfg(desktop)]

            // ---- 自动拉起局域网服务(端口被占用自动 +1 重试) ----
            match server::autostart() {
                Ok(port) => eprintln!("[wft] 服务已启动: http://{}:{port}", server::lan_ip()),
                Err(e) => eprintln!("[wft] 服务启动失败: {e}"),
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
