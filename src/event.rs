use std::sync::Arc;

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
  AppTick,
  Key(KeyEvent),
  Mouse(MouseEvent),
  Resize(u16, u16),
}

pub struct EventHandler {
  pub task: JoinHandle<()>,
  cancellation_token: CancellationToken,
}

impl EventHandler {
  pub fn new(tick_rate: (u64, u64), home: Arc<Mutex<Home>>, action_tx: mpsc::UnboundedSender<Action>) -> Self {
    let (app_tick_rate, render_tick_rate) = tick_rate;

    let (event_tx, mut event_rx) = mpsc::unbounded_channel();

    let app_tick_rate = std::time::Duration::from_millis(app_tick_rate);
    let render_tick_rate = std::time::Duration::from_millis(render_tick_rate);

    let cancellation_token = CancellationToken::new();
    let _cancellation_token = cancellation_token.clone();
    let task = tokio::spawn(async move {
      let mut reader = crossterm::event::EventStream::new();
      let mut app_interval = tokio::time::interval(app_tick_rate);
      let mut render_interval = tokio::time::interval(render_tick_rate);
      loop {
        let app_delay = app_interval.tick();
        let render_delay = render_interval.tick();
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
          _ = app_delay => {
              event_tx.send(Event::AppTick).unwrap();
          },
          _ = render_delay => {
              event_tx.send(Event::RenderTick).unwrap();
          },
          event = event_rx.recv() => {
            let action = home.lock().await.handle_events(event);
            action_tx.send(action).unwrap();
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
