use std::{sync::Arc, time::Duration};

use crossterm::event::{Event as CrosstermEvent, KeyEvent, KeyEventKind, MouseEvent};
use futures::{FutureExt, StreamExt};
use tokio::{
  sync::{mpsc, Mutex},
  task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

use crate::{
  action::Action,
  components::{home::Home, Component},
};

#[derive(Clone, Copy, Debug)]
pub enum Event {
  Quit,
  Error,
  Closed,
  RenderTick,
  RefreshTick,
  Key(KeyEvent),
  Mouse(MouseEvent),
  Resize(u16, u16),
}

pub struct EventHandler {
  pub task: JoinHandle<()>,
  cancellation_token: CancellationToken,
}

const SERVICE_REFRESH_INTERVAL_MS: u64 = 5000;

impl EventHandler {
  pub fn new(home: Arc<Mutex<Home>>, action_tx: mpsc::UnboundedSender<Action>) -> Self {
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let cancellation_token = CancellationToken::new();
    let _cancellation_token = cancellation_token.clone();
    let task = tokio::spawn(async move {
      let mut reader = crossterm::event::EventStream::new();
      let mut refresh_services_interval = tokio::time::interval(Duration::from_millis(SERVICE_REFRESH_INTERVAL_MS));
      refresh_services_interval.tick().await;
      loop {
        let refresh_delay = refresh_services_interval.tick();
        let crossterm_event = reader.next().fuse();
        tokio::select! {
          _ = _cancellation_token.cancelled() => {
            break;
          }
          maybe_event = crossterm_event => {
            match maybe_event {
              Some(Ok(evt)) => {
                match evt {
                  CrosstermEvent::Key(key) => {
                    if key.kind == KeyEventKind::Press {
                      event_tx.send(Event::Key(key)).unwrap();
                    }
                  },
                  // interestingly, we never get these if running in dev mode with watchexec
                  CrosstermEvent::Resize(x, y) => {
                    event_tx.send(Event::Resize(x, y)).unwrap();
                  },
                  _ => {},
                }
              }
              Some(Err(_)) => {
                event_tx.send(Event::Error).unwrap();
              }
              None => {},
            }
          },
          _ = refresh_delay => {
            event_tx.send(Event::RefreshTick).unwrap();
          },
          event = event_rx.recv() => {
            let actions = home.lock().await.handle_events(event);
            for action in actions {
              action_tx.send(action).unwrap();
            }
          }
        }
      }
    });
    Self { task, cancellation_token }
  }

  pub fn stop(&mut self) {
    self.cancellation_token.cancel();
  }
}
