use anyhow::Result;
use clap::Parser;
use systemctl_tui::{
  app::App,
  utils::{initialize_logging, initialize_panic_handler, version},
};

// Define the command line arguments structure
#[derive(Parser, Debug)]
#[command(version = version(), about = "A simple TUI for systemd services")]
struct Args {
  /// Enable performance tracing (in Chromium Event JSON format)
  #[clap(short, long)]
  trace: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
  let args = Args::parse();
  initialize_logging(args.trace)?;
  initialize_panic_handler();

  let mut app = App::new()?;
  app.run().await?;

  Ok(())
}
