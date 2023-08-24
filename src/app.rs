use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::sync::{mpsc, Mutex};

use crate::{
  action::Action,
  components::{home::Home, Component},
  event::EventHandler,
  systemd::get_services,
  terminal::TerminalHandler,
};

pub struct App {
  pub tick_rate: u64,
  pub home: Arc<Mutex<Home>>,
  pub should_quit: bool,
  pub should_suspend: bool,
}

impl App {
  pub fn new(tick_rate: u64) -> Result<Self> {
    let home = Arc::new(Mutex::new(Home::new()));
    Ok(Self { tick_rate, home, should_quit: false, should_suspend: false })
  }

  pub async fn run(&mut self) -> Result<()> {
    let (action_tx, mut action_rx) = mpsc::unbounded_channel();

    self.home.lock().await.init(action_tx.clone())?;

    let units = get_services()
      .await
      .context("Unable to get services. Check that systemd is running and try running this tool with sudo.")?;
    self.home.lock().await.set_units(units);

    let mut terminal = TerminalHandler::new(self.home.clone());
    let mut event = EventHandler::new(self.tick_rate, self.home.clone(), action_tx.clone());

    loop {
      if let Some(action) = action_rx.recv().await {
        if !matches!(action, Action::RenderTick) {
          // re-enable this to debug actions
          // crate::trace_dbg!(&action);
        }
        match action {
          Action::RenderTick => terminal.render()?,
          Action::Quit => self.should_quit = true,
          Action::Suspend => self.should_suspend = true,
          Action::Resume => self.should_suspend = false,
          Action::Resize(_, _) => terminal.render()?,
          _ => {
            if let Some(_action) = self.home.lock().await.dispatch(action) {
              action_tx.send(_action)?
            };
            // Reilly: if we got a keystroke, we probably want to render immediately instead of waiting for the next tick
            terminal.render()?;
          },
        }
      }
      if self.should_suspend {
        terminal.suspend()?;
        event.stop();
        terminal.task.await?;
        event.task.await?;
        terminal = TerminalHandler::new(self.home.clone());
        event = EventHandler::new(self.tick_rate, self.home.clone(), action_tx.clone());
        action_tx.send(Action::Resume)?;
        action_tx.send(Action::RenderTick)?;
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
