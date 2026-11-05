mod protocol;

use protocol::{Battery, Buds, PortInfo};
use serde::Serialize;
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::{
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Emitter, Manager, State, WebviewUrl, WebviewWindowBuilder, WindowEvent,
};

/// Latest known status, kept in memory. Every Tauri command reads this
/// instantly; NO serial I/O ever runs on the UI thread.
#[derive(Debug, Clone, Serialize)]
pub struct Status {
    connected: bool,
    port: Option<String>,
    mode: u8,
    battery: Option<Battery>,
    message: String,
}

impl Status {
    fn offline() -> Self {
        Status {
            connected: false,
            port: None,
            mode: 0,
            battery: None,
            message: "离线 · 耳机未连接".into(),
        }
    }
}

struct SharedStatus(Arc<Mutex<Status>>);

/// Requests sent to the single serial-owning worker thread.
enum Request {
    /// One poll cycle (also handles reconnect detection + popup).
    Poll,
    /// User changed the spatial-audio mode.
    SetMode(u8),
    /// User clicked "preview popup".
    Preview,
}

struct WorkerTx(Mutex<Sender<Request>>);

/// Try to find the earbuds. Every candidate is opened, probed, and CLOSED
/// immediately (RAII), so no COM handle ever survives this function.
fn find_earbuds(override_port: Option<&str>, quick: bool) -> Result<(String, Buds), protocol::Error> {
    let mut candidates: Vec<String> = Vec::new();
    if let Some(p) = override_port {
        if !p.is_empty() {
            candidates.push(p.to_string());
        }
    }
    for p in protocol::bluetooth_ports() {
        if !candidates.contains(&p.name) {
            candidates.push(p.name);
        }
    }
    if candidates.is_empty() {
        return Err(protocol::Error::Unavailable("没找到蓝牙虚拟串口".into()));
    }

    let mut last_err = protocol::Error::NoReply;
    for name in &candidates {
        eprintln!("[find] try {} quick={}", name, quick);
        match Buds::open(name) {
            Ok(mut buds) => match buds.read_battery_quick(quick) {
                Ok(_) => {
                    eprintln!("[find] {} OK", name);
                    let _ = buds.handshake();
                    return Ok((name.clone(), buds));
                }
                Err(e) => {
                    eprintln!("[find] {} read fail: {:?}", name, e);
                    last_err = e;
                }
            },
            Err(e) => {
                eprintln!("[find] {} open fail: {:?}", name, e);
                last_err = e;
            }
        }
    }
    eprintln!("[find] all failed: {:?}", last_err);
    Err(last_err)
}

/// Full status read (mode + battery). Called only from the worker thread.
fn full_status(cached_port: &mut Option<String>, quick: bool) -> Status {
    let preferred = cached_port.clone();
    let found = match preferred.as_deref() {
        Some(p) => {
            eprintln!("[full] try cached {} quick={}", p, quick);
            match Buds::open(p) {
                Ok(mut buds) => match buds.read_battery_quick(quick) {
                    Ok(_) => {
                        eprintln!("[full] cached {} OK", p);
                        let _ = buds.handshake();
                        Some((p.to_string(), buds))
                    }
                    Err(e) => {
                        eprintln!("[full] cached {} read fail: {:?}", p, e);
                        None
                    }
                },
                Err(e) => {
                    eprintln!("[full] cached {} open fail: {:?}", p, e);
                    None
                }
            }
        }
        None => None,
    };
    let found = match found {
        Some(v) => Ok(v),
        None => find_earbuds(None, quick),
    };

    match found {
        Ok((port, mut buds)) => {
            *cached_port = Some(port.clone());
            if quick {
                return Status {
                    connected: true,
                    port: Some(port),
                    mode: 0,
                    battery: None,
                    message: "已连接".into(),
                };
            }
            let mode = buds.read_spatial().unwrap_or(0);
            let battery = buds.read_battery().ok();
            Status {
                connected: true,
                port: Some(port),
                mode,
                battery,
                message: "已连接".into(),
            }
        }
        // Any low-level serial error while disconnected is just "offline".
        Err(_) => {
            *cached_port = None;
            Status::offline()
        }
    }
}

#[tauri::command]
fn list_ports() -> Vec<PortInfo> {
    protocol::bluetooth_ports()
}

/// Instant: returns the cached snapshot, never touches the COM port.
#[tauri::command]
fn get_status(shared: State<'_, SharedStatus>) -> Status {
    shared.0.lock().unwrap().clone()
}

/// Instant: queues a write request; the result arrives via `status-changed`.
#[tauri::command]
fn set_spatial(tx: State<'_, WorkerTx>, mode: u8) -> Status {
    if mode > 2 {
        return Status {
            connected: false,
            port: None,
            mode: 0,
            battery: None,
            message: "无效模式".into(),
        };
    }
    let _ = tx.0.lock().unwrap().send(Request::SetMode(mode));
    // Optimistic: the worker will emit the verified status shortly.
    Status {
        connected: true,
        port: None,
        mode,
        battery: None,
        message: "写入中…".into(),
    }
}

#[tauri::command]
fn quit_app(app: AppHandle) {
    app.exit(0);
}

#[tauri::command]
fn open_bluetooth_settings() {
    let _ = std::process::Command::new("cmd")
        .args(["/C", "start", "ms-settings:bluetooth"])
        .spawn();
}

/// Destroy the reconnect popup. `close()` only hides it in some Tauri builds
/// and `get_webview_window("popup")` keeps returning the stale window.
#[tauri::command]
fn dismiss_popup(app: AppHandle) {
    if let Some(w) = app.get_webview_window("popup") {
        let _ = w.destroy();
    }
}

#[tauri::command]
fn preview_popup(tx: State<'_, WorkerTx>) {
    let _ = tx.0.lock().unwrap().send(Request::Preview);
}

/// Center a window on the primary monitor's work area using Win32 physical
/// pixels. tao's monitor size can report the panel's native mode (e.g.
/// 3840x2160) instead of the current desktop resolution (2560x1440), which
/// pushes a "centered" window far to the right.
#[cfg(windows)]
fn center_on_work_area(win: &tauri::WebviewWindow, w: f64, h: f64) {
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::UI::WindowsAndMessaging::{SystemParametersInfoW, SPI_GETWORKAREA};

    let mut rect = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    let ok = unsafe {
        SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            &mut rect as *mut RECT as *mut std::ffi::c_void,
            0,
        )
    };
    if ok == 0 {
        return;
    }
    let scale = win.scale_factor().unwrap_or(1.0);
    let w_phys = (w * scale) as i32;
    let h_phys = (h * scale) as i32;
    let x = rect.left + ((rect.right - rect.left) - w_phys) / 2;
    let y = rect.top + ((rect.bottom - rect.top) - h_phys) / 2;
    eprintln!(
        "[popup] centering: work=({}, {})-({}, {}) scale={} -> pos=({}, {})",
        rect.left, rect.top, rect.right, rect.bottom, scale, x, y
    );
    let _ = win.set_position(tauri::PhysicalPosition::new(x, y));
}

/// Create the transparent, always-on-top popup window at the right side,
/// mirroring Huawei's "reconnected popup" (回连弹窗).
fn spawn_popup(app: &AppHandle, status: &Status) {
    eprintln!("[popup] spawn_popup called, connected={}", status.connected);
    if let Some(w) = app.get_webview_window("popup") {
        if w.is_visible().unwrap_or(false) {
            eprintln!("[popup] existing popup visible, re-emit");
            let _ = w.emit("popup-status", status.clone());
            let _ = w.set_focus();
            return;
        }
        eprintln!("[popup] destroying stale popup");
        let _ = w.destroy();
    }

    let w = 252.0;
    let h = 420.0;

    eprintln!("[popup] building centered window {}x{}", w, h);
    let builder = WebviewWindowBuilder::new(app, "popup", WebviewUrl::App("popup.html".into()))
        .title("FreeClip 2")
        .inner_size(w, h)
        .resizable(false)
        .decorations(false)
        .transparent(true)
        .always_on_top(true)
        .skip_taskbar(true)
        .visible(true)
        .shadow(false);

    match builder.build() {
        Ok(win) => {
            #[cfg(windows)]
            center_on_work_area(&win, w, h);
            let _ = win.show();
            let _ = win.set_focus();
            eprintln!("[popup] window shown OK");

            let st = status.clone();
            let app2 = app.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(350));
                eprintln!("[popup] emitting popup-status");
                let _ = app2.emit("popup-status", &st);
                if let Some(w) = app2.get_webview_window("popup") {
                    let _ = w.emit("popup-status", st);
                }
                std::thread::sleep(Duration::from_millis(8500));
                eprintln!("[popup] auto-destroy after 8.5s");
                if let Some(w) = app2.get_webview_window("popup") {
                    let _ = w.destroy();
                }
                let _ = win;
            });
        }
        Err(e) => {
            eprintln!("[popup] build FAILED: {:?}", e);
        }
    }
}

/// The single owner of the COM port. Runs entirely on a background thread:
///   - polls every 2s (cheap cached-port probe)
///   - handles SetMode / Preview requests from the UI
///   - emits `status-changed` on every change
///   - fires the popup on a stable disconnected -> connected edge
fn start_worker(app: AppHandle, shared: Arc<Mutex<Status>>, rx: std::sync::mpsc::Receiver<Request>) {
    std::thread::spawn(move || {
        let mut cached_port: Option<String> = None;
        let mut was_connected = false;
        let mut connected_streak: u32 = 0;
        let mut fired_for_this_edge = false;
        let mut popup_dismissed = false;
        let mut last_full_poll = std::time::Instant::now() - Duration::from_secs(60);

        eprintln!("[worker] started");
        loop {
            // Drain pending UI requests without blocking.
            while let Ok(req) = rx.try_recv() {
                match req {
                    Request::SetMode(mode) => {
                        let s = match find_earbuds(cached_port.as_deref(), false) {
                            Ok((port, mut buds)) => {
                                cached_port = Some(port.clone());
                                let ok = buds.write_spatial(mode).is_ok();
                                let confirmed = buds.read_spatial().unwrap_or(mode);
                                let battery = buds.read_battery().ok();
                                Status {
                                    connected: ok,
                                    port: Some(port),
                                    mode: confirmed,
                                    battery,
                                    message: if ok && confirmed == mode {
                                        "已应用".into()
                                    } else if !ok {
                                        "写入失败，请重试".into()
                                    } else {
                                        "写入后回读不一致，请重试".into()
                                    },
                                }
                            }
                            Err(_) => Status::offline(),
                        };
                        *shared.lock().unwrap() = s.clone();
                        let _ = app.emit("status-changed", &s);
                    }
                    Request::Preview => {
                        let mut s = shared.lock().unwrap().clone();
                        if !s.connected {
                            s = Status {
                                connected: true,
                                port: Some("预览模式".into()),
                                mode: 0,
                                battery: Some(Battery { left: 100, right: 100, case: 62 }),
                                message: "预览".into(),
                            };
                        }
                        spawn_popup(&app, &s);
                    }
                    Request::Poll => {}
                }
            }

            // Cheap poll: only the cached port, short timeout.
            let probe = full_status(&mut cached_port, true);
            let now = probe.connected;
            eprintln!("[worker] now={} cached={:?} fired={} streak={} changed={}",
                now, cached_port, fired_for_this_edge, connected_streak, now != was_connected);

            if now {
                connected_streak += 1;
                popup_dismissed = false;
            } else {
                connected_streak = 0;
                fired_for_this_edge = false;
                // Do NOT touch windows from this worker thread: a cross-thread
                // destroy() can block the loop. The popup self-dismisses.
                popup_dismissed = true;
            }

            let changed = now != was_connected;
            // Fire as soon as a connection is confirmed (one sample is enough:
            // a half-created BTHENUM port fails at Buds::open, not as a false
            // positive). Fetch full status, show the popup once, push to UI.
            if now && connected_streak >= 1 && !fired_for_this_edge {
                fired_for_this_edge = true;
                eprintln!("[worker] >>> SPAWN POPUP");
                let full = full_status(&mut cached_port, false);
                *shared.lock().unwrap() = full.clone();
                spawn_popup(&app, &full);
                let _ = app.emit("status-changed", &full);
                last_full_poll = std::time::Instant::now();
            } else if changed || last_full_poll.elapsed() > Duration::from_secs(15) {
                // Full read on state change, and periodically for battery.
                let full = full_status(&mut cached_port, false);
                *shared.lock().unwrap() = full.clone();
                let _ = app.emit("status-changed", &full);
                last_full_poll = std::time::Instant::now();
            } else if !now {
                *shared.lock().unwrap() = Status::offline();
            }

            if changed {
                was_connected = now;
            }
            std::thread::sleep(Duration::from_millis(2000));
            eprintln!("[worker] --- cycle end ---");
        }
    });
}

fn show_window(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.set_focus();
    }
}

fn apply_mode(app: &AppHandle, mode: u8) {
    if let Some(tx) = app.try_state::<WorkerTx>() {
        let _ = tx.0.lock().unwrap().send(Request::SetMode(mode));
    }
    show_window(app);
}

pub fn run() {
    tauri::Builder::default()
        // MUST be the first plugin: any second launch is forwarded here and
        // the process exits, so no second COM handle can ever be opened.
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            show_window(app);
        }))
        .plugin(tauri_plugin_autostart::Builder::new().build())
        .setup(|app| {
            let handle = app.handle().clone();

            let shared = Arc::new(Mutex::new(Status::offline()));
            app.manage(SharedStatus(shared.clone()));
            let (tx, rx) = channel::<Request>();
            app.manage(WorkerTx(Mutex::new(tx)));

            let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
            let show = MenuItem::with_id(app, "show", "显示窗口", true, None::<&str>)?;
            let m_off = MenuItem::with_id(app, "mode_off", "空间音频：关闭", true, None::<&str>)?;
            let m_head = MenuItem::with_id(app, "mode_head", "空间音频：头部跟踪", true, None::<&str>)?;
            let m_fixed = MenuItem::with_id(app, "mode_fixed", "空间音频：固定声场", true, None::<&str>)?;
            let sep = PredefinedMenuItem::separator(app)?;
            let menu = Menu::with_items(app, &[&m_off, &m_head, &m_fixed, &sep, &show, &quit])?;

            let tray = TrayIconBuilder::new()
                .icon(app.default_window_icon().unwrap().clone())
                .tooltip("FreeClip 2 空间音频")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        show_window(tray.app_handle());
                    }
                })
                .on_menu_event(|app, event| match event.id().as_ref() {
                    "quit" => app.exit(0),
                    "show" => show_window(app),
                    "mode_off" => apply_mode(app, 0),
                    "mode_head" => apply_mode(app, 1),
                    "mode_fixed" => apply_mode(app, 2),
                    _ => {}
                })
                .build(app)?;
            let _ = tray;

            if let Some(window) = app.get_webview_window("main") {
                let h = handle.clone();
                window.on_window_event(move |e| {
                    if let WindowEvent::CloseRequested { api, .. } = e {
                        api.prevent_close();
                        let _ = h.get_webview_window("main").map(|w| w.hide());
                    }
                });
            }

            let argv: Vec<String> = std::env::args().collect();
            if !argv.iter().any(|a| a == "--minimized") {
                show_window(&handle);
            }

            start_worker(handle.clone(), shared, rx);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            list_ports,
            get_status,
            set_spatial,
            open_bluetooth_settings,
            quit_app,
            dismiss_popup,
            preview_popup
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
