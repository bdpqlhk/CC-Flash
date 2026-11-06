//! CC-Flash · Tauri 桌面壳
//! 职责:托盘(关窗收到托盘,服务不停) + 启动时自动拉起局域网文件服务。

use tauri::{Emitter, Manager, WebviewUrl};
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

#[derive(Clone, serde::Serialize)]
struct MobileMode(bool);

/// 唤出主窗口的统一实现(托盘菜单 / 单实例重复启动 / macOS 点程序坞图标 三条路径共用):
/// 必须先 unminimize 再 show —— show() 只置可见, 不会把最小化的窗口还原。
/// 同时同步主窗查看状态，最小化或切到别的应用后仍需提醒新内容。
#[cfg(desktop)]
fn show_main_window(app: &tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
        emit_main_window_state(app);
    }
}

fn main_window_state(app: &tauri::AppHandle) -> serde_json::Value {
    let (visible, minimized, focused) = app.get_webview_window("main")
        .map(|w| (
            w.is_visible().unwrap_or(false),
            w.is_minimized().unwrap_or(false),
            w.is_focused().unwrap_or(false),
        ))
        .unwrap_or((false, false, false));
    serde_json::json!({
        "ok": true, "visible": visible, "minimized": minimized, "focused": focused,
        "active": visible && !minimized && focused,
    })
}

#[cfg(desktop)]
fn emit_main_window_state(app: &tauri::AppHandle) {
    let _ = app.emit("main-vis://changed", main_window_state(app));
}

/// 关窗 = 收到托盘(服务继续运行);托盘菜单「退出」才真正结束进程。
#[cfg(desktop)]
fn main_window_event(window: &tauri::WebviewWindow, event: &tauri::WindowEvent) {
    if let tauri::WindowEvent::CloseRequested { api, .. } = event {
        api.prevent_close();
        let _ = window.hide();
        emit_main_window_state(window.app_handle());
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
    } else if matches!(event, tauri::WindowEvent::Focused(_) | tauri::WindowEvent::Resized(_)) {
        // 不能只在 show/hide 时广播：后台窗口通常仍然 is_visible() == true。
        emit_main_window_state(window.app_handle());
    }
}

pub mod commands {
    use super::*;

    #[tauri::command]
    pub fn show_main_window_cmd(app: tauri::AppHandle) -> serde_json::Value {
        #[cfg(desktop)]
        {
            show_main_window(&app);
        }
        #[cfg(not(desktop))]
        let _ = app;
        serde_json::json!({ "ok": true })
    }

    #[tauri::command]
    pub fn show_qr_pet_cmd(app: tauri::AppHandle, show: bool) -> serde_json::Value {
        if let Some(pet) = app.get_webview_window("qr-pet") {
            if show { let _ = pet.show(); } else { let _ = pet.hide(); }
            serde_json::json!({ "ok": true })
        } else {
            serde_json::json!({ "ok": false, "error": "Pet 窗口不存在" })
        }
    }

    /// 悬浮码吸附记忆持久化: 落盘到 app_config_dir/pet-dock.json。
    /// 不用 webview localStorage —— WKWebView 的写入在本环境长期只驻内存不落 sqlite,
    /// 重启即丢(实测连 mount 时的默认位保存都没进磁盘), 位置记忆必须走 Rust 同步写文件。
    #[tauri::command]
    pub fn save_pet_dock_cmd(app: tauri::AppHandle, dock: String) -> serde_json::Value {
        // 结构由前端控制({edge,cross}), 限长防呆
        if dock.len() > 512 {
            return serde_json::json!({ "ok": false, "error": "dock 数据异常" });
        }
        match app.path().app_config_dir() {
            Ok(dir) => {
                let _ = std::fs::create_dir_all(&dir);
                match std::fs::write(dir.join("pet-dock.json"), dock) {
                    Ok(_) => serde_json::json!({ "ok": true }),
                    Err(e) => serde_json::json!({ "ok": false, "error": e.to_string() }),
                }
            }
            Err(e) => serde_json::json!({ "ok": false, "error": e.to_string() }),
        }
    }

    #[tauri::command]
    pub fn load_pet_dock_cmd(app: tauri::AppHandle) -> serde_json::Value {
        let path = app
            .path()
            .app_config_dir()
            .map(|d| d.join("pet-dock.json"));
        match path {
            Ok(p) => match std::fs::read_to_string(p) {
                Ok(s) => serde_json::json!({ "ok": true, "dock": s }),
                // 文件不存在 = 首次运行, 前端走默认左侧中部
                Err(_) => serde_json::json!({ "ok": true, "dock": serde_json::Value::Null }),
            },
            Err(e) => serde_json::json!({ "ok": false, "error": e.to_string(), "dock": serde_json::Value::Null }),
        }
    }

    fn pref_path(app: &tauri::AppHandle, key: &str) -> Result<std::path::PathBuf, serde_json::Value> {
        let valid = !key.is_empty()
            && key.len() <= 40
            && key.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
        if !valid {
            return Err(serde_json::json!({ "ok": false, "error": "非法 pref key" }));
        }
        app.path()
            .app_config_dir()
            .map(|d| d.join(format!("pref-{key}")))
            .map_err(|e| serde_json::json!({ "ok": false, "error": e.to_string() }))
    }

    /// 通用文本偏好持久化(app_config_dir/pref-<key>, 同步落盘)。
    /// 动因同 save_pet_dock_cmd: WKWebView localStorage 写入不落盘, 悬浮码开关等
    /// "重启后仍保持"的状态必须走 Rust 文件, localStorage 只作会话内缓存。
    #[tauri::command]
    pub fn save_text_pref_cmd(app: tauri::AppHandle, key: String, value: String) -> serde_json::Value {
        // 上限与前端 petDiag 的 slice(0,480) 对齐 —— 旧的 256 会静默拒掉
        // 带 rect/wa 的 snap 诊断(前端 catch 吞掉, 表现为"诊断文件里少了一条")
        if value.len() > 512 {
            return serde_json::json!({ "ok": false, "error": "value 过长" });
        }
        match pref_path(&app, &key) {
            Ok(p) => match std::fs::write(p, value) {
                Ok(_) => serde_json::json!({ "ok": true }),
                Err(e) => serde_json::json!({ "ok": false, "error": e.to_string() }),
            },
            Err(e) => e,
        }
    }

    /// 主窗实际查看状态：可见、未最小化、拥有焦点才清除提醒。
    #[tauri::command]
    pub fn main_visible_cmd(app: tauri::AppHandle) -> serde_json::Value {
        main_window_state(&app)
    }

    #[tauri::command]
    pub fn load_text_pref_cmd(app: tauri::AppHandle, key: String) -> serde_json::Value {
        match pref_path(&app, &key) {
            Ok(p) => match std::fs::read_to_string(p) {
                Ok(v) => serde_json::json!({ "ok": true, "value": v }),
                // 文件不存在 = 从未设置过, 前端用默认值
                Err(_) => serde_json::json!({ "ok": true, "value": serde_json::Value::Null }),
            },
            Err(e) => e,
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
            server::set_access_pin_cmd,
            server::set_open_lan_cmd,
            server::open_path_cmd,
            server::open_url_cmd,
            server::copy_file_cmd,
            commands::show_main_window_cmd,
            commands::show_qr_pet_cmd,
            commands::save_pet_dock_cmd,
            commands::load_pet_dock_cmd,
            commands::save_text_pref_cmd,
            commands::load_text_pref_cmd,
            commands::main_visible_cmd
        ])
        .manage(MobileMode(std::env::consts::OS == "android" || std::env::consts::OS == "ios"));
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
                // 注入的是 app_data_dir 本身(父目录): 共享目录与目录偏好文件分置其下
                // (share=父/CC-Flash, pref=父/.ccflash-share)。若直接注入 share,
                // 偏好文件会落进共享目录内部, 被 HTTP 列表当普通文件展示/下载/删除
                if let Ok(dir) = app.path().app_data_dir() {
                    let _ = std::fs::create_dir_all(dir.join(config::SHARE_DIR_NAME));
                    server::set_platform_default_dir(dir.to_string_lossy().to_string());
                }
            }
            // ---- 主窗口(代码创建以挂下载处理器): 配置文件不再定义窗口 ----
            // 移动壳的"下载"按钮依赖此处理器(WebView 锚点下载默认无操作);
            // 统一落盘到系统下载目录, 完成后事件通知前端 toast
            let app_handle = app.handle();
    // 桌面端常驻二维码：独立置顶无边框小窗，真正悬浮在系统桌面与其他应用之上。
    // 收拢态窗口 = 96×96 逻辑px(64 呼吸圆点 + 四周 16px 透明缓冲, 与前端 PET_ORB 一致);
    // 悬停展开扫码卡由前端 setSize 到 136×190, min 必须同步 96 否则 set_size 被夹紧失效。
    #[cfg(desktop)]
    {
        let show_qr = std::env::args().any(|a| a == "--show-qr");
        let qr = tauri::WebviewWindowBuilder::new(
            app_handle,
            "qr-pet",
            WebviewUrl::App("index.html?view=qr-pet".into()),
        )
        .title("CC-Flash 扫码")
        .inner_size(96.0, 96.0)
        .decorations(false)
        .transparent(true)
        .always_on_top(true)
        .skip_taskbar(true)
        .resizable(true)
        .min_inner_size(96.0, 96.0)
        // 悬浮窗几乎总是非焦点态: macOS 首点默认只用于激活窗口、不传给 webview,
        // 导致 Pet 的拖动/点按/按钮全部失灵 —— 必须开启首点穿透
        .accept_first_mouse(true)
        .visible(show_qr)
        .build()?;
        qr.on_window_event(move |event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
            }
        });
        // 默认停靠: 屏幕左侧、窗口中心压工作区边缘线 —— 圆点漏一半在屏内(贴边收拢态)。
        // 前端 Pet 挂载后按 localStorage 记忆(wft-qr-pet-dock)覆盖此位置。
        if let Ok(Some(mon)) = qr.primary_monitor() {
            let sf = qr.scale_factor().unwrap_or(1.0);
            let orb = (96.0 * sf).round() as i32;
            let wa = mon.work_area();
            let x = wa.position.x - orb / 2;   // 球心在左边缘线 → 左半出屏
            let y = wa.position.y + ((wa.size.height as i32 - orb) / 2).max(0);
            let _ = qr.set_position(tauri::PhysicalPosition::new(x, y));
        }
        if show_qr {
            let _ = qr.show();
            let _ = qr.set_focus();
        }
    }
            tauri::WebviewWindowBuilder::new(app_handle, "main", WebviewUrl::App("index.html".into()))
                .title(config::APP_NAME)
                .inner_size(1180.0, 840.0)
                .min_inner_size(560.0, 520.0)
                .visible(false)
                .background_color(tauri::webview::Color(13, 16, 23, 255))
                // macOS 首点默认只用于激活窗口、不传给 webview —— 从别的应用切回来后
                // 第一击凭空消失(表现为"按钮要点两次"), 必须开启首点穿透
                .accept_first_mouse(true)
                // 前端拖拽上传走 HTML5 API, Windows 上必须关掉内部拖拽 handler
                .disable_drag_drop_handler()
                .on_download(move |webview, event| match event {
                    tauri::webview::DownloadEvent::Requested { url, destination } => {
                        // 文件名三道防线: 先取 URL 末段 → 百分号解码 → 再取末段。
                        // 解码可能还原出 / \ (如 %2F), join 后会逃出下载目录;
                        // 空段(路径以 / 结尾)与 .. 同样兜底为 download
                        let raw = url.path().rsplit('/').next().unwrap_or("download");
                        let decoded = percent_encoding::percent_decode_str(raw)
                            .decode_utf8_lossy()
                            .to_string();
                        let fname = decoded
                            .rsplit(['/', '\\'])
                            .next()
                            .unwrap_or("download")
                            .to_string();
                        let fname = if fname.is_empty() || fname == ".." {
                            "download".to_string()
                        } else {
                            fname
                        };
                        let dir = match webview.app_handle().path().download_dir() {
                            Ok(d) => d,
                            // 移动端无系统下载目录概念时退回应用专属目录
                            Err(_) => match webview.app_handle().path().app_data_dir() {
                                Ok(d) => d.join("downloads"),
                                Err(_) => return false,
                            },
                        };
                        let _ = std::fs::create_dir_all(&dir);
                        // 同名不静默覆盖: stem(1).ext 顺延(审查回归 —— 原实现直接 join,
                        // Downloads 里的同名旧文件可能被无声替换)
                        let mut target = dir.join(&fname);
                        if target.exists() {
                            let (stem, ext) = match fname.rsplit_once('.') {
                                Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
                                _ => (fname.clone(), String::new()),
                            };
                            for i in 1..10000u32 {
                                let cand = dir.join(format!("{stem}({i}){ext}"));
                                if !cand.exists() {
                                    target = cand;
                                    break;
                                }
                            }
                        }
                        *destination = target;
                        true
                    }
                    tauri::webview::DownloadEvent::Finished { path, success, .. } => {
                        let saved = path.as_deref().map(|p| p.to_string_lossy()).unwrap_or_default();
                        let _ = webview.emit(
                            "download://done",
                            serde_json::json!({ "success": success, "path": saved }),
                        );
                        true
                    }
                    _ => true,
                })
                .build()
                .map_err(|e| format!("创建主窗口失败: {e}"))?;
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
                            // 回放用户最近指定的端口: start(None) 会漂移回默认 1688,
                            // 已复制给手机的地址/二维码静默失效
                            let _ = server::start(server::last_preferred_port());
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

            // Pet 窗关闭只隐藏：主窗口托盘化后，独立扫码 Pet 仍留在桌面。
            // 同时广播开关关闭 —— 否则前端 qrFloatEnabled 仍为 true，主窗状态轮询
            // 2.5s 内会把 Pet 重新拉起，且 CC设置开关/侧栏恢复入口不同步(与 ✕ 同一问题)。
            if let Some(pet) = app.get_webview_window("qr-pet") {
                let pet2 = pet.clone();
                pet.on_window_event(move |event| {
                    if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                        api.prevent_close();
                        let _ = pet2.hide();
                        let _ = pet2.emit(
                            "qr-float://changed",
                            serde_json::json!({ "enabled": false }),
                        );
                        // 同步落盘开关(只广播+localStorage 的话重启会按 pref 复活,
                        // 违背"重启后仍保持关闭"; 设置页路径本来就双写)
                        if let Ok(dir) = pet2.app_handle().path().app_config_dir() {
                            let _ = std::fs::write(dir.join("pref-qr-float-enabled"), "0");
                        }
                    }
                });
            }
            } // #[cfg(desktop)]

            // ---- 自动拉起局域网服务(端口被占用自动 +1 重试) ----
            // Android/iOS 壳只是“访问电脑服务”的客户端形态，不应把手机本机目录
            // 暴露到所属 Wi-Fi。服务托管与共享目录控制只在真实桌面启用。
            #[cfg(not(any(target_os = "android", target_os = "ios")))]
            match server::autostart() {
                Ok(port) => eprintln!("[wft] 服务已启动: http://{}:{port}", server::lan_ip()),
                Err(e) => eprintln!("[wft] 服务启动失败: {e}"),
            }
            #[cfg(any(target_os = "android", target_os = "ios"))]
            eprintln!("[wft] 移动壳模式：本机局域网服务保持关闭");
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

