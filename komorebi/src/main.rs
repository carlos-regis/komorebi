#![warn(clippy::all, clippy::nursery, clippy::pedantic)]
#![allow(clippy::missing_errors_doc)]

use std::process::Command;
use std::sync::Arc;
use std::sync::Mutex;

use color_eyre::eyre::ContextCompat;
use color_eyre::Result;
use crossbeam_channel::Receiver;
use crossbeam_channel::Sender;
use lazy_static::lazy_static;
use sysinfo::SystemExt;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::EnvFilter;
use which::which;

use crate::process_command::listen_for_commands;
use crate::process_event::listen_for_events;
use crate::window_manager_event::WindowManagerEvent;
use crate::windows_api::WindowsApi;

#[macro_use]
mod ring;

mod container;
mod monitor;
mod process_command;
mod process_event;
mod set_window_position;
mod styles;
mod window;
mod window_manager;
mod window_manager_event;
mod windows_api;
mod windows_callbacks;
mod winevent;
mod winevent_listener;
mod workspace;

lazy_static! {
    static ref FLOAT_CLASSES: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(vec![]));
    static ref FLOAT_EXES: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(vec![]));
    static ref FLOAT_TITLES: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(vec![]));
    static ref HIDDEN_HWNDS: Arc<Mutex<Vec<isize>>> = Arc::new(Mutex::new(vec![]));
    static ref LAYERED_EXE_WHITELIST: Arc<Mutex<Vec<String>>> =
        Arc::new(Mutex::new(vec!["steam.exe".to_string()]));
    static ref MULTI_WINDOW_EXES: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(vec![
        "explorer.exe".to_string(),
        "firefox.exe".to_string(),
        "chrome.exe".to_string(),
        "idea64.exe".to_string(),
        "ApplicationFrameHost.exe".to_string(),
        "steam.exe".to_string()
    ]));
}

fn setup() -> Result<WorkerGuard> {
    if std::env::var("RUST_LIB_BACKTRACE").is_err() {
        std::env::set_var("RUST_LIB_BACKTRACE", "1");
    }

    color_eyre::install()?;

    if std::env::var("RUST_LOG").is_err() {
        std::env::set_var("RUST_LOG", "info");
    }

    let home = dirs::home_dir().context("there is no home directory")?;
    let appender = tracing_appender::rolling::never(home, "komorebi.log");
    let (non_blocking, guard) = tracing_appender::non_blocking(appender);

    tracing::subscriber::set_global_default(
        tracing_subscriber::fmt::Subscriber::builder()
            .with_env_filter(EnvFilter::from_default_env())
            .finish()
            .with(
                tracing_subscriber::fmt::Layer::default()
                    .with_writer(non_blocking)
                    .with_ansi(false),
            ),
    )?;

    // https://github.com/tokio-rs/tracing/blob/master/examples/examples/panic_hook.rs
    // Set a panic hook that records the panic as a `tracing` event at the
    // `ERROR` verbosity level.
    //
    // If we are currently in a span when the panic occurred, the logged event
    // will include the current span, allowing the context in which the panic
    // occurred to be recorded.
    std::panic::set_hook(Box::new(|panic| {
        // If the panic has a source location, record it as structured fields.
        if let Some(location) = panic.location() {
            // On nightly Rust, where the `PanicInfo` type also exposes a
            // `message()` method returning just the message, we could record
            // just the message instead of the entire `fmt::Display`
            // implementation, avoiding the duplciated location
            tracing::error!(
                message = %panic,
                panic.file = location.file(),
                panic.line = location.line(),
                panic.column = location.column(),
            );
        } else {
            tracing::error!(message = %panic);
        }
    }));

    Ok(guard)
}

#[tracing::instrument]
fn main() -> Result<()> {
    match std::env::args().count() {
        1 => {
            let mut system = sysinfo::System::new_all();
            system.refresh_processes();

            if system.process_by_name("komorebi.exe").len() > 1 {
                tracing::error!("komorebi.exe is already running, please exit the existing process before starting a new one");
                std::process::exit(1);
            }

            // File logging worker guard has to have an assignment in the main fn to work
            let _guard = setup()?;

            let process_id = WindowsApi::current_process_id();
            WindowsApi::allow_set_foreground_window(process_id)?;

            let (outgoing, incoming): (Sender<WindowManagerEvent>, Receiver<WindowManagerEvent>) =
                crossbeam_channel::unbounded();

            let winevent_listener = winevent_listener::new(Arc::new(Mutex::new(outgoing)));
            winevent_listener.start();

            let wm = Arc::new(Mutex::new(window_manager::new(Arc::new(Mutex::new(
                incoming,
            )))?));

            wm.lock().unwrap().init()?;
            listen_for_commands(wm.clone());
            listen_for_events(wm.clone());

            let home = dirs::home_dir().context("there is no home directory")?;
            let mut config = home;
            config.push("komorebi.ahk");

            if config.exists() && which("autohotkey.exe").is_ok() {
                Command::new("autohotkey.exe")
                    .arg(config.as_os_str())
                    .output()?;
            }

            let (ctrlc_sender, ctrlc_receiver) = crossbeam_channel::bounded(1);
            ctrlc::set_handler(move || {
                ctrlc_sender
                    .send(())
                    .expect("could not send signal on ctrl-c channel");
            })?;

            ctrlc_receiver
                .recv()
                .expect("could not receive signal on ctrl-c channel");

            tracing::error!(
                "received ctrl-c, restoring all hidden windows and terminating process"
            );

            wm.lock().unwrap().restore_all_windows();
            std::process::exit(130);
        }
        _ => Ok(()),
    }
}