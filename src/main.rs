use anyhow::Result;
use clap::{Parser, ValueEnum};
use systemctl_tui::{
  app::App,
  systemd,
  utils::{initialize_logging, initialize_panic_handler, version},
};

// Define the command line arguments structure
#[derive(Parser, Debug)]
#[command(version = version(), about = "A simple TUI for systemd services")]
struct Args {
  /// The scope of the services to display. Defaults to "all" normally and "global" on WSL
  #[clap(short, long)]
  scope: Option<Scope>,
  /// Enable performance tracing (in Chromium Event JSON format)
  #[clap(short, long)]
  trace: bool,
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
  initialize_logging(args.trace)?;
  initialize_panic_handler();

  // There's probably a nicer way to do this than defining the scope enum twice, but this is fine for now
  let scope = match args.scope {
    Some(Scope::Global) => systemd::Scope::Global,
    Some(Scope::User) => systemd::Scope::User,
    Some(Scope::All) => systemd::Scope::All,
    // So, WSL doesn't *really* support user services yet: https://github.com/microsoft/WSL/issues/8842
    // Revisit this if that changes
    None => if is_wsl::is_wsl() { systemd::Scope::Global } else { systemd::Scope::All },
  };

  eprintln!("Using scope: {:?}", scope);

  let mut app = App::new(scope)?;
  app.run().await?;

  Ok(())
}
