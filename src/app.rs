use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::sync::{mpsc, Mutex};
use tracing::debug;

use crate::{
  action::Action,
  components::{home::Home, Component},
  event::EventHandler,
  systemd::get_services,
  terminal::TerminalHandler,
};

pub struct App {
  pub home: Arc<Mutex<Home>>,
  pub should_quit: bool,
  pub should_suspend: bool,
}

impl App {
  pub fn new() -> Result<Self> {
    let mut home = Home::new();
    home.show_logger = true;
    let home = Arc::new(Mutex::new(home));
    Ok(Self { home, should_quit: false, should_suspend: false })
  }

  pub async fn run(&mut self) -> Result<()> {
    let (action_tx, mut action_rx) = mpsc::unbounded_channel();

    self.home.lock().await.init(action_tx.clone())?;

    let units = get_services()
      .await
      .context("Unable to get services. Check that systemd is running and try running this tool with sudo.")?;
    self.home.lock().await.set_units(units);

    let mut terminal = TerminalHandler::new(self.home.clone());
    let mut event = EventHandler::new(self.home.clone(), action_tx.clone());

    terminal.render().await;

    loop {
      if let Some(action) = action_rx.recv().await {
        match &action {
          Action::SetLogs { .. } => {
            debug!("action: SetLogs");
          },
          Action::SetServices { .. } => {
            debug!("action: SetServices");
          },
          _ => debug!("action: {:?}", action),
        }

        match action {
          Action::Render => terminal.render().await,
          Action::Noop => {},
          Action::Quit => self.should_quit = true,
          Action::Suspend => self.should_suspend = true,
          Action::Resume => self.should_suspend = false,
          Action::Resize(_, _) => terminal.render().await,
          _ => {
            if let Some(_action) = self.home.lock().await.dispatch(action) {
              action_tx.send(_action)?
            };
          },
        }
      }
      if self.should_suspend {
        terminal.suspend()?;
        event.stop();
        terminal.task.await?;
        event.task.await?;
        terminal = TerminalHandler::new(self.home.clone());
        event = EventHandler::new(self.home.clone(), action_tx.clone());
        action_tx.send(Action::Resume)?;
        action_tx.send(Action::Render)?;
      } else if self.should_quit {
        terminal.stop()?;
        event.stop();
        terminal.task.await?;
        event.task.await?;
        break;
      }
    }
    Ok(())
  }
}
