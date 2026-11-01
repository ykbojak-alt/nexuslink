//! Desktop shell: the gateway runs on a background tokio thread inside the
//! same process while a WebView2 window hosts the management panel.
//! Closing the window hides it to the system tray instead of quitting —
//! the proxy keeps serving Claude Desktop from the tray.
//!
//! `nexuslink app` = server + client in one exe.

use std::net::TcpStream;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tao::window::WindowBuilder;
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon as TrayIcon, MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use wry::{WebContext, WebViewBuilder};

use crate::config::Config;
use crate::proxy::state_from_config;

const ICON_RGBA: &[u8] = include_bytes!("../assets/nexuslink.rgba");
const ICON_SIZE: u32 = 64;
const WINDOW_TITLE: &str = "NexusLink";

/// Named mutex guarding `nexuslink app`. The first process creates the name;
/// a later launch gets ERROR_ALREADY_EXISTS, restores the running panel and
/// exits before spawning a second server or window.
#[cfg(target_os = "windows")]
mod single_instance {
    use std::time::{Duration, Instant};
    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE,
    };
    use windows_sys::Win32::System::Threading::CreateMutexW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        FindWindowW, SetForegroundWindow, ShowWindow, SW_RESTORE,
    };

    const MUTEX_NAME: &str = r"Local\NexusLink-single-instance";

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Holds the mutex handle for the process lifetime; the name disappears
    /// when the primary instance exits.
    pub struct InstanceGuard(HANDLE);

    impl Drop for InstanceGuard {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.0) };
        }
    }

    /// Some(guard) — this is the first instance. None — another instance owns
    /// the name; it should have been focused already.
    pub fn acquire(name: &str) -> std::io::Result<Option<InstanceGuard>> {
        let name_w = wide(name);
        let handle = unsafe { CreateMutexW(std::ptr::null(), 0, name_w.as_ptr()) };
        if handle.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        let last = unsafe { GetLastError() };
        if last == ERROR_ALREADY_EXISTS {
            unsafe { CloseHandle(handle) };
            return Ok(None);
        }
        Ok(Some(InstanceGuard(handle)))
    }

    pub fn default_name() -> &'static str {
        MUTEX_NAME
    }

    /// Bring the running panel back from the tray (or from minimized) and put
    /// it in front. The first instance may still be starting, so wait briefly
    /// for its window to appear.
    pub fn focus_existing_window(title: &str) -> bool {
        let title_w = wide(title);
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let hwnd = unsafe { FindWindowW(std::ptr::null(), title_w.as_ptr()) };
            if !hwnd.is_null() {
                unsafe {
                    ShowWindow(hwnd, SW_RESTORE);
                    SetForegroundWindow(hwnd);
                }
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

#[cfg(not(target_os = "windows"))]
mod single_instance {
    pub struct InstanceGuard;

    pub fn acquire(_name: &str) -> std::io::Result<Option<InstanceGuard>> {
        Ok(Some(InstanceGuard))
    }

    pub fn default_name() -> &'static str {
        "nexuslink-single-instance"
    }

    pub fn focus_existing_window(_title: &str) -> bool {
        false
    }
}

enum UserEvent {
    Tray(TrayIconEvent),
    Menu(MenuEvent),
    Ctl(Ctl),
}

/// Window controls coming from the in-webview titlebar over IPC.
enum Ctl {
    Min,
    Max,
    Close,
    Drag,
}

/// Last window geometry, persisted to `~/.nexuslink/window.json`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct WindowState {
    x: i32,
    y: i32,
    w: u32,
    h: u32,
    #[serde(default)]
    maximized: bool,
}

/// A screen in the same physical pixels tao reports.
#[derive(Debug, Clone, Copy)]
struct MonitorRect {
    x: i32,
    y: i32,
    w: u32,
    h: u32,
}

/// How much of the window has to land on a screen for a restore to be trusted:
/// wide enough to grab the titlebar, short enough that a monitor change can't
/// strand the panel off-screen with no way back.
const ON_SCREEN_SLIVER: i32 = 60;

impl WindowState {
    fn load() -> Option<Self> {
        let path = crate::config::window_state_path();
        let raw = std::fs::read_to_string(&path).ok()?;
        match serde_json::from_str::<WindowState>(&raw) {
            Ok(st) => Some(st),
            Err(e) => {
                tracing::warn!("ignoring unreadable {} ({e})", path.display());
                None
            }
        }
    }

    fn save(&self) {
        let path = crate::config::window_state_path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        match serde_json::to_string(self) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&path, json) {
                    tracing::warn!("window state not saved to {} ({e})", path.display());
                }
            }
            Err(e) => tracing::warn!("window state not serialized ({e})"),
        }
    }

    /// None means "let the OS place the window": the saved rect hangs off every
    /// screen we can see right now, usually because a monitor was unplugged.
    fn on_screen(self, monitors: &[MonitorRect]) -> Option<Self> {
        let fits = monitors.iter().any(|m| {
            self.x > m.x
                && self.x < m.x + m.w as i32 - ON_SCREEN_SLIVER
                && self.y >= m.y
                && self.y <= m.y + m.h as i32 - ON_SCREEN_SLIVER
        });
        fits.then_some(self)
    }
}

/// Geometry snapshot for the sidecar. While maximized, Windows reports a rect
/// that overshoots the monitor, so keep the restored rect we already know and
/// store only the flag. A minimized window reports a useless rect too.
fn snapshot(window: &tao::window::Window, prev: Option<WindowState>) -> Option<WindowState> {
    if window.is_minimized() {
        return None;
    }
    if window.is_maximized() {
        let mut st = prev.unwrap_or_default();
        st.maximized = true;
        if st.w == 0 || st.h == 0 {
            let size = window.inner_size();
            st.w = size.width;
            st.h = size.height;
        }
        return Some(st);
    }
    let pos = window.outer_position().ok()?;
    let size = window.inner_size();
    Some(WindowState {
        x: pos.x,
        y: pos.y,
        w: size.width,
        h: size.height,
        maximized: false,
    })
}

/// Write the pending geometry now — used on hide and quit, where the loop may
/// not run another event before the process is gone.
fn flush_state(pending: &mut Option<WindowState>) {
    if let Some(st) = pending.take() {
        st.save();
    }
}

/// WebView2's own right-click menu ("Inspect", "Back", "Print"…) has no place in
/// an app shell. Windows-only knob; other platforms keep the builder as-is.
#[cfg(windows)]
fn hide_browser_chrome(builder: WebViewBuilder<'_>) -> WebViewBuilder<'_> {
    use wry::WebViewBuilderExtWindows;
    builder.with_default_context_menus(false)
}

#[cfg(not(windows))]
fn hide_browser_chrome(builder: WebViewBuilder<'_>) -> WebViewBuilder<'_> {
    builder
}

/// Rounded corners + resize edges for the frameless window (Windows 11).
#[cfg(target_os = "windows")]
fn dress_window(window: &tao::window::Window) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::Graphics::Dwm::DwmSetWindowAttribute;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetWindowLongW, SetWindowLongW, SetWindowPos, GWL_STYLE, SWP_FRAMECHANGED, SWP_NOMOVE,
        SWP_NOSIZE, SWP_NOZORDER, WS_MAXIMIZEBOX, WS_MINIMIZEBOX, WS_THICKFRAME,
    };
    let Ok(h) = window.window_handle() else { return };
    let RawWindowHandle::Win32(w) = h.as_raw() else { return };
    let hwnd: HWND = w.hwnd.get() as HWND;
    unsafe {
        // DWMWA_WINDOW_CORNER_PREFERENCE = 33, DWMWCP_ROUND = 2.
        let pref: u32 = 2;
        DwmSetWindowAttribute(hwnd, 33, &pref as *const _ as _, 4);
        // Decorations(false) drops WS_THICKFRAME — restore it so the window
        // keeps invisible resize borders without a native titlebar.
        let style = GetWindowLongW(hwnd, GWL_STYLE)
            | (WS_THICKFRAME | WS_MAXIMIZEBOX | WS_MINIMIZEBOX) as i32;
        SetWindowLongW(hwnd, GWL_STYLE, style);
        SetWindowPos(hwnd, std::ptr::null_mut(), 0, 0, 0, 0, SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER);
    }
}

#[cfg(not(target_os = "windows"))]
fn dress_window(_window: &tao::window::Window) {}

/// Injected into every page the WebView loads (including error pages):
/// a 34px Nexus Lux titlebar with drag region and min/max/close buttons
/// that talk back over `window.ipc`.
const TITLEBAR_JS: &str = r#"
(function () {
  function mk() {
    if (document.getElementById('nl-titlebar')) return;
    var st = document.createElement('style');
    st.textContent =
      '#nl-titlebar{position:fixed;top:0;left:0;right:0;height:32px;z-index:2147483647;' +
      'display:flex;align-items:stretch;user-select:none;-webkit-user-select:none;' +
      'background:rgba(12,12,15,.88);backdrop-filter:blur(16px);-webkit-backdrop-filter:blur(16px);' +
      'border-bottom:1px solid rgba(246,240,228,.08);' +
      'box-shadow:0 1px 0 rgba(201,164,92,.14),0 8px 22px rgba(0,0,0,.35);' +
      'font-family:"Segoe UI",system-ui,sans-serif;}' +
      '#nl-titlebar .nl-drag{flex:1;display:flex;align-items:center;gap:8px;padding:0 12px;cursor:default;}' +
      '#nl-titlebar .nl-logo{width:13px;height:13px;border-radius:4px;' +
      'background:linear-gradient(135deg,#E4CB93,#8E6B2E);' +
      'box-shadow:0 0 10px rgba(201,164,92,.45),inset 0 1px 0 rgba(255,255,255,.4);}' +
      '#nl-titlebar .nl-title{font-size:11px;letter-spacing:2.4px;font-weight:600;' +
      'color:rgba(246,240,228,.72);white-space:nowrap;}' +
      '#nl-titlebar .nl-btn{width:46px;border:0;background:transparent;color:rgba(246,240,228,.72);' +
      'font-family:"Segoe MDL2 Assets","Segoe UI Symbol";font-size:10px;cursor:default;' +
      'display:flex;align-items:center;justify-content:center;' +
      'transition:background .15s ease,color .15s ease;}' +
      '#nl-titlebar .nl-btn:hover{background:rgba(201,164,92,.16);color:#F6F0E4;}' +
      '#nl-titlebar .nl-close:hover{background:#e81123;color:#fff;}' +
      'body{padding-top:32px !important;box-sizing:border-box;}';
    document.documentElement.style.setProperty('--nl-tb-h', '32px');
    var bar = document.createElement('div');
    bar.id = 'nl-titlebar';
    bar.innerHTML =
      '<div class="nl-drag" id="nl-drag"><span class="nl-logo"></span>' +
      '<span class="nl-title">NEXUS LINK</span></div>' +
      '<button class="nl-btn" id="nl-min" title="Свернуть">\uE921</button>' +
      '<button class="nl-btn" id="nl-max" title="Развернуть">\uE922</button>' +
      '<button class="nl-btn nl-close" id="nl-x" title="Скрыть в трей">\uE8BB</button>';
    (document.body || document.documentElement).appendChild(bar);
    (document.head || document.documentElement).appendChild(st);
    var ipc = function (m) { try { window.ipc.postMessage(m); } catch (e) {} };
    document.getElementById('nl-min').onclick = function () { ipc('min'); };
    document.getElementById('nl-max').onclick = function () { ipc('max'); };
    document.getElementById('nl-x').onclick = function () { ipc('close'); };
    var drag = document.getElementById('nl-drag');
    drag.onmousedown = function (e) { if (e.button === 0) ipc('drag'); };
    drag.ondblclick = function () { ipc('max'); };
    window.__nlMax = function (m) {
      var b = document.getElementById('nl-max');
      if (b) b.textContent = m ? '\uE923' : '\uE922';
    };
  }
  if (document.body) mk(); else addEventListener('DOMContentLoaded', mk);
})();
"#;

fn spawn_server(cfg: Config) {
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime for the server thread");
        rt.block_on(async move {
            let port = cfg.port;
            let state = state_from_config(cfg);
            match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
                Ok(listener) => {
                    tracing::info!("NexusLink listening on http://127.0.0.1:{port}/ (desktop mode)");
                    let app = crate::proxy::router(state);
                    if let Err(e) = axum::serve(listener, app).await {
                        tracing::error!("gateway server stopped: {e}");
                    }
                }
                // Port is taken — most likely another NexusLink instance is
                // already serving. The window simply attaches to it; if it is
                // a foreign service the panel's token gate will show instead.
                Err(e) => tracing::warn!("cannot bind 127.0.0.1:{port} ({e}) — attaching the window to whatever is already there"),
            }
        });
    });
}

/// Wait until the port accepts connections — either our own server thread
/// (a few hundred ms) or an already-running instance we attach to.
fn wait_for_gateway(port: u16) -> bool {
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(120));
    }
    false
}

/// Entry for `nexuslink app`: background server + panel window + tray.
pub fn run_app(cfg: Config) -> anyhow::Result<()> {
    // Must run before the server thread: a second launch should never bind the
    // port or build a window, only surface the one already running.
    let _instance = match single_instance::acquire(single_instance::default_name()) {
        Ok(Some(guard)) => Some(guard),
        Ok(None) => {
            tracing::info!("NexusLink is already running — focusing its window");
            if !single_instance::focus_existing_window(WINDOW_TITLE) {
                tracing::warn!("running NexusLink instance has no window yet — leaving it alone");
            }
            return Ok(());
        }
        Err(e) => {
            // Fail open: a broken mutex must not keep the gateway from starting.
            tracing::warn!("single-instance mutex unavailable ({e}) — continuing");
            None
        }
    };

    let port = cfg.port;
    let token = cfg.incoming_token.clone();

    spawn_server(cfg);
    if !wait_for_gateway(port) {
        tracing::warn!("gateway did not come up on 127.0.0.1:{port} — opening the panel anyway");
    }
    // Token rides in the URL fragment: it never reaches the wire or logs,
    // and the panel stores it in localStorage so the gate appears once.
    let url = if token.is_empty() {
        format!("http://127.0.0.1:{port}/")
    } else {
        format!("http://127.0.0.1:{port}/#t={token}")
    };

    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    let monitors: Vec<MonitorRect> = event_loop
        .available_monitors()
        .map(|m| {
            let (pos, size) = (m.position(), m.size());
            MonitorRect {
                x: pos.x,
                y: pos.y,
                w: size.width,
                h: size.height,
            }
        })
        .collect();
    let restored = WindowState::load().and_then(|st| st.on_screen(&monitors));

    let mut builder = WindowBuilder::new()
        .with_title(WINDOW_TITLE)
        .with_decorations(false)
        .with_min_inner_size(tao::dpi::LogicalSize::new(760.0_f64, 520.0_f64))
        .with_window_icon(
            tao::window::Icon::from_rgba(ICON_RGBA.to_vec(), ICON_SIZE, ICON_SIZE).ok(),
        );
    builder = match restored {
        Some(st) => builder
            .with_position(tao::dpi::PhysicalPosition::new(st.x, st.y))
            .with_inner_size(tao::dpi::PhysicalSize::new(st.w, st.h))
            .with_maximized(st.maximized),
        None => builder.with_inner_size(tao::dpi::LogicalSize::new(1180.0_f64, 780.0_f64)),
    };
    let window = builder.build(&event_loop)?;
    dress_window(&window);

    let ipc_proxy = proxy.clone();
    // WebView2 needs a writable user data folder. Without an explicit
    // context wry falls back to the default UDF `<exe>.WebView2`, which
    // lives inside Program Files on a normal MSI install → E_ACCESSDENIED
    // → blank window (confirmed: NexusLink 0.2.0 MSI bug report).
    let data_dir = crate::config::webview_data_dir();
    if let Err(e) = std::fs::create_dir_all(&data_dir) {
        tracing::error!("cannot create WebView2 data dir {} ({e})", data_dir.display());
    }
    let mut web_context = WebContext::new(Some(data_dir));
    let builder = WebViewBuilder::new_with_web_context(&mut web_context)
        .with_url(&url)
        .with_initialization_script(TITLEBAR_JS)
        .with_hotkeys_zoom(false)
        .with_ipc_handler(move |req: wry::http::Request<String>| {
            let ctl = match req.body().as_str() {
                "min" => Ctl::Min,
                "max" => Ctl::Max,
                "close" => Ctl::Close,
                "drag" => Ctl::Drag,
                _ => return,
            };
            let _ = ipc_proxy.send_event(UserEvent::Ctl(ctl));
        });
    let webview = match hide_browser_chrome(builder).build(&window) {
        Ok(wv) => Some(wv),
        Err(e) => {
            tracing::error!(
                "WebView2 init failed ({e}) — gateway keeps running; \
                 open the panel in a browser: {url}"
            );
            None
        }
    };

    // Tray: left click re-opens the panel, right click opens the menu.
    let menu = Menu::new();
    let open_item = MenuItem::new("Открыть NexusLink", true, None);
    let quit_item = MenuItem::new("Выход", true, None);
    let open_id = open_item.id().clone();
    let quit_id = quit_item.id().clone();
    let _ = menu.append_items(&[&open_item, &PredefinedMenuItem::separator(), &quit_item]);
    let tray = TrayIcon::from_rgba(ICON_RGBA.to_vec(), ICON_SIZE, ICON_SIZE)
        .ok()
        .and_then(|icon| {
            TrayIconBuilder::new()
                .with_menu(Box::new(menu))
                .with_tooltip("NexusLink — локальный AI-шлюз")
                .with_icon(icon)
                .with_menu_on_left_click(false)
                .build()
                .map_err(|e| tracing::warn!("tray icon failed ({e}) — closing the window will quit"))
                .ok()
        });
    let has_tray = tray.is_some();

    // Forward tray/menu events into the event loop.
    let px = proxy.clone();
    std::thread::spawn(move || {
        while let Ok(ev) = TrayIconEvent::receiver().recv() {
            if px.send_event(UserEvent::Tray(ev)).is_err() {
                break;
            }
        }
    });
    let px2 = proxy.clone();
    std::thread::spawn(move || {
        while let Ok(ev) = MenuEvent::receiver().recv() {
            if px2.send_event(UserEvent::Menu(ev)).is_err() {
                break;
            }
        }
    });

    // Debounced geometry save: a drag fires Moved continuously, and the file is
    // a sidecar, not something worth writing 60 times a second.
    let mut pending = snapshot(&window, restored);
    let mut flush_at = Instant::now();

    event_loop.run(move |event, _elwt, control| {
        *control = ControlFlow::Wait;
        if pending.is_some() {
            if Instant::now() >= flush_at {
                flush_state(&mut pending);
            } else {
                *control = ControlFlow::WaitUntil(flush_at);
            }
        }
        match event {
            // X hides to the tray — the gateway keeps serving in background.
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => {
                flush_state(&mut pending);
                if has_tray {
                    window.set_visible(false);
                } else {
                    *control = ControlFlow::Exit;
                }
            }
            Event::WindowEvent {
                event: WindowEvent::Resized(_) | WindowEvent::Moved(_),
                ..
            } => {
                if let Some(st) = snapshot(&window, pending) {
                    pending = Some(st);
                    flush_at = Instant::now() + Duration::from_millis(400);
                }
            }
            Event::UserEvent(UserEvent::Menu(m)) if m.id == quit_id => {
                flush_state(&mut pending);
                *control = ControlFlow::Exit;
            }
            Event::UserEvent(UserEvent::Menu(m)) if m.id == open_id => {
                window.set_visible(true);
                window.set_minimized(false);
                window.set_focus();
            }
            Event::UserEvent(UserEvent::Ctl(Ctl::Min)) => window.set_minimized(true),
            Event::UserEvent(UserEvent::Ctl(Ctl::Max)) => {
                window.set_maximized(!window.is_maximized());
                if let Some(wv) = &webview {
                    let _ = wv.evaluate_script(&format!(
                        "window.__nlMax && window.__nlMax({})",
                        window.is_maximized()
                    ));
                }
            }
            Event::UserEvent(UserEvent::Ctl(Ctl::Close)) => {
                flush_state(&mut pending);
                if has_tray {
                    window.set_visible(false);
                } else {
                    *control = ControlFlow::Exit;
                }
            }
            Event::UserEvent(UserEvent::Ctl(Ctl::Drag)) => {
                let _ = window.drag_window();
            }
            Event::UserEvent(UserEvent::Tray(TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            })) => {
                window.set_visible(true);
                window.set_minimized(false);
                window.set_focus();
            }
            _ => {}
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn primary() -> MonitorRect {
        MonitorRect {
            x: 0,
            y: 0,
            w: 1920,
            h: 1080,
        }
    }

    fn st(x: i32, y: i32) -> WindowState {
        WindowState {
            x,
            y,
            w: 1180,
            h: 780,
            maximized: false,
        }
    }

    #[test]
    fn restores_geometry_that_still_sits_on_a_screen() {
        assert_eq!(st(120, 80).on_screen(&[primary()]), Some(st(120, 80)));
    }

    #[test]
    fn drops_geometry_left_off_an_unplugged_monitor() {
        assert_eq!(st(4200, 900).on_screen(&[primary()]), None);
        assert_eq!(st(1900, 40).on_screen(&[primary()]), None);
        assert_eq!(st(40, 1200).on_screen(&[primary()]), None);
    }

    #[test]
    fn honors_a_second_monitor_at_negative_coordinates() {
        let left = MonitorRect {
            x: -2560,
            y: 0,
            w: 2560,
            h: 1440,
        };
        assert_eq!(st(-1400, 200).on_screen(&[primary(), left]), Some(st(-1400, 200)));
    }

    #[test]
    fn sidecar_still_loads_without_the_maximized_flag() {
        let old: WindowState =
            serde_json::from_str(r#"{"x":1,"y":2,"w":3,"h":4}"#).expect("legacy sidecar");
        assert_eq!(
            old,
            WindowState {
                x: 1,
                y: 2,
                w: 3,
                h: 4,
                maximized: false
            }
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn second_launch_sees_the_instance_mutex() {
        let name = format!(r"Local\NexusLink-test-{}", std::process::id());
        let first = single_instance::acquire(&name).expect("create mutex");
        assert!(first.is_some(), "first launch owns the name");
        let second = single_instance::acquire(&name).expect("open existing mutex");
        assert!(second.is_none(), "second launch must not become primary");
        drop(first);
        let third = single_instance::acquire(&name).expect("recreate mutex");
        assert!(third.is_some(), "name is free again after the owner exits");
    }
}
