use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use better_panic::Settings;
use directories::ProjectDirs;
use tracing::error;
use tracing_subscriber::{
  self, filter::EnvFilter, prelude::__tracing_subscriber_SubscriberExt, util::SubscriberInitExt, Layer,
};

pub fn initialize_panic_handler() {
  std::panic::set_hook(Box::new(|panic_info| {
    if let Err(r) = crate::terminal::exit() {
      error!("Unable to exit Terminal: {r:?}");
    }

    Settings::auto().most_recent_first(false).lineno_suffix(true).create_panic_handler()(panic_info);
    std::process::exit(libc::EXIT_FAILURE);
  }));
}

pub fn get_data_dir() -> Result<PathBuf> {
  let directory = if let Ok(s) = std::env::var("SYSTEMCTL_TUI_DATA") {
    PathBuf::from(s)
  } else if let Some(proj_dirs) = ProjectDirs::from("com", "kdheepak", "systemctl-tui") {
    proj_dirs.data_local_dir().to_path_buf()
  } else {
    return Err(anyhow!("Unable to find data directory for systemctl-tui"));
  };
  Ok(directory)
}

pub fn get_config_dir() -> Result<PathBuf> {
  let directory = if let Ok(s) = std::env::var("SYSTEMCTL_TUI_CONFIG") {
    PathBuf::from(s)
  } else if let Some(proj_dirs) = ProjectDirs::from("com", "kdheepak", "systemctl-tui") {
    proj_dirs.config_local_dir().to_path_buf()
  } else {
    return Err(anyhow!("Unable to find config directory for systemctl-tui"));
  };
  Ok(directory)
}

pub fn initialize_logging() -> Result<()> {
  let directory = get_data_dir()?;
  std::fs::create_dir_all(directory.clone()).context(format!("{directory:?} could not be created"))?;
  let log_path = directory.join("systemctl-tui.log");
  let log_file = std::fs::File::create(log_path)?;
  let file_subscriber = tracing_subscriber::fmt::layer()
    .with_file(true)
    .with_line_number(true)
    .with_writer(log_file)
    .with_target(false)
    .with_ansi(false)
    .with_filter(EnvFilter::from_default_env());
  tracing_subscriber::registry().with(file_subscriber).with(tui_logger::tracing_subscriber_layer()).init();
  let default_level =
    std::env::var("RUST_LOG").map_or(log::LevelFilter::Debug, |val| match val.to_lowercase().as_str() {
      "off" => log::LevelFilter::Off,
      "error" => log::LevelFilter::Error,
      "warn" => log::LevelFilter::Warn,
      "info" => log::LevelFilter::Info,
      "debug" => log::LevelFilter::Debug,
      "trace" => log::LevelFilter::Trace,
      _ => log::LevelFilter::Debug,
    });

  tui_logger::set_default_level(default_level);
  Ok(())
}

/// Similar to the `std::dbg!` macro, but generates `tracing` events rather
/// than printing to stdout.
///
/// By default, the verbosity level for the generated events is `DEBUG`, but
/// this can be customized.
#[macro_export]
macro_rules! trace_dbg {
    (target: $target:expr, level: $level:expr, $ex:expr) => {{
        match $ex {
            value => {
                tracing::event!(target: $target, $level, ?value, stringify!($ex));
                value
            }
        }
    }};
    (level: $level:expr, $ex:expr) => {
        trace_dbg!(target: module_path!(), level: $level, $ex)
    };
    (target: $target:expr, $ex:expr) => {
        trace_dbg!(target: $target, level: tracing::Level::DEBUG, $ex)
    };
    ($ex:expr) => {
        trace_dbg!(level: tracing::Level::DEBUG, $ex)
    };
}

pub fn version() -> String {
  let author = clap::crate_authors!();

  let commit_hash = env!("SYSTEMCTL_TUI_GIT_INFO");

  let config_dir_path = get_config_dir().unwrap().display().to_string();
  let data_dir_path = get_data_dir().unwrap().display().to_string();

  format!(
    "\
{commit_hash}

Authors: {author}

Config directory: {config_dir_path}
Data directory: {data_dir_path}"
  )
}
