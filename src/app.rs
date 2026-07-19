use std::{process::Command, sync::Arc};

use anyhow::{Context, Result};
use log::error;
use tokio::sync::{mpsc, Mutex};
use tracing::debug;

use crate::{
  action::Action,
  components::{
    home::{Home, LogOrder, Mode},
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
  pub should_quit: bool,
  pub should_suspend: bool,
}

impl App {
  pub fn new(scope: Scope, limit_units: Vec<String>, log_order: LogOrder) -> Result<Self> {
    let home = Home::new(scope, &limit_units, log_order);
    let home = Arc::new(Mutex::new(home));
    Ok(Self { scope, home, limit_units, should_quit: false, should_suspend: false })
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

    let units = get_all_services(self.scope, &self.limit_units).await.with_context(|| {
      // "run with sudo" is only sensible advice for the local failure mode; on a remote
      // host the usual cause is that systemd isn't actually running there (e.g. a distro
      // that merely has the systemd package installed)
      match crate::ssh::remote_host() {
        Some(ssh_host) => format!("Unable to get services from {}. Is systemd running on it?", ssh_host.host),
        None => "Unable to get services. Check that systemd is running and try running this tool with sudo.".into(),
      }
    })?;
    self.home.lock().await.set_units(units.units);

    // Fetch unit files (includes enablement state and disabled units not returned by ListUnits)
    action_tx.send(Action::RefreshUnitFiles)?;

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
          Action::Render => terminal.render().await,
          Action::DebouncedRender => debounce_tx.send(Action::Render).unwrap(),
          Action::Noop => {},
          Action::Quit => self.should_quit = true,
          Action::Suspend => self.should_suspend = true,
          Action::Resume => self.should_suspend = false,
          Action::Resize(_, _) => terminal.render().await,
          // This would normally be in home.rs, but it needs to do some terminal and event handling stuff that's easier here
          Action::EditUnitFile { unit, path } => {
            // The unit file lives on the remote host; we can't open it in a local editor,
            // but we can fetch it over SSH and show it read-only in the local pager
            if let Some(ssh_host) = crate::ssh::remote_host() {
              match ssh_host.command("cat", &[&path]).output() {
                Ok(output) if output.status.success() => {
                  // Name the temp file after the unit file so the pager prompt shows e.g. "docker.service"
                  let file_name = std::path::Path::new(&path)
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "unit-file.txt".into());
                  let contents = String::from_utf8_lossy(&output.stdout).into_owned();
                  show_in_pager(self.home.clone(), &terminal, &mut event, &action_tx, &file_name, &contents).await?;
                },
                Ok(output) => {
                  let stderr = String::from_utf8_lossy(&output.stderr);
                  action_tx
                    .send(Action::EnterError(format!("Failed to read unit file `{path}`: {}", stderr.trim())))?;
                },
                Err(e) => {
                  action_tx.send(Action::EnterError(format!("Failed to read unit file `{path}`: {e}")))?;
                },
              }
              continue;
            }
            event.stop();
            let mut tui = terminal.tui.lock().await;
            tui.exit()?;

            let read_unit_file_contents = || match std::fs::read_to_string(&path) {
              Ok(contents) => contents,
              Err(e) => {
                error!("Failed to read unit file `{path}`: {e}");
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
                action_tx.send(Action::EnterError(format!("Failed to open editor `{editor}`: {e}")))?;
              },
            }
          },
          Action::OpenLogsInPager { logs } => {
            show_in_pager(
              self.home.clone(),
              &terminal,
              &mut event,
              &action_tx,
              "systemctl-tui-logs.txt",
              &logs.join("\n"),
            )
            .await?;
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

/// Drop out of the TUI, show `contents` in the user's pager via a temp file named `file_name`,
/// then restore the TUI. Replaces `event` with a fresh handler (the old one is stopped).
async fn show_in_pager(
  home: Arc<Mutex<Home>>,
  terminal: &TerminalHandler,
  event: &mut EventHandler,
  action_tx: &mpsc::UnboundedSender<Action>,
  file_name: &str,
  contents: &str,
) -> Result<()> {
  event.stop();
  let mut tui = terminal.tui.lock().await;
  tui.exit()?;

  let temp_path = std::env::temp_dir().join(file_name);
  if let Err(e) = std::fs::write(&temp_path, contents) {
    tui.enter()?;
    tui.clear()?;
    *event = EventHandler::new(home, action_tx.clone());
    action_tx.send(Action::EnterError(format!("Failed to write temp file: {e}")))?;
  } else {
    let pager = std::env::var("PAGER").unwrap_or_else(|_| "less".to_string());
    match Command::new(&pager).arg(&temp_path).status() {
      Ok(_) => {
        tui.enter()?;
        tui.clear()?;
        *event = EventHandler::new(home, action_tx.clone());
        action_tx.send(Action::EnterMode(Mode::ServiceList))?;
      },
      Err(e) => {
        tui.enter()?;
        tui.clear()?;
        *event = EventHandler::new(home, action_tx.clone());
        action_tx.send(Action::EnterError(format!("Failed to open pager `{pager}`: {e}")))?;
      },
    }
    let _ = std::fs::remove_file(&temp_path);
  }
  Ok(())
}
