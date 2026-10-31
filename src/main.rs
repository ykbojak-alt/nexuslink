//! NexusLink — local gateway: Anthropic /v1/messages in, OpenAI
//! /v1/chat/completions out, with provider/key rotation and a local
//! management panel. Built for Claude Desktop's third-party inference mode.
//!
//! Subcommands:
//!   serve            run the proxy (default)
//!   app              desktop shell: proxy + panel window that hides to tray
//!   ui               run the proxy and open the NexusLink panel in a browser
//!   mcp              stdio MCP server for Claude Desktop (generate_video,
//!                    video_status via the Agnes provider)
//!   init             write a starter config.json and print its path
//!   apply            write gateway keys + model slots into Claude Desktop
//!   slots            print current slot -> model mappings
#![windows_subsystem = "windows"]

use nexuslink::{config, gateway, proxy};

/// Windows-subsystem exe: no console window is created for `app`/double-click,
/// but when a CLI subcommand is invoked from a terminal we reattach to the
/// parent console so serve/init/logs still print where the user expects.
#[cfg(windows)]
fn attach_parent_console() {
    use std::os::windows::io::IntoRawHandle;
    extern "system" {
        fn AttachConsole(pid: u32) -> i32;
        fn SetStdHandle(n: i32, h: *mut core::ffi::c_void) -> i32;
        fn GetStdHandle(n: i32) -> *mut core::ffi::c_void;
    }
    const ATTACH_PARENT_PROCESS: u32 = 0xFFFF_FFFF;
    unsafe {
        // If stdout already carries a real handle (pipe, file redirect,
        // parent's console) — keep it; attaching would divert prints away
        // from whoever piped us.
        let stdout = GetStdHandle(-11);
        if !stdout.is_null() && stdout as isize != -1 {
            return;
        }
        // Attaching a console does not fix the standard handles of a
        // GUI-subsystem process — they must be reopened via CONOUT$/CONIN$.
        if AttachConsole(ATTACH_PARENT_PROCESS) == 0 {
            return;
        }
        if let Ok(f) = std::fs::File::create("CONOUT$") {
            SetStdHandle(-11, f.into_raw_handle() as _); // STD_OUTPUT_HANDLE
        }
        if let Ok(f) = std::fs::File::create("CONOUT$") {
            SetStdHandle(-12, f.into_raw_handle() as _); // STD_ERROR_HANDLE
        }
        if let Ok(f) = std::fs::File::options().read(true).open("CONIN$") {
            SetStdHandle(-10, f.into_raw_handle() as _); // STD_INPUT_HANDLE
        }
    }
}
#[cfg(not(windows))]
fn attach_parent_console() {}

#[tokio::main]
async fn main() {
    attach_parent_console();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "nexuslink=info".into()),
        )
        .init();

    let cmd = std::env::args().nth(1).unwrap_or_else(|| "serve".into());

    match cmd.as_str() {
        "init" => {
            let path = config::config_path();
            if path.exists() {
                println!("config already exists: {}", path.display());
            } else {
                let cfg = config::scaffold();
                config::save(&cfg).expect("cannot save config");
                println!("wrote {}", path.display());
                println!("incoming_token = {}", cfg.incoming_token);
            }
        }
        "slots" => {
            let cfg = config::load();
            for (slot, name) in config::flat_slots(&cfg) {
                println!("{slot} -> {name}");
            }
        }
        "apply" => {
            let cfg = config::load();
            match gateway::apply_to_claude_desktop(&cfg) {
                Ok(msg) => println!("{msg}\nrestart Claude Desktop to pick it up"),
                Err(e) => {
                    eprintln!("apply failed: {e}");
                    std::process::exit(1);
                }
            }
        }
        "mcp" => {
            let cfg = config::load();
            if let Err(e) = nexuslink::mcp::run(cfg) {
                eprintln!("mcp server failed: {e:#}");
                std::process::exit(1);
            }
        }
        "app" => {
            let cfg = config::load();
            warn_empty_token(&cfg);
            if let Err(e) = nexuslink::desktop::run_app(cfg) {
                eprintln!("desktop app failed: {e:#}");
                std::process::exit(1);
            }
        }
        _ => {
            serve(cmd.as_str() == "ui").await;
        }
    }
}

fn warn_empty_token(cfg: &config::Config) {
    if cfg.incoming_token.is_empty() {
        eprintln!("warning: incoming_token is empty — proxy will reject all requests.");
        eprintln!("run `nexuslink init` to generate one, or set it in config.json");
    }
}

async fn serve(open_ui: bool) {
    let cfg = config::load();
    warn_empty_token(&cfg);
    let port = cfg.port;
    let token = cfg.incoming_token.clone();

    let state = proxy::state_from_config(cfg);

    let app = proxy::router(state);
    let listener = match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("cannot bind 127.0.0.1:{port}: {e}");
            std::process::exit(1);
        }
    };
    let url = format!("http://127.0.0.1:{port}/");
    tracing::info!("NexusLink listening on {url} — panel at {url}, API at {url}v1/");
    if open_ui {
        // The panel token travels in the URL fragment: fragments never
        // leave the browser, so the secret skips logs, history and wire.
        let ui_url = if token.is_empty() {
            url.clone()
        } else {
            format!("{url}#t={token}")
        };
        open_browser(&ui_url);
    }
    axum::serve(listener, app).await.expect("server error");
}

/// Best-effort browser open; failure is silent — the URL is in the log.
fn open_browser(url: &str) {
    let result = if cfg!(target_os = "windows") {
        std::process::Command::new("cmd")
            .args(["/c", "start", "", url])
            .spawn()
    } else if cfg!(target_os = "macos") {
        std::process::Command::new("open").arg(url).spawn()
    } else {
        std::process::Command::new("xdg-open").arg(url).spawn()
    };
    if let Err(e) = result {
        tracing::warn!("could not open browser ({e}) — visit {url} manually");
    }
}
