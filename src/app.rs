use std::{process::Command, sync::Arc};

use anyhow::{Context, Result};
use log::error;
use tokio::sync::{mpsc, Mutex};
use tracing::debug;

use crate::{
  action::Action,
  components::{
    home::{Home, Mode},
    Component,
  },
  event::EventHandler,
  systemd::{get_all_services, Scope},
  terminal::TerminalHandler,
};

pub struct App {
  pub scope: Scope,
  pub home: Arc<Mutex<Home>>,
  pub limit_units: Vec<String>,
  pub host: Option<String>,
  pub should_quit: bool,
  pub should_suspend: bool,
}

impl App {
  pub fn new(scope: Scope, limit_units: Vec<String>, host: Option<String>) -> Result<Self> {
    let home = Home::with_host(scope, &limit_units, host.clone());
    let home = Arc::new(Mutex::new(home));
    Ok(Self { scope, home, limit_units, host, should_quit: false, should_suspend: false })
  }

  pub async fn run(&mut self) -> Result<()> {
    let (action_tx, mut action_rx) = mpsc::unbounded_channel();

    let (debounce_tx, mut debounce_rx) = mpsc::unbounded_channel();

    let cloned_action_tx = action_tx.clone();
    tokio::spawn(async move {
      let debounce_duration = std::time::Duration::from_millis(0);
      let debouncing = Arc::new(Mutex::new(false));

      loop {
        let _ = debounce_rx.recv().await;

        if *debouncing.lock().await {
          continue;
        }

        *debouncing.lock().await = true;

        let action_tx = cloned_action_tx.clone();
        let debouncing = debouncing.clone();
        tokio::spawn(async move {
          tokio::time::sleep(debounce_duration).await;
          let _ = action_tx.send(Action::Render);
          *debouncing.lock().await = false;
        });
      }
    });

    self.home.lock().await.init(action_tx.clone())?;

    let units = get_all_services(self.scope, &self.limit_units, self.host.clone())
      .await
      .context("Unable to get services. Check that systemd is running and try running this tool with sudo.")?;
    self.home.lock().await.set_units(units);

    let mut terminal = TerminalHandler::new(self.home.clone());
    let mut event = EventHandler::new(self.home.clone(), action_tx.clone());

    terminal.render().await;

    loop {
      if let Some(action) = action_rx.recv().await {
        match &action {
          // these are too big to log in full
          Action::SetLogs { .. } => debug!("action: SetLogs"),
          Action::SetServices { .. } => debug!("action: SetServices"),
          _ => debug!("action: {:?}", action),
        }

        match action {
          Action::Render => {
            let start = std::time::Instant::now();
            terminal.render().await;
            let duration = start.elapsed();
            crate::utils::log_perf_event("render", duration);
          },
          Action::DebouncedRender => debounce_tx.send(Action::Render).unwrap(),
          Action::Noop => {},
          Action::Quit => self.should_quit = true,
          Action::Suspend => self.should_suspend = true,
          Action::Resume => self.should_suspend = false,
          Action::Resize(_, _) => terminal.render().await,
          // This would normally be in home.rs, but it needs to do some terminal and event handling stuff that's easier here
          Action::EditUnitFile { unit, path } => {
            event.stop();
            let mut tui = terminal.tui.lock().await;
            tui.exit()?;

            let read_unit_file_contents = || match std::fs::read_to_string(&path) {
              Ok(contents) => contents,
              Err(e) => {
                error!("Failed to read unit file `{}`: {}", path, e);
                "".to_string()
              },
            };

            let unit_file_contents = read_unit_file_contents();
            let editor = std::env::var("EDITOR").unwrap_or_else(|_| "nano".to_string());
            match Command::new(&editor).arg(&path).status() {
              Ok(_) => {
                tui.enter()?;
                tui.clear()?;
                event = EventHandler::new(self.home.clone(), action_tx.clone());

                let new_unit_file_contents = read_unit_file_contents();
                if unit_file_contents != new_unit_file_contents {
                  action_tx.send(Action::ReloadService(unit))?;
                }

                action_tx.send(Action::EnterMode(Mode::ServiceList))?;
              },
              Err(e) => {
                tui.enter()?;
                tui.clear()?;
                event = EventHandler::new(self.home.clone(), action_tx.clone());
                action_tx.send(Action::EnterError(format!("Failed to open editor `{}`: {}", editor, e)))?;
              },
            }
          },
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
