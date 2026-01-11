use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use systemctl_tui::{
  app::App,
  systemd,
  utils::{get_data_dir, initialize_logging, initialize_panic_handler, version},
};

// Define the command line arguments structure
#[derive(Parser, Debug)]
#[command(version = version(), about = "A simple TUI for systemd services")]
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
  #[clap(short, long, default_value="*.service", num_args=1..)]
  limit_units: Vec<String>,
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

  let mut app = App::new(scope, args.limit_units)?;
  app.run().await?;

  Ok(())
}
