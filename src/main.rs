use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use systemctl_tui::{
  app::App,
  components::home::LogOrder,
  remote_picker, ssh, systemd,
  utils::{get_data_dir, initialize_logging, initialize_panic_handler, version},
};

// Define the command line arguments structure
#[derive(Parser, Debug)]
#[command(version = version(), about = "A simple TUI for systemd units")]
struct Args {
  #[command(subcommand)]
  command: Option<Commands>,
  /// The scope of the services to display. Defaults to "all" normally and "global" on WSL
  #[clap(short, long)]
  scope: Option<Scope>,
  /// Disable file logging (logs are enabled by default)
  #[arg(
    long,
      env = "SYSTEMCTL_TUI_NO_LOG",
      default_value_t = false,
      action = clap::ArgAction::SetTrue
    )]
  no_log: bool,
  /// Limit view to only these unit files
  #[clap(short, long, default_values_t = ["*.service".to_string(), "*.timer".to_string()], num_args=1..)]
  limit_units: Vec<String>,
  /// Manage a remote host over SSH (e.g. user@hostname). Requires systemd-stdio-bridge on the remote host.
  #[clap(long)]
  host: Option<String>,
  /// Choose a remote host from SSH config
  #[clap(short, long, conflicts_with = "host")]
  remote: bool,
  /// Order used to display service logs
  #[clap(long, value_enum, default_value_t = CliLogOrder::NewestFirst)]
  log_order: CliLogOrder,
}

#[derive(Subcommand, Debug)]
enum Commands {
  /// Show the path to the logs directory
  ShowLogsPath,
}

#[derive(Parser, Debug, ValueEnum, Clone)]
pub enum Scope {
  Global,
  User,
  All,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CliLogOrder {
  NewestFirst,
  OldestFirst,
}

impl From<CliLogOrder> for LogOrder {
  fn from(value: CliLogOrder) -> Self {
    match value {
      CliLogOrder::NewestFirst => LogOrder::NewestFirst,
      CliLogOrder::OldestFirst => LogOrder::OldestFirst,
    }
  }
}

#[tokio::main]
async fn main() -> Result<()> {
  // Help users help me with bug reports by making sure they have stack traces
  if std::env::var("RUST_BACKTRACE").is_err() {
    std::env::set_var("RUST_BACKTRACE", "1");
  }

  let args = Args::parse();

  // Handle subcommands
  match args.command {
    Some(Commands::ShowLogsPath) => {
      let logs_path = get_data_dir()?;
      println!("{}", logs_path.display());
      return Ok(());
    },
    None => {
      // Default behavior - run the TUI
    },
  }

  let _guard = initialize_logging(!args.no_log)?;
  initialize_panic_handler();

  // There's probably a nicer way to do this than defining the scope enum twice, but this is fine for now
  let scope = match args.scope {
    Some(Scope::Global) => systemd::Scope::Global,
    Some(Scope::User) => systemd::Scope::User,
    Some(Scope::All) => systemd::Scope::All,
    // So, WSL doesn't *really* support user services yet: https://github.com/microsoft/WSL/issues/8842
    // Revisit this if that changes
    None => {
      if is_wsl::is_wsl() {
        systemd::Scope::Global
      } else {
        systemd::Scope::All
      }
    },
  };

  // Connect before entering the TUI so SSH auth prompts (password, 2FA) work
  let host = if args.remote { Some(remote_picker::choose_remote_host()?) } else { args.host };
  if let Some(host) = host {
    println!("Connecting to {host}...");
    if let Err(e) = ssh::init(host) {
      // ssh has already printed its own error to stderr
      eprintln!("{e}");
      std::process::exit(1);
    }
  }

  let mut app = App::new(scope, args.limit_units, args.log_order.into())?;
  let result = app.run().await;
  ssh::teardown();
  result?;

  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn default_unit_patterns_include_services_and_timers() {
    let args = Args::try_parse_from(["systemctl-tui"]).unwrap();
    assert_eq!(args.limit_units, ["*.service", "*.timer"]);
  }

  #[test]
  fn explicit_unit_patterns_replace_defaults() {
    let args = Args::try_parse_from(["systemctl-tui", "--limit-units", "*.timer"]).unwrap();
    assert_eq!(args.limit_units, ["*.timer"]);
  }
}
