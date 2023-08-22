use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use duct::cmd;
use itertools::Itertools;
use ratatui::{
  layout::{Constraint, Direction, Layout, Rect},
  style::{Color, Modifier, Style},
  text::{Line, Span},
  widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};
use tokio::sync::mpsc::{self, UnboundedSender};
use tracing::{info, warn};
use tui_input::{backend::crossterm::EventHandler, Input};

use super::{logger::Logger, Component, Frame};
use crate::{action::Action, systemd::UnitStatus};

#[derive(Default, Copy, Clone, PartialEq, Eq)]
pub enum Mode {
  Normal,
  #[default]
  Search,
  Help,
}

#[derive(Default)]
pub struct Home {
  pub logger: Logger,
  pub show_logger: bool,
  pub all_units: Vec<UnitStatus>,
  pub filtered_units: StatefulList<UnitStatus>,
  pub logs: Vec<String>,
  pub logs_scroll_offset: u16,
  pub mode: Mode,
  pub input: Input,
  pub action_tx: Option<mpsc::UnboundedSender<Action>>,
  pub journalctl_tx: Option<std::sync::mpsc::Sender<String>>,
}

pub struct StatefulList<T> {
  state: ListState,
  items: Vec<T>,
}

impl Default for StatefulList<UnitStatus> {
  fn default() -> Self {
    Self::with_items(vec![])
  }
}

impl<T> StatefulList<T> {
  pub fn with_items(items: Vec<T>) -> StatefulList<T> {
    StatefulList { state: ListState::default(), items }
  }

  fn selected(&self) -> Option<&T> {
    if self.items.is_empty() {
      return None;
    }
    match self.state.selected() {
      Some(i) => Some(&self.items[i]),
      None => None,
    }
  }

  fn next(&mut self) {
    let i = match self.state.selected() {
      Some(i) => {
        if i >= self.items.len().saturating_sub(1) {
          0
        } else {
          i + 1
        }
      },
      None => 0,
    };
    self.state.select(Some(i));
  }

  fn previous(&mut self) {
    let i = match self.state.selected() {
      Some(i) => {
        if i == 0 {
          self.items.len() - 1
        } else {
          i - 1
        }
      },
      None => 0,
    };
    self.state.select(Some(i));
  }

  fn unselect(&mut self) {
    self.state.select(None);
  }
}

impl Home {
  pub fn new() -> Self {
    Self::default()
  }

  pub fn set_units(&mut self, units: Vec<UnitStatus>) {
    self.all_units = units.clone();
    self.filtered_units = StatefulList::with_items(units);

    if self.filtered_units.items.len() > 0 {
      // select the first item automatically
      self.next();
    }
  }

  pub fn set_filtered_units(&mut self, units: Vec<UnitStatus>) {
    self.filtered_units = StatefulList::with_items(units);

    // if self.filtered_units.items.len() > 0 {
    // select the first item automatically
    self.next();
    // }
  }

  pub fn next(&mut self) {
    self.logs = vec![];
    self.filtered_units.next();
    self.get_logs();
    self.logs_scroll_offset = 0;
  }

  pub fn previous(&mut self) {
    self.logs = vec![];
    self.filtered_units.previous();
    self.get_logs();
    self.logs_scroll_offset = 0;
  }

  pub fn unselect(&mut self) {
    self.logs = vec![];
    self.filtered_units.unselect();
  }

  pub fn get_logs(&mut self) {
    if let Some(selected) = self.filtered_units.selected() {
      let unit_name = selected.name.to_string();
      if let Err(e) = self.journalctl_tx.as_ref().unwrap().send(unit_name) {
        warn!("Error sending unit name to journalctl thread: {}", e);
      }
    } else {
      self.logs = vec![];
    }
  }
}

impl Component for Home {
  fn init(&mut self, tx: UnboundedSender<Action>) -> anyhow::Result<()> {
    self.action_tx = Some(tx.clone());
    let (journalctl_tx, journalctl_rx) = std::sync::mpsc::channel::<String>();
    self.journalctl_tx = Some(journalctl_tx);

    tokio::task::spawn_blocking(move || {
      loop {
        let mut unit_name: String = match journalctl_rx.recv() {
          Ok(unit) => unit,
          Err(_) => return,
        };

        // drain the channel, use the last value
        while let Ok(service) = journalctl_rx.try_recv() {
          info!("Skipping logs for {}...", unit_name);
          unit_name = service;
        }

        info!("Getting logs for {}", unit_name);
        let start = std::time::Instant::now();
        match cmd!("journalctl", "-u", unit_name.clone(), "--output=short-iso", "--lines=500").read() {
          Ok(stdout) => {
            info!("Got logs for {} in {:?}", unit_name, start.elapsed());
            let _ = tx.send(Action::SetLogs { unit_name, logs: stdout });
          },
          Err(e) => warn!("Error getting logs for {}: {}", unit_name, e),
        }
      }
    });
    Ok(())
  }

  fn handle_key_events(&mut self, key: KeyEvent) -> Action {
    if key.modifiers.contains(KeyModifiers::CONTROL) {
      match key.code {
        KeyCode::Char('c') => return Action::Quit,
        KeyCode::Char('d') => return Action::Quit,
        KeyCode::Char('q') => return Action::Quit,
        KeyCode::Char('z') => return Action::Suspend,
        KeyCode::Char('f') => return Action::EnterSearch,
        KeyCode::Char('l') => return Action::ToggleShowLogger,
        _ => (),
      }
    }

    if matches!(key.code, KeyCode::Char('?')) || matches!(key.code, KeyCode::F(1)) {
      return Action::ToggleHelp;
    }

    let increment = if key.modifiers.contains(KeyModifiers::SHIFT) { 10 } else { 1 };
    if matches!(key.code, KeyCode::PageDown) {
      return Action::ScrollDown(increment);
    }
    if matches!(key.code, KeyCode::PageUp) {
      return Action::ScrollUp(increment);
    }

    match self.mode {
      Mode::Normal | Mode::Help => {
        match key.code {
          KeyCode::Esc | KeyCode::Enter => Action::EnterNormal,
          KeyCode::Char('q') => Action::Quit,

          KeyCode::Up => {
            // if we're filtering the list, and we're at the top, and there's text in the search box, go to search mode
            if self.filtered_units.state.selected() == Some(0) {
              return Action::EnterSearch;
            }

            self.previous();
            Action::Update // is this right?
          },
          KeyCode::Down => {
            self.next();
            Action::Update // is this right?
          },
          KeyCode::Char('/') => Action::EnterSearch,
          _ => Action::Noop,
        }
      },
      Mode::Search => match key.code {
        KeyCode::Esc | KeyCode::Enter => Action::EnterNormal,
        KeyCode::Down | KeyCode::Tab => {
          self.next();
          Action::EnterNormal
        },
        KeyCode::Up => {
          self.previous();
          Action::EnterNormal
        },
        _ => {
          let prev_search_value = self.input.value().to_owned();
          self.input.handle_event(&crossterm::event::Event::Key(key));

          // if the search value changed, filter the list
          if prev_search_value != self.input.value() {
            let search_value_lower = self.input.value().to_lowercase();
            let matching = self
              .all_units
              .iter()
              .filter(|u| u.name.to_lowercase().contains(&search_value_lower))
              .cloned()
              .collect_vec();
            self.set_filtered_units(matching);
          }
          Action::Update
        },
      },
    }
  }

  fn dispatch(&mut self, action: Action) -> Option<Action> {
    match action {
      Action::ToggleShowLogger => self.show_logger = !self.show_logger,
      Action::EnterNormal => {
        self.mode = Mode::Normal;
      },
      Action::EnterSearch => {
        self.mode = Mode::Search;
      },
      Action::ToggleHelp => {
        if self.mode != Mode::Help {
          self.mode = Mode::Help;
        } else {
          // TODO: go back to the previous mode
          self.mode = Mode::Normal;
        }
      },
      Action::SetLogs { unit_name: service_name, logs } => {
        if let Some(selected) = self.filtered_units.selected() {
          if selected.name == service_name {
            // split by lines
            let mut logs = logs.split("\n").map(String::from).collect_vec();
            logs.reverse();
            self.logs = logs;
          }
        }
      },
      Action::ScrollUp(offset) => {
        self.logs_scroll_offset = self.logs_scroll_offset.saturating_sub(offset);
        info!("scroll offset: {}", self.logs_scroll_offset);
      },
      Action::ScrollDown(offset) => {
        self.logs_scroll_offset = self.logs_scroll_offset.saturating_add(offset);
        info!("scroll offset: {}", self.logs_scroll_offset);
      },
      _ => (),
    }
    None
  }

  fn render(&mut self, f: &mut Frame<'_>, rect: Rect) {
    let rect = if self.show_logger {
      let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(rect);
      self.logger.render(f, chunks[1]);
      chunks[0]
    } else {
      rect
    };

    let rects = Layout::default().constraints([Constraint::Min(3), Constraint::Percentage(100)].as_ref()).split(rect);
    let search_panel = rects[0];
    let main_panel = rects[1];

    let items: Vec<ListItem> = self.filtered_units.items.iter().map(|i| ListItem::new(i.short_name())).collect();

    // Create a List from all list items and highlight the currently selected one
    let items = List::new(items)
      .block(
        Block::default()
          .borders(Borders::ALL)
          .border_style(if self.mode == Mode::Normal {
            Style::default().fg(Color::LightGreen)
          } else {
            Style::default()
          })
          .title("Services"),
      )
      .highlight_style(Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD));

    let chunks = Layout::default()
      .direction(Direction::Horizontal)
      .constraints([Constraint::Min(40), Constraint::Percentage(100)].as_ref())
      .split(main_panel);
    let right_panel = chunks[1];

    f.render_stateful_widget(items, chunks[0], &mut self.filtered_units.state);

    let selected_item = self.filtered_units.selected();

    let right_panel = Layout::default()
      .direction(Direction::Vertical)
      .constraints([Constraint::Min(7), Constraint::Percentage(100)].as_ref())
      .split(right_panel);

    let details_panel = right_panel[0];
    let logs_panel = right_panel[1];

    let details_block = Block::default().title("Details").borders(Borders::ALL);
    let details_panel_panes = Layout::default()
      .direction(Direction::Horizontal)
      .constraints([Constraint::Min(14), Constraint::Percentage(100)].as_ref())
      .split(details_block.inner(details_panel));
    let props_pane = details_panel_panes[0];
    let values_pane = details_panel_panes[1];

    let props_lines = vec![
      Line::from("Description: "),
      Line::from("Load State: "),
      Line::from("Active State: "),
      Line::from("Sub State: "),
      Line::from("Path: "),
    ];

    let details_text = if let Some(i) = selected_item {
      fn line_color<'a>(value: &'a str, color: Color) -> Line<'a> {
        Line::from(vec![Span::styled(value, Style::default().fg(color))])
      }

      let load_color = match i.load_state.as_str() {
        "loaded" => Color::Green,
        "error" => Color::Red,
        _ => Color::Black,
      };

      let active_color = match i.active_state.as_str() {
        "active" => Color::Green,
        "inactive" => Color::Red,
        _ => Color::Black,
      };

      let sub_color = match i.sub_state.as_str() {
        "running" => Color::Green,
        "exited" => Color::Red,
        _ => Color::Black,
      };

      let lines = vec![
        line_color(&i.description, Color::White),
        line_color(&i.load_state, load_color),
        line_color(&i.active_state, active_color),
        line_color(&i.sub_state, sub_color),
        line_color(&i.path, Color::White),
      ];

      lines
    } else {
      vec![]
    };

    let paragraph = Paragraph::new(details_text).style(Style::default()).wrap(Wrap { trim: true });

    let props_widget = Paragraph::new(props_lines).alignment(ratatui::layout::Alignment::Right);
    f.render_widget(props_widget, props_pane);

    f.render_widget(paragraph, values_pane);
    f.render_widget(details_block, details_panel);

    let log_lines = self
      .logs
      .iter()
      .map(|l| {
        if let Some((date, rest)) = l.splitn(2, " ").collect_tuple() {
          if date.len() != 24 {
            return Line::from(l.as_str());
          }
          Line::from(vec![Span::styled(date, Style::default().fg(Color::DarkGray)), Span::raw(" "), Span::raw(rest)])
        } else {
          Line::from(l.as_str())
        }
      })
      .collect_vec();

    let paragraph = Paragraph::new(log_lines)
      .block(Block::default().title("Logs").borders(Borders::ALL))
      .style(Style::default())
      .wrap(Wrap { trim: true })
      .scroll((self.logs_scroll_offset, 0));
    f.render_widget(paragraph, logs_panel);

    let width = search_panel.width.max(3) - 3; // keep 2 for borders and 1 for cursor
    let scroll = self.input.visual_scroll(width as usize);
    let input = Paragraph::new(self.input.value())
      .style(match self.mode {
        Mode::Search => Style::default().fg(Color::LightGreen),
        _ => Style::default(),
      })
      .scroll((0, scroll as u16))
      .block(Block::default().borders(Borders::ALL).title(Line::from(vec![
        Span::raw(" 🔍️ Search "),
        Span::styled("(", Style::default().fg(Color::DarkGray)),
        Span::styled("ctrl+f", Style::default().add_modifier(Modifier::BOLD).fg(Color::Gray)),
        Span::styled(" or ", Style::default().fg(Color::DarkGray)),
        Span::styled("/", Style::default().add_modifier(Modifier::BOLD).fg(Color::Gray)),
        Span::styled(" to focus", Style::default().fg(Color::DarkGray)),
        Span::styled(") ", Style::default().fg(Color::DarkGray)),
      ])));
    f.render_widget(input, search_panel);
    // clear top right of search panel so we can put help instructions there
    let help_width = 24;
    let help_area = Rect::new(search_panel.x + search_panel.width - help_width - 2, search_panel.y, help_width, 1);
    f.render_widget(Clear, help_area);
    let help_text = Paragraph::new(Line::from(vec![
      Span::raw(" Press "),
      Span::styled("?", Style::default().add_modifier(Modifier::BOLD).fg(Color::Gray)),
      Span::raw(" or "),
      Span::styled("F1", Style::default().add_modifier(Modifier::BOLD).fg(Color::Gray)),
      Span::raw(" for help "),
    ]))
    .style(Style::default().fg(Color::DarkGray));
    f.render_widget(help_text, help_area);

    if self.mode == Mode::Search {
      f.set_cursor(
        (search_panel.x + 1 + self.input.cursor() as u16).min(search_panel.x + search_panel.width - 2),
        search_panel.y + 1,
      )
    }

    if self.mode == Mode::Help {
      let popup = centered_rect(80, 80, f.size());

      fn white(s: &str) -> Span {
        Span::styled(s, Style::default().fg(Color::White))
      }

      let help_lines = vec![
        Line::from(""),
        Line::from(Span::styled("Keyboard Shortcuts", Style::default().add_modifier(Modifier::UNDERLINED))),
        Line::from(""),
        Line::from(vec![white("CTRL+L"), Span::raw(" toggles the logger pane")]),
        Line::from(vec![
          white("CTRL+C"),
          Span::raw(" / "),
          white("CTRL+D"),
          Span::raw(" / "),
          white("CTRL+Q"),
          Span::raw(" quits the application"),
        ]),
        Line::from(vec![white("PageUp"), Span::raw(" / "), white("PageDown"), Span::raw(" scrolls the logs")]),
        Line::from(vec![white("?"), Span::raw(" or "), white("F1"), Span::raw(" opens this help pane")]),
      ];

      let name = env!("CARGO_PKG_NAME");
      let version = env!("CARGO_PKG_VERSION");
      let title = format!("✨️ Help for {} v{} ✨️", name, version);

      let paragraph = Paragraph::new(help_lines)
        .block(Block::default().title(title).borders(Borders::ALL))
        .style(Style::default())
        .wrap(Wrap { trim: true });

      f.render_widget(Clear, popup);
      f.render_widget(paragraph, popup);
    }
  }
}

/// helper function to create a centered rect using up certain percentage of the available rect `r`
fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
  let popup_layout = Layout::default()
    .direction(Direction::Vertical)
    .constraints(
      [
        Constraint::Percentage((100 - percent_y) / 2),
        Constraint::Percentage(percent_y),
        Constraint::Percentage((100 - percent_y) / 2),
      ]
      .as_ref(),
    )
    .split(r);

  Layout::default()
    .direction(Direction::Horizontal)
    .constraints(
      [
        Constraint::Percentage((100 - percent_x) / 2),
        Constraint::Percentage(percent_x),
        Constraint::Percentage((100 - percent_x) / 2),
      ]
      .as_ref(),
    )
    .split(popup_layout[1])[1]
}
