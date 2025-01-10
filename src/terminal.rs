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
use signal_hook::{iterator::Signals, low_level};
use tokio::{
  sync::{mpsc, Mutex},
  task::JoinHandle,
};

use crate::components::{home::Home, Component};

// A struct that mostly exists to be a catch-all for terminal operations that should be synchronized
pub struct Tui {
  pub terminal: ratatui::Terminal<Backend<std::io::Stderr>>,
}

impl Tui {
  pub fn new() -> Result<Self> {
    let terminal = ratatui::Terminal::new(Backend::new(std::io::stderr()))?;

    // spin up a signal handler to catch SIGTERM and exit gracefully
    let _ = std::thread::spawn(move || {
      const SIGNALS: &[libc::c_int] = &[signal_hook::consts::signal::SIGTERM];
      let mut sigs = Signals::new(SIGNALS).unwrap();
      let signal = sigs.into_iter().next().unwrap();
      let _ = exit();
      low_level::emulate_default_handler(signal).unwrap();
    });

    Ok(Self { terminal })
  }

  pub fn enter(&self) -> Result<()> {
    crossterm::terminal::enable_raw_mode()?;
    crossterm::execute!(std::io::stderr(), EnterAlternateScreen, EnableMouseCapture, cursor::Hide)?;
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

  pub fn exit(&self) -> Result<()> {
    exit()
  }
}

// This one's public because we want to expose it to the panic handler
pub fn exit() -> Result<()> {
  crossterm::execute!(std::io::stderr(), LeaveAlternateScreen, DisableMouseCapture, cursor::Show)?;
  crossterm::terminal::disable_raw_mode()?;
  Ok(())
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
    exit().unwrap();
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
  home: Arc<Mutex<Home>>,
  pub tui: Arc<Mutex<Tui>>,
}

impl TerminalHandler {
  pub fn new(home: Arc<Mutex<Home>>) -> Self {
    let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
    let cloned_home = home.clone();
    let tui = Tui::new().context(anyhow!("Unable to create terminal")).unwrap();
    tui.enter().unwrap();
    let tui = Arc::new(Mutex::new(tui));
    let cloned_tui = tui.clone();
    let task = tokio::spawn(async move {
      loop {
        match rx.recv().await {
          Some(Message::Stop) => {
            exit().unwrap_or_default();
            break;
          },
          Some(Message::Suspend) => {
            let t = tui.lock().await;
            t.suspend().unwrap_or_default();
            break;
          },
          Some(Message::Render) => {
            let mut t = tui.lock().await;
            let mut home = home.lock().await;
            render(&mut t, &mut home);
          },
          None => {},
        }
      }
    });
    Self { task, tx, home: cloned_home, tui: cloned_tui }
  }

  pub fn suspend(&self) -> Result<()> {
    self.tx.send(Message::Suspend)?;
    Ok(())
  }

  pub fn stop(&self) -> Result<()> {
    self.tx.send(Message::Stop)?;
    Ok(())
  }

  pub async fn render(&self) {
    let mut home = self.home.lock().await;
    let mut tui = self.tui.lock().await;
    render(&mut tui, &mut home);
  }

  // little more performant in situations where we don't need to wait for the render to complete
  pub fn enqueue_render(&self) -> Result<()> {
    self.tx.send(Message::Render)?;
    Ok(())
  }
}

fn render(tui: &mut Tui, home: &mut Home) {
  tui
    .draw(|f| {
      home.render(f, f.area());
    })
    .expect("Unable to draw");
}
