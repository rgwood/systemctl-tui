use std::{
  cell::{Cell, RefCell},
  collections::HashMap,
  rc::Rc,
  sync::mpsc,
  time::Duration,
};

use anyhow::Result;
use gtk::{glib, prelude::*};
use gtk4 as gtk;
use systemctl_ui_core::{
  format,
  journal::LogEntry,
  systemd::{self, Scope, ServiceList, TimerListEntry, UnitId, UnitKind, UnitRuntimeInfo, UnitScope, UnitWithStatus},
  unit_descriptions,
};
use tokio_util::sync::CancellationToken;

mod gui_backend;

#[derive(Clone)]
struct Row {
  unit: UnitWithStatus,
}

/// One name/value row in the details grid. `link` makes the value a clickable
/// reference to another unit (a timer's service or a service's timer).
struct DetailRow {
  name: &'static str,
  value: String,
  link: Option<UnitId>,
  /// Wrapped rows span the full grid width below the columned rows and wrap up
  /// to a few lines, for prose values like About that don't fit one cell.
  wrap: bool,
}

impl DetailRow {
  fn text(name: &'static str, value: impl Into<String>) -> Self {
    Self { name, value: value.into(), link: None, wrap: false }
  }

  fn link(name: &'static str, value: impl Into<String>, link: UnitId) -> Self {
    Self { name, value: value.into(), link: Some(link), wrap: false }
  }

  fn wrapped(name: &'static str, value: impl Into<String>) -> Self {
    Self { name, value: value.into(), link: None, wrap: true }
  }
}

enum Reply {
  Units(Result<(ServiceList, Vec<(UnitScope, TimerListEntry)>), String>),
  Action(Result<(), String>),
  Details {
    unit: UnitId,
    generation: u64,
    details: Box<Result<UnitRuntimeInfo, String>>,
    logs: Result<Vec<LogEntry>, String>,
    definition: Result<String, String>,
  },
  LogLines {
    unit: UnitId,
    generation: u64,
    entries: Vec<LogEntry>,
  },
  LogFollowError {
    unit: UnitId,
    generation: u64,
    error: String,
  },
}

#[derive(Clone, Copy, PartialEq)]
enum StatusFilter {
  All,
  Active,
  Failed,
  Inactive,
}

#[derive(Clone, Copy, PartialEq)]
enum ScopeFilter {
  All,
  System,
  User,
}

fn state_label(unit: &UnitWithStatus) -> &str {
  match (unit.activation_state.as_str(), unit.sub_state.as_str()) {
    ("active", "running") => "Running",
    ("active", "waiting") => "Waiting",
    ("active", "elapsed") => "Elapsed",
    ("active", _) => "Active",
    ("inactive", "dead") => "Stopped",
    ("inactive", _) => "Inactive",
    ("failed", _) => "Failed",
    ("activating", _) => "Starting…",
    ("deactivating", _) => "Stopping…",
    ("reloading", _) => "Reloading…",
    _ => unit.activation_state.as_str(),
  }
}

/// Masked and not-found units can't be started and are hidden unless the user
/// asks for them, matching the TUI's default.
fn is_hidden_by_default(unit: &UnitWithStatus) -> bool {
  unit.is_not_found()
    || unit.load_state == "masked"
    || matches!(unit.enablement_state.as_deref(), Some("masked" | "masked-runtime"))
}

fn category_icon(unit: &UnitWithStatus) -> &'static str {
  let haystack = format!("{} {}", unit.name, unit.description).to_lowercase();
  if ["network", "wifi", "ssh", "ethernet"].iter().any(|word| haystack.contains(word)) {
    "network-wired-symbolic"
  } else if haystack.contains("bluetooth") {
    "bluetooth-symbolic"
  } else if ["disk", "mount", "storage"].iter().any(|word| haystack.contains(word)) {
    "drive-harddisk-symbolic"
  } else if ["audio", "sound", "pipewire"].iter().any(|word| haystack.contains(word)) {
    "audio-speakers-symbolic"
  } else if ["print", "cups"].iter().any(|word| haystack.contains(word)) {
    "printer-symbolic"
  } else if ["security", "auth", "polkit"].iter().any(|word| haystack.contains(word)) {
    "security-high-symbolic"
  } else if ["docker", "podman", "container"].iter().any(|word| haystack.contains(word)) {
    "package-x-generic-symbolic"
  } else if ["time", "timer", "cron"].iter().any(|word| haystack.contains(word)) {
    "preferences-system-time-symbolic"
  } else {
    "application-x-executable-symbolic"
  }
}

fn row_index(object: &glib::Object) -> Option<usize> {
  object.downcast_ref::<gtk::StringObject>()?.string().parse().ok()
}

fn selected_row_index(selection: &gtk::SingleSelection) -> Option<usize> {
  selection.selected_item().as_ref().and_then(row_index)
}

fn unit_origin(unit: &UnitWithStatus) -> &'static str {
  let path = unit.file_path.as_ref().and_then(|path| path.as_ref().ok()).map(String::as_str);
  match path {
    Some(path) if path.contains("/systemd/transient/") => "Transient",
    Some(path) if path.contains("/systemd/generator") => "Generated",
    Some(path) if path.starts_with("/run/") => "Runtime",
    Some(path) if path.starts_with("/etc/") => "Local",
    Some(path) if path.contains("/.config/") || path.contains("/systemd/user/") => "User",
    Some(path) if path.starts_with("/usr/") || path.starts_with("/lib/") => "Vendor",
    Some(_) => "File",
    None if unit.is_not_found() => "Referenced",
    None if unit.load_state == "loaded" => "Runtime",
    None => "No file",
  }
}

fn scope_label(scope: UnitScope) -> &'static str {
  match scope {
    UnitScope::Global => "system",
    UnitScope::User => "user",
  }
}

/// "in 4h 12m" / "42m 10s ago", for timer columns. Falls back to the raw string.
fn relative_timestamp(timestamp: &str) -> String {
  match format::format_systemd_timestamp(timestamp, false) {
    Some((absolute, relative)) => relative.unwrap_or(absolute),
    None => timestamp.to_string(),
  }
}

/// "2026-07-20 00:00 (in 4h 12m)", for details rows and tooltips.
fn absolute_and_relative_timestamp(timestamp: &str) -> String {
  match format::format_systemd_timestamp(timestamp, false) {
    Some((absolute, Some(relative))) => format!("{absolute} ({relative})"),
    Some((absolute, None)) => absolute,
    None => timestamp.to_string(),
  }
}

fn show_error(window: &gtk::ApplicationWindow, message: &str) {
  let dialog = gtk::MessageDialog::builder()
    .transient_for(window)
    .modal(true)
    .message_type(gtk::MessageType::Error)
    .buttons(gtk::ButtonsType::Close)
    .text("The operation failed")
    .secondary_text(message)
    .build();
  dialog.connect_response(|dialog, _| dialog.close());
  dialog.present();
}

fn demo_units() -> Vec<UnitWithStatus> {
  [
    ("accounts-daemon.service", "Accounts Service", "active", "running", "enabled"),
    ("anacron.timer", "Trigger anacron every hour", "active", "waiting", "enabled"),
    ("anacron.service", "Run anacron jobs", "inactive", "dead", "static"),
    ("bluetooth.service", "Bluetooth service", "inactive", "dead", "enabled"),
    ("cron.service", "Regular background program processing daemon", "active", "running", "enabled"),
    ("docker.service", "Docker Application Container Engine", "failed", "failed", "enabled"),
    ("logrotate.timer", "Daily rotation of log files", "active", "waiting", "enabled"),
    ("logrotate.service", "Rotate log files", "inactive", "dead", "static"),
    ("NetworkManager.service", "Network Manager", "active", "running", "enabled"),
    (
      "snapd.snap-repair.timer",
      "Timer to automatically fetch and run repair assertions",
      "inactive",
      "dead",
      "enabled",
    ),
    ("ssh.service", "OpenBSD Secure Shell server", "active", "running", "enabled"),
    ("systemd-resolved.service", "Network Name Resolution", "active", "running", "enabled"),
    ("systemd-timesyncd.service", "Network Time Synchronization", "inactive", "dead", "disabled"),
  ]
  .into_iter()
  .enumerate()
  .map(|(i, (name, description, active, sub, enabled))| UnitWithStatus {
    name: name.into(),
    scope: if i == 4 { UnitScope::User } else { UnitScope::Global },
    description: description.into(),
    file_path: Some(Ok(if name == "docker.service" {
      "/run/systemd/transient/docker.service".into()
    } else {
      format!("/usr/lib/systemd/system/{name}")
    })),
    load_state: "loaded".into(),
    activation_state: active.into(),
    sub_state: sub.into(),
    enablement_state: Some(enabled.into()),
  })
  .collect()
}

/// systemd-show-style timestamp ("Sun 2026-07-19 18:33:22 PDT") at an offset from now,
/// so demo timers get plausible relative times.
fn demo_timestamp(offset_seconds: i64) -> String {
  (chrono::Local::now() + chrono::TimeDelta::seconds(offset_seconds)).format("%a %Y-%m-%d %H:%M:%S %Z").to_string()
}

fn demo_timers() -> Vec<(UnitScope, TimerListEntry)> {
  vec![
    (
      UnitScope::Global,
      TimerListEntry {
        timer: "anacron.timer".into(),
        next_elapse: Some(demo_timestamp(14 * 60)),
        last_trigger: Some(demo_timestamp(-42 * 60)),
        activates: Some("anacron.service".into()),
      },
    ),
    (
      UnitScope::Global,
      TimerListEntry {
        timer: "logrotate.timer".into(),
        next_elapse: Some(demo_timestamp(6 * 3600)),
        last_trigger: Some(demo_timestamp(-18 * 3600)),
        activates: Some("logrotate.service".into()),
      },
    ),
    (
      UnitScope::Global,
      TimerListEntry {
        timer: "snapd.snap-repair.timer".into(),
        next_elapse: None,
        last_trigger: None,
        activates: Some("snapd.snap-repair.service".into()),
      },
    ),
  ]
}

fn main() -> glib::ExitCode {
  let demo = std::env::args().any(|arg| arg == "--demo");
  let app = gtk::Application::builder().application_id("com.github.rgwood.SystemctlGui").build();
  app.add_main_option(
    "demo",
    b'\0'.into(),
    glib::OptionFlags::NONE,
    glib::OptionArg::None,
    "Use deterministic demo data",
    None,
  );
  app.connect_activate(move |app| build_ui(app, demo));
  app.run()
}

/// Which columns each unit list shows. Services carry service-shaped columns; timers
/// get trigger times and the unit they activate, like `systemctl list-timers`.
#[derive(Clone, Copy, PartialEq)]
enum ColumnId {
  Name,
  Description,
  State,
  Startup,
  Origin,
  Scope,
  NextTrigger,
  LastTrigger,
  Activates,
}

struct UnitListView {
  view: gtk::ColumnView,
  selection: gtk::SingleSelection,
  store: gtk::StringList,
}

/// Opens the right-click menu for a clicked cell: (cell, list position, x, y).
/// Populated late because the menu builder needs widgets created after the lists.
type ContextMenuOpener = Rc<RefCell<Option<Box<dyn Fn(&gtk::Widget, u32, f64, f64)>>>>;

/// Build one ColumnView over indices into `rows`. `timer_meta` feeds the timer columns.
fn build_unit_list(
  columns: &[(&'static str, ColumnId, bool)],
  rows: Rc<RefCell<Vec<Row>>>,
  timer_meta: Rc<RefCell<HashMap<UnitId, TimerListEntry>>>,
  open_context_menu: ContextMenuOpener,
) -> UnitListView {
  let view = gtk::ColumnView::new(None::<gtk::SingleSelection>);
  view.set_show_column_separators(true);
  view.set_show_row_separators(true);
  view.set_hexpand(true);
  view.set_vexpand(true);

  for &(title, column_id, expand) in columns {
    let factory = gtk::SignalListItemFactory::new();
    let setup_menu = open_context_menu.clone();
    factory.connect_setup(move |_, item| {
      let list_item = item.downcast_ref::<gtk::ListItem>().unwrap().clone();
      let label = gtk::Label::new(None);
      label.set_xalign(0.0);
      label.set_ellipsize(gtk::pango::EllipsizeMode::End);
      label.add_css_class("cell-label");
      if column_id == ColumnId::Name {
        let cell = gtk::Box::new(gtk::Orientation::Horizontal, 3);
        let dot = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        dot.add_css_class("status-dot");
        let icon = gtk::Image::new();
        icon.set_pixel_size(13);
        icon.add_css_class("dim-label");
        cell.append(&dot);
        cell.append(&icon);
        cell.append(&label);
        list_item.set_child(Some(&cell));
      } else {
        list_item.set_child(Some(&label));
      }
      let cell = list_item.child().unwrap();
      let click = gtk::GestureClick::new();
      click.set_button(3);
      click.connect_pressed({
        let setup_menu = setup_menu.clone();
        let context_cell = cell.clone();
        move |gesture, _, x, y| {
          let position = list_item.position();
          if position == gtk::INVALID_LIST_POSITION {
            return;
          }
          if let Some(open) = setup_menu.borrow().as_ref() {
            open(&context_cell, position, x, y);
          }
          gesture.set_state(gtk::EventSequenceState::Claimed);
        }
      });
      cell.add_controller(click);
    });

    let bind_rows = rows.clone();
    let bind_meta = timer_meta.clone();
    factory.connect_bind(move |_, item| {
      let item = item.downcast_ref::<gtk::ListItem>().unwrap();
      let Some(row_index) = item.item().as_ref().and_then(row_index) else { return };
      let rows = bind_rows.borrow();
      let unit = &rows[row_index].unit;
      let meta = bind_meta.borrow().get(&unit.id()).cloned();
      let mut tooltip: Option<String> = None;
      let text = match column_id {
        ColumnId::Name => unit.short_name().to_string(),
        ColumnId::Description => {
          tooltip = unit_descriptions::explain(&unit.name, unit.scope).map(String::from);
          unit.description.clone()
        },
        ColumnId::State => state_label(unit).to_string(),
        ColumnId::Startup => unit.enablement_state.clone().unwrap_or_else(|| "—".into()),
        ColumnId::Origin => unit_origin(unit).to_string(),
        ColumnId::Scope => scope_label(unit.scope).to_string(),
        ColumnId::NextTrigger => match meta.as_ref().and_then(|meta| meta.next_elapse.as_deref()) {
          Some(timestamp) => {
            tooltip = Some(absolute_and_relative_timestamp(timestamp));
            relative_timestamp(timestamp)
          },
          None => "—".into(),
        },
        ColumnId::LastTrigger => match meta.as_ref().and_then(|meta| meta.last_trigger.as_deref()) {
          Some(timestamp) => {
            tooltip = Some(absolute_and_relative_timestamp(timestamp));
            relative_timestamp(timestamp)
          },
          None => "—".into(),
        },
        ColumnId::Activates => meta.and_then(|meta| meta.activates).unwrap_or_else(|| "—".into()),
      };
      if column_id == ColumnId::Name {
        let cell = item.child().and_downcast::<gtk::Box>().unwrap();
        let dot = cell.first_child().and_downcast::<gtk::Box>().unwrap();
        for class in ["active", "failed", "transition", "inactive"] {
          dot.remove_css_class(class);
        }
        dot.add_css_class(if unit.is_failed() {
          "failed"
        } else if unit.activation_state == "active" {
          "active"
        } else if matches!(unit.activation_state.as_str(), "activating" | "deactivating" | "reloading")
          || unit.is_not_found()
        {
          "transition"
        } else {
          "inactive"
        });
        let icon = dot.next_sibling().and_downcast::<gtk::Image>().unwrap();
        icon.set_icon_name(Some(category_icon(unit)));
        let label = icon.next_sibling().and_downcast::<gtk::Label>().unwrap();
        label.set_text(&text);
        cell.set_tooltip_text(Some(&format!("{} — {} ({})", unit.name, unit.activation_state, unit.sub_state)));
      } else {
        let label = item.child().and_downcast::<gtk::Label>().unwrap();
        label.set_text(&text);
        label.set_tooltip_text(tooltip.as_deref());
      }
    });

    let column = gtk::ColumnViewColumn::new(Some(title), Some(factory));
    let sorter_rows = rows.clone();
    let sorter_meta = timer_meta.clone();
    column.set_sorter(Some(&gtk::CustomSorter::new(move |left, right| {
      let Some(left_index) = row_index(left) else { return gtk::Ordering::Equal };
      let Some(right_index) = row_index(right) else { return gtk::Ordering::Equal };
      let rows = sorter_rows.borrow();
      let Some(left) = rows.get(left_index).map(|row| &row.unit) else { return gtk::Ordering::Equal };
      let Some(right) = rows.get(right_index).map(|row| &row.unit) else { return gtk::Ordering::Equal };
      // Absolute "%Y-%m-%d %H:%M" timestamps sort chronologically as strings; timers
      // with nothing scheduled sort last.
      let timestamp_key = |unit: &UnitWithStatus, pick: fn(&TimerListEntry) -> Option<&String>| {
        sorter_meta
          .borrow()
          .get(&unit.id())
          .and_then(|meta| pick(meta).map(|t| format::format_systemd_timestamp(t, false).map_or(t.clone(), |(a, _)| a)))
          .unwrap_or_else(|| "~".into())
      };
      let ordering = match column_id {
        ColumnId::Name => left.short_name().to_lowercase().cmp(&right.short_name().to_lowercase()),
        ColumnId::Description => left.description.to_lowercase().cmp(&right.description.to_lowercase()),
        ColumnId::State => state_label(left).to_lowercase().cmp(&state_label(right).to_lowercase()),
        ColumnId::Startup => {
          left.enablement_state.as_deref().unwrap_or("").cmp(right.enablement_state.as_deref().unwrap_or(""))
        },
        ColumnId::Origin => unit_origin(left).cmp(unit_origin(right)),
        ColumnId::Scope => scope_label(left.scope).cmp(scope_label(right.scope)),
        ColumnId::NextTrigger => timestamp_key(left, |meta| meta.next_elapse.as_ref())
          .cmp(&timestamp_key(right, |meta| meta.next_elapse.as_ref())),
        ColumnId::LastTrigger => timestamp_key(left, |meta| meta.last_trigger.as_ref())
          .cmp(&timestamp_key(right, |meta| meta.last_trigger.as_ref())),
        ColumnId::Activates => {
          let key = |unit: &UnitWithStatus| {
            sorter_meta.borrow().get(&unit.id()).and_then(|meta| meta.activates.clone()).unwrap_or_default()
          };
          key(left).cmp(&key(right))
        },
      };
      ordering.then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase())).into()
    })));
    column.set_resizable(true);
    column.set_expand(expand);
    if column_id == ColumnId::Name {
      column.set_fixed_width(230);
    }
    view.append_column(&column);
  }

  let store = gtk::StringList::new(&[]);
  let sorted = gtk::SortListModel::new(Some(store.clone()), view.sorter());
  let selection = gtk::SingleSelection::new(Some(sorted));
  selection.set_autoselect(true);
  view.set_model(Some(&selection));
  UnitListView { view, selection, store }
}

fn build_ui(app: &gtk::Application, demo: bool) {
  let rows = Rc::new(RefCell::new(Vec::<Row>::new()));
  let timer_meta = Rc::new(RefCell::new(HashMap::<UnitId, TimerListEntry>::new()));
  // service -> the timer that activates it, recomputed on every refresh
  let activated_by = Rc::new(RefCell::new(HashMap::<UnitId, UnitId>::new()));
  let context_actions = gtk::gio::SimpleActionGroup::new();
  let open_context_menu = Rc::new(RefCell::new(None::<Box<dyn Fn(&gtk::Widget, u32, f64, f64)>>));

  let services = Rc::new(build_unit_list(
    &[
      ("Service", ColumnId::Name, false),
      ("Description", ColumnId::Description, true),
      ("State", ColumnId::State, false),
      ("Startup", ColumnId::Startup, false),
      ("Origin", ColumnId::Origin, false),
      ("Scope", ColumnId::Scope, false),
    ],
    rows.clone(),
    timer_meta.clone(),
    open_context_menu.clone(),
  ));
  let timers = Rc::new(build_unit_list(
    &[
      ("Timer", ColumnId::Name, false),
      ("State", ColumnId::State, false),
      ("Next trigger", ColumnId::NextTrigger, false),
      ("Last trigger", ColumnId::LastTrigger, false),
      ("Activates", ColumnId::Activates, true),
      ("Startup", ColumnId::Startup, false),
      ("Scope", ColumnId::Scope, false),
    ],
    rows.clone(),
    timer_meta.clone(),
    open_context_menu.clone(),
  ));
  // Soonest-firing timers first, like `systemctl list-timers`
  if let Some(next_trigger_column) = timers.view.columns().item(2).and_downcast::<gtk::ColumnViewColumn>() {
    timers.view.sort_by_column(Some(&next_trigger_column), gtk::SortType::Ascending);
  }

  let stack = gtk::Stack::new();
  let services_scroller = gtk::ScrolledWindow::builder().child(&services.view).vexpand(true).hexpand(true).build();
  let timers_scroller = gtk::ScrolledWindow::builder().child(&timers.view).vexpand(true).hexpand(true).build();
  stack.add_titled(&services_scroller, Some("services"), "Services");
  stack.add_titled(&timers_scroller, Some("timers"), "Timers");
  let switcher = gtk::StackSwitcher::new();
  switcher.set_stack(Some(&stack));

  let on_timers_page = {
    let stack = stack.clone();
    move || stack.visible_child_name().as_deref() == Some("timers")
  };
  let current_selection = {
    let services = services.clone();
    let timers = timers.clone();
    let on_timers_page = on_timers_page.clone();
    move || if on_timers_page() { timers.selection.clone() } else { services.selection.clone() }
  };
  let selected_unit = {
    let rows = rows.clone();
    let current_selection = current_selection.clone();
    move || -> Option<UnitWithStatus> {
      selected_row_index(&current_selection()).and_then(|index| rows.borrow().get(index).map(|row| row.unit.clone()))
    }
  };

  let all_filter = gtk::ToggleButton::with_label("All");
  let active_filter = gtk::ToggleButton::with_label("Active");
  let failed_filter = gtk::ToggleButton::with_label("Failed");
  let inactive_filter = gtk::ToggleButton::with_label("Inactive");
  all_filter.set_active(true);
  let filter = Rc::new(Cell::new(StatusFilter::All));
  let scope_filter = Rc::new(Cell::new(ScopeFilter::All));
  let scope = gtk::DropDown::from_strings(&["All scopes", "System", "User"]);
  scope.set_tooltip_text(Some("Filter by service scope"));
  let show_hidden = gtk::ToggleButton::builder()
    .icon_name("view-reveal-symbolic")
    .tooltip_text("Show masked and not-found units")
    .build();
  let search = gtk::SearchEntry::builder().placeholder_text("Filter units").hexpand(true).build();
  let start = gtk::Button::builder().icon_name("media-playback-start-symbolic").tooltip_text("Start service").build();
  let stop = gtk::Button::builder().icon_name("media-playback-stop-symbolic").tooltip_text("Stop service").build();
  let restart = gtk::Button::builder().icon_name("view-refresh-symbolic").tooltip_text("Restart service").build();
  let run_now = gtk::Button::builder()
    .icon_name("media-skip-forward-symbolic")
    .tooltip_text("Run the timer's service now")
    .visible(false)
    .build();
  let enable = gtk::Button::builder().icon_name("emblem-default-symbolic").tooltip_text("Enable at startup").build();
  let disable =
    gtk::Button::builder().icon_name("action-unavailable-symbolic").tooltip_text("Disable at startup").build();
  let status = gtk::Label::new(Some(if demo { "Demo data" } else { "Loading…" }));
  status.set_xalign(0.0);

  let toolbar = gtk::Box::new(gtk::Orientation::Horizontal, 4);
  toolbar.add_css_class("toolbar");
  toolbar.append(&switcher);
  toolbar.append(&all_filter);
  toolbar.append(&active_filter);
  toolbar.append(&failed_filter);
  toolbar.append(&inactive_filter);
  toolbar.append(&show_hidden);
  toolbar.append(&scope);
  toolbar.append(&search);
  toolbar.append(&start);
  toolbar.append(&stop);
  toolbar.append(&restart);
  toolbar.append(&run_now);
  toolbar.append(&enable);
  toolbar.append(&disable);

  let details_grid = gtk::Grid::builder().column_spacing(12).row_spacing(3).build();
  // Prose (the About description) reads as a footnote under the grid, not a grid row
  let details_about = gtk::Label::builder()
    .xalign(0.0)
    .wrap(true)
    .wrap_mode(gtk::pango::WrapMode::WordChar)
    .lines(3)
    .ellipsize(gtk::pango::EllipsizeMode::End)
    .selectable(true)
    .visible(false)
    .css_classes(["dim-label"])
    .build();
  let details_box = gtk::Box::builder()
    .orientation(gtk::Orientation::Vertical)
    .spacing(6)
    .margin_top(6)
    .margin_bottom(6)
    .margin_start(8)
    .margin_end(8)
    .build();
  details_box.append(&details_grid);
  details_box.append(&details_about);

  let logs_view = gtk::TextView::builder()
    .editable(false)
    .cursor_visible(false)
    .monospace(true)
    .wrap_mode(gtk::WrapMode::None)
    .build();
  logs_view.add_css_class("logs");
  let log_buffer = logs_view.buffer();
  log_buffer.create_tag(Some("dim"), &[("foreground", &"#87878c")]);
  log_buffer.create_tag(Some("err"), &[("foreground", &"#c01c28")]);
  log_buffer.create_tag(Some("warn"), &[("foreground", &"#b57500")]);
  let append_log_entry = {
    let buffer = log_buffer.clone();
    move |entry: &LogEntry| {
      let mut end = buffer.end_iter();
      if buffer.char_count() > 0 {
        buffer.insert(&mut end, "\n");
      }
      if let Some(timestamp) = &entry.timestamp {
        buffer.insert_with_tags_by_name(&mut end, &format!("{timestamp} "), &["dim"]);
      }
      // journalctl-style priority colors: err and worse red, warnings yellow, debug dim
      let tags: &[&str] = match entry.priority {
        Some(0..=3) => &["err"],
        Some(4) => &["warn"],
        Some(7) => &["dim"],
        _ => &[],
      };
      if tags.is_empty() {
        buffer.insert(&mut end, &entry.content);
      } else {
        buffer.insert_with_tags_by_name(&mut end, &entry.content, tags);
      }
    }
  };

  let logs_scroller = gtk::ScrolledWindow::builder().child(&logs_view).vexpand(true).hexpand(true).build();
  let logs_page = gtk::Box::new(gtk::Orientation::Vertical, 0);
  logs_page.append(&logs_scroller);
  let unit_file_view = gtk::TextView::builder()
    .editable(false)
    .cursor_visible(false)
    .monospace(true)
    .wrap_mode(gtk::WrapMode::None)
    .build();
  unit_file_view.add_css_class("unit-file");
  let unit_file_scroller = gtk::ScrolledWindow::builder().child(&unit_file_view).vexpand(true).hexpand(true).build();
  let details_scroller = gtk::ScrolledWindow::builder().child(&details_box).vexpand(true).hexpand(true).build();
  let inspector = gtk::Notebook::new();
  inspector.append_page(&details_scroller, Some(&gtk::Label::new(Some("Details"))));
  inspector.append_page(&logs_page, Some(&gtk::Label::new(Some("Logs (live)"))));
  inspector.append_page(&unit_file_scroller, Some(&gtk::Label::new(Some("Unit File"))));
  inspector.set_size_request(-1, 190);

  let workspace = gtk::Paned::new(gtk::Orientation::Vertical);
  workspace.set_start_child(Some(&stack));
  workspace.set_end_child(Some(&inspector));
  workspace.set_resize_start_child(true);
  workspace.set_shrink_end_child(false);
  workspace.set_position(410);
  workspace.set_vexpand(true);

  let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
  content.append(&toolbar);
  content.append(&workspace);
  let status_bar = gtk::Box::new(gtk::Orientation::Horizontal, 0);
  status_bar.add_css_class("status-bar");
  let selected_summary = gtk::Label::new(Some("No unit selected"));
  selected_summary.set_xalign(0.0);
  selected_summary.set_ellipsize(gtk::pango::EllipsizeMode::End);
  selected_summary.set_hexpand(true);
  status.set_xalign(1.0);
  status_bar.append(&selected_summary);
  status_bar.append(&status);
  content.append(&status_bar);

  let window = gtk::ApplicationWindow::builder()
    .application(app)
    .title("systemctl-gui")
    .default_width(1100)
    .default_height(650)
    .child(&content)
    .build();

  let provider = gtk::CssProvider::new();
  provider.load_from_data(
    ".toolbar { padding: 3px; } .status-bar { padding: 2px 5px; border-top: 1px solid alpha(currentColor, .15); } columnview.view > listview > row { min-height: 18px; padding: 0; } columnview.view > listview > row > cell { padding: 0; } .cell-label { padding: 0 3px; font-size: 12px; } .logs, .unit-file { font-size: 12px; } .status-dot { min-width: 6px; min-height: 6px; border-radius: 999px; margin-left: 3px; } .status-dot.active { background: #2ec27e; } .status-dot.failed { background: #e01b24; } .status-dot.transition { background: #f6d32d; } .status-dot.inactive { background: alpha(currentColor, .32); } .toolbar stackswitcher button { padding: 0 8px; }",
  );
  gtk::style_context_add_provider_for_display(
    &gtk::prelude::WidgetExt::display(&window),
    &provider,
    gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
  );

  // Select a unit in whichever list it belongs to, switching pages if needed.
  let select_unit = Rc::new({
    let rows = rows.clone();
    let services = services.clone();
    let timers = timers.clone();
    let stack = stack.clone();
    move |unit: &UnitId| {
      let is_timer = unit.name.ends_with(".timer");
      let target = if is_timer { &timers.selection } else { &services.selection };
      let position = (0..target.n_items()).find(|&position| {
        target
          .item(position)
          .as_ref()
          .and_then(row_index)
          .and_then(|index| rows.borrow().get(index).map(|row| row.unit.id() == *unit))
          .unwrap_or(false)
      });
      if let Some(position) = position {
        stack.set_visible_child_name(if is_timer { "timers" } else { "services" });
        target.set_selected(position);
      }
    }
  });

  let set_details = Rc::new({
    let details_grid = details_grid.clone();
    let details_about = details_about.clone();
    let select_unit = select_unit.clone();
    move |detail_rows: Vec<DetailRow>| {
      while let Some(child) = details_grid.first_child() {
        details_grid.remove(&child);
      }
      const ROWS_PER_COLUMN: usize = 5;
      let (wrapped, columned): (Vec<DetailRow>, Vec<DetailRow>) = detail_rows.into_iter().partition(|row| row.wrap);
      for (index, detail) in columned.into_iter().enumerate() {
        let name_label = gtk::Label::new(Some(detail.name));
        name_label.set_xalign(1.0);
        name_label.add_css_class("dim-label");
        let value = gtk::Label::new(None);
        value.set_xalign(0.0);
        value.set_selectable(true);
        value.set_hexpand(true);
        value.set_tooltip_text(Some(&detail.value));
        value.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        match detail.link {
          Some(link) => {
            value.set_markup(&format!("<a href=\"#\">{}</a>", glib::markup_escape_text(&detail.value)));
            let select_unit = select_unit.clone();
            value.connect_activate_link(move |_, _| {
              select_unit(&link);
              glib::Propagation::Stop
            });
          },
          None => value.set_text(&detail.value),
        }
        let column = (index / ROWS_PER_COLUMN) * 2;
        let row = index % ROWS_PER_COLUMN;
        details_grid.attach(&name_label, column as i32, row as i32, 1, 1);
        details_grid.attach(&value, (column + 1) as i32, row as i32, 1, 1);
      }
      let about = wrapped.into_iter().map(|detail| detail.value).collect::<Vec<_>>().join("\n");
      details_about.set_visible(!about.is_empty());
      details_about.set_tooltip_text(Some(&about));
      details_about.set_text(&about);
    }
  });

  let (tx, rx) = mpsc::channel::<Reply>();
  let rebuild = {
    let rows = rows.clone();
    let services = services.clone();
    let timers = timers.clone();
    let search = search.clone();
    let filter = filter.clone();
    let scope_filter = scope_filter.clone();
    let show_hidden = show_hidden.clone();
    let all_filter = all_filter.clone();
    let active_filter = active_filter.clone();
    let failed_filter = failed_filter.clone();
    let inactive_filter = inactive_filter.clone();
    let on_timers_page = on_timers_page.clone();
    Rc::new(move || {
      let needle = search.text().to_lowercase();
      let selected_filter = filter.get();
      let selected_scope = scope_filter.get();
      let include_hidden = show_hidden.is_active();
      let count_timers = on_timers_page();
      let previously_selected: Vec<Option<UnitId>> = [&services.selection, &timers.selection]
        .iter()
        .map(|selection| {
          selected_row_index(selection).and_then(|index| rows.borrow().get(index).map(|row| row.unit.id()))
        })
        .collect();

      {
        let borrowed_rows = rows.borrow();
        // The filter counts describe the visible page, not the whole inventory
        let visible = borrowed_rows
          .iter()
          .filter(|row| (row.unit.kind() == UnitKind::Timer) == count_timers)
          .filter(|row| include_hidden || !is_hidden_by_default(&row.unit));
        let (mut total, mut active, mut failed, mut inactive) = (0, 0, 0, 0);
        for row in visible {
          total += 1;
          match row.unit.activation_state.as_str() {
            "active" => active += 1,
            "inactive" => inactive += 1,
            _ => {},
          }
          if row.unit.is_failed() {
            failed += 1;
          }
        }
        all_filter.set_label(&format!("All {total}"));
        active_filter.set_label(&format!("Active {active}"));
        failed_filter.set_label(&format!("Failed {failed}"));
        inactive_filter.set_label(&format!("Inactive {inactive}"));
        let hidden = borrowed_rows
          .iter()
          .filter(|row| (row.unit.kind() == UnitKind::Timer) == count_timers)
          .filter(|row| is_hidden_by_default(&row.unit))
          .count();
        show_hidden.set_tooltip_text(Some(&format!("Show masked and not-found units ({hidden})")));
      }

      let (service_indices, timer_indices): (Vec<usize>, Vec<usize>) = {
        let borrowed_rows = rows.borrow();
        let mut service_indices = Vec::new();
        let mut timer_indices = Vec::new();
        for (index, row) in borrowed_rows.iter().enumerate() {
          let unit = &row.unit;
          let matches_text =
            unit.name.to_lowercase().contains(&needle) || unit.description.to_lowercase().contains(&needle);
          let matches_state = match selected_filter {
            StatusFilter::All => true,
            StatusFilter::Active => unit.activation_state == "active",
            StatusFilter::Failed => unit.is_failed(),
            StatusFilter::Inactive => unit.activation_state == "inactive",
          };
          let matches_scope = match selected_scope {
            ScopeFilter::All => true,
            ScopeFilter::System => unit.scope == UnitScope::Global,
            ScopeFilter::User => unit.scope == UnitScope::User,
          };
          let visible = include_hidden || !is_hidden_by_default(unit);
          if matches_text && matches_state && matches_scope && visible {
            if unit.kind() == UnitKind::Timer {
              timer_indices.push(index);
            } else {
              service_indices.push(index);
            }
          }
        }
        (service_indices, timer_indices)
      };

      for (list, indices, previous) in
        [(&services, &service_indices, &previously_selected[0]), (&timers, &timer_indices, &previously_selected[1])]
      {
        let strings: Vec<String> = indices.iter().map(|i| i.to_string()).collect();
        list.store.splice(0, list.store.n_items(), &strings.iter().map(String::as_str).collect::<Vec<_>>());
        // Keep the same unit selected across refreshes and filter changes
        if let Some(previous) = previous {
          let position = (0..list.selection.n_items()).find(|&position| {
            list
              .selection
              .item(position)
              .as_ref()
              .and_then(row_index)
              .and_then(|index| rows.borrow().get(index).map(|row| row.unit.id() == *previous))
              .unwrap_or(false)
          });
          if let Some(position) = position {
            list.selection.set_selected(position);
          }
        }
      }
    })
  };

  if demo {
    *rows.borrow_mut() = demo_units().into_iter().map(|unit| Row { unit }).collect();
    let mut meta = timer_meta.borrow_mut();
    let mut reverse = activated_by.borrow_mut();
    for (unit_scope, entry) in demo_timers() {
      let timer_id = UnitId { name: entry.timer.clone(), scope: unit_scope };
      if let Some(activates) = &entry.activates {
        reverse.insert(UnitId { name: activates.clone(), scope: unit_scope }, timer_id.clone());
      }
      meta.insert(timer_id, entry);
    }
    drop(meta);
    drop(reverse);
    rebuild();
  }
  search.connect_search_changed({
    let rebuild = rebuild.clone();
    move |_| rebuild()
  });
  search.connect_stop_search({
    let stack = stack.clone();
    move |search| {
      search.set_text("");
      stack.grab_focus();
    }
  });
  scope.connect_selected_notify({
    let scope_filter = scope_filter.clone();
    let rebuild = rebuild.clone();
    move |scope| {
      scope_filter.set(match scope.selected() {
        1 => ScopeFilter::System,
        2 => ScopeFilter::User,
        _ => ScopeFilter::All,
      });
      rebuild();
    }
  });
  show_hidden.connect_toggled({
    let rebuild = rebuild.clone();
    move |_| rebuild()
  });
  for (button, value) in [
    (&all_filter, StatusFilter::All),
    (&active_filter, StatusFilter::Active),
    (&failed_filter, StatusFilter::Failed),
    (&inactive_filter, StatusFilter::Inactive),
  ] {
    let filter = filter.clone();
    let rebuild = rebuild.clone();
    let buttons = [all_filter.clone(), active_filter.clone(), failed_filter.clone(), inactive_filter.clone()];
    button.connect_clicked(move |_| {
      filter.set(value);
      for (index, button) in buttons.iter().enumerate() {
        button.set_active(index == value as usize);
      }
      rebuild();
    });
  }

  let inventory_loading = Rc::new(Cell::new(false));
  let load = Rc::new({
    let tx = tx.clone();
    let inventory_loading = inventory_loading.clone();
    move || {
      if demo || inventory_loading.replace(true) {
        return;
      }
      let tx = tx.clone();
      std::thread::spawn(move || {
        let patterns = ["*.service".to_string(), "*.timer".to_string()];
        let result = tokio::runtime::Runtime::new()
          .unwrap()
          .block_on(async {
            tokio::try_join!(
              gui_backend::load_service_inventory(Scope::All, &patterns),
              gui_backend::load_timer_lists(Scope::All)
            )
          })
          .map_err(|e| format!("{e:#}"));
        let _ = tx.send(Reply::Units(result));
      });
    }
  });

  let focus_search_action = gtk::gio::SimpleAction::new("focus-search", None);
  focus_search_action.connect_activate({
    let search = search.clone();
    move |_, _| {
      search.grab_focus();
    }
  });
  window.add_action(&focus_search_action);
  app.set_accels_for_action("win.focus-search", &["<Primary>f"]);

  // Start/stop/restart/enable/disable a specific unit (usually the selection, but
  // "run now" targets a timer's service).
  let run_unit_action = Rc::new({
    let tx = tx.clone();
    let status = status.clone();
    move |kind: &'static str, id: UnitId| {
      if demo {
        status.set_text(&format!("Demo: {kind} {}", id.name));
        return;
      }
      status.set_text(&format!("{kind} {}…", id.name));
      let tx = tx.clone();
      std::thread::spawn(move || {
        let token = CancellationToken::new();
        let result = tokio::runtime::Runtime::new()
          .unwrap()
          .block_on(async move {
            match kind {
              "Starting" => systemd::start_service(id, token).await,
              "Stopping" => systemd::stop_service(id, token).await,
              "Restarting" => systemd::restart_service(id, token).await,
              "Enabling" => systemd::enable_service(id, token).await,
              _ => systemd::disable_service(id, false, token).await,
            }
          })
          .map_err(|e| format!("{e:#}"));
        let _ = tx.send(Reply::Action(result));
      });
    }
  });
  let run_action = Rc::new({
    let selected_unit = selected_unit.clone();
    let run_unit_action = run_unit_action.clone();
    move |kind: &'static str| {
      if let Some(unit) = selected_unit() {
        run_unit_action(kind, unit.id());
      }
    }
  });
  let run_timer_service_now = Rc::new({
    let selected_unit = selected_unit.clone();
    let timer_meta = timer_meta.clone();
    let run_unit_action = run_unit_action.clone();
    move || {
      let Some(unit) = selected_unit() else { return };
      let target = timer_meta.borrow().get(&unit.id()).and_then(|meta| meta.activates.clone());
      if let Some(target) = target {
        run_unit_action("Starting", UnitId { name: target, scope: unit.scope });
      }
    }
  });
  start.connect_clicked({
    let f = run_action.clone();
    move |_| f("Starting")
  });
  stop.connect_clicked({
    let f = run_action.clone();
    move |_| f("Stopping")
  });
  restart.connect_clicked({
    let f = run_action.clone();
    move |_| f("Restarting")
  });
  run_now.connect_clicked({
    let f = run_timer_service_now.clone();
    move |_| f()
  });
  enable.connect_clicked({
    let f = run_action.clone();
    move |_| f("Enabling")
  });
  disable.connect_clicked({
    let f = run_action.clone();
    move |_| f("Disabling")
  });

  for (name, kind) in [
    ("start-service", "Starting"),
    ("stop-service", "Stopping"),
    ("restart-service", "Restarting"),
    ("enable-service", "Enabling"),
    ("disable-service", "Disabling"),
  ] {
    let action = gtk::gio::SimpleAction::new(name, None);
    action.connect_activate({
      let run_action = run_action.clone();
      move |_, _| run_action(kind)
    });
    context_actions.add_action(&action);
  }

  let run_now_action = gtk::gio::SimpleAction::new("run-timer-service", None);
  run_now_action.connect_activate({
    let run_timer_service_now = run_timer_service_now.clone();
    move |_, _| run_timer_service_now()
  });
  context_actions.add_action(&run_now_action);

  let kill_action = gtk::gio::SimpleAction::new("kill-service", Some(glib::VariantTy::STRING));
  kill_action.connect_activate({
    let selected_unit = selected_unit.clone();
    let tx = tx.clone();
    let status = status.clone();
    move |_, parameter| {
      let Some(signal) = parameter.and_then(|p| p.str()).map(String::from) else { return };
      let Some(unit) = selected_unit() else { return };
      let id = unit.id();
      if demo {
        status.set_text(&format!("Demo: kill {} with {signal}", id.name));
        return;
      }
      status.set_text(&format!("Sending {signal} to {}…", id.name));
      let tx = tx.clone();
      std::thread::spawn(move || {
        let token = CancellationToken::new();
        let result = tokio::runtime::Runtime::new()
          .unwrap()
          .block_on(systemd::kill_service(id, signal, token))
          .map_err(|e| format!("{e:#}"));
        let _ = tx.send(Reply::Action(result));
      });
    }
  });
  context_actions.add_action(&kill_action);

  let goto_unit_action = gtk::gio::SimpleAction::new("goto-unit", Some(glib::VariantTy::STRING));
  goto_unit_action.connect_activate({
    let select_unit = select_unit.clone();
    move |_, parameter| {
      let Some(target) = parameter.and_then(|p| p.str()) else { return };
      let (scope, name) = match target.split_once(':') {
        Some(("user", name)) => (UnitScope::User, name),
        Some((_, name)) => (UnitScope::Global, name),
        None => (UnitScope::Global, target),
      };
      select_unit(&UnitId { name: name.into(), scope });
    }
  });
  context_actions.add_action(&goto_unit_action);

  let view_logs_action = gtk::gio::SimpleAction::new("view-logs", None);
  view_logs_action.connect_activate({
    let inspector = inspector.clone();
    move |_, _| inspector.set_current_page(Some(1))
  });
  context_actions.add_action(&view_logs_action);

  let view_unit_file_action = gtk::gio::SimpleAction::new("view-unit-file", None);
  view_unit_file_action.connect_activate({
    let inspector = inspector.clone();
    move |_, _| inspector.set_current_page(Some(2))
  });
  context_actions.add_action(&view_unit_file_action);

  let copy_name_action = gtk::gio::SimpleAction::new("copy-unit-name", None);
  copy_name_action.connect_activate({
    let selected_unit = selected_unit.clone();
    let clipboard = gtk::prelude::WidgetExt::display(&window).clipboard();
    move |_, _| {
      if let Some(unit) = selected_unit() {
        clipboard.set_text(&unit.name);
      }
    }
  });
  context_actions.add_action(&copy_name_action);

  for (name, containing_folder) in [("open-unit-file", false), ("show-unit-file", true)] {
    let action = gtk::gio::SimpleAction::new(name, None);
    action.connect_activate({
      let selected_unit = selected_unit.clone();
      let window = window.clone();
      move |_, _| {
        let path =
          selected_unit().and_then(|unit| unit.file_path.as_ref().and_then(|path| path.as_ref().ok()).cloned());
        let Some(path) = path.filter(|path| std::path::Path::new(path).is_absolute()) else {
          show_error(
            &window,
            "This unit has no conventional unit file. Check Origin and Details for how systemd created it.",
          );
          return;
        };
        let mut file = gtk::gio::File::for_path(path);
        if containing_folder {
          file = file.parent().unwrap_or(file);
        }
        if let Err(error) = gtk::gio::AppInfo::launch_default_for_uri(&file.uri(), None::<&gtk::gio::AppLaunchContext>)
        {
          show_error(&window, &format!("Could not open the file: {error}"));
        }
      }
    });
    context_actions.add_action(&action);
  }

  // The context menu is built per-click so it can be state-aware: timers get timer
  // verbs and a jump to their service, services get kill signals and a jump to the
  // timer that schedules them (when one does).
  *open_context_menu.borrow_mut() = Some(Box::new({
    let rows = rows.clone();
    let timer_meta = timer_meta.clone();
    let activated_by = activated_by.clone();
    let services = services.clone();
    let timers = timers.clone();
    let on_timers_page = on_timers_page.clone();
    let context_actions = context_actions.clone();
    move |cell: &gtk::Widget, position: u32, x: f64, y: f64| {
      let selection = if on_timers_page() { &timers.selection } else { &services.selection };
      selection.set_selected(position);
      let Some(unit) =
        selected_row_index(selection).and_then(|index| rows.borrow().get(index).map(|row| row.unit.clone()))
      else {
        return;
      };

      let menu = gtk::gio::Menu::new();
      menu.append(Some("View Logs"), Some("context.view-logs"));
      menu.append(Some("View Unit File"), Some("context.view-unit-file"));
      if unit.kind() == UnitKind::Timer {
        if unit.is_active() {
          menu.append(Some("Stop Timer"), Some("context.stop-service"));
        } else {
          menu.append(Some("Start Timer"), Some("context.start-service"));
        }
        match unit.enablement_state.as_deref() {
          Some("enabled" | "enabled-runtime") => menu.append(Some("Disable Timer"), Some("context.disable-service")),
          Some("disabled") => menu.append(Some("Enable Timer"), Some("context.enable-service")),
          _ => {},
        }
        if let Some(target) = timer_meta.borrow().get(&unit.id()).and_then(|meta| meta.activates.clone()) {
          menu.append(Some(&format!("Run {target} Now")), Some("context.run-timer-service"));
          menu.append(
            Some(&format!("Go to {target}")),
            Some(&format!("context.goto-unit('{}:{target}')", scope_prefix(unit.scope))),
          );
        }
      } else {
        menu.append(Some("Start"), Some("context.start-service"));
        menu.append(Some("Stop"), Some("context.stop-service"));
        menu.append(Some("Restart"), Some("context.restart-service"));
        menu.append(Some("Enable at Startup"), Some("context.enable-service"));
        menu.append(Some("Disable at Startup"), Some("context.disable-service"));
        let kill = gtk::gio::Menu::new();
        for signal in ["SIGTERM", "SIGHUP", "SIGINT", "SIGQUIT", "SIGKILL", "SIGUSR1", "SIGUSR2"] {
          kill.append(Some(signal), Some(&format!("context.kill-service('{signal}')")));
        }
        menu.append_submenu(Some("Kill"), &kill);
        if let Some(timer) = activated_by.borrow().get(&unit.id()) {
          menu.append(
            Some(&format!("Go to {}", timer.name)),
            Some(&format!("context.goto-unit('{}:{}')", scope_prefix(timer.scope), timer.name)),
          );
        }
      }
      let files = gtk::gio::Menu::new();
      files.append(Some("Open in Default App"), Some("context.open-unit-file"));
      files.append(Some("Show in Files"), Some("context.show-unit-file"));
      files.append(Some("Copy Unit Name"), Some("context.copy-unit-name"));
      menu.append_section(None, &files);

      let popover = gtk::PopoverMenu::from_model(Some(&menu));
      popover.set_parent(cell);
      popover.insert_action_group("context", Some(&context_actions));
      popover.set_pointing_to(Some(&gtk::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
      popover.connect_closed(|popover| {
        let popover = popover.clone();
        glib::idle_add_local_once(move || popover.unparent());
      });
      popover.popup();
    }
  }));

  let log_generation = Rc::new(Cell::new(0_u64));
  let log_cancel = Rc::new(RefCell::new(None::<CancellationToken>));
  let log_current = Rc::new(RefCell::new(None::<UnitId>));
  let update_actions = Rc::new({
    let selected_unit = selected_unit.clone();
    let timer_meta = timer_meta.clone();
    let start = start.clone();
    let stop = stop.clone();
    let restart = restart.clone();
    let run_now = run_now.clone();
    let enable = enable.clone();
    let disable = disable.clone();
    let selected_summary = selected_summary.clone();
    let tx = tx.clone();
    let log_generation = log_generation.clone();
    let log_cancel = log_cancel.clone();
    let log_current = log_current.clone();
    move || {
      let selected = selected_unit();
      let is_timer = selected.as_ref().is_some_and(|unit| unit.kind() == UnitKind::Timer);
      let active = selected.as_ref().map(|unit| unit.is_active());
      start.set_sensitive(active == Some(false));
      stop.set_sensitive(active == Some(true));
      restart.set_sensitive(active.is_some() && !is_timer);
      let timer_target = selected
        .as_ref()
        .filter(|unit| unit.kind() == UnitKind::Timer)
        .and_then(|unit| timer_meta.borrow().get(&unit.id()).and_then(|meta| meta.activates.clone()));
      run_now.set_visible(is_timer);
      run_now.set_sensitive(timer_target.is_some());
      if let Some(target) = &timer_target {
        run_now.set_tooltip_text(Some(&format!("Run {target} now")));
      }
      let startup = selected.as_ref().and_then(|unit| unit.enablement_state.clone());
      enable.set_sensitive(matches!(startup.as_deref(), Some("disabled")));
      disable.set_sensitive(matches!(startup.as_deref(), Some("enabled" | "enabled-runtime")));
      if let Some(unit) = selected {
        let mut summary =
          format!("{}  ·  {}  ·  {}/{}", unit.name, unit.description, unit.activation_state, unit.sub_state);
        if is_timer {
          if let Some(next) = timer_meta.borrow().get(&unit.id()).and_then(|meta| meta.next_elapse.as_deref()) {
            summary.push_str(&format!("  ·  next run {}", relative_timestamp(next)));
          }
        }
        selected_summary.set_text(&summary);
        let unit_id = unit.id();
        let is_new_selection = log_current.borrow().as_ref() != Some(&unit_id);
        if is_new_selection {
          if let Some(cancel) = log_cancel.borrow_mut().take() {
            cancel.cancel();
          }
          *log_current.borrow_mut() = Some(unit_id.clone());
          let generation = log_generation.get().wrapping_add(1);
          log_generation.set(generation);
          let tx = tx.clone();
          if demo {
            let is_demo_timer = unit.kind() == UnitKind::Timer;
            let details = UnitRuntimeInfo {
              fragment_path: format!("/usr/lib/systemd/system/{}", unit_id.name),
              main_pid: (unit.is_active() && !is_demo_timer).then_some(1234),
              memory_current: (unit.is_active() && !is_demo_timer).then_some(18 * 1024 * 1024),
              tasks_current: (unit.is_active() && !is_demo_timer).then_some(7),
              n_restarts: (!is_demo_timer).then_some(0),
              active_enter_timestamp: Some(demo_timestamp(-30 * 3600)),
              triggered_unit: is_demo_timer.then(|| format!("{}.service", unit.short_name())),
              timer_schedules: if is_demo_timer { vec!["OnCalendar=daily".into()] } else { vec![] },
              persistent: is_demo_timer.then_some(true),
              randomized_delay: is_demo_timer.then(|| "5min".into()),
              accuracy: is_demo_timer.then(|| "1min".into()),
              ..UnitRuntimeInfo::default()
            };
            let _ = tx.send(Reply::Details {
              unit: unit_id.clone(),
              generation,
              details: Box::new(Ok(details)),
              logs: Ok(vec![
                LogEntry {
                  timestamp: Some("2026-07-12 09:41".into()),
                  content: format!("systemd[1]: Starting {}…", unit_id.name),
                  priority: Some(6),
                },
                LogEntry {
                  timestamp: Some("2026-07-12 09:41".into()),
                  content: format!("systemd[1]: Started {}.", unit_id.name),
                  priority: Some(6),
                },
                LogEntry {
                  timestamp: Some("2026-07-12 09:42".into()),
                  content: "demo[1234]: something looks off".into(),
                  priority: Some(4),
                },
                LogEntry {
                  timestamp: Some("2026-07-12 09:43".into()),
                  content: "demo[1234]: something went wrong".into(),
                  priority: Some(3),
                },
              ]),
              definition: Ok(format!(
                "# /usr/lib/systemd/system/{}\n[Unit]\nDescription={}\n\n[Service]\nExecStart=/usr/bin/example-daemon\n\n[Install]\nWantedBy=multi-user.target\n",
                unit_id.name, unit.description
              )),
            });
          } else {
            let cancel = CancellationToken::new();
            *log_cancel.borrow_mut() = Some(cancel.clone());
            std::thread::spawn(move || {
              let runtime = tokio::runtime::Runtime::new().unwrap();
              let (details, logs, definition) = runtime.block_on(async {
                tokio::join!(
                  gui_backend::load_unit_details(unit_id.clone()),
                  gui_backend::load_recent_logs(unit_id.clone(), 200),
                  gui_backend::load_unit_definition(unit_id.clone())
                )
              });
              let _ = tx.send(Reply::Details {
                unit: unit_id.clone(),
                generation,
                details: Box::new(details.map_err(|e| format!("{e:#}"))),
                logs: logs.map_err(|e| format!("{e:#}")),
                definition: definition.map_err(|e| format!("{e:#}")),
              });
              let tx_lines = tx.clone();
              let followed_unit = unit_id.clone();
              let result = runtime.block_on(gui_backend::follow_unit_logs(unit_id.clone(), cancel, move |entries| {
                tx_lines
                  .send(Reply::LogLines { unit: followed_unit.clone(), generation, entries })
                  .map_err(|_| anyhow::anyhow!("GUI closed"))
              }));
              if let Err(error) = result {
                let _ = tx.send(Reply::LogFollowError { unit: unit_id, generation, error: format!("{error:#}") });
              }
            });
          }
        }
      } else {
        if let Some(cancel) = log_cancel.borrow_mut().take() {
          cancel.cancel();
        }
        *log_current.borrow_mut() = None;
        selected_summary.set_text("No unit selected");
      }
    }
  });
  services.selection.connect_selected_notify({
    let update_actions = update_actions.clone();
    move |_| update_actions()
  });
  timers.selection.connect_selected_notify({
    let update_actions = update_actions.clone();
    move |_| update_actions()
  });
  stack.connect_visible_child_notify({
    let update_actions = update_actions.clone();
    let rebuild = rebuild.clone();
    move |_| {
      rebuild();
      update_actions();
    }
  });
  update_actions();

  glib::timeout_add_local(Duration::from_millis(50), {
    let rows = rows.clone();
    let timer_meta = timer_meta.clone();
    let activated_by = activated_by.clone();
    let selected_unit = selected_unit.clone();
    let rebuild = rebuild.clone();
    let status = status.clone();
    let load = load.clone();
    let set_details = set_details.clone();
    let logs_view = logs_view.clone();
    let append_log_entry = append_log_entry.clone();
    let unit_file_view = unit_file_view.clone();
    let window = window.clone();
    let update_actions = update_actions.clone();
    let inventory_loading = inventory_loading.clone();
    let log_generation = log_generation.clone();
    let current_selection = current_selection.clone();
    move || {
      while let Ok(reply) = rx.try_recv() {
        match reply {
          Reply::Units(Ok((service_list, timer_list))) => {
            inventory_loading.set(false);
            let count = service_list.units.len();
            *rows.borrow_mut() = service_list.units.into_iter().map(|unit| Row { unit }).collect();
            {
              let mut meta = timer_meta.borrow_mut();
              let mut reverse = activated_by.borrow_mut();
              meta.clear();
              reverse.clear();
              for (unit_scope, entry) in timer_list {
                let timer_id = UnitId { name: entry.timer.clone(), scope: unit_scope };
                if let Some(activates) = &entry.activates {
                  reverse.insert(UnitId { name: activates.clone(), scope: unit_scope }, timer_id.clone());
                }
                meta.insert(timer_id, entry);
              }
            }
            rebuild();
            update_actions();
            status.set_text(&format!("{count} units"));
          },
          Reply::Units(Err(e)) => {
            inventory_loading.set(false);
            status.set_text("Inventory update failed");
            if rows.borrow().is_empty() {
              show_error(&window, &e);
            }
          },
          Reply::Action(Err(e)) => {
            status.set_text("Operation failed");
            show_error(&window, &e);
          },
          Reply::Action(Ok(())) => {
            status.set_text("Done");
            load();
            for delay in [700, 1_500, 3_000] {
              let load = load.clone();
              glib::timeout_add_local_once(Duration::from_millis(delay), move || load());
            }
          },
          Reply::Details { unit, generation, details, logs, definition } => {
            let selected = selected_unit();
            if selected.as_ref().map(|s| s.id()) != Some(unit.clone()) || generation != log_generation.get() {
              continue;
            }

            match *details {
              Ok(info) => {
                let selected_unit_status = selected.expect("checked above");
                let discovered_path =
                  if info.fragment_path.is_empty() { &info.source_path } else { &info.fragment_path };
                if !discovered_path.is_empty() {
                  if let Some(index) = selected_row_index(&current_selection()) {
                    rows.borrow_mut()[index].unit.file_path = Some(Ok(discovered_path.clone()));
                  }
                }
                let detail_rows =
                  build_detail_rows(&selected_unit_status, &info, &timer_meta.borrow(), &activated_by.borrow());
                set_details(detail_rows);
              },
              Err(error) => set_details(vec![DetailRow::text("Error", error)]),
            }
            let buffer = logs_view.buffer();
            buffer.set_text("");
            match logs {
              Ok(entries) => {
                for entry in &entries {
                  append_log_entry(entry);
                }
              },
              Err(error) => buffer.set_text(&error),
            }
            unit_file_view.buffer().set_text(&match definition {
              Ok(definition) => definition,
              Err(error) => format!("Could not load the effective unit definition:\n\n{error}"),
            });
          },
          Reply::LogLines { unit, generation, entries } => {
            let selected_id = selected_unit().map(|s| s.id());
            if selected_id != Some(unit) || generation != log_generation.get() {
              continue;
            }
            let buffer = logs_view.buffer();
            for entry in &entries {
              append_log_entry(entry);
            }
            if buffer.line_count() > 2_000 {
              let mut start = buffer.start_iter();
              let mut keep_from =
                buffer.iter_at_line(buffer.line_count() - 2_000).unwrap_or_else(|| buffer.start_iter());
              buffer.delete(&mut start, &mut keep_from);
            }
            logs_view.scroll_to_iter(&mut buffer.end_iter(), 0.0, false, 0.0, 1.0);
          },
          Reply::LogFollowError { unit, generation, error } => {
            if generation == log_generation.get() {
              status.set_text(&format!("Stopped following {}: {}", unit.name, error));
            }
          },
        }
      }
      glib::ControlFlow::Continue
    }
  });

  glib::timeout_add_local(Duration::from_secs(5), {
    let load = load.clone();
    move || {
      load();
      glib::ControlFlow::Continue
    }
  });

  window.present();
  load();
}

fn scope_prefix(scope: UnitScope) -> &'static str {
  match scope {
    UnitScope::Global => "system",
    UnitScope::User => "user",
  }
}

/// Assemble the details rows for a unit: shared basics, then service stats or timer
/// scheduling info, plus cross-links between timers and the units they activate.
fn build_detail_rows(
  unit: &UnitWithStatus,
  info: &UnitRuntimeInfo,
  timer_meta: &HashMap<UnitId, TimerListEntry>,
  activated_by: &HashMap<UnitId, UnitId>,
) -> Vec<DetailRow> {
  let mut detail_rows = vec![
    DetailRow::text("Status", format!("{} / {}", unit.activation_state, unit.sub_state)),
    DetailRow::text("Startup", unit.enablement_state.clone().unwrap_or_else(|| "—".into())),
    DetailRow::text("Origin", format!("{:?}", systemd::unit_origin(unit, info))),
    DetailRow::text("Scope", scope_label(unit.scope)),
  ];

  if unit.kind() == UnitKind::Timer {
    let meta = timer_meta.get(&unit.id());
    let next_elapse = info.next_elapse.as_deref().or(meta.and_then(|meta| meta.next_elapse.as_deref()));
    detail_rows
      .push(DetailRow::text("Next trigger", next_elapse.map_or_else(|| "—".into(), absolute_and_relative_timestamp)));
    detail_rows.push(DetailRow::text(
      "Last trigger",
      info
        .last_trigger
        .as_deref()
        .or(meta.and_then(|meta| meta.last_trigger.as_deref()))
        .map_or_else(|| "—".into(), absolute_and_relative_timestamp),
    ));
    let activates = info.triggered_unit.clone().or_else(|| meta.and_then(|meta| meta.activates.clone()));
    match activates {
      Some(target) => {
        detail_rows.push(DetailRow::link("Activates", target.clone(), UnitId { name: target, scope: unit.scope }))
      },
      None => detail_rows.push(DetailRow::text("Activates", "—")),
    }
    if !info.timer_schedules.is_empty() {
      detail_rows.push(DetailRow::text("Schedule", info.timer_schedules.join("   ")));
    }
    if info.persistent == Some(true) {
      detail_rows.push(DetailRow::text("Persistent", "yes"));
    }
    if let Some(delay) = info.randomized_delay.as_deref().filter(|delay| *delay != "0") {
      detail_rows.push(DetailRow::text("Random delay", delay));
    }
    if let Some(accuracy) = &info.accuracy {
      detail_rows.push(DetailRow::text("Accuracy", accuracy));
    }
  } else {
    detail_rows.push(DetailRow::text("PID", info.main_pid.map_or_else(|| "—".into(), |value| value.to_string())));
    detail_rows.push(DetailRow::text("Memory", info.memory_current.map_or_else(|| "—".into(), format::format_bytes)));
    detail_rows
      .push(DetailRow::text("Tasks", info.tasks_current.map_or_else(|| "—".into(), |value| value.to_string())));
    detail_rows
      .push(DetailRow::text("Restarts", info.n_restarts.map_or_else(|| "—".into(), |value| value.to_string())));
    if let Some(timer) = activated_by.get(&unit.id()) {
      let mut value = timer.name.clone();
      if let Some(next) = timer_meta.get(timer).and_then(|meta| meta.next_elapse.as_deref()) {
        value.push_str(&format!(" — next run {}", relative_timestamp(next)));
      }
      detail_rows.push(DetailRow::link("Triggered by", value, timer.clone()));
    }
  }

  detail_rows.push(DetailRow::text(
    "Since",
    info.active_enter_timestamp.clone().or(info.inactive_enter_timestamp.clone()).unwrap_or_else(|| "—".into()),
  ));
  detail_rows.push(DetailRow::text("Result", info.result.clone().unwrap_or_else(|| "—".into())));
  detail_rows.push(DetailRow::text(
    "Unit file",
    if info.fragment_path.is_empty() { "—".into() } else { info.fragment_path.clone() },
  ));
  if !info.source_path.is_empty() {
    detail_rows.push(DetailRow::text("Source", info.source_path.clone()));
  }
  if !info.drop_in_paths.is_empty() {
    detail_rows.push(DetailRow::text("Drop-ins", info.drop_in_paths.join(", ")));
  }
  if let Some(preset) = &info.unit_file_preset {
    detail_rows.push(DetailRow::text("Preset", preset));
  }
  if let Some(about) = unit_descriptions::explain(&unit.name, unit.scope) {
    detail_rows.push(DetailRow::wrapped("About", about));
  }
  detail_rows
}
