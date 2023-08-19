use anyhow::Result;
use clap::Parser;
use systemctl_tui::{
  app::App,
  utils::{initialize_logging, initialize_panic_handler, version},
};

// Define the command line arguments structure
#[derive(Parser, Debug)]
#[command(version = version(), about = "ratatui template with crossterm and tokio")]
struct Args {
  /// App tick rate
  #[arg(short, long, default_value_t = 1000)]
  app_tick_rate: u64,
  /// Render tick rate
  #[arg(short, long, default_value_t = 50)]
  render_tick_rate: u64,
}

// Main function
#[tokio::main]
async fn main() -> Result<()> {
  // Start with initializing logging
  initialize_logging()?;

  // Next initialize the panic handler
  initialize_panic_handler();

  let args = Args::parse();
  let tick_rate = (args.app_tick_rate, args.render_tick_rate);

  let mut app = App::new(tick_rate)?;
  app.run().await?;

  Ok(())
}
