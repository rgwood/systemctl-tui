use anyhow::Result;
use clap::Parser;
use systemctl_tui::{
  app::App,
  utils::{initialize_logging, initialize_panic_handler, version},
};

// Define the command line arguments structure
#[derive(Parser, Debug)]
#[command(version = version(), about = "A simple TUI for systemd services")]
struct Args {}

#[tokio::main]
async fn main() -> Result<()> {
  initialize_logging()?;
  initialize_panic_handler();

  let _args = Args::parse();
  let mut app = App::new()?;
  app.run().await?;

  Ok(())
}
