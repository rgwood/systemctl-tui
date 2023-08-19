use std::{
  ops::{Deref, DerefMut},
  sync::Arc,
};

use anyhow::{anyhow, Context, Result};
use crossterm::{
  cursor,
  event::{DisableMouseCapture, EnableMouseCapture},
  terminal::{EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::backend::CrosstermBackend as Backend;
use tokio::{
  sync::{mpsc, Mutex},
  task::JoinHandle,
};

use crate::components::{home::Home, Component};

pub type Frame<'a> = ratatui::Frame<'a, Backend<std::io::Stderr>>;

pub struct Tui {
  pub terminal: ratatui::Terminal<Backend<std::io::Stderr>>,
}

impl Tui {
  pub fn new() -> Result<Self> {
    let terminal = ratatui::Terminal::new(Backend::new(std::io::stderr()))?;
    Ok(Self { terminal })
  }

  pub fn enter(&self) -> Result<()> {
    crossterm::terminal::enable_raw_mode()?;
    crossterm::execute!(std::io::stderr(), EnterAlternateScreen, EnableMouseCapture, cursor::Hide)?;
    Ok(())
  }

  pub fn exit(&self) -> Result<()> {
    crossterm::execute!(std::io::stderr(), LeaveAlternateScreen, DisableMouseCapture, cursor::Show)?;
    crossterm::terminal::disable_raw_mode()?;
    Ok(())
  }

  pub fn suspend(&self) -> Result<()> {
    self.exit()?;
    #[cfg(not(windows))]
    signal_hook::low_level::raise(signal_hook::consts::signal::SIGTSTP)?;
    Ok(())
  }

  pub fn resume(&self) -> Result<()> {
    self.enter()?;
    Ok(())
  }
}

impl Deref for Tui {
  type Target = ratatui::Terminal<Backend<std::io::Stderr>>;

  fn deref(&self) -> &Self::Target {
    &self.terminal
  }
}

impl DerefMut for Tui {
  fn deref_mut(&mut self) -> &mut Self::Target {
    &mut self.terminal
  }
}

impl Drop for Tui {
  fn drop(&mut self) {
    self.exit().unwrap();
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Message {
  Render,
  Stop,
  Suspend,
}

pub struct TerminalHandler {
  pub task: JoinHandle<()>,
  tx: mpsc::UnboundedSender<Message>,
}

impl TerminalHandler {
  pub fn new(home: Arc<Mutex<Home>>) -> Self {
    let (tx, mut rx) = mpsc::unbounded_channel::<Message>();

    let task = tokio::spawn(async move {
      let mut t = Tui::new().context(anyhow!("Unable to create terminal")).unwrap();
      t.enter().unwrap();
      loop {
        match rx.recv().await {
          Some(Message::Stop) => {
            t.exit().unwrap_or_default();
            break;
          },
          Some(Message::Suspend) => {
            t.suspend().unwrap_or_default();
            break;
          },
          Some(Message::Render) => {
            let mut h = home.lock().await;
            t.draw(|f| {
              h.render(f, f.size());
            })
            .unwrap();
          },
          None => {},
        }
      }
    });
    Self { task, tx }
  }

  pub fn suspend(&self) -> Result<()> {
    self.tx.send(Message::Suspend)?;
    Ok(())
  }

  pub fn stop(&self) -> Result<()> {
    self.tx.send(Message::Stop)?;
    Ok(())
  }

  pub fn render(&self) -> Result<()> {
    self.tx.send(Message::Render)?;
    Ok(())
  }
}
