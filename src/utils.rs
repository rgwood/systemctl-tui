use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use better_panic::Settings;
use directories::{BaseDirs, ProjectDirs};
use serde::{Deserialize, Serialize};
use tracing::{error, level_filters::LevelFilter};
use tracing_appender::{
  non_blocking::WorkerGuard,
  rolling::{RollingFileAppender, Rotation},
};
use tracing_subscriber::{
  self, filter::EnvFilter, prelude::__tracing_subscriber_SubscriberExt, util::SubscriberInitExt, Layer,
};

use crate::systemd::UnitId;

#[derive(Debug, Serialize, Deserialize, Default)]
pub struct SystemctlTuiConfig {
  #[serde(default)]
  pub favorites: Vec<UnitId>,
}

pub fn get_config_file_path() -> Result<PathBuf> {
  let base_dirs = BaseDirs::new().ok_or_else(|| anyhow!("Unable to find home directory for systemctl-tui config"))?;
  Ok(base_dirs.home_dir().join(".config/systemctl-tui.yaml"))
}

pub fn load_config() -> Result<SystemctlTuiConfig> {
  let path = get_config_file_path()?;
  if !path.exists() {
    return Ok(SystemctlTuiConfig::default());
  }

  let contents = std::fs::read_to_string(&path).context(format!("Failed reading config file at {}", path.display()))?;
  let config =
    serde_yaml::from_str(&contents).context(format!("Failed parsing config file at {}", path.display()))?;
  Ok(config)
}

pub fn save_config(config: &SystemctlTuiConfig) -> Result<()> {
  let path = get_config_file_path()?;
  if let Some(parent) = path.parent() {
    std::fs::create_dir_all(parent).context(format!("Failed creating config dir {}", parent.display()))?;
  }
  let contents = serde_yaml::to_string(config).context("Failed serializing config")?;
  std::fs::write(&path, contents).context(format!("Failed writing config file at {}", path.display()))?;
  Ok(())
}

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
  } else if let Some(proj_dirs) = ProjectDirs::from("com", "rgwood", "systemctl-tui") {
    proj_dirs.data_local_dir().to_path_buf()
  } else {
    return Err(anyhow!("Unable to find data directory for systemctl-tui"));
  };
  Ok(directory)
}

pub fn get_config_dir() -> Result<PathBuf> {
  let directory = if let Ok(s) = std::env::var("SYSTEMCTL_TUI_CONFIG") {
    PathBuf::from(s)
  } else if let Some(proj_dirs) = ProjectDirs::from("com", "rgwood", "systemctl-tui") {
    proj_dirs.config_local_dir().to_path_buf()
  } else {
    return Err(anyhow!("Unable to find config directory for systemctl-tui"));
  };
  Ok(directory)
}

pub fn initialize_logging(enable_file_logging: bool) -> Result<Option<WorkerGuard>> {
  let mut guard = None;

  let file_layer = if enable_file_logging {
    let directory = get_data_dir()?;
    std::fs::create_dir_all(directory.clone()).context(format!("{directory:?} could not be created"))?;

    // create a file appender that rolls daily
    let file_appender = RollingFileAppender::new(Rotation::DAILY, &directory, "systemctl-tui.log");

    // create a non-blocking writer
    let (non_blocking, g) = tracing_appender::non_blocking(file_appender);

    // We must return this guard to main.rs and keep it alive
    guard = Some(g);

    // Log initialization info only if we are actually logging
    tracing::info!(directory = %directory.display(), "Logging initialized");

    // create a layer for the file logger
    Some(
      tracing_subscriber::fmt::layer()
        .with_writer(non_blocking)
        .with_file(true)
        .with_line_number(true)
        .with_target(false)
        .with_ansi(false)
        .with_filter(EnvFilter::builder().with_default_directive(LevelFilter::INFO.into()).from_env_lossy()),
    )
  } else {
    None
  };

  tui_logger::init_logger(tui_logger::LevelFilter::Debug)?;

  let tui_layer = tui_logger::TuiTracingSubscriberLayer
    .with_filter(EnvFilter::builder().with_default_directive(LevelFilter::INFO.into()).from_env_lossy());

  tracing_subscriber::registry().with(file_layer).with(tui_layer).init();

  Ok(guard)
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

  let version = env!("CARGO_PKG_VERSION");

  let config_dir_path = get_config_dir().unwrap().display().to_string();
  let data_dir_path = get_data_dir().unwrap().display().to_string();

  format!(
    "\
{version}

Authors: {author}

Config directory: {config_dir_path}
Data directory: {data_dir_path}"
  )
}
