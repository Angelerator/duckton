//! Duckton Node — a desktop app that runs your machine as a host on the Duckton
//! peer-to-peer DuckDB grid (settled on TON). The Rust backend embeds the grid
//! core; the Svelte frontend drives it over Tauri commands.

mod commands;
mod config_store;
mod dto;
mod node_manager;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex as StdMutex};

use tauri::Manager;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use config_store::Paths;
use node_manager::{AppState, LogBuffer};

const LOG_CAP: usize = 500;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let logs: LogBuffer = Arc::new(StdMutex::new(VecDeque::with_capacity(LOG_CAP)));
    init_tracing(Arc::clone(&logs));

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .setup(move |app| {
            let config_dir = app
                .path()
                .app_config_dir()
                .expect("resolve app config dir");
            let paths = Paths::new(config_dir);
            paths.ensure()?;
            let config = config_store::load_config(&paths)?;
            let state = AppState::new(paths, config, Arc::clone(&logs));
            app.manage(state);

            // Auto-start the host so the machine begins serving as a node on
            // launch (free, no wallet needed). Failures (e.g. port in use) are
            // logged and surfaced as a "stopped" status the user can retry.
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let state = handle.state::<AppState>();
                if let Err(e) = state.start().await {
                    tracing::error!("auto-start failed: {e}");
                }
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_status,
            commands::start_node,
            commands::stop_node,
            commands::get_config,
            commands::save_config,
            commands::get_logs,
            commands::set_economics,
            commands::set_wallet,
            commands::set_contracts,
            commands::set_pricing,
            commands::stake,
            commands::unstake,
        ])
        .run(tauri::generate_context!())
        .expect("error while running Duckton Node");
}

/// Wire tracing to BOTH stderr and the in-app log ring buffer (the Logs screen).
fn init_tracing(logs: LogBuffer) {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,duckton_node_lib=info,p2p_node=info,p2p_transport=warn"));

    let buffer_layer = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_target(false)
        .with_writer(BufMaker { logs });

    let stderr_layer = tracing_subscriber::fmt::layer().with_target(false);

    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(stderr_layer)
        .with(buffer_layer)
        .try_init();
}

/// `MakeWriter` that funnels each formatted log line into the shared ring buffer.
#[derive(Clone)]
struct BufMaker {
    logs: LogBuffer,
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for BufMaker {
    type Writer = BufGuard;
    fn make_writer(&'a self) -> Self::Writer {
        BufGuard {
            logs: Arc::clone(&self.logs),
            line: String::new(),
        }
    }
}

/// Accumulates one event's bytes and pushes the completed line on drop.
struct BufGuard {
    logs: LogBuffer,
    line: String,
}

impl std::io::Write for BufGuard {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.line.push_str(&String::from_utf8_lossy(buf));
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for BufGuard {
    fn drop(&mut self) {
        let line = self.line.trim_end().to_string();
        if line.is_empty() {
            return;
        }
        if let Ok(mut buf) = self.logs.lock() {
            if buf.len() >= LOG_CAP {
                buf.pop_front();
            }
            buf.push_back(line);
        }
    }
}
