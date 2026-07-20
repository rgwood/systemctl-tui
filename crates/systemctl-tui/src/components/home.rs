use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use futures::Future;
use fuzzy_matcher::{skim::SkimMatcherV2, FuzzyMatcher};
use indexmap::IndexMap;
use itertools::Itertools;
use ratatui::{
  layout::{Constraint, Direction, Layout, Margin, Position, Rect},
  style::{Color, Modifier, Style},
  text::{Line, Span},
  widgets::{
    Block, BorderType, Borders, Clear, List, ListItem, ListState, Paragraph, Scrollbar, ScrollbarOrientation,
    ScrollbarState, Wrap,
  },
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
  journal::{parse_json_log_line, LogEntry},
  systemd::{
    self, diagnose_missing_logs, parse_journalctl_error, Scope, UnitFile, UnitId, UnitKind, UnitRuntimeInfo, UnitScope,
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
  UnitExplanation,
}

#[derive(Debug, Default, Copy, Clone, Eq, PartialEq)]
pub enum LogOrder {
  #[default]
  NewestFirst,
  OldestFirst,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash)]
pub enum UnitStatus {
  // Unit type
  Service,
  Timer,
  Other,
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
  const ALL: [UnitStatus; 12] = [
    UnitStatus::Service,
    UnitStatus::Timer,
    UnitStatus::Other,
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

  /// Hidden by default on first run: not-found units are dangling references
  /// that can't be started, and masked units are deliberately disabled.
  const DEFAULT_HIDDEN: [UnitStatus; 2] = [UnitStatus::Masked, UnitStatus::NotFound];

  fn label(&self) -> &'static str {
    match self {
      UnitStatus::Service => "services",
      UnitStatus::Timer => "timers",
      UnitStatus::Other => "other",
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
      UnitStatus::Service => KeyCode::Char('v'),
      UnitStatus::Timer => KeyCode::Char('t'),
      UnitStatus::Other => KeyCode::Char('o'),
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

  fn unit_kind_bucket(kind: UnitKind) -> UnitStatus {
    match kind {
      UnitKind::Service => UnitStatus::Service,
      UnitKind::Timer => UnitStatus::Timer,
      UnitKind::Other => UnitStatus::Other,
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
  ("Type", &[UnitStatus::Service, UnitStatus::Timer, UnitStatus::Other]),
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
  pub logs: Vec<LogEntry>,
  pub logs_scroll_offset: u16,
  /// Maximum wrapped-line scroll offset calculated during the most recent render.
  pub logs_max_scroll_offset: u16,
  pub log_order: LogOrder,
  /// Whether new entries should keep an oldest-first view pinned to the end.
  pub follow_logs: bool,
  /// Runtime info for the currently selected unit, fetched lazily after selection
  pub runtime_info: Option<UnitRuntimeInfo>,
  pub mode: Mode,
  pub previous_mode: Option<Mode>,
  pub filter_return_mode: Mode,
  pub input: Input,
  pub menu_items: StatefulList<MenuItem>,
  pub cancel_token: Option<CancellationToken>,
  pub spinner_tick: u8,
  pub error_message: String,
  pub refresh_error_shown: bool,
  pub action_tx: Option<mpsc::UnboundedSender<Action>>,
  pub journalctl_tx: Option<std::sync::mpsc::Sender<UnitId>>,
  pub fuzzy_matcher: SkimMatcherV2,
  pub filtered_statuses: HashSet<UnitStatus>,
  pub filter_cursor: usize,
  /// Number of units excluded by the status filter (before fuzzy search),
  /// shown in the services panel title.
  pub status_hidden_count: usize,
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
  /// Screen rect of the clickable description in the details pane, as of the most recent render.
  /// `Some` only when the selected unit has a baked-in explanation.
  pub explanation_button_rect: Option<Rect>,
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
  pub fn new(scope: Scope, limit_units: &[String], log_order: LogOrder) -> Self {
    let limit_units = limit_units.to_vec();
    let filtered_statuses = UnitStatus::ALL.into_iter().filter(|s| !UnitStatus::DEFAULT_HIDDEN.contains(s)).collect();
    Self { scope, limit_units, filtered_statuses, filter_cursor: 0, log_order, follow_logs: true, ..Default::default() }
  }

  fn reset_logs_viewport(&mut self) {
    self.follow_logs = true;
    self.logs_scroll_offset = match self.log_order {
      LogOrder::NewestFirst => 0,
      LogOrder::OldestFirst => u16::MAX,
    };
    self.clear_logs_selection();
  }

  fn logs_for_pager(&self) -> Vec<String> {
    match self.log_order {
      LogOrder::NewestFirst => self.logs.iter().rev().map(LogEntry::to_plain_string).collect(),
      LogOrder::OldestFirst => self.logs.iter().map(LogEntry::to_plain_string).collect(),
    }
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
    self.reset_logs_viewport();
  }

  pub fn previous(&mut self) {
    self.logs = vec![];
    self.runtime_info = None;
    self.filtered_units.previous();
    self.get_logs();
    self.reset_logs_viewport();
  }

  pub fn select(&mut self, index: Option<usize>, refresh_logs: bool) {
    if refresh_logs {
      self.logs = vec![];
      self.runtime_info = None;
    }
    self.filtered_units.select(index);
    if refresh_logs {
      self.get_logs();
      self.reset_logs_viewport();
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
    let previous_unit_ids: Vec<_> = self.filtered_units.items.iter().map(|m| m.unit.id()).collect();
    let search_value = self.input.value();
    let status_filtered_units: Vec<_> = self
      .all_units
      .values()
      .filter(|u| {
        let passes_type = self.filtered_statuses.contains(&UnitStatus::unit_kind_bucket(u.kind()));
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

        passes_type && passes_activation && passes_enablement && passes_load
      })
      .collect();
    self.status_hidden_count = self.all_units.len() - status_filtered_units.len();

    let matching: Vec<MatchedUnit> = if search_value.is_empty() {
      // No search - return all units without highlighting
      status_filtered_units.into_iter().map(|u| MatchedUnit { unit: u.clone(), match_indices: vec![] }).collect()
    } else {
      // Fuzzy match with indices for highlighting
      let mut scored: Vec<(i64, MatchedUnit)> = status_filtered_units
        .into_iter()
        .filter_map(|u| {
          let short_name = u.short_name();
          let matched = self.fuzzy_matcher.fuzzy_indices(short_name, search_value).or_else(|| {
            // The suffix is intentionally hidden in the list, so only consider
            // it when the query explicitly looks like a full unit name. This
            // keeps ordinary queries such as "ser" from matching every service.
            search_value.contains('.').then(|| self.fuzzy_matcher.fuzzy_indices(&u.name, search_value)).flatten()
          });
          matched.map(|(score, indices)| {
            let visible_indices = indices.into_iter().filter(|&index| index < short_name.len()).collect();
            (score, MatchedUnit { unit: u.clone(), match_indices: visible_indices })
          })
        })
        .collect();

      // Sort by score descending (best matches first)
      scored.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
      scored.into_iter().map(|(_, m)| m).collect()
    };

    let matching_unit_ids: Vec<_> = matching.iter().map(|m| m.unit.id()).collect();
    self.filtered_units.items = matching;

    // Keep the viewport stable when a background refresh only updates unit metadata.
    // If filtering changed the list itself, reset the offset so a stale offset from a
    // larger list cannot leave the first matches scrolled out of view.
    if matching_unit_ids != previous_unit_ids {
      *self.filtered_units.state.offset_mut() = 0;
    }

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

  fn disable_service(&mut self, service: UnitId, runtime: bool) {
    let cancel_token = CancellationToken::new();
    let future = systemd::disable_service(service.clone(), runtime, cancel_token.clone());
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
          let _ = tx.send(Action::RefreshUnitRuntimeInfo(service.clone()));
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

        let mut args = vec![
          "--quiet",
          "--output=json",
          "--output-fields=PRIORITY,MESSAGE,SYSLOG_IDENTIFIER,_COMM,_PID",
          "--lines=500",
          "-u",
        ];

        args.push(&unit.name);

        if unit.scope == UnitScope::User {
          args.push("--user");
        }

        match crate::ssh::host_command("journalctl", &args).output() {
          Ok(output) => {
            if output.status.success() {
              info!("Got logs for {} in {:?}", unit.name, start.elapsed());
              if let Ok(stdout) = std::str::from_utf8(&output.stdout) {
                let mut logs =
                  stdout.trim().lines().filter(|l| !l.is_empty()).flat_map(parse_json_log_line).collect_vec();

                if logs.is_empty() {
                  let diagnostic = diagnose_missing_logs(&unit);
                  logs = vec![LogEntry::plain(diagnostic.message())];
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
              let _ =
                tx.send(Action::SetLogs { unit: unit.clone(), logs: vec![LogEntry::plain(diagnostic.message())] });
              let _ = tx.send(Action::Render);
            }
          },
          Err(e) => {
            warn!("Error getting logs for {}: {}", unit.name, e);
            let _ = tx.send(Action::SetLogs {
              unit: unit.clone(),
              logs: vec![LogEntry::plain(format!("Failed to run journalctl: {}", e))],
            });
            let _ = tx.send(Action::Render);
          },
        }

        // Then follow the logs
        // Splitting this into two commands is a bit of a hack that makes it easier to get the initial batch of logs
        // This does mean that we'll miss any logs that are written between the two commands, low enough risk for now
        let tx = tx.clone();
        last_follow_handle = Some(tokio::spawn(async move {
          let mut args = vec![
            "-u",
            &unit.name,
            "--output=json",
            "--output-fields=PRIORITY,MESSAGE,SYSLOG_IDENTIFIER,_COMM,_PID",
            "--follow",
            "--lines=0",
            "--quiet",
          ];
          if unit.scope == UnitScope::User {
            args.push("--user");
          }
          let mut command = crate::ssh::host_tokio_command("journalctl", &args);
          command.stdout(Stdio::piped());
          command.stderr(Stdio::piped());
          command.kill_on_drop(true);

          let mut child = match command.spawn() {
            Ok(child) => child,
            Err(e) => {
              error!("Failed to spawn journalctl: {:?}", e);
              return;
            },
          };

          let Some(stdout) = child.stdout.take() else { return };

          let reader = tokio::io::BufReader::new(stdout);
          let mut lines = reader.lines();
          // An Err on read (e.g. remote disconnect) just ends log following
          while let Ok(Some(line)) = lines.next_line().await {
            let _ = tx.send(Action::AppendLogLines { unit: unit.clone(), lines: parse_json_log_line(&line) });
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
        KeyCode::Char('r') => return vec![Action::ToggleLogOrder, Action::Render],
        // vim-style half-page scrolling
        KeyCode::Char('d') => return vec![Action::ScrollDown(10), Action::Render],
        KeyCode::Char('u') => return vec![Action::ScrollUp(10), Action::Render],
        _ => (),
      }
    }

    if matches!(key.code, KeyCode::Char('?')) || matches!(key.code, KeyCode::F(1)) {
      return vec![Action::ToggleHelp, Action::Render];
    }

    if matches!(key.code, KeyCode::F(2)) && matches!(self.mode, Mode::Search | Mode::ServiceList) {
      return vec![Action::EnterMode(Mode::StatusFilter)];
    }

    match key.code {
      KeyCode::PageDown => return vec![Action::ScrollDown(2), Action::Render],
      KeyCode::PageUp => return vec![Action::ScrollUp(2), Action::Render],
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
            vec![Action::OpenLogsInPager { logs: self.logs_for_pager() }]
          },
          KeyCode::Char('r') => {
            vec![Action::ToggleLogOrder, Action::Render]
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
      Mode::UnitExplanation => match key.code {
        KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => vec![Action::EnterMode(Mode::ServiceList)],
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
        KeyCode::Esc => vec![Action::EnterMode(self.filter_return_mode)],
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

    if self.mode == Mode::Error || self.mode == Mode::UnitExplanation {
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
            vec![Action::EnterMode(self.filter_return_mode)]
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
        let explanation_rect = self.explanation_button_rect;
        let over_explanation = |p| explanation_rect.map(|r| r.contains(p)).unwrap_or(false);
        let was_hovering = (self.hovered_field(self.mouse_position), over_explanation(self.mouse_position));
        self.mouse_position = pos;
        let is_hovering = (self.hovered_field(pos), over_explanation(pos));
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
        if let Some(rect) = self.explanation_button_rect {
          if rect.contains(pos) {
            return vec![Action::EnterMode(Mode::UnitExplanation)];
          }
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
            let previously_selected_action = (self.mode == Mode::ActionMenu)
              .then(|| self.menu_items.selected().map(|item| item.name.clone()))
              .flatten();
            let selected = self.filtered_units.selected()?;
            let is_timer = selected.unit.kind() == UnitKind::Timer;
            let mut menu_items = if is_timer {
              let mut items = Vec::new();
              if selected.unit.is_active() {
                items.push(MenuItem::new(
                  "Stop timer",
                  Action::StopService(selected.unit.id()),
                  Some(KeyCode::Char('t')),
                ));
              } else {
                items.push(MenuItem::new(
                  "Start timer",
                  Action::StartService(selected.unit.id()),
                  Some(KeyCode::Char('s')),
                ));
              }

              match selected.unit.enablement_state.as_deref() {
                Some(enablement @ ("enabled" | "enabled-runtime")) => items.push(MenuItem::new(
                  "Disable timer",
                  Action::DisableService { unit: selected.unit.id(), runtime: enablement == "enabled-runtime" },
                  Some(KeyCode::Char('d')),
                )),
                Some("disabled") => items.push(MenuItem::new(
                  "Enable timer",
                  Action::EnableService(selected.unit.id()),
                  Some(KeyCode::Char('n')),
                )),
                _ => {},
              }
              items
            } else {
              vec![
                MenuItem::new("Start", Action::StartService(selected.unit.id()), Some(KeyCode::Char('s'))),
                MenuItem::new("Stop", Action::StopService(selected.unit.id()), Some(KeyCode::Char('t'))),
                MenuItem::new("Restart", Action::RestartService(selected.unit.id()), Some(KeyCode::Char('r'))),
                MenuItem::new("Reload", Action::ReloadService(selected.unit.id()), Some(KeyCode::Char('l'))),
                MenuItem::new("Enable", Action::EnableService(selected.unit.id()), Some(KeyCode::Char('n'))),
                MenuItem::new(
                  "Disable",
                  Action::DisableService {
                    unit: selected.unit.id(),
                    runtime: selected.unit.enablement_state.as_deref() == Some("enabled-runtime"),
                  },
                  Some(KeyCode::Char('d')),
                ),
                MenuItem::new("Kill", Action::EnterMode(Mode::SignalMenu), Some(KeyCode::Char('K'))),
              ]
            };

            if is_timer {
              if let Some(target) = self.runtime_info.as_ref().and_then(|info| info.triggered_unit.as_ref()) {
                let label = format!("Start {target} now");
                menu_items.push(MenuItem::new(
                  &label,
                  Action::StartService(UnitId { name: target.clone(), scope: selected.unit.scope }),
                  Some(KeyCode::Char('g')),
                ));
              }
            }

            menu_items.push(MenuItem::new(
              "Open logs in pager",
              Action::OpenLogsInPager { logs: self.logs_for_pager() },
              Some(KeyCode::Char('o')),
            ));

            if let Some(Ok(file_path)) = &selected.unit.file_path {
              menu_items.push(MenuItem::new("Copy unit file path", Action::CopyUnitFilePath, Some(KeyCode::Char('c'))));
              menu_items.push(MenuItem::new(
                "Edit unit file",
                Action::EditUnitFile { unit: selected.unit.id(), path: file_path.clone() },
                Some(KeyCode::Char('e')),
              ));
            }

            let selected_index = previously_selected_action
              .and_then(|name| menu_items.iter().position(|item| item.name == name))
              .unwrap_or(0);
            self.menu_items = StatefulList::with_items(menu_items);
            self.menu_items.state.select(Some(selected_index));
          }
        } else if mode == Mode::SignalMenu {
          {
            let selected = self.filtered_units.selected()?;
            let signals = vec![
              ("SIGTERM", KeyCode::Char('t')),
              ("SIGHUP", KeyCode::Char('h')),
              ("SIGINT", KeyCode::Char('i')),
              ("SIGQUIT", KeyCode::Char('q')),
              ("SIGKILL", KeyCode::Char('9')),
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
          self.filter_return_mode = self.mode;
        }

        self.mode = mode;
        return Some(Action::Render);
      },
      Action::EnterError(err) => {
        tracing::error!(err);
        self.error_message = err;
        return Some(Action::EnterMode(Mode::Error));
      },
      Action::ServicesRefreshFailed(err) => {
        // The refresh tick retries continuously (e.g. after a remote disconnect); show the modal
        // once rather than re-opening it every tick after the user dismisses it
        if !self.refresh_error_shown {
          self.refresh_error_shown = true;
          return Some(Action::EnterError(err));
        }
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
        let target_changed = self.runtime_info.as_ref().and_then(|current| current.triggered_unit.as_ref())
          != info.triggered_unit.as_ref();
        let rebuild_timer_menu = self.mode == Mode::ActionMenu
          && target_changed
          && self
            .filtered_units
            .selected()
            .is_some_and(|selected| selected.unit.id() == unit && selected.unit.kind() == UnitKind::Timer);
        if let Some(selected) = self.filtered_units.selected() {
          if selected.unit.id() == unit {
            self.runtime_info = Some(*info);
          }
        }
        if rebuild_timer_menu {
          return Some(Action::EnterMode(Mode::ActionMenu));
        }
      },
      Action::SetLogs { unit, logs } => {
        if let Some(selected) = self.filtered_units.selected() {
          if selected.unit.id() == unit {
            self.logs = logs;
            self.reset_logs_viewport();
          }
        }
      },
      Action::AppendLogLines { unit, lines } => {
        if let Some(selected) = self.filtered_units.selected() {
          if selected.unit.id() == unit {
            self.logs.extend(lines);
            if self.log_order == LogOrder::OldestFirst && self.follow_logs {
              self.logs_scroll_offset = u16::MAX;
            }
          }
        }
      },
      Action::ScrollUp(offset) => {
        self.logs_scroll_offset = self.logs_scroll_offset.saturating_sub(offset);
        self.follow_logs = false;
        info!("scroll offset: {}", self.logs_scroll_offset);
        self.clear_logs_selection();
      },
      Action::ScrollDown(offset) => {
        self.logs_scroll_offset = self.logs_scroll_offset.saturating_add(offset).min(self.logs_max_scroll_offset);
        self.follow_logs =
          self.log_order == LogOrder::OldestFirst && self.logs_scroll_offset == self.logs_max_scroll_offset;
        info!("scroll offset: {}", self.logs_scroll_offset);
        self.clear_logs_selection();
      },
      Action::ScrollToTop => {
        self.logs_scroll_offset = 0;
        self.follow_logs = false;
        self.clear_logs_selection();
      },
      Action::ScrollToBottom => {
        // Clamped to the actual wrapped height at render time (see `render`).
        self.logs_scroll_offset = u16::MAX;
        self.follow_logs = true;
        self.clear_logs_selection();
      },
      Action::ToggleLogOrder => {
        self.log_order = match self.log_order {
          LogOrder::NewestFirst => LogOrder::OldestFirst,
          LogOrder::OldestFirst => LogOrder::NewestFirst,
        };
        self.reset_logs_viewport();
      },

      Action::StartService(service_name) => self.start_service(service_name),
      Action::StopService(service_name) => self.stop_service(service_name),
      Action::ReloadService(service_name) => self.reload_service(service_name),
      Action::RestartService(service_name) => self.restart_service(service_name),
      Action::EnableService(service_name) => self.enable_service(service_name),
      Action::DisableService { unit, runtime } => self.disable_service(unit, runtime),
      Action::RefreshServices => {
        let tx = self.action_tx.clone().unwrap();
        let scope = self.scope;
        let limit_units = self.limit_units.to_vec();
        tokio::spawn(async move {
          match systemd::get_all_services(scope, &limit_units).await {
            Ok(units) => {
              let _ = tx.send(Action::SetServices(units));
            },
            Err(e) => {
              error!("Failed to get services: {:?}", e);
              let error_string = match crate::ssh::remote_host() {
                Some(ssh_host) => format!(
                  "Lost connection to {} (or systemd stopped responding):\n{}\n\nRestart systemctl-tui to reconnect.",
                  ssh_host.host, e
                ),
                None => {
                  format!("Failed to get services:\n{e}\n\nCheck that systemd is running, or try running this tool with sudo.")
                },
              };
              let _ = tx.send(Action::ServicesRefreshFailed(error_string));
            },
          }
        });
      },
      Action::SetServices(units) => {
        self.refresh_error_shown = false;
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
      Action::RefreshUnitRuntimeInfo(unit) => {
        let tx = self.action_tx.clone().unwrap();
        tokio::task::spawn_blocking(move || match systemd::get_unit_runtime_info(&unit) {
          Ok(info) => {
            let _ = tx.send(Action::SetUnitRuntimeInfo { unit, info: Box::new(info) });
            let _ = tx.send(Action::Render);
          },
          Err(e) => error!("Failed to refresh runtime info for {}: {e}", unit.name),
        });
      },
      Action::RefreshStatusFilterMenu => {
        self.refresh_filtered_units();
        return Some(Action::Render);
      },
      Action::SetUnitFiles(unit_files) => {
        let selected_timer = (self.mode == Mode::ActionMenu)
          .then(|| {
            self.filtered_units.selected().and_then(|selected| {
              (selected.unit.kind() == UnitKind::Timer)
                .then(|| (selected.unit.id(), selected.unit.enablement_state.clone()))
            })
          })
          .flatten();
        self.merge_unit_files(unit_files);
        if let Some((unit, old_enablement)) = selected_timer {
          let enablement_changed =
            self.all_units.get(&unit).map(|unit| &unit.enablement_state) != Some(&old_enablement);
          if enablement_changed {
            return Some(Action::EnterMode(Mode::ActionMenu));
          }
        }
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
        let kind_prefix = match m.unit.kind() {
          UnitKind::Timer => "[T] ",
          UnitKind::Service | UnitKind::Other => "",
        };

        if m.match_indices.is_empty() {
          ListItem::new(Line::from(vec![
            Span::styled(kind_prefix, Style::default().fg(theme.muted).add_modifier(Modifier::DIM)),
            Span::styled(name, Style::default().fg(color)),
          ]))
        } else {
          // Build spans with highlighted matched characters
          let mut spans = vec![Span::styled(kind_prefix, Style::default().fg(theme.muted).add_modifier(Modifier::DIM))];
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
          .title(if self.is_status_filter_active() {
            format!("─Units ({} hidden)", self.status_hidden_count)
          } else {
            "─Units".to_string()
          }),
      )
      .highlight_style(Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD));

    let chunks =
      Layout::new(Direction::Horizontal, [Constraint::Min(30), Constraint::Percentage(100)]).split(main_panel);
    let right_panel = chunks[1];

    self.services_panel = chunks[0];

    f.render_stateful_widget(items, chunks[0], &mut self.filtered_units.state);

    let selected_item = self.filtered_units.selected();
    let is_remote = crate::ssh::remote_host().is_some();

    // Details rows: base unit facts on the left, runtime stats (fetched lazily on selection)
    // in a second column on wide terminals, or folded into a single "Runtime" row otherwise.
    let dim = Style::default().add_modifier(Modifier::DIM);
    let mut rows: Vec<(&str, Line)> = vec![];
    let mut stat_rows: Vec<(&str, Line)> = vec![];
    let mut explanation: Option<&'static str> = None;

    if let Some(m) = selected_item {
      rows.push(("Description", Line::from(m.unit.description.as_str())));
      explanation = crate::unit_descriptions::explain(&m.unit.name, m.unit.scope);

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
        if let Some((absolute, relative)) = since.and_then(|timestamp| format_systemd_timestamp(timestamp, is_remote)) {
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
        if m.unit.kind() == UnitKind::Timer {
          if let Some((absolute, relative)) =
            info.next_elapse.as_deref().and_then(|timestamp| format_systemd_timestamp(timestamp, is_remote))
          {
            let relative = relative.map(|r| format!(" ({r})")).unwrap_or_default();
            stat_rows.push(("Next trigger", Line::from(format!("{absolute}{relative}"))));
          }
          if let Some((absolute, relative)) =
            info.last_trigger.as_deref().and_then(|timestamp| format_systemd_timestamp(timestamp, is_remote))
          {
            let relative = relative.map(|r| format!(" ({r})")).unwrap_or_default();
            stat_rows.push(("Last trigger", Line::from(format!("{absolute}{relative}"))));
          }
          if let Some(unit) = &info.triggered_unit {
            stat_rows.push(("Activates", Line::from(unit.as_str())));
          }
          for schedule in &info.timer_schedules {
            stat_rows.push(("Schedule", Line::from(schedule.as_str())));
          }
          if info.persistent == Some(true) {
            stat_rows.push(("Persistent", Line::from("yes")));
          }
          if let Some(delay) = &info.randomized_delay {
            if delay != "0" {
              stat_rows.push(("Random delay", Line::from(delay.as_str())));
            }
          }
          if let Some(accuracy) = &info.accuracy {
            stat_rows.push(("Accuracy", Line::from(accuracy.as_str())));
          }
        }
      }
    }

    let timer_details = selected_item.is_some_and(|m| m.unit.kind() == UnitKind::Timer);
    let two_column_min_width = if timer_details { 120 } else { 90 };
    let two_columns = right_panel.width >= two_column_min_width && !stat_rows.is_empty();
    if !two_columns && !stat_rows.is_empty() {
      if timer_details {
        // Timer metadata is the main reason to select a timer. Keep it readable on
        // narrow terminals instead of folding the whole schedule into one clipped line.
        rows.append(&mut stat_rows);
      } else {
        // Narrow terminal: fold service stats into one line to save vertical space.
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
    }

    fn label_width(rows: &[(&str, Line)]) -> u16 {
      rows.iter().map(|(label, _)| label.len() + 2).max().unwrap_or(0) as u16
    }

    fn split_labels_values<'a>(rows: Vec<(&'a str, Line<'a>)>) -> (Vec<Line<'a>>, Vec<Line<'a>>) {
      rows.into_iter().map(|(label, value)| (Line::from(format!("{label}: ")), value)).unzip()
    }

    // Size the details panel to its content instead of a fixed height
    let grid_height = rows.len().max(stat_rows.len()).max(1) as u16;
    // Always leave a few rows for logs. On tiny terminals the lower-priority
    // detail rows are clipped, which is more useful than hiding logs entirely.
    let max_details_height = right_panel.height.saturating_sub(4).max(3).min(right_panel.height);
    let details_height = (grid_height + 2).min(max_details_height);
    let right_panel =
      Layout::new(Direction::Vertical, [Constraint::Length(details_height), Constraint::Percentage(100)])
        .split(right_panel);
    let details_panel = right_panel[0];
    let logs_panel = right_panel[1];

    let details_block = Block::default().title("─Details").borders(Borders::ALL).border_type(BorderType::Rounded);
    let details_inner = details_block.inner(details_panel);
    f.render_widget(details_block, details_panel);

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
    fn register_copyable_fields(
      values: &mut [Line],
      pane: Rect,
      mouse: Position,
      skip: Option<usize>,
      out: &mut Vec<(Rect, String)>,
    ) {
      for (i, line) in values.iter_mut().enumerate() {
        if i as u16 >= pane.height {
          break;
        }
        if skip == Some(i) {
          continue;
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
    self.explanation_button_rect = None;
    let mut explanation_description = None;
    if explanation.is_some() && !values.is_empty() && panes[1].width > 0 {
      let (separator, hint) = match panes[1].width {
        1 => ("", "?"),
        2..=3 => (" ", "?"),
        _ => (" ", "[?]"),
      };
      let hint_width = Line::from(format!("{separator}{hint}")).width() as u16;
      let description_width = panes[1].width.saturating_sub(hint_width);
      let full_description = values[0].spans.iter().map(|s| s.content.as_ref()).collect::<String>();

      fn truncate_to_width(text: &str, width: u16) -> String {
        if width == 0 {
          return String::new();
        }
        if Line::from(text).width() <= width as usize {
          return text.to_string();
        }

        let mut truncated = String::new();
        let content_width = width.saturating_sub(1) as usize;
        for ch in text.chars() {
          truncated.push(ch);
          if Line::from(truncated.as_str()).width() > content_width {
            truncated.pop();
            break;
          }
        }
        truncated.push('…');
        truncated
      }

      let description = truncate_to_width(&full_description, description_width);
      let description_width = Line::from(description.as_str()).width() as u16;
      let separator_width = Line::from(separator).width() as u16;
      let hint_width = Line::from(hint).width() as u16;
      let button_rect =
        Rect { x: panes[1].x + description_width + separator_width, y: panes[1].y, width: hint_width, height: 1 };
      let hovered = button_rect.contains(self.mouse_position);
      let mut hint_style = Style::default().fg(theme.accent).add_modifier(Modifier::UNDERLINED);
      if hovered {
        hint_style = hint_style.add_modifier(Modifier::BOLD | Modifier::REVERSED);
      }
      values[0] = Line::from(vec![Span::raw(description), Span::raw(separator), Span::styled(hint, hint_style)]);
      self.explanation_button_rect = Some(button_rect);
      explanation_description = Some((full_description, description_width));
    }

    let explanation_row = self.explanation_button_rect.map(|_| 0);
    if let Some((description, width)) = explanation_description {
      if width > 0 {
        let rect = Rect { x: panes[1].x, y: panes[1].y, width, height: 1 };
        if rect.contains(self.mouse_position) {
          values[0].spans[0].style = values[0].spans[0].style.add_modifier(Modifier::BOLD);
        }
        self.copyable_fields.push((rect, description));
      }
    }
    register_copyable_fields(&mut values, panes[1], self.mouse_position, explanation_row, &mut self.copyable_fields);
    f.render_widget(Paragraph::new(labels).alignment(ratatui::layout::Alignment::Right), panes[0]);
    f.render_widget(Paragraph::new(values), panes[1]);

    if two_columns {
      let (stat_labels, mut stat_values) = split_labels_values(stat_rows);
      register_copyable_fields(&mut stat_values, panes[3], self.mouse_position, None, &mut self.copyable_fields);
      f.render_widget(Paragraph::new(stat_labels).alignment(ratatui::layout::Alignment::Right), panes[2]);
      f.render_widget(Paragraph::new(stat_values), panes[3]);
    }

    let logs: Box<dyn Iterator<Item = &LogEntry>> = match self.log_order {
      LogOrder::NewestFirst => Box::new(self.logs.iter().rev()),
      LogOrder::OldestFirst => Box::new(self.logs.iter()),
    };
    let log_lines = logs
      .map(|entry| {
        // Colorize by syslog priority, like journalctl does in a terminal:
        // err and worse red, warnings yellow, debug dimmed.
        let content_style = match entry.priority {
          Some(p) if p <= 3 => Style::default().fg(Color::Red),
          Some(4) => Style::default().fg(Color::Yellow),
          Some(7) => Style::default().add_modifier(Modifier::DIM),
          _ => Style::default(),
        };
        match &entry.timestamp {
          Some(timestamp) => Line::from(vec![
            Span::styled(timestamp.as_str(), Style::default().add_modifier(Modifier::DIM)),
            Span::raw(" "),
            Span::styled(entry.content.as_str(), content_style),
          ]),
          None => Line::from(Span::styled(entry.content.as_str(), content_style)),
        }
      })
      .collect_vec();

    let order_label = match self.log_order {
      LogOrder::NewestFirst => "newest first",
      LogOrder::OldestFirst => "oldest first",
    };
    let logs_unit = selected_item.map(|selected| selected.unit.name.as_str()).unwrap_or("unit");
    let paragraph = Paragraph::new(log_lines)
      .block(
        Block::default()
          .title(format!("─Logs — {logs_unit} [{order_label}; ctrl+r to reverse]"))
          .borders(Borders::ALL)
          .border_type(BorderType::Rounded),
      )
      .style(Style::default())
      .wrap(Wrap { trim: true });

    // line_count wraps at the given width but includes the block's border rows in its count,
    // so wrap at the inner width and compare against the full panel height.
    let inner_width = logs_panel.width.saturating_sub(2);
    let total_lines = u16::try_from(paragraph.line_count(inner_width)).unwrap_or(u16::MAX);
    let max_offset = total_lines.saturating_sub(logs_panel.height);
    self.logs_max_scroll_offset = max_offset;
    self.logs_scroll_offset = if self.log_order == LogOrder::OldestFirst && self.follow_logs {
      max_offset
    } else {
      self.logs_scroll_offset.min(max_offset)
    };

    let paragraph = paragraph.scroll((self.logs_scroll_offset, 0));
    f.render_widget(paragraph, logs_panel);

    if max_offset > 0 {
      let viewport_height = logs_panel.height.saturating_sub(2) as usize;
      // ScrollbarState treats content_length - 1 as its maximum position, so describe the
      // possible scroll offsets here rather than passing the paragraph's total height.
      let scroll_positions = max_offset as usize + 1;
      let mut scrollbar_state = ScrollbarState::new(scroll_positions)
        .position(self.logs_scroll_offset as usize)
        .viewport_content_length(viewport_height);
      let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight).begin_symbol(None).end_symbol(None);
      f.render_stateful_widget(scrollbar, logs_panel.inner(Margin::new(0, 1)), &mut scrollbar_state);
    }

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
      let popup = f.area().centered(Constraint::Length(50), Constraint::Length(20));

      let primary = |s| Span::styled(s, Style::default().fg(theme.primary));
      let help_lines = vec![
        Line::from(""),
        Line::from(Span::styled("Shortcuts", Style::default().add_modifier(Modifier::UNDERLINED))),
        Line::from(""),
        Line::from(vec![primary("ctrl+C"), Span::raw(" or "), primary("ctrl+Q"), Span::raw(" to quit")]),
        Line::from(vec![primary("ctrl+L"), Span::raw(" toggles the logger pane")]),
        Line::from(vec![primary("ctrl+R"), Span::raw(" reverses the log order")]),
        Line::from(vec![primary("PageUp"), Span::raw(" / "), primary("PageDown"), Span::raw(" scroll the logs")]),
        Line::from(vec![primary("Home"), Span::raw(" / "), primary("End"), Span::raw(" scroll to top/bottom")]),
        Line::from(vec![primary("Enter"), Span::raw(" or "), primary("Space"), Span::raw(" open the action menu")]),
        Line::from(vec![primary("f"), Span::raw(" / "), primary("F2"), Span::raw(" filter units")]),
        Line::from(vec![primary("?"), Span::raw(" / "), primary("F1"), Span::raw(" open this help pane")]),
        Line::from(vec![primary("mouse"), Span::raw(": drag to select+copy logs, wheel to scroll")]),
        Line::from(""),
        Line::from(Span::styled("Vim Style Shortcuts", Style::default().add_modifier(Modifier::UNDERLINED))),
        Line::from(""),
        Line::from(vec![primary("j"), Span::raw(" / "), primary("k"), Span::raw(" navigate down/up")]),
        Line::from(vec![primary("g"), Span::raw(" / "), primary("G"), Span::raw(" jump to first/last unit")]),
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

    if self.mode == Mode::UnitExplanation {
      let selected = self.filtered_units.selected();
      let text = selected.and_then(|s| crate::unit_descriptions::explain(&s.unit.name, s.unit.scope)).unwrap_or("");
      let title = selected.map(|s| format!("─What is {}?", s.unit.name)).unwrap_or_else(|| "─What is it?".to_string());

      let area = f.area();
      let popup_width = 64u16.min(area.width.saturating_sub(4)).max(20);
      let wrapped = word_wrap(text, popup_width.saturating_sub(4) as usize);
      let disclaimer = word_wrap(
        "Matched by unit name and scope; locally replaced units may differ.",
        popup_width.saturating_sub(4) as usize,
      );
      // leading blank + text + blank + disclaimer + blank + hint + 2 borders
      let popup_height = (wrapped.len() as u16 + disclaimer.len() as u16 + 6).min(area.height.saturating_sub(2));
      let popup = area.centered(Constraint::Length(popup_width), Constraint::Length(popup_height));

      let mut lines = vec![Line::from("")];
      lines.extend(wrapped.into_iter().map(Line::from));
      lines.push(Line::from(""));
      lines.extend(disclaimer.into_iter().map(|line| Line::from(Span::styled(line, dim))));
      lines.push(Line::from(""));
      lines.push(Line::from(Span::styled("Press esc to close", dim)));

      let paragraph = Paragraph::new(lines)
        .block(
          Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme.accent))
            .padding(ratatui::widgets::Padding::horizontal(1)),
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
      Mode::Search => Line::from(span("Actions: enter | Filter: F2 | Navigate: tab", theme.primary)),
      Mode::ServiceList => {
        let mut spans = vec![span("Actions: enter | Logs: o | Edit: e | Filter: f/F2", theme.primary)];
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
      Mode::UnitExplanation => Line::from(span("Close: <esc>", theme.primary)),
      Mode::SignalMenu => Line::from(span("Send signal: <enter> | Close menu: <esc>", theme.primary)),
      Mode::StatusFilter => Line::from(span("Toggle: <space> | All: a | None: n | Close: <esc>", theme.primary)),
    };

    f.render_widget(help_line, help_rect);
    f.render_widget(Line::from(version), version_rect);

    let title = format!("Actions for {}", selected_unit_name);
    let mut min_width = title.len() as u16 + 2; // title plus corners
    let item_width = self.menu_items.items.iter().map(|item| item.name.chars().count() as u16 + 5).max().unwrap_or(0);
    min_width = min_width.max(item_width);

    let popup_width = min_width.min(f.area().width);

    self.filter_item_rects.clear();
    self.menu_item_rects.clear();
    if self.mode == Mode::StatusFilter {
      // Custom grouped layout for the status filter popup
      let mut lines: Vec<Line> = Vec::new();
      let mut line_item_indices: Vec<Option<usize>> = Vec::new();
      let mut idx = 0;
      let full_height = STATUS_CATEGORIES.len() + UnitStatus::ALL.len() + 2;
      let compact = usize::from(f.area().height) < full_height;
      for (category_name, filters) in STATUS_CATEGORIES {
        if !compact {
          lines.push(Line::from(Span::styled(
            *category_name,
            Style::default().fg(theme.muted).add_modifier(Modifier::BOLD),
          )));
          line_item_indices.push(None);
        }
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
          .title("─Unit filters"),
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
/// absolute form and, for local units, a relative one ("2d 4h ago" or "in 2d 4h").
/// Remote timestamps retain their source timezone and omit the relative time because
/// comparing naive wall-clock values from different timezones would be misleading.
fn format_systemd_timestamp(timestamp: &str, is_remote: bool) -> Option<(String, Option<String>)> {
  let mut parts = timestamp.split_whitespace();
  let _weekday = parts.next()?;
  let date = parts.next()?;
  let time = parts.next()?;
  let timezone = parts.next();
  let naive = chrono::NaiveDateTime::parse_from_str(&format!("{date} {time}"), "%Y-%m-%d %H:%M:%S").ok()?;
  let mut absolute = naive.format("%Y-%m-%d %H:%M").to_string();
  if is_remote {
    if let Some(timezone) = timezone {
      absolute.push(' ');
      absolute.push_str(timezone);
    }
    return Some((absolute, None));
  }
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

/// Greedy word-wrap to a given column width. Returns one string per visual line. Words longer than
/// the width are placed on their own line and allowed to overflow (rare for prose). Width is
/// measured in chars, which matches display width for the plain English these descriptions use.
fn word_wrap(text: &str, width: usize) -> Vec<String> {
  let width = width.max(1);
  let mut lines: Vec<String> = Vec::new();
  let mut current = String::new();
  for word in text.split_whitespace() {
    if current.is_empty() {
      current.push_str(word);
    } else if current.chars().count() + 1 + word.chars().count() <= width {
      current.push(' ');
      current.push_str(word);
    } else {
      lines.push(std::mem::take(&mut current));
      current.push_str(word);
    }
  }
  if !current.is_empty() {
    lines.push(current);
  }
  lines
}

fn format_cpu_nsec(nsec: u64) -> String {
  if nsec < 1_000_000_000 {
    format!("{}ms", nsec / 1_000_000)
  } else {
    format_duration(nsec / 1_000_000_000)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn test_unit(name: &str) -> UnitWithStatus {
    UnitWithStatus {
      name: name.to_string(),
      scope: UnitScope::Global,
      description: String::new(),
      file_path: None,
      load_state: "loaded".to_string(),
      activation_state: "active".to_string(),
      sub_state: "running".to_string(),
      enablement_state: None,
    }
  }

  #[test]
  fn active_timer_actions_are_state_aware() {
    let mut home = Home::new(Scope::All, &[], LogOrder::NewestFirst);
    let mut timer = test_unit("backup.timer");
    timer.sub_state = "waiting".into();
    timer.enablement_state = Some("enabled".into());
    home.filtered_units = StatefulList::with_items(vec![MatchedUnit { unit: timer, match_indices: vec![] }]);
    home.filtered_units.state.select(Some(0));
    home.runtime_info = Some(UnitRuntimeInfo { triggered_unit: Some("backup.service".into()), ..Default::default() });

    home.dispatch(Action::EnterMode(Mode::ActionMenu));

    let names = home.menu_items.items.iter().map(|item| item.name.as_str()).collect_vec();
    assert_eq!(names, ["Stop timer", "Disable timer", "Start backup.service now", "Open logs in pager"]);
  }

  #[test]
  fn inactive_timer_can_be_armed_and_enabled() {
    let mut home = Home::new(Scope::All, &[], LogOrder::NewestFirst);
    let mut timer = test_unit("backup.timer");
    timer.activation_state = "inactive".into();
    timer.sub_state = "dead".into();
    timer.enablement_state = Some("disabled".into());
    home.filtered_units = StatefulList::with_items(vec![MatchedUnit { unit: timer, match_indices: vec![] }]);
    home.filtered_units.state.select(Some(0));

    home.dispatch(Action::EnterMode(Mode::ActionMenu));

    let names = home.menu_items.items.iter().map(|item| item.name.as_str()).collect_vec();
    assert_eq!(names, ["Start timer", "Enable timer", "Open logs in pager"]);
  }

  #[test]
  fn runtime_enabled_timer_uses_runtime_disable() {
    let mut home = Home::new(Scope::All, &[], LogOrder::NewestFirst);
    let mut timer = test_unit("backup.timer");
    timer.enablement_state = Some("enabled-runtime".into());
    let timer_id = timer.id();
    home.filtered_units = StatefulList::with_items(vec![MatchedUnit { unit: timer, match_indices: vec![] }]);
    home.filtered_units.state.select(Some(0));

    home.dispatch(Action::EnterMode(Mode::ActionMenu));

    let disable = home.menu_items.items.iter().find(|item| item.name == "Disable timer").unwrap();
    assert!(matches!(
      &disable.action,
      Action::DisableService { unit, runtime: true } if unit == &timer_id
    ));
  }

  #[test]
  fn refreshing_unchanged_units_preserves_list_offset() {
    let mut home = Home::new(Scope::All, &[], LogOrder::NewestFirst);
    let units: Vec<_> = (0..10).map(|i| test_unit(&format!("unit-{i}.service"))).collect();

    for unit in &units {
      home.all_units.insert(unit.id(), unit.clone());
    }
    home.filtered_units.items = units.into_iter().map(|unit| MatchedUnit { unit, match_indices: vec![] }).collect();
    home.filtered_units.state.select(Some(6));
    *home.filtered_units.state.offset_mut() = 3;

    home.refresh_filtered_units();

    assert_eq!(home.filtered_units.state.offset(), 3);
  }

  #[test]
  fn refreshing_changed_units_resets_list_offset() {
    let mut home = Home::new(Scope::All, &[], LogOrder::NewestFirst);
    let units: Vec<_> = (0..10).map(|i| test_unit(&format!("unit-{i}.service"))).collect();

    for unit in &units {
      home.all_units.insert(unit.id(), unit.clone());
    }
    home.filtered_units.items = units.into_iter().map(|unit| MatchedUnit { unit, match_indices: vec![] }).collect();
    home.filtered_units.state.select(Some(6));
    *home.filtered_units.state.offset_mut() = 3;
    home.all_units.shift_remove(&test_unit("unit-0.service").id());

    home.refresh_filtered_units();

    assert_eq!(home.filtered_units.state.offset(), 0);
  }

  #[test]
  fn log_lines_are_colored_by_priority() {
    use ratatui::{backend::TestBackend, Terminal};

    let mut home = Home::new(Scope::All, &[], LogOrder::NewestFirst);
    let unit = test_unit("cron.service");
    home.all_units.insert(unit.id(), unit.clone());
    home.filtered_units.items = vec![MatchedUnit { unit, match_indices: vec![] }];
    home.filtered_units.state.select(Some(0));
    home.logs = vec![
      LogEntry { timestamp: None, content: "an error".into(), priority: Some(3) },
      LogEntry { timestamp: None, content: "a warning".into(), priority: Some(4) },
      LogEntry { timestamp: None, content: "plain info".into(), priority: Some(6) },
    ];

    let backend = TestBackend::new(120, 40);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| home.render(f, f.area())).unwrap();

    let buffer = terminal.backend().buffer();
    let style_of = |needle: &str| {
      let area = *buffer.area();
      for y in 0..area.height {
        let row: String = (0..area.width).map(|x| buffer[(x, y)].symbol()).collect();
        if let Some(col) = row.find(needle) {
          return buffer[(col as u16, y)].style();
        }
      }
      panic!("{needle:?} not found in rendered buffer");
    };

    assert_eq!(style_of("an error").fg, Some(Color::Red));
    assert_eq!(style_of("a warning").fg, Some(Color::Yellow));
    assert_eq!(style_of("plain info").fg, Some(Color::Reset));
  }

  #[test]
  fn log_order_controls_pager_order() {
    let mut home = Home::new(Scope::All, &[], LogOrder::NewestFirst);
    home.logs = vec![LogEntry::plain("oldest"), LogEntry::plain("newest")];

    assert_eq!(home.logs_for_pager(), ["newest", "oldest"]);
    home.dispatch(Action::ToggleLogOrder);
    assert_eq!(home.logs_for_pager(), ["oldest", "newest"]);
  }

  #[test]
  fn scrolling_stops_oldest_first_log_following() {
    let mut home = Home::new(Scope::All, &[], LogOrder::OldestFirst);
    home.logs_scroll_offset = 20;
    home.logs_max_scroll_offset = 20;

    home.dispatch(Action::ScrollUp(5));
    assert_eq!(home.logs_scroll_offset, 15);
    assert!(!home.follow_logs);

    home.dispatch(Action::ScrollToBottom);
    assert_eq!(home.logs_scroll_offset, u16::MAX);
    assert!(home.follow_logs);
  }

  #[test]
  fn scrolling_to_bottom_resumes_oldest_first_log_following() {
    let mut home = Home::new(Scope::All, &[], LogOrder::OldestFirst);
    home.logs_scroll_offset = 15;
    home.logs_max_scroll_offset = 20;
    home.follow_logs = false;

    home.dispatch(Action::ScrollDown(5));

    assert_eq!(home.logs_scroll_offset, 20);
    assert!(home.follow_logs);
  }

  #[test]
  fn control_r_reverses_logs_while_search_has_focus() {
    let mut home = Home::new(Scope::All, &[], LogOrder::NewestFirst);
    home.mode = Mode::Search;

    let actions = home.handle_key_events(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL));

    assert!(matches!(actions.as_slice(), [Action::ToggleLogOrder, Action::Render]));
  }

  #[test]
  fn f2_filter_returns_focus_to_search() {
    let mut home = Home::new(Scope::All, &[], LogOrder::NewestFirst);
    home.mode = Mode::Search;

    let actions = home.handle_key_events(KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE));
    let [open] = actions.as_slice() else {
      panic!("F2 should open the filter");
    };
    home.dispatch(open.clone());
    assert_eq!(home.mode, Mode::StatusFilter);

    let actions = home.handle_key_events(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    let [close] = actions.as_slice() else {
      panic!("Escape should close the filter");
    };
    assert!(matches!(close, Action::EnterMode(Mode::Search)));
  }

  #[test]
  fn f2_does_not_interrupt_processing() {
    let mut home = Home::new(Scope::All, &[], LogOrder::NewestFirst);
    home.mode = Mode::Processing;

    assert!(home.handle_key_events(KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE)).is_empty());
  }

  #[test]
  fn search_can_match_the_timer_suffix() {
    let mut home = Home::new(Scope::All, &[], LogOrder::NewestFirst);
    let (journalctl_tx, _journalctl_rx) = std::sync::mpsc::channel();
    home.journalctl_tx = Some(journalctl_tx);
    home.all_units.insert(test_unit("backup.service").id(), test_unit("backup.service"));
    home.all_units.insert(test_unit("backup.timer").id(), test_unit("backup.timer"));
    home.input = Input::default().with_value("backup.timer".to_string());

    home.refresh_filtered_units();

    assert_eq!(home.filtered_units.items.len(), 1);
    assert_eq!(home.filtered_units.items[0].unit.name, "backup.timer");
  }

  #[test]
  fn search_does_not_match_an_invisible_suffix() {
    let mut home = Home::new(Scope::All, &[], LogOrder::NewestFirst);
    home.all_units.insert(test_unit("backup.service").id(), test_unit("backup.service"));
    home.all_units.insert(test_unit("backup.timer").id(), test_unit("backup.timer"));
    home.input = Input::default().with_value("timer".to_string());

    home.refresh_filtered_units();

    assert!(home.filtered_units.items.is_empty());
  }

  #[test]
  fn late_timer_metadata_rebuilds_the_open_action_menu() {
    let mut home = Home::new(Scope::All, &[], LogOrder::NewestFirst);
    let mut timer = test_unit("backup.timer");
    timer.enablement_state = Some("enabled".into());
    let timer_id = timer.id();
    home.filtered_units = StatefulList::with_items(vec![MatchedUnit { unit: timer, match_indices: vec![] }]);
    home.filtered_units.state.select(Some(0));
    home.dispatch(Action::EnterMode(Mode::ActionMenu));
    assert!(!home.menu_items.items.iter().any(|item| item.name.contains("backup.service")));
    let disable_index = home.menu_items.items.iter().position(|item| item.name == "Disable timer").unwrap();
    home.menu_items.state.select(Some(disable_index));

    let follow_up = home.dispatch(Action::SetUnitRuntimeInfo {
      unit: timer_id,
      info: Box::new(UnitRuntimeInfo { triggered_unit: Some("backup.service".into()), ..Default::default() }),
    });
    home.dispatch(follow_up.expect("the action menu should be rebuilt"));

    assert!(home.menu_items.items.iter().any(|item| item.name == "Start backup.service now"));
    assert_eq!(home.menu_items.selected().map(|item| item.name.as_str()), Some("Disable timer"));
  }

  #[test]
  fn remote_timestamps_keep_their_timezone_without_a_relative_guess() {
    let formatted = format_systemd_timestamp("Wed 2026-07-08 10:00:00 PDT", true);

    assert_eq!(formatted, Some(("2026-07-08 10:00 PDT".into(), None)));
  }

  #[test]
  fn timer_next_trigger_uses_an_absolute_timestamp() {
    use ratatui::{backend::TestBackend, Terminal};

    let mut home = Home::new(Scope::All, &[], LogOrder::NewestFirst);
    let timer = test_unit("backup.timer");
    home.filtered_units = StatefulList::with_items(vec![MatchedUnit { unit: timer, match_indices: vec![] }]);
    home.filtered_units.state.select(Some(0));
    let next = chrono::Local::now() + chrono::Duration::hours(2);
    let expected_absolute = next.format("%Y-%m-%d %H:%M").to_string();
    home.runtime_info = Some(UnitRuntimeInfo {
      next_elapse: Some(next.format("%a %Y-%m-%d %H:%M:%S %Z").to_string()),
      ..Default::default()
    });

    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    terminal.draw(|frame| home.render(frame, frame.area())).unwrap();
    let rendered = format!("{}", terminal.backend());

    assert!(rendered.contains("Next trigger"));
    assert!(rendered.contains(&expected_absolute));
  }

  mod snapshots {
    //! Snapshot tests of `Home`'s rendering, using `insta` + ratatui's `TestBackend`.
    //!
    //! These build a `Home` directly (no live systemd/D-Bus/async runtime) and feed it
    //! fake unit data, so everything here is deterministic. `spinner_tick` is pinned to 0
    //! and `toast` is left `None` to avoid time-dependence.

    use ratatui::{backend::TestBackend, Terminal};

    use super::*;

    fn unit(name: &str, activation_state: &str, sub_state: &str, load_state: &str) -> UnitWithStatus {
      UnitWithStatus {
        name: name.to_string(),
        scope: UnitScope::Global,
        description: format!("{name} description"),
        file_path: Some(Ok(format!("/etc/systemd/system/{name}"))),
        load_state: load_state.to_string(),
        activation_state: activation_state.to_string(),
        sub_state: sub_state.to_string(),
        enablement_state: Some("enabled".to_string()),
      }
    }

    /// A `Home` populated with a handful of units covering varied statuses, and no
    /// time-dependent fields set (so renders are deterministic across runs).
    fn fixture() -> Home {
      let mut home = Home::new(Scope::All, &[], LogOrder::NewestFirst);
      // `get_logs` (triggered by selection changes) sends unit ids down this channel to a
      // background thread that shells out to `journalctl`. Give it a receiver so the sends
      // succeed, but never drain it - we don't want to spawn a real journalctl process.
      let (journalctl_tx, _journalctl_rx) = std::sync::mpsc::channel();
      home.journalctl_tx = Some(journalctl_tx);

      let units = vec![
        unit("cron.service", "active", "running", "loaded"),
        unit("docker.service", "active", "running", "loaded"),
        unit("nginx.service", "inactive", "dead", "loaded"),
        unit("bad-config.service", "failed", "failed", "loaded"),
        unit("backup.timer", "active", "waiting", "loaded"),
        unit("ssh.service", "active", "running", "loaded"),
        unit("old-thing.service", "inactive", "dead", "not-found"),
      ];
      home.set_units(units);
      home.filtered_units.state.select(Some(0));

      // Deterministic: no spinner animation, no toast timing.
      home.spinner_tick = 0;
      home.toast = None;

      home
    }

    fn render(home: &mut Home, width: u16, height: u16) -> Terminal<TestBackend> {
      let backend = TestBackend::new(width, height);
      let mut terminal = Terminal::new(backend).unwrap();
      terminal.draw(|f| home.render(f, f.area())).unwrap();
      terminal
    }

    fn timer_fixture() -> Home {
      let mut home = fixture();
      let timer_index = home.filtered_units.items.iter().position(|item| item.unit.name == "backup.timer").unwrap();
      home.filtered_units.state.select(Some(timer_index));
      home.runtime_info = Some(UnitRuntimeInfo {
        triggered_unit: Some("backup.service".into()),
        timer_schedules: vec!["OnBootSec=10min".into(), "OnUnitActiveSec=2h".into()],
        persistent: Some(true),
        randomized_delay: Some("5min".into()),
        accuracy: Some("1min".into()),
        ..Default::default()
      });
      home
    }

    #[test]
    fn services_list_120x40() {
      let mut home = fixture();
      let terminal = render(&mut home, 120, 40);
      insta::assert_snapshot!(terminal.backend());
    }

    #[test]
    fn services_list_tiny_40x15() {
      let mut home = fixture();
      let terminal = render(&mut home, 40, 15);
      insta::assert_snapshot!(terminal.backend());
    }

    #[test]
    fn timer_details_120x40() {
      let mut home = timer_fixture();
      let terminal = render(&mut home, 120, 40);
      insta::assert_snapshot!(terminal.backend());
    }

    #[test]
    fn timer_details_80x24() {
      let mut home = timer_fixture();
      let terminal = render(&mut home, 80, 24);
      insta::assert_snapshot!(terminal.backend());
    }

    #[test]
    fn filter_popup_tiny_40x15() {
      let mut home = fixture();
      home.dispatch(Action::EnterMode(Mode::StatusFilter));
      let terminal = render(&mut home, 40, 15);
      insta::assert_snapshot!(terminal.backend());
    }

    #[test]
    fn search_mode_with_query() {
      let mut home = fixture();
      home.mode = Mode::Search;
      home.input = Input::default().with_value("ssh".to_string());
      home.refresh_filtered_units();
      let terminal = render(&mut home, 120, 40);
      insta::assert_snapshot!(terminal.backend());
    }

    #[test]
    fn action_menu_open() {
      let mut home = fixture();
      home.dispatch(Action::EnterMode(Mode::ActionMenu));
      let terminal = render(&mut home, 120, 40);
      insta::assert_snapshot!(terminal.backend());
    }

    #[test]
    fn help_screen() {
      let mut home = fixture();
      home.mode = Mode::Help;
      let terminal = render(&mut home, 120, 40);
      insta::assert_snapshot!(terminal.backend());
    }

    #[test]
    fn error_state() {
      let mut home = fixture();
      home.error_message = "Failed to connect to systemd:\nUnit dbus not found".to_string();
      home.mode = Mode::Error;
      let terminal = render(&mut home, 120, 40);
      insta::assert_snapshot!(terminal.backend());
    }
  }
}
