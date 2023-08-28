use anyhow::Result;
use crossterm::event::{KeyEvent, MouseEvent};
use ratatui::layout::Rect;
use tokio::sync::mpsc::UnboundedSender;

use crate::{action::Action, event::Event, terminal::Frame};

pub mod home;
pub mod logger;

pub trait Component {
  #[allow(unused_variables)]
  fn init(&mut self, tx: UnboundedSender<Action>) -> Result<()> {
    Ok(())
  }
  fn handle_events(&mut self, event: Option<Event>) -> Vec<Action> {
    match event {
      Some(Event::Quit) => vec![Action::Quit],
      Some(Event::RenderTick) => vec![Action::Render],
      Some(Event::Key(key_event)) => self.handle_key_events(key_event),
      Some(Event::Mouse(mouse_event)) => self.handle_mouse_events(mouse_event),
      Some(Event::Resize(x, y)) => vec![Action::Resize(x, y)],
      Some(Event::RefreshTick) => vec![Action::RefreshServicesAndLog],
      Some(_) => vec![],
      None => vec![],
    }
  }
  #[allow(unused_variables)]
  fn handle_key_events(&mut self, key: KeyEvent) -> Vec<Action> {
    vec![]
  }
  #[allow(unused_variables)]
  fn handle_mouse_events(&mut self, mouse: MouseEvent) -> Vec<Action> {
    vec![]
  }
  #[allow(unused_variables)]
  fn dispatch(&mut self, action: Action) -> Option<Action> {
    None
  }
  fn render(&mut self, f: &mut Frame<'_>, rect: Rect);
}
