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
  /// The scope of the services to display
  #[clap(short, long, default_value = "all")]
  scope: Scope,
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

  // There's probably a nicer way to do this than defining scope in separate places, but this is fine for now
  let scope = match args.scope {
    Scope::Global => systemd::Scope::Global,
    Scope::User => systemd::Scope::User,
    Scope::All => systemd::Scope::All,
  };

  let mut app = App::new(scope)?;
  app.run().await?;

  Ok(())
}
