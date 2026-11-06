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
    /// The Bluetooth serial port device object is dead: it is still listed by
    /// PnP but refuses to open. Distinct from "earbuds are switched off",
    /// where the port disappears from enumeration entirely.
    needs_repair: bool,
}

impl Status {
    fn offline() -> Self {
        Status {
            connected: false,
            port: None,
            mode: 0,
            battery: None,
            message: "离线 · 耳机未连接".into(),
            needs_repair: false,
        }
    }

    fn broken() -> Self {
        Status {
            connected: false,
            port: None,
            mode: 0,
            battery: None,
            message: "蓝牙串口异常 · 点击修复".into(),
            needs_repair: true,
        }
    }

    fn repairing() -> Self {
        Status {
            connected: false,
            port: None,
            mode: 0,
            battery: None,
            message: "正在重启蓝牙，请稍候…".into(),
            needs_repair: true,
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

/// Result of a probe round.
struct ProbeFail {
    err: protocol::Error,
    /// At least one port that Windows still lists could not be opened. That is
    /// the signature of a dead BTHENUM device object (ERROR_NO_SUCH_DEVICE /
    /// ERROR_SEM_TIMEOUT), which a Bluetooth radio restart fixes.
    port_dead: bool,
}

impl From<ProbeFail> for Status {
    fn from(f: ProbeFail) -> Self {
        if f.port_dead {
            Status::broken()
        } else {
            Status::offline()
        }
    }
}

/// Try to find the earbuds. Every candidate is opened, probed, and CLOSED
/// immediately (RAII), so no COM handle ever survives this function.
fn find_earbuds(override_port: Option<&str>, quick: bool) -> Result<(String, Buds), ProbeFail> {
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
        return Err(ProbeFail {
            err: protocol::Error::Unavailable("没找到蓝牙虚拟串口".into()),
            port_dead: false,
        });
    }

    let mut last_err = protocol::Error::NoReply;
    let mut port_dead = false;
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
                // The port is listed but the device object is gone/refusing.
                port_dead = true;
                last_err = e;
            }
        }
    }
    eprintln!("[find] all failed: {:?} (port_dead={})", last_err, port_dead);
    Err(ProbeFail { err: last_err, port_dead })
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
                    needs_repair: false,
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
                needs_repair: false,
            }
        }
        // A dead port object gets its own state so the UI can offer a repair;
        // a genuinely absent device is plain "offline".
        Err(fail) => {
            *cached_port = None;
            Status::from(fail)
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
            needs_repair: false,
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
        needs_repair: false,
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

/// Called by the popup page after its first paint. Until this fires the window
/// is parked below the screen with a blank surface, so starting the entrance
/// earlier would slide an empty rectangle into view.
#[tauri::command]
fn popup_ready(app: AppHandle) {
    eprintln!("[popup] page ready at {}ms", popup_ms());
    // Deliver the status the popup was opened with. The page's listener only
    // exists now, so anything emitted earlier was dropped.
    if let Some(w) = app.get_webview_window("popup") {
        if let Some(s) = POPUP_PENDING.lock().unwrap().take() {
            let _ = w.emit("popup-status", s);
        }
    }
    if POPUP_SLIDE_STARTED.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return; // already started (watchdog beat us, or a duplicate call)
    }
    #[cfg(windows)]
    if let Some(w) = app.get_webview_window("popup") {
        begin_popup_slide(&w);
    }
}

/// Exit animation: slide the popup back down off the bottom edge while the
/// page fades the card out, then `dismiss_popup` destroys the window.
#[cfg(windows)]
fn slide_popup_down(win: tauri::WebviewWindow) {
    std::thread::spawn(move || {
        let Ok(pos) = win.outer_position() else { return };
        let to_y = work_area().map(|r| r.3 + 8).unwrap_or(pos.y + 600);
        const STEPS: i32 = 16;
        const STEP_MS: u64 = 16;
        for i in 1..=STEPS {
            // ease-in quadratic: hangs for a beat, then drops away
            let t = i as f64 / STEPS as f64;
            let eased = t * t;
            let y = pos.y + ((to_y - pos.y) as f64 * eased) as i32;
            let _ = win.set_position(tauri::PhysicalPosition::new(pos.x, y));
            std::thread::sleep(Duration::from_millis(STEP_MS));
        }
    });
}

#[tauri::command]
fn slide_popup_out(app: AppHandle) {
    #[cfg(windows)]
    if let Some(w) = app.get_webview_window("popup") {
        slide_popup_down(w);
    }
    #[cfg(not(windows))]
    let _ = app;
}

/// Turn the Bluetooth radio off and back on. This forces Windows to
/// re-enumerate every BTHENUM child device, which revives a COM port whose
/// device object died while PnP still reports it as OK.
///
/// Implemented through PowerShell's WinRT bridge: the radio API is only a few
/// lines there, whereas calling IAsyncOperation directly from Rust would drag
/// in an async runtime for no benefit. Runs only on explicit user request.
#[cfg(windows)]
fn restart_bluetooth_radio() -> Result<(), String> {
    let script = r#"
Add-Type -AssemblyName System.Runtime.WindowsRuntime
$g = ([System.WindowsRuntimeSystemExtensions].GetMethods() | Where-Object {
    $_.Name -eq 'AsTask' -and $_.GetParameters().Count -eq 1 -and
    $_.GetParameters()[0].ParameterType.Name -eq 'IAsyncOperation`1'
})[0]
function Await($op, $t) {
    $task = $g.MakeGenericMethod($t).Invoke($null, @($op))
    $task.Wait(-1) | Out-Null
    $task.Result
}
[Windows.Devices.Radios.Radio, Windows.System.Devices, ContentType=WindowsRuntime] | Out-Null
$radios = Await ([Windows.Devices.Radios.Radio]::GetRadiosAsync()) ([System.Collections.Generic.IReadOnlyList[Windows.Devices.Radios.Radio]])
foreach ($r in $radios) {
    if ($r.Kind -eq 'Bluetooth') {
        Await ($r.SetStateAsync('Off')) ([Windows.Devices.Radios.RadioAccessStatus]) | Out-Null
        Start-Sleep -Milliseconds 3500
        Await ($r.SetStateAsync('On')) ([Windows.Devices.Radios.RadioAccessStatus]) | Out-Null
        exit 0
    }
}
exit 1
"#;
    let out = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", script])
        .output()
        .map_err(|e| format!("无法启动 PowerShell: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "蓝牙重启失败: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// User-triggered repair: restart the radio, then let the worker re-probe.
/// Returns immediately; the result arrives through `status-changed`.
#[tauri::command]
fn repair_bluetooth(app: AppHandle, shared: State<'_, SharedStatus>) -> Status {
    let shared = shared.0.clone();
    let status = Status::repairing();
    *shared.lock().unwrap() = status.clone();
    let _ = app.emit("status-changed", &status);

    let app2 = app.clone();
    std::thread::spawn(move || {
        let result = restart_bluetooth_radio();
        match result {
            Ok(()) => {
                eprintln!("[repair] radio restarted");
                // Give the stack a moment to re-enumerate BTHENUM children.
                std::thread::sleep(Duration::from_millis(1500));
                let s = Status {
                    connected: false,
                    port: None,
                    mode: 0,
                    battery: None,
                    message: "蓝牙已重启，正在重新检测…".into(),
                    needs_repair: false,
                };
                *shared.lock().unwrap() = s.clone();
                let _ = app2.emit("status-changed", &s);
            }
            Err(e) => {
                eprintln!("[repair] failed: {e}");
                let s = Status {
                    connected: false,
                    port: None,
                    mode: 0,
                    battery: None,
                    message: format!("修复失败：{e}"),
                    needs_repair: true,
                };
                *shared.lock().unwrap() = s.clone();
                let _ = app2.emit("status-changed", &s);
            }
        }
    });

    status
}

/// Primary monitor work area in physical pixels. tao's monitor size can report
/// the panel's native mode (e.g. 3840x2160) instead of the current desktop
/// resolution (2560x1440), which would misplace a "centered" window.
#[cfg(windows)]
fn work_area() -> Option<(i32, i32, i32, i32)> {
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
        None
    } else {
        Some((rect.left, rect.top, rect.right, rect.bottom))
    }
}

const POPUP_W: f64 = 252.0;
const POPUP_H: f64 = 420.0;

/// Set once the entrance slide has been kicked off for the current popup,
/// so `popup_ready` and the watchdog can't both start it.
static POPUP_SLIDE_STARTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Timestamp of the current popup's creation, for the slide diagnostics.
static POPUP_T0: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);

fn popup_ms() -> u128 {
    POPUP_T0
        .lock()
        .unwrap()
        .map(|t| t.elapsed().as_millis())
        .unwrap_or(0)
}

/// Status the popup was opened with, held until the page reports it is ready
/// to receive events (its listener does not exist before that).
static POPUP_PENDING: std::sync::Mutex<Option<Status>> = std::sync::Mutex::new(None);

/// Target position of the popup: horizontally centered, vertically centered in
/// the work area. Also returns the "below the screen" start position used for
/// the slide-up entrance.
#[cfg(windows)]
fn popup_positions(win: &tauri::WebviewWindow, w: f64, h: f64) -> Option<((i32, i32), (i32, i32))> {
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CYSCREEN};

    let (left, top, right, bottom) = work_area()?;
    let scale = win.scale_factor().unwrap_or(1.0);
    let w_phys = (w * scale) as i32;
    let h_phys = (h * scale) as i32;
    let x = left + ((right - left) - w_phys) / 2;
    let y = top + ((bottom - top) - h_phys) / 2;
    // Start completely below the *panel*, not merely below the work area: the
    // taskbar leaves a strip of desktop at the bottom and a window parked
    // there would show its top edge.
    let screen_h = unsafe { GetSystemMetrics(SM_CYSCREEN) };
    let start_y = screen_h + 8;
    Some(((x, y), (x, start_y)))
}

/// Move the popup window upward from its parked position below the screen to
/// its centered resting place, easing out. Runs on a short-lived thread so the
/// worker loop is never blocked.
#[cfg(windows)]
fn begin_popup_slide(win: &tauri::WebviewWindow) {
    let Some(((x, to_y), (_, parked_y))) = popup_positions(win, POPUP_W, POPUP_H) else {
        return;
    };
    let _ = win.set_position(tauri::PhysicalPosition::new(x, parked_y));
    eprintln!("[popup] slide from y={} to y={} at x={}", parked_y, to_y, x);

    let win = win.clone();
    std::thread::spawn(move || {
        const STEPS: i32 = 20;
        const STEP_MS: u64 = 16; // ~320ms total
        for i in 1..=STEPS {
            // ease-out cubic: fast at the start, settling at the end
            let t = i as f64 / STEPS as f64;
            let eased = 1.0 - (1.0 - t).powi(3);
            let y = parked_y + ((to_y - parked_y) as f64 * eased) as i32;
            let _ = win.set_position(tauri::PhysicalPosition::new(x, y));
            std::thread::sleep(Duration::from_millis(STEP_MS));
        }
        eprintln!("[popup] slide done, pos={:?}", win.outer_position());
    });
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

    let w = POPUP_W;
    let h = POPUP_H;

    // Build hidden, park it below the screen, then slide it up once the page
    // has actually painted. Doing the entrance with the real window (rather
    // than an in-page CSS transform) is what makes it read like the Huawei
    // popup rising from the bottom edge.
    POPUP_SLIDE_STARTED.store(false, std::sync::atomic::Ordering::SeqCst);
    *POPUP_T0.lock().unwrap() = Some(std::time::Instant::now());
    *POPUP_PENDING.lock().unwrap() = Some(status.clone());
    let builder = WebviewWindowBuilder::new(app, "popup", WebviewUrl::App("popup.html".into()))
        .title("FreeClip 2")
        .inner_size(w, h)
        .resizable(false)
        .decorations(false)
        .transparent(true)
        .always_on_top(true)
        .skip_taskbar(true)
        .visible(false)
        .shadow(false);

    match builder.build() {
        Ok(win) => {
            #[cfg(windows)]
            {
                if let Some(((x, to_y), (_, from_y))) = popup_positions(&win, w, h) {
                    eprintln!("[popup] park at y={} (target y={}) x={}", from_y, to_y, x);
                    // Place it off-screen before the first paint so the very
                    // first visible frame is already below the screen.
                    let _ = win.set_position(tauri::PhysicalPosition::new(x, from_y));
                }
            }
            let _ = win.show();
            eprintln!(
                "[popup] after show: visible={:?} pos={:?} size={:?}",
                win.is_visible(),
                win.outer_position(),
                win.outer_size()
            );

            // The page calls `popup_ready` after its first paint; only then is
            // there anything to see, so only then does the slide make sense.
            // This thread is the watchdog for a page that never reports in.
            let app_w = app.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(900));
                if !POPUP_SLIDE_STARTED.swap(true, std::sync::atomic::Ordering::SeqCst) {
                    eprintln!("[popup] page never reported ready, sliding anyway");
                    if let Some(w) = app_w.get_webview_window("popup") {
                        begin_popup_slide(&w);
                    }
                }
            });

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
                                    needs_repair: false,
                                }
                            }
                            Err(fail) => Status::from(fail),
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
                                needs_repair: false,
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
                // The cheap probe already carries the correct offline/broken state.
                *shared.lock().unwrap() = probe;
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
            slide_popup_out,
            popup_ready,
            preview_popup,
            repair_bluetooth
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
