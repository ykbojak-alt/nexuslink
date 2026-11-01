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

    let command = match parse_command(std::env::args().nth(1).as_deref()) {
        Ok(command) => command,
        Err(cmd) => {
            eprintln!("unknown command: {cmd}");
            eprint!("{USAGE}");
            std::process::exit(2);
        }
    };

    match command {
        CliCommand::Help => println!("{USAGE}"),
        CliCommand::Version => println!("nexuslink {}", env!("CARGO_PKG_VERSION")),
        CliCommand::Serve(open_ui) => serve(open_ui).await,
        CliCommand::Init => {
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
        CliCommand::Slots => {
            let cfg = config::load();
            for (slot, name) in config::flat_slots(&cfg) {
                println!("{slot} -> {name}");
            }
        }
        CliCommand::Apply => {
            let cfg = config::load();
            match gateway::apply_to_claude_desktop(&cfg) {
                Ok(msg) => println!("{msg}\nrestart Claude Desktop to pick it up"),
                Err(e) => {
                    eprintln!("apply failed: {e}");
                    std::process::exit(1);
                }
            }
        }
        CliCommand::Mcp => {
            let cfg = config::load();
            if let Err(e) = nexuslink::mcp::run(cfg) {
                eprintln!("mcp server failed: {e:#}");
                std::process::exit(1);
            }
        }
        CliCommand::App => {
            let cfg = load_or_scaffold();
            warn_empty_token(&cfg);
            #[cfg(windows)]
            {
                if let Err(e) = nexuslink::desktop::run_app(cfg) {
                    eprintln!("desktop app failed: {e:#}");
                    std::process::exit(1);
                }
            }
            #[cfg(not(windows))]
            {
                let _ = cfg;
                eprintln!("`app` (desktop shell) is Windows-only; use `serve` on this platform");
                std::process::exit(2);
            }
        }
    }
}

const USAGE: &str = "Usage: nexuslink [COMMAND]\n\
\n\
Commands:\n\
  serve       Run the local gateway (default)\n\
  init        Create the starter config\n\
  slots       Print slot-to-model mappings\n\
  apply       Configure Claude Desktop to use this gateway\n\
  mcp         Run the stdio MCP server\n\
  app         Run the Windows desktop shell\n\
  ui          Run the gateway and open its panel\n\
\n\
Options:\n\
  -h, --help      Print this help\n\
  -V, --version   Print version\n\
\n\
Config keys: port, nue_port, incoming_token, providers, nue_upstream,\n\
nue_register_token (config.json)\n";

#[derive(Debug, PartialEq, Eq)]
enum CliCommand {
    Help,
    Version,
    Serve(bool),
    Init,
    Slots,
    Apply,
    Mcp,
    App,
}

fn parse_command(arg: Option<&str>) -> Result<CliCommand, &str> {
    match arg {
        None | Some("serve") => Ok(CliCommand::Serve(false)),
        Some("ui") => Ok(CliCommand::Serve(true)),
        Some("help" | "--help" | "-h") => Ok(CliCommand::Help),
        Some("version" | "--version" | "-V") => Ok(CliCommand::Version),
        Some("init") => Ok(CliCommand::Init),
        Some("slots") => Ok(CliCommand::Slots),
        Some("apply") => Ok(CliCommand::Apply),
        Some("mcp") => Ok(CliCommand::Mcp),
        Some("app") => Ok(CliCommand::App),
        Some(other) => Err(other),
    }
}

fn warn_empty_token(cfg: &config::Config) {
    if cfg.incoming_token.is_empty() {
        eprintln!("warning: incoming_token is empty — proxy will reject all requests.");
        eprintln!("run `nexuslink init` to generate one, or set it in config.json");
    }
}

/// MSI first-run: there is no config.json yet, so `load()` returns an
/// empty-token default that rejects every request and locks the panel's
/// token gate with no file to read the token from. Scaffold + save the
/// starter config instead — same thing `nexuslink init` does.
fn load_or_scaffold() -> config::Config {
    if !config::config_path().exists() {
        let cfg = config::scaffold();
        if let Err(e) = config::save(&cfg) {
            eprintln!("warning: cannot create config.json ({e}) — running with defaults");
        } else {
            eprintln!("created starter config: {}", config::config_path().display());
        }
        return cfg;
    }
    config::load()
}

async fn serve(open_ui: bool) {
    let cfg = load_or_scaffold();
    warn_empty_token(&cfg);
    let port = cfg.port;
    let nue_port = cfg.nue_port;
    let token = cfg.incoming_token.clone();

    let state = proxy::state_from_config(cfg);

    let app = proxy::router(state.clone());
    let listener = match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("cannot bind 127.0.0.1:{port}: {e}");
            std::process::exit(1);
        }
    };
    let url = format!("http://127.0.0.1:{port}/");
    tracing::info!("NexusLink listening on {url} — panel at {url}, API at {url}v1/");
    if nue_port != port {
        let nue_app = proxy::nue_router(state);
        match tokio::net::TcpListener::bind(("127.0.0.1", nue_port)).await {
            Ok(l) => {
                tracing::info!("NUE passthrough on 127.0.0.1:{nue_port} — register at /nue/register, api at /v1/");
                tokio::spawn(async move {
                    if let Err(e) = axum::serve(l, nue_app).await {
                        tracing::error!("nue gateway died: {e}");
                    }
                });
            }
            Err(e) => {
                eprintln!("warning: cannot bind NUE port {nue_port}: {e}");
            }
        }
    } else {
        eprintln!("warning: nue_port == port — NUE gateway disabled (set nue_port != {port})");
    }
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

#[cfg(test)]
mod cli_tests {
    use super::{parse_command, CliCommand};

    #[test]
    fn no_argument_keeps_default_serve() {
        assert_eq!(parse_command(None), Ok(CliCommand::Serve(false)));
    }

    #[test]
    fn help_and_version_aliases_are_recognized() {
        for arg in ["help", "--help", "-h"] {
            assert_eq!(parse_command(Some(arg)), Ok(CliCommand::Help));
        }
        for arg in ["version", "--version", "-V"] {
            assert_eq!(parse_command(Some(arg)), Ok(CliCommand::Version));
        }
    }

    #[test]
    fn unknown_arguments_are_rejected_instead_of_serving() {
        assert_eq!(parse_command(Some("--unknown")), Err("--unknown"));
        assert_eq!(parse_command(Some("nonsense")), Err("nonsense"));
    }
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
