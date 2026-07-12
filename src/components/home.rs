use chrono::DateTime;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use futures::Future;
use fuzzy_matcher::{skim::SkimMatcherV2, FuzzyMatcher};
use indexmap::IndexMap;
use itertools::Itertools;
use ratatui::{
  layout::{Constraint, Direction, Layout, Margin, Position, Rect},
  style::{Color, Modifier, Style},
  text::{Line, Span},
  widgets::{Block, BorderType, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};
use tokio::{
  io::AsyncBufReadExt,
  sync::mpsc::{self, UnboundedSender},
  task::JoinHandle,
};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use tui_input::{backend::crossterm::EventHandler, Input};

use std::{collections::HashSet, process::Stdio, time::Duration};

use super::{logger::Logger, Component, Frame};
use crate::{
  action::Action,
  systemd::{
    self, diagnose_missing_logs, parse_journalctl_error, Scope, UnitFile, UnitId, UnitRuntimeInfo, UnitScope,
    UnitWithStatus,
  },
};

#[derive(Debug, Default, Copy, Clone, PartialEq)]
pub enum Mode {
  #[default]
  Search,
  ServiceList,
  Help,
  ActionMenu,
  Processing,
  Error,
  SignalMenu,
  StatusFilter,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash)]
pub enum UnitStatus {
  // Activation state
  Active,
  Inactive,
  Failed,
  // Enablement state
  Enabled,
  Disabled,
  Static,
  Masked,
  // Load state
  Loaded,
  NotFound,
}

impl UnitStatus {
  const ALL: [UnitStatus; 9] = [
    UnitStatus::Active,
    UnitStatus::Inactive,
    UnitStatus::Failed,
    UnitStatus::Enabled,
    UnitStatus::Disabled,
    UnitStatus::Static,
    UnitStatus::Masked,
    UnitStatus::Loaded,
    UnitStatus::NotFound,
  ];

  const NONE: [UnitStatus; 0] = [];

  fn label(&self) -> &'static str {
    match self {
      UnitStatus::Active => "active",
      UnitStatus::Inactive => "inactive",
      UnitStatus::Failed => "failed",
      UnitStatus::Enabled => "enabled",
      UnitStatus::Disabled => "disabled",
      UnitStatus::Static => "static",
      UnitStatus::Masked => "masked",
      UnitStatus::Loaded => "loaded",
      UnitStatus::NotFound => "not-found",
    }
  }

  fn shortcut_key(&self) -> KeyCode {
    match self {
      UnitStatus::Active => KeyCode::Char('c'),
      UnitStatus::Inactive => KeyCode::Char('i'),
      UnitStatus::Failed => KeyCode::Char('f'),
      UnitStatus::Enabled => KeyCode::Char('e'),
      UnitStatus::Disabled => KeyCode::Char('d'),
      UnitStatus::Static => KeyCode::Char('s'),
      UnitStatus::Masked => KeyCode::Char('m'),
      UnitStatus::Loaded => KeyCode::Char('l'),
      UnitStatus::NotFound => KeyCode::Char('u'),
    }
  }

  /// Map a unit's ActiveState to a filter bucket. Transitional states
  /// (activating/deactivating) are momentary, so they get lumped into the
  /// nearest stable bucket rather than getting their own checkboxes.
  fn activation_bucket(state: &str) -> UnitStatus {
    match state {
      "active" | "reloading" => UnitStatus::Active,
      "failed" => UnitStatus::Failed,
      // inactive, activating, deactivating, maintenance
      _ => UnitStatus::Inactive,
    }
  }

  /// Map a unit's UnitFileState to a filter bucket. Unknown or future states
  /// return None, meaning the unit is never filtered out on enablement.
  fn enablement_bucket(state: &str) -> Option<UnitStatus> {
    match state {
      // alias/indirect/linked follow another unit's enablement, so "enabled"
      // is the closest fit
      "enabled" | "enabled-runtime" | "alias" | "indirect" | "linked" | "linked-runtime" => Some(UnitStatus::Enabled),
      // no [Install] section / not user-managed
      "static" | "generated" | "transient" => Some(UnitStatus::Static),
      "disabled" => Some(UnitStatus::Disabled),
      "masked" | "masked-runtime" => Some(UnitStatus::Masked),
      _ => None,
    }
  }

  /// Map a unit's LoadState to a filter bucket. A "masked" load state counts
  /// as loaded here; visibility of masked units is governed by the Masked
  /// enablement filter instead.
  fn load_bucket(state: &str) -> UnitStatus {
    match state {
      "not-found" | "bad-setting" | "error" => UnitStatus::NotFound,
      _ => UnitStatus::Loaded,
    }
  }
}

const STATUS_CATEGORIES: &[(&str, &[UnitStatus])] = &[
  ("Activation", &[UnitStatus::Active, UnitStatus::Inactive, UnitStatus::Failed]),
  ("Enablement", &[UnitStatus::Enabled, UnitStatus::Disabled, UnitStatus::Static, UnitStatus::Masked]),
  ("Load", &[UnitStatus::Loaded, UnitStatus::NotFound]),
];

#[derive(Clone, Copy)]
pub struct Theme {
  pub primary: Color,   // Cyan (dark) / Blue (light) - used in help popup
  pub accent: Color,    // LightGreen (dark) / Green (light) - borders
  pub kbd: Color,       // Gray (dark, appears white-ish) / Blue (light) - keyboard shortcuts
  pub muted: Color,     // Gray (dark) / DarkGray (light)
  pub muted_alt: Color, // DarkGray (dark) / Reset (light)
}

impl Default for Theme {
  fn default() -> Self {
    Self::detect()
  }
}

impl Theme {
  pub fn detect() -> Self {
    let is_light = terminal_light::luma().is_ok_and(|luma| luma > 0.5);

    if is_light {
      Self {
        primary: Color::Blue,
        accent: Color::Green,
        kbd: Color::Blue,
        muted: Color::DarkGray,
        muted_alt: Color::Reset,
      }
    } else {
      Self {
        primary: Color::Cyan,
        accent: Color::LightGreen,
        kbd: Color::Gray, // appears white-ish when bold on dark terminals
        muted: Color::Gray,
        muted_alt: Color::DarkGray,
      }
    }
  }
}

/// A unit with fuzzy match indices for highlighting
#[derive(Clone)]
pub struct MatchedUnit {
  pub unit: UnitWithStatus,
  pub match_indices: Vec<usize>,
}

#[derive(Default)]
pub struct Home {
  pub scope: Scope,
  pub limit_units: Vec<String>,
  pub theme: Theme,
  pub logger: Logger,
  pub show_logger: bool,
  pub all_units: IndexMap<UnitId, UnitWithStatus>,
  pub filtered_units: StatefulList<MatchedUnit>,
  pub logs: Vec<String>,
  pub logs_scroll_offset: u16,
  /// Runtime info for the currently selected unit, fetched lazily after selection
  pub runtime_info: Option<UnitRuntimeInfo>,
  pub mode: Mode,
  pub previous_mode: Option<Mode>,
  pub input: Input,
  pub menu_items: StatefulList<MenuItem>,
  pub cancel_token: Option<CancellationToken>,
  pub spinner_tick: u8,
  pub error_message: String,
  pub action_tx: Option<mpsc::UnboundedSender<Action>>,
  pub journalctl_tx: Option<std::sync::mpsc::Sender<UnitId>>,
  pub fuzzy_matcher: SkimMatcherV2,
  pub filtered_statuses: HashSet<UnitStatus>,
  pub filter_cursor: usize,
  /// Inner (border-excluded) area of the logs panel, as of the most recent render.
  pub logs_panel_inner: Rect,
  /// Area of the services list panel, as of the most recent render.
  pub services_panel: Rect,
  /// Active mouse-drag text selection in the logs panel: (anchor, cursor), in absolute screen
  /// coordinates.
  pub logs_selection: Option<(Position, Position)>,
  /// Text extracted from the current `logs_selection`, computed at render time.
  pub selected_log_text: String,
  /// Most recent mouse position, in absolute screen coordinates.
  pub mouse_position: Position,
  /// Copyable fields in the details pane (rect on screen -> full text to copy), as of the most
  /// recent render.
  pub copyable_fields: Vec<(Rect, String)>,
  /// Area of the search input panel, as of the most recent render.
  pub search_panel: Rect,
  /// A transient toast message and when it was shown.
  pub toast: Option<(String, std::time::Instant)>,
  /// Screen rects of each filter item line in the status filter popup, paired with the filter
  /// index they correspond to, as of the most recent render. Empty when the popup isn't shown.
  pub filter_item_rects: Vec<(Rect, usize)>,
  /// Area of the status filter popup, as of the most recent render.
  pub filter_popup_rect: Rect,
  /// Screen rects of each item line in the action/signal menu popup, paired with the item
  /// index they correspond to, as of the most recent render. Empty when the popup isn't shown.
  pub menu_item_rects: Vec<(Rect, usize)>,
  /// Area of the action/signal menu popup, as of the most recent render.
  pub menu_popup_rect: Rect,
}

pub struct MenuItem {
  pub name: String,
  pub action: Action,
  pub key: Option<KeyCode>,
}

impl MenuItem {
  pub fn new(name: &str, action: Action, key: Option<KeyCode>) -> Self {
    Self { name: name.to_owned(), action, key }
  }

  pub fn key_string(&self) -> String {
    if let Some(key) = self.key {
      format!("{key}")
    } else {
      String::new()
    }
  }
}

pub struct StatefulList<T> {
  state: ListState,
  items: Vec<T>,
}

impl<T> Default for StatefulList<T> {
  fn default() -> Self {
    Self::with_items(vec![])
  }
}

impl<T> StatefulList<T> {
  pub fn with_items(items: Vec<T>) -> StatefulList<T> {
    StatefulList { state: ListState::default(), items }
  }

  #[allow(dead_code)]
  fn selected_mut(&mut self) -> Option<&mut T> {
    if self.items.is_empty() {
      return None;
    }
    match self.state.selected() {
      Some(i) => Some(&mut self.items[i]),
      None => None,
    }
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

  fn select(&mut self, index: Option<usize>) {
    self.state.select(index);
  }

  fn unselect(&mut self) {
    self.state.select(None);
  }
}

impl Home {
  pub fn new(scope: Scope, limit_units: &[String]) -> Self {
    let limit_units = limit_units.to_vec();
    let filtered_statuses = UnitStatus::ALL.into_iter().collect();
    Self { scope, limit_units, filtered_statuses, filter_cursor: 0, ..Default::default() }
  }

  pub fn set_units(&mut self, units: Vec<UnitWithStatus>) {
    self.all_units.clear();
    for unit_status in units.into_iter() {
      self.all_units.insert(unit_status.id(), unit_status);
    }
    self.refresh_filtered_units();
  }

  pub fn sort_units(&mut self) {
    self.all_units.sort_by(|_, a, _, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
  }

  /// Merge unit file info (enablement state, file path) into existing units.
  /// Also adds units that aren't returned by ListUnits (e.g. disabled, static, masked).
  pub fn merge_unit_files(&mut self, unit_files: Vec<UnitFile>) {
    for unit_file in unit_files {
      let id = unit_file.id();
      if let Some(unit) = self.all_units.get_mut(&id) {
        // Update existing unit with enablement state and file path
        unit.enablement_state = Some(unit_file.enablement_state);
        unit.file_path = Some(Ok(unit_file.path));
      } else if unit_file.enablement_state != "generated" && unit_file.enablement_state != "alias" {
        // Add units not returned by ListUnits (disabled, static, masked, etc.)
        // Skip generated units - they're created dynamically by systemd generators and aren't user-manageable
        // Skip alias units - they're just symlinks to other units already in the list
        let new_unit = UnitWithStatus {
          name: unit_file.name,
          scope: unit_file.scope,
          description: String::new(),
          file_path: Some(Ok(unit_file.path)),
          load_state: "not-loaded".into(),
          activation_state: "inactive".into(),
          sub_state: "dead".into(),
          enablement_state: Some(unit_file.enablement_state),
        };
        self.all_units.insert(id, new_unit);
      }
    }
    self.sort_units();
    self.refresh_filtered_units();
  }

  // Update units in-place, then filter the list
  // This is inefficient but it's fast enough
  // (on gen 13 i7: ~100 microseconds to update, ~100 microseconds to filter)
  // revisit if needed
  pub fn update_units(&mut self, service_list: systemd::ServiceList) {
    let now = std::time::Instant::now();

    let refreshed_ids: std::collections::HashSet<UnitId> = service_list.units.iter().map(|u| u.id()).collect();

    for unit in service_list.units {
      if let Some(existing) = self.all_units.get_mut(&unit.id()) {
        existing.update(unit);
      } else {
        self.all_units.insert(unit.id(), unit);
      }
    }

    // ListUnits only returns *loaded* units. A unit missing from a scope we successfully
    // refreshed has been unloaded (e.g. a disabled unit unloads when it stops), so reset it
    // to the same state merge_unit_files uses for not-loaded units instead of showing its
    // last-known (stale) state forever.
    for (id, unit) in self.all_units.iter_mut() {
      if service_list.refreshed_scopes.contains(&unit.scope) && !refreshed_ids.contains(id) {
        unit.load_state = "not-loaded".into();
        unit.activation_state = "inactive".into();
        unit.sub_state = "dead".into();
      }
    }
    info!("Updated units in {:?}", now.elapsed());

    let now = std::time::Instant::now();
    self.refresh_filtered_units();
    info!("Filtered units in {:?}", now.elapsed());
  }

  pub fn next(&mut self) {
    self.logs = vec![];
    self.runtime_info = None;
    self.filtered_units.next();
    self.get_logs();
    self.logs_scroll_offset = 0;
    self.clear_logs_selection();
  }

  pub fn previous(&mut self) {
    self.logs = vec![];
    self.runtime_info = None;
    self.filtered_units.previous();
    self.get_logs();
    self.logs_scroll_offset = 0;
    self.clear_logs_selection();
  }

  pub fn select(&mut self, index: Option<usize>, refresh_logs: bool) {
    if refresh_logs {
      self.logs = vec![];
      self.runtime_info = None;
    }
    self.filtered_units.select(index);
    if refresh_logs {
      self.get_logs();
      self.logs_scroll_offset = 0;
      self.clear_logs_selection();
    }
  }

  pub fn unselect(&mut self) {
    self.logs = vec![];
    self.runtime_info = None;
    self.filtered_units.unselect();
  }

  fn clear_logs_selection(&mut self) {
    self.logs_selection = None;
    self.selected_log_text.clear();
  }

  fn hovered_field(&self, pos: Position) -> Option<usize> {
    self.copyable_fields.iter().position(|(rect, _)| rect.contains(pos))
  }

  fn copy_with_toast(&mut self, text: &str) {
    crate::utils::copy_to_clipboard(text);
    let n = text.chars().count();
    self.show_toast(&format!("Copied {n} chars"));
  }

  fn show_toast(&mut self, msg: &str) {
    self.toast = Some((msg.to_string(), std::time::Instant::now()));
    if let Some(tx) = self.action_tx.clone() {
      tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(2100)).await;
        let _ = tx.send(Action::Render);
      });
    }
  }

  /// Draw the transient copy toast in the bottom-right corner. Must be called at the very end of
  /// `render` so nothing draws over it.
  fn render_toast(&mut self, f: &mut Frame<'_>) {
    if let Some((msg, shown_at)) = &self.toast {
      if shown_at.elapsed() < std::time::Duration::from_secs(2) {
        let area = f.area();
        let width = msg.len() as u16 + 2;
        if area.width > width && area.height > 1 {
          let toast_rect =
            Rect { x: area.right().saturating_sub(width), y: area.bottom().saturating_sub(1), width, height: 1 };
          let paragraph = Paragraph::new(Line::from(format!(" {msg} ")))
            .style(Style::default().fg(self.theme.accent).add_modifier(Modifier::REVERSED));
          f.render_widget(Clear, toast_rect);
          f.render_widget(paragraph, toast_rect);
        }
      } else {
        self.toast = None;
      }
    }
  }

  pub fn selected_service(&self) -> Option<UnitId> {
    self.filtered_units.selected().map(|m| m.unit.id())
  }

  pub fn get_logs(&mut self) {
    if let Some(selected) = self.filtered_units.selected() {
      let unit_id = selected.unit.id();
      if let Err(e) = self.journalctl_tx.as_ref().unwrap().send(unit_id) {
        warn!("Error sending unit name to journalctl thread: {}", e);
      }
    } else {
      self.logs = vec![];
    }
  }

  pub fn refresh_filtered_units(&mut self) {
    let previously_selected = self.selected_service();
    let search_value = self.input.value();
    let status_filtered_units = self.all_units.values().filter(|u| {
      let passes_activation = self.filtered_statuses.contains(&UnitStatus::activation_bucket(&u.activation_state));

      let passes_enablement = match u.enablement_state.as_deref() {
        // Enablement state not loaded yet — don't filter on it
        None => true,
        Some(state) => match UnitStatus::enablement_bucket(state) {
          // Unknown state — don't filter on it
          None => true,
          Some(bucket) => self.filtered_statuses.contains(&bucket),
        },
      };

      let passes_load = self.filtered_statuses.contains(&UnitStatus::load_bucket(&u.load_state));

      passes_activation && passes_enablement && passes_load
    });

    let matching: Vec<MatchedUnit> = if search_value.is_empty() {
      // No search - return all units without highlighting
      status_filtered_units.map(|u| MatchedUnit { unit: u.clone(), match_indices: vec![] }).collect()
    } else {
      // Fuzzy match with indices for highlighting
      let mut scored: Vec<(i64, MatchedUnit)> = status_filtered_units
        .filter_map(|u| {
          self
            .fuzzy_matcher
            .fuzzy_indices(u.short_name(), search_value)
            .map(|(score, indices)| (score, MatchedUnit { unit: u.clone(), match_indices: indices }))
        })
        .collect();

      // Sort by score descending (best matches first)
      scored.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
      scored.into_iter().map(|(_, m)| m).collect()
    };

    self.filtered_units.items = matching;
    // Reset the visible-window offset whenever the items list is rebuilt.
    // Without this, a stale offset from a larger list can leave items above the
    // viewport hidden when the list shrinks (e.g. typing a query, clearing it,
    // and retyping it can leave the first matches scrolled out of view).
    *self.filtered_units.state.offset_mut() = 0;

    // try to select the same item we had selected before
    if let Some(previously_selected) = previously_selected {
      if let Some(index) = self
        .filtered_units
        .items
        .iter()
        .position(|m| m.unit.name == previously_selected.name && m.unit.scope == previously_selected.scope)
      {
        self.select(Some(index), false);
      } else {
        self.select(Some(0), true);
      }
    } else {
      // if we can't, select the first item in the list
      if !self.filtered_units.items.is_empty() {
        self.select(Some(0), true);
      } else {
        self.unselect();
      }
    }
  }

  fn start_service(&mut self, service: UnitId) {
    let cancel_token = CancellationToken::new();
    let future = systemd::start_service(service.clone(), cancel_token.clone());
    self.service_action(service, "Start".into(), cancel_token, future, false);
  }

  fn stop_service(&mut self, service: UnitId) {
    let cancel_token = CancellationToken::new();
    let future = systemd::stop_service(service.clone(), cancel_token.clone());
    self.service_action(service, "Stop".into(), cancel_token, future, false);
  }

  fn reload_service(&mut self, service: UnitId) {
    let cancel_token = CancellationToken::new();
    let future = systemd::reload(service.scope, cancel_token.clone());
    self.service_action(service, "Reload".into(), cancel_token, future, false);
  }

  fn restart_service(&mut self, service: UnitId) {
    let cancel_token = CancellationToken::new();
    let future = systemd::restart_service(service.clone(), cancel_token.clone());
    self.service_action(service, "Restart".into(), cancel_token, future, false);
  }

  fn enable_service(&mut self, service: UnitId) {
    let cancel_token = CancellationToken::new();
    let future = systemd::enable_service(service.clone(), cancel_token.clone());
    self.service_action(service, "Enable".into(), cancel_token, future, true);
  }

  fn disable_service(&mut self, service: UnitId) {
    let cancel_token = CancellationToken::new();
    let future = systemd::disable_service(service.clone(), cancel_token.clone());
    self.service_action(service, "Disable".into(), cancel_token, future, true);
  }

  fn is_status_filter_active(&self) -> bool {
    self.filtered_statuses.len() != UnitStatus::ALL.len()
  }

  fn toggle_filtered_status(&mut self, status: UnitStatus) {
    if !self.filtered_statuses.remove(&status) {
      self.filtered_statuses.insert(status);
    }
  }

  fn service_action<Fut>(
    &mut self,
    service: UnitId,
    action_name: String,
    cancel_token: CancellationToken,
    action: Fut,
    refresh_unit_files: bool,
  ) where
    Fut: Future<Output = anyhow::Result<()>> + Send + 'static,
  {
    let tx = self.action_tx.clone().unwrap();

    self.cancel_token = Some(cancel_token.clone());

    let tx_clone = tx.clone();
    let spinner_task = tokio::spawn(async move {
      let mut interval = tokio::time::interval(Duration::from_millis(200));
      loop {
        interval.tick().await;
        let _ = tx_clone.send(Action::SpinnerTick);
      }
    });

    tokio::spawn(async move {
      let _ = tx.send(Action::EnterMode(Mode::Processing));
      match action.await {
        Ok(_) => {
          info!("{} of {:?} service {} succeeded", action_name, service.scope, service.name);
          let _ = tx.send(Action::EnterMode(Mode::ServiceList));
        },
        // would be nicer to check the error type here, but this is easier
        Err(_) if cancel_token.is_cancelled() => {
          warn!("{} of {:?} service {} was cancelled", action_name, service.scope, service.name)
        },
        Err(e) => {
          error!("{} of {:?} service {} failed: {}", action_name, service.scope, service.name, e);
          let mut error_string = e.to_string();

          if error_string.contains("AccessDenied") {
            error_string.push('\n');
            error_string.push('\n');
            error_string.push_str("Try running this tool with sudo.");
          }

          let _ = tx.send(Action::EnterError(error_string));
        },
      }
      spinner_task.abort();
      let _ = tx.send(Action::RefreshServices);
      if refresh_unit_files {
        let _ = tx.send(Action::RefreshUnitFiles);
      }

      // Refresh a bit more frequently after a service action
      for _ in 0..3 {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let _ = tx.send(Action::RefreshServices);
      }
    });
  }

  fn kill_service(&mut self, service: UnitId, signal: String) {
    let cancel_token = CancellationToken::new();
    let future = systemd::kill_service(service.clone(), signal.clone(), cancel_token.clone());
    self.service_action(service, format!("Kill with {}", signal), cancel_token, future, false);
  }
}

impl Component for Home {
  fn init(&mut self, tx: UnboundedSender<Action>) -> anyhow::Result<()> {
    self.action_tx = Some(tx.clone());
    // TODO find a better name for these. They're used to run any async data loading that needs to happen after the selection is changed,
    // not just journalctl stuff
    let (journalctl_tx, journalctl_rx) = std::sync::mpsc::channel::<UnitId>();
    self.journalctl_tx = Some(journalctl_tx);

    // TODO: move into function
    tokio::task::spawn_blocking(move || {
      let mut last_follow_handle: Option<JoinHandle<()>> = None;

      loop {
        let mut unit: UnitId = match journalctl_rx.recv() {
          Ok(unit) => unit,
          Err(_) => return,
        };

        // drain the channel, use the last value
        while let Ok(service) = journalctl_rx.try_recv() {
          info!("Skipping logs for {}...", unit.name);
          unit = service;
        }

        if let Some(handle) = last_follow_handle.take() {
          info!("Cancelling previous journalctl task");
          handle.abort();
        }

        // lazy debounce to avoid spamming journalctl on slow connections/systems
        std::thread::sleep(Duration::from_millis(100));

        // get the unit file path + runtime info (one systemctl call for both)
        match systemd::get_unit_runtime_info(&unit) {
          Ok(info) => {
            let path = if info.fragment_path.is_empty() {
              Err("could not be determined".into())
            } else {
              Ok(info.fragment_path.clone())
            };
            let _ = tx.send(Action::SetUnitFilePath { unit: unit.clone(), path });
            let _ = tx.send(Action::SetUnitRuntimeInfo { unit: unit.clone(), info: Box::new(info) });
            let _ = tx.send(Action::Render);
          },
          Err(e) => {
            // Fix this!!! Set the path to an error enum variant instead of a string
            let _ =
              tx.send(Action::SetUnitFilePath { unit: unit.clone(), path: Err("could not be determined".into()) });
            let _ = tx.send(Action::Render);
            error!("Error getting unit info for {}: {}", unit.name, e);
          },
        }

        // First, get the N lines in a batch
        info!("Getting logs for {}", unit.name);
        let start = std::time::Instant::now();

        let mut args = vec!["--quiet", "--output=short-iso", "--lines=500", "-u"];

        args.push(&unit.name);

        if unit.scope == UnitScope::User {
          args.push("--user");
        }

        match crate::ssh::host_command("journalctl", &args).output() {
          Ok(output) => {
            if output.status.success() {
              info!("Got logs for {} in {:?}", unit.name, start.elapsed());
              if let Ok(stdout) = std::str::from_utf8(&output.stdout) {
                let mut logs = stdout.trim().split('\n').map(String::from).collect_vec();

                if logs.is_empty() || logs[0].is_empty() {
                  let diagnostic = diagnose_missing_logs(&unit);
                  logs = vec![diagnostic.message()];
                }
                let _ = tx.send(Action::SetLogs { unit: unit.clone(), logs });
                let _ = tx.send(Action::Render);
              } else {
                warn!("Error parsing stdout for {}", unit.name);
              }
            } else {
              let stderr = String::from_utf8_lossy(&output.stderr);
              warn!("Error getting logs for {}: {}", unit.name, stderr);
              let diagnostic = parse_journalctl_error(&stderr);
              let _ = tx.send(Action::SetLogs { unit: unit.clone(), logs: vec![diagnostic.message()] });
              let _ = tx.send(Action::Render);
            }
          },
          Err(e) => {
            warn!("Error getting logs for {}: {}", unit.name, e);
            let _ =
              tx.send(Action::SetLogs { unit: unit.clone(), logs: vec![format!("Failed to run journalctl: {}", e)] });
            let _ = tx.send(Action::Render);
          },
        }

        // Then follow the logs
        // Splitting this into two commands is a bit of a hack that makes it easier to get the initial batch of logs
        // This does mean that we'll miss any logs that are written between the two commands, low enough risk for now
        let tx = tx.clone();
        last_follow_handle = Some(tokio::spawn(async move {
          let mut args = vec!["-u", &unit.name, "--output=short-iso", "--follow", "--lines=0", "--quiet"];
          if unit.scope == UnitScope::User {
            args.push("--user");
          }
          let mut command = crate::ssh::host_tokio_command("journalctl", &args);
          command.stdout(Stdio::piped());
          command.stderr(Stdio::piped());
          command.kill_on_drop(true);

          let mut child = command.spawn().expect("failed to execute process");

          let stdout = child.stdout.take().unwrap();

          let reader = tokio::io::BufReader::new(stdout);
          let mut lines = reader.lines();
          while let Some(line) = lines.next_line().await.unwrap() {
            let _ = tx.send(Action::AppendLogLine { unit: unit.clone(), line });
            let _ = tx.send(Action::Render);
          }
        }));
      }
    });
    Ok(())
  }

  fn handle_key_events(&mut self, key: KeyEvent) -> Vec<Action> {
    if key.modifiers.contains(KeyModifiers::CONTROL) {
      match key.code {
        KeyCode::Char('c') => return vec![Action::Quit],
        KeyCode::Char('q') => return vec![Action::Quit],
        KeyCode::Char('z') => return vec![Action::Suspend],
        KeyCode::Char('f') => return vec![Action::EnterMode(Mode::Search)],
        KeyCode::Char('l') => return vec![Action::ToggleShowLogger],
        // vim-style half-page scrolling
        KeyCode::Char('d') => return vec![Action::ScrollDown(10), Action::Render],
        KeyCode::Char('u') => return vec![Action::ScrollUp(10), Action::Render],
        _ => (),
      }
    }

    if matches!(key.code, KeyCode::Char('?')) || matches!(key.code, KeyCode::F(1)) {
      return vec![Action::ToggleHelp, Action::Render];
    }

    match key.code {
      KeyCode::PageDown => return vec![Action::ScrollDown(1), Action::Render],
      KeyCode::PageUp => return vec![Action::ScrollUp(1), Action::Render],
      KeyCode::Home => return vec![Action::ScrollToTop, Action::Render],
      KeyCode::End => return vec![Action::ScrollToBottom, Action::Render],
      _ => (),
    }

    match self.mode {
      Mode::ServiceList => {
        match key.code {
          KeyCode::Esc if self.logs_selection.is_some() => {
            self.clear_logs_selection();
            vec![Action::Render]
          },
          KeyCode::Char('q') => vec![Action::Quit],
          KeyCode::Up | KeyCode::Char('k') => {
            // if we're filtering the list, and we're at the top, and there's text in the search box, go to search mode
            if self.filtered_units.state.selected() == Some(0) {
              return vec![Action::EnterMode(Mode::Search)];
            }

            self.previous();
            vec![Action::Render]
          },
          KeyCode::Down | KeyCode::Char('j') => {
            self.next();
            vec![Action::Render]
          },
          KeyCode::Char('/') => vec![Action::EnterMode(Mode::Search)],
          KeyCode::Char('e') => {
            if let Some(selected) = self.filtered_units.selected() {
              if let Some(Ok(file_path)) = &selected.unit.file_path {
                return vec![Action::EditUnitFile { unit: selected.unit.id(), path: file_path.clone() }];
              }
            }
            vec![]
          },
          KeyCode::Char('o') => {
            vec![Action::OpenLogsInPager { logs: self.logs.clone() }]
          },
          KeyCode::Char('G') => {
            let last = self.filtered_units.items.len().saturating_sub(1);
            self.select(Some(last), true);
            vec![Action::Render]
          },
          KeyCode::Char('g') => {
            self.select(Some(0), true);
            vec![Action::Render]
          },
          KeyCode::Char('f') => vec![Action::EnterMode(Mode::StatusFilter)],
          KeyCode::Enter | KeyCode::Char(' ') => vec![Action::EnterMode(Mode::ActionMenu)],
          _ => vec![],
        }
      },
      Mode::Help => match key.code {
        KeyCode::Esc | KeyCode::Enter => vec![Action::ToggleHelp],
        _ => vec![],
      },
      Mode::Error => match key.code {
        KeyCode::Esc | KeyCode::Enter => vec![Action::EnterMode(Mode::ServiceList)],
        _ => vec![],
      },
      Mode::Search => match key.code {
        KeyCode::Esc => vec![Action::EnterMode(Mode::ServiceList)],
        KeyCode::Enter => vec![Action::EnterMode(Mode::ActionMenu)],
        KeyCode::Down | KeyCode::Tab => {
          self.next();
          vec![Action::EnterMode(Mode::ServiceList)]
        },
        KeyCode::Up => {
          self.previous();
          vec![Action::EnterMode(Mode::ServiceList)]
        },
        _ => {
          let prev_search_value = self.input.value().to_owned();
          self.input.handle_event(&crossterm::event::Event::Key(key));

          // if the search value changed, filter the list
          if prev_search_value != self.input.value() {
            self.refresh_filtered_units();
          }
          vec![Action::Render]
        },
      },
      Mode::ActionMenu => match key.code {
        KeyCode::Esc => vec![Action::EnterMode(Mode::ServiceList)],
        KeyCode::Down | KeyCode::Char('j') => {
          self.menu_items.next();
          vec![Action::Render]
        },
        KeyCode::Up | KeyCode::Char('k') => {
          self.menu_items.previous();
          vec![Action::Render]
        },
        KeyCode::Enter | KeyCode::Char(' ') => match self.menu_items.selected() {
          Some(i) => vec![i.action.clone()],
          None => vec![Action::EnterMode(Mode::ServiceList)],
        },
        _ => {
          for item in self.menu_items.items.iter() {
            if let Some(key_code) = item.key {
              if key_code == key.code {
                return vec![item.action.clone()];
              }
            }
          }
          vec![]
        },
      },
      Mode::Processing => match key.code {
        KeyCode::Esc => vec![Action::CancelTask],
        _ => vec![],
      },
      Mode::SignalMenu => match key.code {
        KeyCode::Esc => vec![Action::EnterMode(Mode::ServiceList)],
        KeyCode::Down | KeyCode::Char('j') => {
          self.menu_items.next();
          vec![Action::Render]
        },
        KeyCode::Up | KeyCode::Char('k') => {
          self.menu_items.previous();
          vec![Action::Render]
        },
        KeyCode::Enter | KeyCode::Char(' ') => match self.menu_items.selected() {
          Some(i) => vec![i.action.clone()],
          None => vec![Action::EnterMode(Mode::ServiceList)],
        },
        _ => {
          for item in self.menu_items.items.iter() {
            if let Some(key_code) = item.key {
              if key_code == key.code {
                return vec![item.action.clone()];
              }
            }
          }
          vec![]
        },
      },
      Mode::StatusFilter => match key.code {
        KeyCode::Esc => vec![Action::EnterMode(Mode::ServiceList)],
        KeyCode::Down | KeyCode::Char('j') => {
          if self.filter_cursor < UnitStatus::ALL.len() - 1 {
            self.filter_cursor += 1;
          } else {
            self.filter_cursor = 0;
          }
          vec![Action::Render]
        },
        KeyCode::Up | KeyCode::Char('k') => {
          if self.filter_cursor > 0 {
            self.filter_cursor -= 1;
          } else {
            self.filter_cursor = UnitStatus::ALL.len() - 1;
          }
          vec![Action::Render]
        },
        KeyCode::Enter | KeyCode::Char(' ') => {
          self.toggle_filtered_status(UnitStatus::ALL[self.filter_cursor]);
          vec![Action::RefreshStatusFilterMenu]
        },
        KeyCode::Char('a') => {
          self.filtered_statuses = UnitStatus::ALL.into_iter().collect();
          vec![Action::RefreshStatusFilterMenu]
        },
        KeyCode::Char('n') => {
          self.filtered_statuses = UnitStatus::NONE.into_iter().collect();
          vec![Action::RefreshStatusFilterMenu]
        },
        _ => {
          // Shortcut keys: toggle the matching filter directly
          for (i, status) in UnitStatus::ALL.iter().enumerate() {
            if status.shortcut_key() == key.code {
              self.filter_cursor = i;
              self.toggle_filtered_status(*status);
              return vec![Action::RefreshStatusFilterMenu];
            }
          }
          vec![]
        },
      },
    }
  }

  fn handle_mouse_events(&mut self, mouse: MouseEvent) -> Vec<Action> {
    // In modal/transient modes, mouse events shouldn't affect selection or scrolling.
    if matches!(self.mode, Mode::Processing) {
      return vec![];
    }

    let pos = Position::new(mouse.column, mouse.row);

    if self.mode == Mode::Help {
      return match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => vec![Action::ToggleHelp],
        _ => vec![],
      };
    }

    if self.mode == Mode::Error {
      return match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => vec![Action::EnterMode(Mode::ServiceList)],
        _ => vec![],
      };
    }

    if self.mode == Mode::ActionMenu || self.mode == Mode::SignalMenu {
      let hovered_item = self.menu_item_rects.iter().find(|(rect, _)| rect.contains(pos)).map(|(_, idx)| *idx);

      return match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => {
          if let Some(idx) = hovered_item {
            self.menu_items.state.select(Some(idx));
            match self.menu_items.selected() {
              Some(i) => vec![i.action.clone()],
              None => vec![Action::EnterMode(Mode::ServiceList)],
            }
          } else if !self.menu_popup_rect.contains(pos) {
            // Clicked outside the popup: close it, same as Escape.
            vec![Action::EnterMode(Mode::ServiceList)]
          } else {
            vec![]
          }
        },
        MouseEventKind::Moved => {
          if let Some(idx) = hovered_item {
            if Some(idx) != self.menu_items.state.selected() {
              self.menu_items.state.select(Some(idx));
              return vec![Action::Render];
            }
          }
          vec![]
        },
        MouseEventKind::ScrollDown => {
          self.menu_items.next();
          vec![Action::Render]
        },
        MouseEventKind::ScrollUp => {
          self.menu_items.previous();
          vec![Action::Render]
        },
        _ => vec![],
      };
    }

    if self.mode == Mode::StatusFilter {
      let hovered_item = self.filter_item_rects.iter().find(|(rect, _)| rect.contains(pos)).map(|(_, idx)| *idx);

      return match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => {
          if let Some(idx) = hovered_item {
            self.filter_cursor = idx;
            self.toggle_filtered_status(UnitStatus::ALL[idx]);
            vec![Action::RefreshStatusFilterMenu]
          } else if !self.filter_popup_rect.contains(pos) {
            // Clicked outside the popup: close it, same as Escape.
            vec![Action::EnterMode(Mode::ServiceList)]
          } else {
            // Clicked inside the popup but not on an item (e.g. a category header).
            vec![]
          }
        },
        MouseEventKind::Moved => {
          if let Some(idx) = hovered_item {
            if idx != self.filter_cursor {
              self.filter_cursor = idx;
              return vec![Action::RefreshStatusFilterMenu];
            }
          }
          vec![]
        },
        MouseEventKind::ScrollDown => {
          if self.filter_cursor < UnitStatus::ALL.len() - 1 {
            self.filter_cursor += 1;
          } else {
            self.filter_cursor = 0;
          }
          vec![Action::RefreshStatusFilterMenu]
        },
        MouseEventKind::ScrollUp => {
          if self.filter_cursor > 0 {
            self.filter_cursor -= 1;
          } else {
            self.filter_cursor = UnitStatus::ALL.len() - 1;
          }
          vec![Action::RefreshStatusFilterMenu]
        },
        _ => vec![],
      };
    }

    fn clamp_into(rect: Rect, pos: Position) -> Position {
      if rect.width == 0 || rect.height == 0 {
        return Position::new(rect.x, rect.y);
      }
      let x = pos.x.clamp(rect.x, rect.x + rect.width - 1);
      let y = pos.y.clamp(rect.y, rect.y + rect.height - 1);
      Position::new(x, y)
    }

    match mouse.kind {
      MouseEventKind::Moved => {
        let was_hovering = self.hovered_field(self.mouse_position);
        self.mouse_position = pos;
        let is_hovering = self.hovered_field(pos);
        if was_hovering != is_hovering {
          vec![Action::Render]
        } else {
          vec![]
        }
      },
      MouseEventKind::ScrollUp => {
        self.mouse_position = pos;
        self.clear_logs_selection();
        if self.services_panel.contains(pos) {
          if self.filtered_units.state.selected() == Some(0) {
            return vec![Action::EnterMode(Mode::Search)];
          }
          self.previous();
          vec![Action::Render]
        } else {
          vec![Action::ScrollUp(2), Action::Render]
        }
      },
      MouseEventKind::ScrollDown => {
        self.mouse_position = pos;
        self.clear_logs_selection();
        if self.services_panel.contains(pos) {
          self.next();
          vec![Action::Render]
        } else {
          vec![Action::ScrollDown(2), Action::Render]
        }
      },
      MouseEventKind::Down(MouseButton::Left) => {
        self.mouse_position = pos;
        if let Some(idx) = self.hovered_field(pos) {
          let text = self.copyable_fields[idx].1.clone();
          self.copy_with_toast(&text);
          return vec![Action::Render];
        }
        if self.search_panel.contains(pos) && self.mode == Mode::ServiceList {
          return vec![Action::EnterMode(Mode::Search)];
        }
        if self.services_panel.contains(pos) && matches!(self.mode, Mode::ServiceList | Mode::Search) {
          let inner = self.services_panel.inner(Margin::new(1, 1));
          if inner.contains(pos) {
            let clicked_index = self.filtered_units.state.offset() + (pos.y - inner.y) as usize;
            if clicked_index < self.filtered_units.items.len() {
              let was_search = self.mode == Mode::Search;
              self.select(Some(clicked_index), true);
              let mut actions = vec![];
              if was_search {
                actions.push(Action::EnterMode(Mode::ServiceList));
              }
              actions.push(Action::Render);
              return actions;
            }
          }
          return vec![];
        }
        if self.logs_panel_inner.contains(pos) {
          self.logs_selection = Some((pos, pos));
        } else {
          self.clear_logs_selection();
        }
        vec![Action::Render]
      },
      MouseEventKind::Drag(MouseButton::Left) => {
        self.mouse_position = pos;
        if let Some((anchor, _)) = self.logs_selection {
          let clamped = clamp_into(self.logs_panel_inner, pos);
          self.logs_selection = Some((anchor, clamped));
        }
        vec![Action::Render]
      },
      MouseEventKind::Up(MouseButton::Left) => {
        self.mouse_position = pos;
        if let Some((anchor, cursor)) = self.logs_selection {
          if anchor != cursor && !self.selected_log_text.is_empty() {
            let text = self.selected_log_text.clone();
            self.copy_with_toast(&text);
          } else {
            self.clear_logs_selection();
          }
        }
        vec![Action::Render]
      },
      _ => vec![],
    }
  }

  fn dispatch(&mut self, action: Action) -> Option<Action> {
    match action {
      Action::ToggleShowLogger => {
        self.show_logger = !self.show_logger;
        return Some(Action::Render);
      },
      Action::EnterMode(mode) => {
        if mode == Mode::ActionMenu {
          {
            let selected = self.filtered_units.selected()?;
            let mut menu_items = vec![
              MenuItem::new("Start", Action::StartService(selected.unit.id()), Some(KeyCode::Char('s'))),
              MenuItem::new("Stop", Action::StopService(selected.unit.id()), Some(KeyCode::Char('t'))),
              MenuItem::new("Restart", Action::RestartService(selected.unit.id()), Some(KeyCode::Char('r'))),
              MenuItem::new("Reload", Action::ReloadService(selected.unit.id()), Some(KeyCode::Char('l'))),
              MenuItem::new("Enable", Action::EnableService(selected.unit.id()), Some(KeyCode::Char('n'))),
              MenuItem::new("Disable", Action::DisableService(selected.unit.id()), Some(KeyCode::Char('d'))),
              MenuItem::new("Kill", Action::EnterMode(Mode::SignalMenu), Some(KeyCode::Char('k'))),
              MenuItem::new(
                "Open logs in pager",
                Action::OpenLogsInPager { logs: self.logs.clone() },
                Some(KeyCode::Char('o')),
              ),
            ];

            if let Some(Ok(file_path)) = &selected.unit.file_path {
              menu_items.push(MenuItem::new("Copy unit file path", Action::CopyUnitFilePath, Some(KeyCode::Char('c'))));
              menu_items.push(MenuItem::new(
                "Edit unit file",
                Action::EditUnitFile { unit: selected.unit.id(), path: file_path.clone() },
                Some(KeyCode::Char('e')),
              ));
            }

            self.menu_items = StatefulList::with_items(menu_items);
            self.menu_items.state.select(Some(0));
          }
        } else if mode == Mode::SignalMenu {
          {
            let selected = self.filtered_units.selected()?;
            let signals = vec![
              ("SIGTERM", KeyCode::Char('t')),
              ("SIGHUP", KeyCode::Char('h')),
              ("SIGINT", KeyCode::Char('i')),
              ("SIGQUIT", KeyCode::Char('q')),
              ("SIGKILL", KeyCode::Char('k')),
              ("SIGUSR1", KeyCode::Char('1')),
              ("SIGUSR2", KeyCode::Char('2')),
            ];

            let menu_items: Vec<MenuItem> = signals
              .into_iter()
              .map(|(name, key_code)| {
                MenuItem::new(name, Action::KillService(selected.unit.id(), name.to_string()), Some(key_code))
              })
              .collect();

            self.menu_items = StatefulList::with_items(menu_items);
            self.menu_items.state.select(Some(0));
          }
        } else if mode == Mode::StatusFilter {
          self.filter_cursor = 0;
        }

        self.mode = mode;
        return Some(Action::Render);
      },
      Action::EnterError(err) => {
        tracing::error!(err);
        self.error_message = err;
        return Some(Action::EnterMode(Mode::Error));
      },
      Action::ToggleHelp => {
        if self.mode != Mode::Help {
          self.previous_mode = Some(self.mode);
          self.mode = Mode::Help;
        } else {
          self.mode = self.previous_mode.unwrap_or(Mode::Search);
        }
        return Some(Action::Render);
      },
      Action::CopyUnitFilePath => {
        if let Some(selected) = self.filtered_units.selected() {
          if let Some(Ok(file_path)) = &selected.unit.file_path {
            let file_path = file_path.clone();
            self.copy_with_toast(&file_path);
            return Some(Action::EnterMode(Mode::ServiceList));
          } else {
            return Some(Action::EnterError("No unit file path available".into()));
          }
        }
      },
      Action::SetUnitFilePath { unit, path } => {
        if let Some(unit) = self.all_units.get_mut(&unit) {
          unit.file_path = Some(path.clone());
        }
        self.refresh_filtered_units(); // copy the updated unit file path to the filtered list
      },
      Action::SetUnitRuntimeInfo { unit, info } => {
        if let Some(selected) = self.filtered_units.selected() {
          if selected.unit.id() == unit {
            self.runtime_info = Some(*info);
          }
        }
      },
      Action::SetLogs { unit, logs } => {
        if let Some(selected) = self.filtered_units.selected() {
          if selected.unit.id() == unit {
            self.logs = logs;
          }
        }
      },
      Action::AppendLogLine { unit, line } => {
        if let Some(selected) = self.filtered_units.selected() {
          if selected.unit.id() == unit {
            self.logs.push(line);
          }
        }
      },
      Action::ScrollUp(offset) => {
        self.logs_scroll_offset = self.logs_scroll_offset.saturating_sub(offset);
        info!("scroll offset: {}", self.logs_scroll_offset);
        self.clear_logs_selection();
      },
      Action::ScrollDown(offset) => {
        self.logs_scroll_offset = self.logs_scroll_offset.saturating_add(offset);
        info!("scroll offset: {}", self.logs_scroll_offset);
        self.clear_logs_selection();
      },
      Action::ScrollToTop => {
        self.logs_scroll_offset = 0;
        self.clear_logs_selection();
      },
      Action::ScrollToBottom => {
        // Clamped to the actual wrapped height at render time (see `render`).
        self.logs_scroll_offset = u16::MAX;
        self.clear_logs_selection();
      },

      Action::StartService(service_name) => self.start_service(service_name),
      Action::StopService(service_name) => self.stop_service(service_name),
      Action::ReloadService(service_name) => self.reload_service(service_name),
      Action::RestartService(service_name) => self.restart_service(service_name),
      Action::EnableService(service_name) => self.enable_service(service_name),
      Action::DisableService(service_name) => self.disable_service(service_name),
      Action::RefreshServices => {
        let tx = self.action_tx.clone().unwrap();
        let scope = self.scope;
        let limit_units = self.limit_units.to_vec();
        tokio::spawn(async move {
          let units = systemd::get_all_services(scope, &limit_units)
            .await
            .expect("Failed to get services. Check that systemd is running and try running this tool with sudo.");
          let _ = tx.send(Action::SetServices(units));
        });
      },
      Action::SetServices(units) => {
        self.update_units(units);
        return Some(Action::Render);
      },
      Action::RefreshUnitFiles => {
        let tx = self.action_tx.clone().unwrap();
        let scope = self.scope;
        let limit_units = self.limit_units.clone();
        tokio::spawn(async move {
          match systemd::get_unit_files(scope, &limit_units).await {
            Ok(unit_files) => {
              let _ = tx.send(Action::SetUnitFiles(unit_files));
            },
            Err(e) => {
              error!("Failed to get unit files: {:?}", e);
            },
          }
        });
      },
      Action::RefreshStatusFilterMenu => {
        self.refresh_filtered_units();
        return Some(Action::Render);
      },
      Action::SetUnitFiles(unit_files) => {
        self.merge_unit_files(unit_files);
        return Some(Action::Render);
      },
      Action::KillService(service_name, signal) => self.kill_service(service_name, signal),
      Action::SpinnerTick => {
        self.spinner_tick = self.spinner_tick.wrapping_add(1);
        return Some(Action::Render);
      },
      Action::CancelTask => {
        if let Some(cancel_token) = self.cancel_token.take() {
          cancel_token.cancel();
        }
        self.mode = Mode::ServiceList;
        return Some(Action::Render);
      },
      _ => (),
    }
    None
  }

  fn render(&mut self, f: &mut Frame<'_>, rect: Rect) {
    // Theme colors for adaptive light/dark support
    let theme = self.theme;

    fn span(s: &str, color: Color) -> Span<'_> {
      Span::styled(s, Style::default().fg(color))
    }

    fn colored_line(value: &str, color: Color) -> Line<'_> {
      Line::from(vec![Span::styled(value, Style::default().fg(color))])
    }

    let rect = if self.show_logger {
      let chunks = Layout::new(Direction::Vertical, Constraint::from_percentages([50, 50])).split(rect);

      self.logger.render(f, chunks[1]);
      chunks[0]
    } else {
      rect
    };

    let rects =
      Layout::new(Direction::Vertical, [Constraint::Min(3), Constraint::Percentage(100), Constraint::Length(1)])
        .split(rect);
    let search_panel = rects[0];
    let main_panel = rects[1];
    let help_line_rect = rects[2];

    self.search_panel = search_panel;

    // Helper for colouring based on the same logic as sysz
    // https://github.com/joehillen/sysz/blob/8da8e0dcbfde8d68fbdb22382671e395bd370d69/sysz#L69C1-L72C24
    //    Some units are colored based on state:
    //    green       active
    //    red         failed
    //    yellow      not-found
    fn unit_color(unit: &UnitWithStatus) -> Color {
      if unit.is_active() {
        Color::Green
      } else if unit.is_failed() {
        Color::Red
      } else if unit.is_not_found() {
        Color::Yellow
      } else {
        Color::Reset
      }
    }

    let items: Vec<ListItem> = self
      .filtered_units
      .items
      .iter()
      .map(|m| {
        let color = unit_color(&m.unit);
        let name = m.unit.short_name();

        if m.match_indices.is_empty() {
          ListItem::new(colored_line(name, color))
        } else {
          // Build spans with highlighted matched characters
          let mut spans = Vec::new();
          let mut last_end = 0;

          for &idx in &m.match_indices {
            if idx > last_end && idx <= name.len() {
              // Non-matched portion
              spans.push(Span::styled(&name[last_end..idx], Style::default().fg(color)));
            }
            // Matched character - bold + underlined
            if idx < name.len() {
              let char_end = name[idx..].chars().next().map(|c| idx + c.len_utf8()).unwrap_or(idx + 1);
              spans.push(Span::styled(
                &name[idx..char_end],
                Style::default().fg(color).add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
              ));
              last_end = char_end;
            }
          }

          if last_end < name.len() {
            spans.push(Span::styled(&name[last_end..], Style::default().fg(color)));
          }

          ListItem::new(Line::from(spans))
        }
      })
      .collect();

    // Create a List from all list items and highlight the currently selected one
    let items = List::new(items)
      .block(
        Block::default()
          .borders(Borders::ALL)
          .border_type(BorderType::Rounded)
          .border_style(if self.mode == Mode::ServiceList {
            Style::default().fg(theme.accent)
          } else {
            Style::default()
          })
          .title(if self.is_status_filter_active() { "─Services (filtered)" } else { "─Services" }),
      )
      .highlight_style(Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD));

    let chunks =
      Layout::new(Direction::Horizontal, [Constraint::Min(30), Constraint::Percentage(100)]).split(main_panel);
    let right_panel = chunks[1];

    self.services_panel = chunks[0];

    f.render_stateful_widget(items, chunks[0], &mut self.filtered_units.state);

    let selected_item = self.filtered_units.selected();

    // Details rows: base unit facts on the left, runtime stats (fetched lazily on selection)
    // in a second column on wide terminals, or folded into a single "Runtime" row otherwise.
    let dim = Style::default().add_modifier(Modifier::DIM);
    let mut rows: Vec<(&str, Line)> = vec![];
    let mut stat_rows: Vec<(&str, Line)> = vec![];

    if let Some(m) = selected_item {
      rows.push(("Description", Line::from(m.unit.description.as_str())));

      let active_color = match m.unit.activation_state.as_str() {
        "active" => Color::Green,
        "failed" => Color::Red,
        _ => Color::Reset,
      };
      let mut active_spans = vec![Span::styled(
        format!("{} ({})", m.unit.activation_state, m.unit.sub_state),
        Style::default().fg(active_color),
      )];
      if let Some(info) = &self.runtime_info {
        if m.unit.activation_state == "failed" {
          if let Some(result) = info.result.as_deref().filter(|r| *r != "success") {
            let status = info.exec_main_status.filter(|&s| s != 0).map(|s| format!(", status={s}")).unwrap_or_default();
            active_spans.push(Span::styled(format!(" ({result}{status})"), Style::default().fg(Color::Red)));
          }
        }
        let since = if m.unit.activation_state == "active" {
          info.active_enter_timestamp.as_deref()
        } else {
          info.inactive_enter_timestamp.as_deref()
        };
        if let Some((absolute, relative)) = since.and_then(format_systemd_timestamp) {
          let relative = relative.map(|r| format!(" ({r})")).unwrap_or_default();
          active_spans.push(Span::styled(format!(" since {absolute}{relative}"), dim));
        }
      }
      rows.push(("Active", Line::from(active_spans)));

      // Enablement, scope, and any load problem share a line
      let enablement_state = m.unit.enablement_state.as_deref().unwrap_or("");
      let enablement_color = match enablement_state {
        "enabled" => Color::Green,
        "disabled" => Color::Yellow,
        "masked" => Color::Red,
        _ => Color::Reset,
      };
      let scope = match m.unit.scope {
        UnitScope::Global => "global",
        UnitScope::User => "user",
      };
      let mut state_spans = vec![
        Span::styled(enablement_state, Style::default().fg(enablement_color)),
        Span::styled(format!(" · {scope}"), dim),
      ];
      if m.unit.load_state != "loaded" {
        let load_color = match m.unit.load_state.as_str() {
          "not-found" => Color::Yellow,
          "error" | "masked" | "bad-setting" => Color::Red,
          _ => Color::Reset,
        };
        state_spans.push(Span::styled(format!(" · {}", m.unit.load_state), Style::default().fg(load_color)));
      }
      rows.push(("Enablement", Line::from(state_spans)));

      rows.push((
        "Unit file",
        match &m.unit.file_path {
          Some(Ok(file_path)) => Line::from(file_path.as_str()),
          Some(Err(e)) => Line::from(Span::styled(e.as_str(), Style::default().fg(Color::Red))),
          None => Line::from(""),
        },
      ));

      if let Some(info) = &self.runtime_info {
        if let Some(pid) = info.main_pid {
          stat_rows.push(("PID", Line::from(pid.to_string())));
        }
        if let Some(memory) = info.memory_current {
          stat_rows.push(("Memory", Line::from(format_bytes(memory))));
        }
        if let Some(tasks) = info.tasks_current {
          stat_rows.push(("Tasks", Line::from(tasks.to_string())));
        }
        if let Some(cpu) = info.cpu_usage_nsec.filter(|&n| n > 0) {
          stat_rows.push(("CPU", Line::from(format_cpu_nsec(cpu))));
        }
        if let Some(restarts) = info.n_restarts.filter(|&n| n > 0) {
          stat_rows
            .push(("Restarts", Line::from(Span::styled(restarts.to_string(), Style::default().fg(Color::Yellow)))));
        }
        if let Some((absolute, relative)) = info.next_elapse.as_deref().and_then(format_systemd_timestamp) {
          let relative = relative.map(|r| format!(" ({r})")).unwrap_or_default();
          stat_rows.push(("Next run", Line::from(format!("{absolute}{relative}"))));
        }
      }
    }

    let two_columns = right_panel.width >= 90 && !stat_rows.is_empty();
    if !two_columns && !stat_rows.is_empty() {
      // Narrow terminal: fold the stats into one line to save vertical space
      let mut spans: Vec<Span> = vec![];
      for (i, (label, line)) in stat_rows.drain(..).enumerate() {
        if i > 0 {
          spans.push(Span::styled(" · ", dim));
        }
        spans.push(Span::styled(format!("{label} "), dim));
        spans.extend(line.spans);
      }
      rows.push(("Runtime", Line::from(spans)));
    }

    // Size the details panel to its content instead of a fixed height
    let details_content_height = rows.len().max(stat_rows.len()).max(1) as u16;
    let right_panel =
      Layout::new(Direction::Vertical, [Constraint::Length(details_content_height + 2), Constraint::Percentage(100)])
        .split(right_panel);
    let details_panel = right_panel[0];
    let logs_panel = right_panel[1];

    let details_block = Block::default().title("─Details").borders(Borders::ALL).border_type(BorderType::Rounded);
    let details_inner = details_block.inner(details_panel);
    f.render_widget(details_block, details_panel);

    fn label_width(rows: &[(&str, Line)]) -> u16 {
      rows.iter().map(|(label, _)| label.len() + 2).max().unwrap_or(0) as u16
    }

    fn split_labels_values<'a>(rows: Vec<(&'a str, Line<'a>)>) -> (Vec<Line<'a>>, Vec<Line<'a>>) {
      rows.into_iter().map(|(label, value)| (Line::from(format!("{label}: ")), value)).unzip()
    }

    let panes = if two_columns {
      Layout::new(
        Direction::Horizontal,
        [
          Constraint::Length(label_width(&rows)),
          Constraint::Fill(2),
          Constraint::Length(label_width(&stat_rows) + 1),
          Constraint::Fill(1),
        ],
      )
      .split(details_inner)
    } else {
      Layout::new(Direction::Horizontal, [Constraint::Length(label_width(&rows)), Constraint::Fill(1)])
        .split(details_inner)
    };

    // Every details value line (and runtime stat line) is hoverable and click-to-copy. Register
    // each rendered line's rect and text, bolding the hovered one.
    self.copyable_fields.clear();
    fn register_copyable_fields(values: &mut [Line], pane: Rect, mouse: Position, out: &mut Vec<(Rect, String)>) {
      for (i, line) in values.iter_mut().enumerate() {
        if i as u16 >= pane.height {
          break;
        }
        let text = line.spans.iter().map(|s| s.content.as_ref()).collect::<String>().trim().to_string();
        if text.is_empty() {
          continue;
        }
        let rect = Rect { x: pane.x, y: pane.y + i as u16, width: (line.width() as u16).min(pane.width), height: 1 };
        if rect.contains(mouse) {
          line.style = line.style.add_modifier(Modifier::BOLD);
        }
        out.push((rect, text));
      }
    }

    let (labels, mut values) = split_labels_values(rows);
    register_copyable_fields(&mut values, panes[1], self.mouse_position, &mut self.copyable_fields);
    f.render_widget(Paragraph::new(labels).alignment(ratatui::layout::Alignment::Right), panes[0]);
    f.render_widget(Paragraph::new(values), panes[1]);

    if two_columns {
      let (stat_labels, mut stat_values) = split_labels_values(stat_rows);
      register_copyable_fields(&mut stat_values, panes[3], self.mouse_position, &mut self.copyable_fields);
      f.render_widget(Paragraph::new(stat_labels).alignment(ratatui::layout::Alignment::Right), panes[2]);
      f.render_widget(Paragraph::new(stat_values), panes[3]);
    }

    let log_lines = self
      .logs
      .iter()
      .rev()
      .map(|l| {
        if let Some((timestamp, rest)) = l.split_once(' ') {
          if let Some(formatted_date) = parse_journalctl_timestamp(timestamp) {
            return Line::from(vec![
              Span::styled(formatted_date, Style::default().add_modifier(Modifier::DIM)),
              Span::raw(" "),
              Span::raw(rest),
            ]);
          }
        }

        Line::from(l.as_str())
      })
      .collect_vec();

    let paragraph = Paragraph::new(log_lines)
      .block(Block::default().title("─Service Logs").borders(Borders::ALL).border_type(BorderType::Rounded))
      .style(Style::default())
      .wrap(Wrap { trim: true });

    // line_count wraps at the given width but includes the block's border rows in its count,
    // so wrap at the inner width and compare against the full panel height.
    let inner_width = logs_panel.width.saturating_sub(2);
    let total_lines = u16::try_from(paragraph.line_count(inner_width)).unwrap_or(u16::MAX);
    let max_offset = total_lines.saturating_sub(logs_panel.height);
    self.logs_scroll_offset = self.logs_scroll_offset.min(max_offset);

    let paragraph = paragraph.scroll((self.logs_scroll_offset, 0));
    f.render_widget(paragraph, logs_panel);

    // Inner area of the logs panel (border-excluded), used for mouse hit-testing and selection.
    self.logs_panel_inner = logs_panel.inner(Margin::new(1, 1));

    if let Some((anchor, cursor)) = self.logs_selection {
      let inner = self.logs_panel_inner;
      // Normalize selection ordering by (y, then x).
      let (start, end) = if (anchor.y, anchor.x) <= (cursor.y, cursor.x) { (anchor, cursor) } else { (cursor, anchor) };

      let buf = f.buffer_mut();
      let mut selected_rows: Vec<String> = Vec::new();

      if inner.width > 0 && inner.height > 0 {
        let left = inner.x;
        let right = inner.x + inner.width - 1;

        for y in start.y..=end.y {
          if y < inner.y || y >= inner.y + inner.height {
            continue;
          }

          let (row_start_x, row_end_x) = if start.y == end.y {
            (start.x.max(left), end.x.min(right))
          } else if y == start.y {
            (start.x.max(left), right)
          } else if y == end.y {
            (left, end.x.min(right))
          } else {
            (left, right)
          };

          if row_start_x > row_end_x {
            continue;
          }

          let mut row_text = String::new();
          for x in row_start_x..=row_end_x {
            if let Some(cell) = buf.cell_mut(Position::new(x, y)) {
              cell.modifier.insert(Modifier::REVERSED);
              row_text.push_str(cell.symbol());
            }
          }
          selected_rows.push(row_text.trim_end().to_string());
        }
      }

      self.selected_log_text = selected_rows.join("\n");
    }

    let width = search_panel.width.max(3) - 3; // keep 2 for borders and 1 for cursor
    let scroll = self.input.visual_scroll(width as usize);
    let input = Paragraph::new(self.input.value())
      .style(match self.mode {
        Mode::Search => Style::default().fg(theme.accent),
        _ => Style::default(),
      })
      .scroll((0, scroll as u16))
      .block(Block::default().borders(Borders::ALL).border_type(BorderType::Rounded).title(Line::from(vec![
        Span::raw("─Search "),
        Span::styled("(", Style::default().fg(theme.muted_alt)),
        Span::styled("ctrl+f", Style::default().add_modifier(Modifier::BOLD).fg(theme.kbd)),
        Span::styled(" or ", Style::default().fg(theme.muted_alt)),
        Span::styled("/", Style::default().add_modifier(Modifier::BOLD).fg(theme.kbd)),
        Span::styled(")", Style::default().fg(theme.muted_alt)),
      ])));
    f.render_widget(input, search_panel);
    // clear top right of search panel so we can put help instructions there
    let help_width = 24;
    let help_area = Rect::new(search_panel.x + search_panel.width - help_width - 2, search_panel.y, help_width, 1);
    f.render_widget(Clear, help_area);
    let help_text = Paragraph::new(Line::from(vec![
      Span::raw(" Press "),
      Span::styled("?", Style::default().add_modifier(Modifier::BOLD).fg(theme.kbd)),
      Span::raw(" or "),
      Span::styled("F1", Style::default().add_modifier(Modifier::BOLD).fg(theme.kbd)),
      Span::raw(" for help "),
    ]))
    .style(Style::default().fg(theme.muted_alt));
    f.render_widget(help_text, help_area);

    if self.mode == Mode::Search {
      f.set_cursor_position((
        (search_panel.x + 1 + self.input.cursor() as u16).min(search_panel.x + search_panel.width - 2),
        search_panel.y + 1,
      ));
    }

    if self.mode == Mode::Help {
      let popup = f.area().centered(Constraint::Length(50), Constraint::Length(19));

      let primary = |s| Span::styled(s, Style::default().fg(theme.primary));
      let help_lines = vec![
        Line::from(""),
        Line::from(Span::styled("Shortcuts", Style::default().add_modifier(Modifier::UNDERLINED))),
        Line::from(""),
        Line::from(vec![primary("ctrl+C"), Span::raw(" or "), primary("ctrl+Q"), Span::raw(" to quit")]),
        Line::from(vec![primary("ctrl+L"), Span::raw(" toggles the logger pane")]),
        Line::from(vec![primary("PageUp"), Span::raw(" / "), primary("PageDown"), Span::raw(" scroll the logs")]),
        Line::from(vec![primary("Home"), Span::raw(" / "), primary("End"), Span::raw(" scroll to top/bottom")]),
        Line::from(vec![primary("Enter"), Span::raw(" or "), primary("Space"), Span::raw(" open the action menu")]),
        Line::from(vec![primary("f"), Span::raw(" filter services by status")]),
        Line::from(vec![primary("?"), Span::raw(" / "), primary("F1"), Span::raw(" open this help pane")]),
        Line::from(vec![primary("mouse"), Span::raw(": drag to select+copy logs, wheel to scroll")]),
        Line::from(""),
        Line::from(Span::styled("Vim Style Shortcuts", Style::default().add_modifier(Modifier::UNDERLINED))),
        Line::from(""),
        Line::from(vec![primary("j"), Span::raw(" / "), primary("k"), Span::raw(" navigate down/up")]),
        Line::from(vec![primary("g"), Span::raw(" / "), primary("G"), Span::raw(" jump to first/last service")]),
        Line::from(vec![primary("ctrl+U"), Span::raw(" / "), primary("ctrl+D"), Span::raw(" scroll the logs")]),
      ];

      let name = env!("CARGO_PKG_NAME");
      let version = env!("CARGO_PKG_VERSION");
      let title = format!("─Help for {name} v{version}");

      let paragraph = Paragraph::new(help_lines)
        .block(Block::default().title(title).borders(Borders::ALL).border_type(BorderType::Rounded))
        .style(Style::default())
        .wrap(Wrap { trim: true });

      f.render_widget(Clear, popup);
      f.render_widget(paragraph, popup);
    }

    if self.mode == Mode::Error {
      let popup = f.area().centered(Constraint::Length(50), Constraint::Length(12));
      let error_lines = self.error_message.split('\n').map(Line::from).collect_vec();
      let paragraph = Paragraph::new(error_lines)
        .block(
          Block::default()
            .title("─Error")
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(Color::Red)),
        )
        .wrap(Wrap { trim: true });

      f.render_widget(Clear, popup);
      f.render_widget(paragraph, popup);
    }

    let selected_unit_name = match self.filtered_units.selected() {
      Some(s) => &s.unit.name,
      None => "",
    };

    // Help line at the bottom

    let version = format!("v{}", env!("CARGO_PKG_VERSION"));

    let help_line_rects =
      Layout::new(Direction::Horizontal, [Constraint::Fill(1), Constraint::Length(version.len() as u16)])
        .split(help_line_rect);
    let help_rect = help_line_rects[0];
    let version_rect = help_line_rects[1];

    let help_line = match self.mode {
      Mode::Search => Line::from(span("Show actions: <enter>", theme.primary)),
      Mode::ServiceList => {
        let mut spans =
          vec![span("Show actions: <enter> | Open logs in pager: o | Edit unit file: e | Filter: f", theme.primary)];
        if self.is_status_filter_active() {
          spans.push(Span::styled(" ●", Style::default().fg(theme.accent)));
        }
        spans.push(span(" | Quit: q", theme.primary));
        Line::from(spans)
      },
      Mode::Help => Line::from(span("Close menu: <esc>", theme.primary)),
      Mode::ActionMenu => Line::from(span("Execute action: <enter> | Close menu: <esc>", theme.primary)),
      Mode::Processing => Line::from(span("Cancel task: <esc>", theme.primary)),
      Mode::Error => Line::from(span("Close menu: <esc>", theme.primary)),
      Mode::SignalMenu => Line::from(span("Send signal: <enter> | Close menu: <esc>", theme.primary)),
      Mode::StatusFilter => Line::from(span("Toggle: <space> | All: a | None: n | Close: <esc>", theme.primary)),
    };

    f.render_widget(help_line, help_rect);
    f.render_widget(Line::from(version), version_rect);

    let title = format!("Actions for {}", selected_unit_name);
    let mut min_width = title.len() as u16 + 2; // title plus corners
    min_width = min_width.max(24); // hack: the width of the longest action name + 2

    let popup_width = min_width.min(f.area().width);

    self.filter_item_rects.clear();
    self.menu_item_rects.clear();
    if self.mode == Mode::StatusFilter {
      // Custom grouped layout for the status filter popup
      let mut lines: Vec<Line> = Vec::new();
      let mut line_item_indices: Vec<Option<usize>> = Vec::new();
      let mut idx = 0;
      for (category_name, filters) in STATUS_CATEGORIES {
        lines.push(Line::from(Span::styled(
          *category_name,
          Style::default().fg(theme.muted).add_modifier(Modifier::BOLD),
        )));
        line_item_indices.push(None);
        for filter in *filters {
          let is_checked = self.filtered_statuses.contains(filter);
          let is_selected = idx == self.filter_cursor;

          let check_span =
            if is_checked { Span::styled("✓ ", Style::default().fg(theme.accent)) } else { Span::raw("  ") };
          let name_color = if is_checked { theme.accent } else { theme.muted };
          let name_span = Span::styled(format!("{:<12}", filter.label()), Style::default().fg(name_color));
          let key_span = Span::styled(format!("{}", filter.shortcut_key()), Style::default().fg(theme.primary));

          let line_spans = vec![Span::raw("  "), check_span, name_span, Span::raw(" "), key_span];
          if is_selected {
            lines.push(Line::from(line_spans).style(Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD)));
          } else {
            lines.push(Line::from(line_spans));
          }
          line_item_indices.push(Some(idx));
          idx += 1;
        }
      }

      let filter_popup_width = 20u16.min(f.area().width);
      let height = lines.len() as u16 + 2;
      let popup = f.area().centered(Constraint::Length(filter_popup_width), Constraint::Length(height));
      self.filter_popup_rect = popup;

      for (i, item_idx) in line_item_indices.iter().enumerate() {
        if let Some(item_idx) = item_idx {
          let rect =
            Rect { x: popup.x + 1, y: popup.y + 1 + i as u16, width: popup.width.saturating_sub(2), height: 1 };
          self.filter_item_rects.push((rect, *item_idx));
        }
      }

      let paragraph = Paragraph::new(lines).block(
        Block::default()
          .borders(Borders::ALL)
          .border_type(BorderType::Rounded)
          .border_style(Style::default().fg(theme.accent))
          .title("─Status filter"),
      );

      f.render_widget(Clear, popup);
      f.render_widget(paragraph, popup);
    }

    if self.mode == Mode::ActionMenu || self.mode == Mode::SignalMenu {
      let title = match self.mode {
        Mode::ActionMenu => format!("Actions for {}", selected_unit_name),
        Mode::SignalMenu => format!("Signals for {}", selected_unit_name),
        _ => unreachable!(),
      };
      let height = self.menu_items.items.len() as u16 + 2;
      let popup = f.area().centered(Constraint::Length(popup_width), Constraint::Length(height));
      self.menu_popup_rect = popup;

      let items: Vec<ListItem> = self
        .menu_items
        .items
        .iter()
        .map(|i| {
          let key_string = Span::styled(format!(" {:1} ", i.key_string()), Style::default().fg(theme.primary));
          let line = Line::from(vec![key_string, Span::raw(&i.name)]);
          ListItem::new(line)
        })
        .collect();
      let items = List::new(items)
        .block(
          Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme.accent))
            .title(title),
        )
        .highlight_style(Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD));

      let offset = self.menu_items.state.offset();
      for i in offset..self.menu_items.items.len() {
        let rect = Rect {
          x: popup.x + 1,
          y: popup.y + 1 + (i - offset) as u16,
          width: popup.width.saturating_sub(2),
          height: 1,
        };
        if popup.height > 2 && rect.y < popup.y + popup.height - 1 {
          self.menu_item_rects.push((rect, i));
        }
      }

      f.render_widget(Clear, popup);
      f.render_stateful_widget(items, popup, &mut self.menu_items.state);
    }

    if self.mode == Mode::Processing {
      let height = self.menu_items.items.len() as u16 + 2;
      let popup = f.area().centered(Constraint::Length(popup_width), Constraint::Length(height));

      static SPINNER_CHARS: &[char] = &['⣷', '⣯', '⣟', '⡿', '⢿', '⣻', '⣽', '⣾'];

      let spinner_char = SPINNER_CHARS[self.spinner_tick as usize % SPINNER_CHARS.len()];
      // TODO: make this a spinner
      let paragraph = Paragraph::new(vec![Line::from(format!("{spinner_char}"))])
        .block(
          Block::default()
            .title("Processing")
            .border_type(BorderType::Rounded)
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme.accent)),
        )
        .style(Style::default())
        .wrap(Wrap { trim: true });

      f.render_widget(Clear, popup);
      f.render_widget(paragraph, popup);
    }

    self.render_toast(f);
  }
}

/// Parse a journalctl timestamp and return a formatted date string.
///
/// systemd v255 changed the timestamp format from `-0700` to `-07:00` (RFC 3339).
/// See: https://github.com/systemd/systemd/pull/29134
/// Parses a systemd "show" timestamp like "Wed 2026-07-08 10:00:00 PDT" into a compact
/// absolute form ("2026-07-08 10:00") and a relative one ("2d 4h ago" or "in 2d 4h").
/// The relative part assumes the host clock/timezone roughly match ours, which can be
/// slightly off in remote mode — good enough for a human-scale "how long ago".
fn format_systemd_timestamp(timestamp: &str) -> Option<(String, Option<String>)> {
  let mut parts = timestamp.split_whitespace();
  let _weekday = parts.next()?;
  let date = parts.next()?;
  let time = parts.next()?;
  let naive = chrono::NaiveDateTime::parse_from_str(&format!("{date} {time}"), "%Y-%m-%d %H:%M:%S").ok()?;
  let absolute = naive.format("%Y-%m-%d %H:%M").to_string();
  let seconds = chrono::Local::now().naive_local().signed_duration_since(naive).num_seconds();
  let relative = if seconds >= 0 {
    format!("{} ago", format_duration(seconds as u64))
  } else {
    format!("in {}", format_duration(seconds.unsigned_abs()))
  };
  Some((absolute, Some(relative)))
}

fn format_duration(seconds: u64) -> String {
  match seconds {
    s if s < 60 => format!("{s}s"),
    s if s < 3600 => format!("{}m {}s", s / 60, s % 60),
    s if s < 86400 => format!("{}h {}m", s / 3600, (s % 3600) / 60),
    s => format!("{}d {}h", s / 86400, (s % 86400) / 3600),
  }
}

fn format_bytes(bytes: u64) -> String {
  const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];
  let mut value = bytes as f64;
  let mut unit = 0;
  while value >= 1024.0 && unit < UNITS.len() - 1 {
    value /= 1024.0;
    unit += 1;
  }
  if unit == 0 {
    format!("{bytes}B")
  } else {
    format!("{value:.1}{}", UNITS[unit])
  }
}

fn format_cpu_nsec(nsec: u64) -> String {
  if nsec < 1_000_000_000 {
    format!("{}ms", nsec / 1_000_000)
  } else {
    format_duration(nsec / 1_000_000_000)
  }
}

fn parse_journalctl_timestamp(timestamp: &str) -> Option<String> {
  // %z accepts both "-0700" (systemd <v255) and "-07:00" (systemd >=v255)
  DateTime::parse_from_str(timestamp, "%Y-%m-%dT%H:%M:%S%z").ok().map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_parse_timestamp_systemd_v255_and_later() {
    // systemd >=v255 uses RFC 3339 format with colon in timezone offset
    // https://github.com/systemd/systemd/pull/29134
    let timestamp = "2025-04-26T06:04:45-07:00";
    let result = parse_journalctl_timestamp(timestamp);
    assert_eq!(result, Some("2025-04-26 06:04".to_string()));
  }

  #[test]
  fn test_parse_timestamp_systemd_before_v255() {
    // systemd <v255 uses ISO 8601 format without colon in timezone offset
    let timestamp = "2025-10-06T11:07:44-0700";
    let result = parse_journalctl_timestamp(timestamp);
    assert_eq!(result, Some("2025-10-06 11:07".to_string()));
  }
}
