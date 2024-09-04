use std::{sync::Arc, time::Duration};

use anyhow::{Context, Result};
use tokio::{
  sync::{mpsc, Mutex},
  time::sleep,
};
use tracing::debug;

use crate::{
  action::Action,
  components::{home::Home, Component},
  event::EventHandler,
  systemd::{self, get_services_from_list_units, ActivationState, Scope, UnitWithStatus},
  terminal::TerminalHandler,
};

pub struct App {
  pub scope: Scope,
  pub home: Arc<Mutex<Home>>,
  pub limit_units: Vec<String>,
  pub should_quit: bool,
  pub should_suspend: bool,
}

impl App {
  pub fn new(scope: Scope, limit_units: Vec<String>) -> Result<Self> {
    let home = Home::new(scope, &limit_units);
    let home = Arc::new(Mutex::new(home));
    Ok(Self { scope, home, limit_units, should_quit: false, should_suspend: false })
  }

  pub async fn run(&mut self) -> Result<()> {
    let (action_tx, mut action_rx) = mpsc::unbounded_channel();

    let (debounce_tx, mut debounce_rx) = mpsc::unbounded_channel();

    // Start a background task to send render events, debounced
    // TODO: I believe we can remove this, no longer used
    tokio::spawn({
      let action_tx = action_tx.clone();
      async move {
        let debounce_duration = std::time::Duration::from_millis(0);
        let debouncing = Arc::new(Mutex::new(false));

        loop {
          let _ = debounce_rx.recv().await;

          if *debouncing.lock().await {
            continue;
          }

          *debouncing.lock().await = true;

          let action_tx = action_tx.clone();
          let debouncing = debouncing.clone();
          tokio::spawn(async move {
            tokio::time::sleep(debounce_duration).await;
            let _ = action_tx.send(Action::Render);
            *debouncing.lock().await = false;
          });
        }
      }
    });

    self.home.lock().await.init(action_tx.clone())?;

    let units = get_services_from_list_units(self.scope, &self.limit_units)
      .await
      .context("Unable to get services. Check that systemd is running and try running this tool with sudo.")?;
    self.home.lock().await.set_units(units);

    let home_cloned = self.home.clone();

    // Start a background task to update services based on the ListUnitFiles dbus call
    tokio::spawn({
      let scope = self.scope.clone();
      let action_tx = action_tx.clone();

      async move {
        loop {
          let unit_files = systemd::get_unit_files(scope).await;
          match unit_files {
            Ok(unit_files) => {
              let mut home = home_cloned.lock().await;
              let all_units = &mut home.all_units;

              for service in unit_files {
                let id = service.id();
                // info!("id: {:?}", id);
                if let Some(unit) = all_units.get_mut(&id) {
                  unit.enablement_state = Some(service.enablement_state);
                  unit.file_path = Some(service.path);
                } else if service.enablement_state == "disabled" {
                  // only adding disabled services because static/generated/masked services seem uninteresting
                  // TODO: check with power users if this is the right approach
                  let new_unit = UnitWithStatus {
                    name: service.name,
                    scope: service.scope,
                    description: "".into(),
                    file_path: Some(service.path),
                    load_state: "unknown".into(),
                    activation_state: ActivationState::Unknown,
                    sub_state: "".into(),
                    enablement_state: Some(service.enablement_state),
                  };

                  all_units.insert(id, new_unit);
                }
              }
              home.sort_units();
              home.refresh_filtered_units();
            },
            Err(e) => {
              tracing::error!("Failed to get services: {:?}", e);
            },
          }

          action_tx.send(Action::Render).unwrap();

          // TODO: figure out the right timing
          sleep(Duration::from_secs(5)).await;
        }
      }
    });

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
            debug!("Render took {:?}", duration);
            crate::utils::log_perf_event("render", duration);
          },
          Action::DebouncedRender => debounce_tx.send(Action::Render).unwrap(),
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
