use std::{
  cell::{Cell, RefCell},
  rc::Rc,
  sync::mpsc,
  time::Duration,
};

use anyhow::Result;
use gtk::{glib, prelude::*};
use gtk4 as gtk;
use systemctl_ui_core::systemd::{self, Scope, ServiceList, UnitId, UnitRuntimeInfo, UnitScope, UnitWithStatus};
use tokio_util::sync::CancellationToken;

mod gui_backend;

#[derive(Clone)]
struct Row {
  unit: UnitWithStatus,
}

enum Reply {
  Units(Result<ServiceList, String>),
  Action(Result<(), String>),
  Details {
    unit: UnitId,
    generation: u64,
    details: Box<Result<UnitRuntimeInfo, String>>,
    logs: Result<Vec<String>, String>,
    definition: Result<String, String>,
  },
  LogLine {
    unit: UnitId,
    generation: u64,
    line: String,
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
    ("bluetooth.service", "Bluetooth service", "inactive", "dead", "enabled"),
    ("cron.service", "Regular background program processing daemon", "active", "running", "enabled"),
    ("docker.service", "Docker Application Container Engine", "failed", "failed", "enabled"),
    ("NetworkManager.service", "Network Manager", "active", "running", "enabled"),
    ("ssh.service", "OpenBSD Secure Shell server", "active", "running", "enabled"),
    ("systemd-resolved.service", "Network Name Resolution", "active", "running", "enabled"),
    ("systemd-timesyncd.service", "Network Time Synchronization", "inactive", "dead", "disabled"),
  ]
  .into_iter()
  .enumerate()
  .map(|(i, (name, description, active, sub, enabled))| UnitWithStatus {
    name: name.into(),
    scope: if i == 2 { UnitScope::User } else { UnitScope::Global },
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

fn build_ui(app: &gtk::Application, demo: bool) {
  let rows = Rc::new(RefCell::new(Vec::<Row>::new()));
  let store = gtk::StringList::new(&[]);
  let selection_holder = Rc::new(RefCell::new(None::<gtk::SingleSelection>));
  let context_actions = gtk::gio::SimpleActionGroup::new();
  let view = gtk::ColumnView::new(None::<gtk::SingleSelection>);
  view.set_show_column_separators(true);
  view.set_show_row_separators(true);
  view.set_hexpand(true);
  view.set_vexpand(true);

  for (title, value, expand) in [
    ("Service", 0, false),
    ("Description", 1, true),
    ("State", 2, false),
    ("Startup", 3, false),
    ("Origin", 4, false),
    ("Scope", 5, false),
  ] {
    let factory = gtk::SignalListItemFactory::new();
    let setup_selection = selection_holder.clone();
    let setup_actions = context_actions.clone();
    factory.connect_setup(move |_, item| {
      let list_item = item.downcast_ref::<gtk::ListItem>().unwrap().clone();
      let label = gtk::Label::new(None);
      label.set_xalign(0.0);
      label.set_ellipsize(gtk::pango::EllipsizeMode::End);
      label.add_css_class("cell-label");
      if value == 0 {
        let cell = gtk::Box::new(gtk::Orientation::Horizontal, 3);
        let dot = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        dot.add_css_class("status-dot");
        let icon = gtk::Image::new();
        icon.set_pixel_size(13);
        icon.add_css_class("dim-label");
        cell.append(&dot);
        cell.append(&icon);
        cell.append(&label);
        item.downcast_ref::<gtk::ListItem>().unwrap().set_child(Some(&cell));
      } else {
        item.downcast_ref::<gtk::ListItem>().unwrap().set_child(Some(&label));
      }
      let cell = item.downcast_ref::<gtk::ListItem>().unwrap().child().unwrap();
      let click = gtk::GestureClick::new();
      click.set_button(3);
      click.connect_pressed({
        let setup_selection = setup_selection.clone();
        let setup_actions = setup_actions.clone();
        let context_cell = cell.clone();
        move |gesture, _, x, y| {
          let position = list_item.position();
          if position == gtk::INVALID_LIST_POSITION {
            return;
          }
          if let Some(selection) = setup_selection.borrow().as_ref() {
            selection.set_selected(position);
          }
          let menu = gtk::gio::Menu::new();
          menu.append(Some("View Logs"), Some("context.view-logs"));
          menu.append(Some("View Unit File"), Some("context.view-unit-file"));
          menu.append(Some("Start"), Some("context.start-service"));
          menu.append(Some("Stop"), Some("context.stop-service"));
          menu.append(Some("Restart"), Some("context.restart-service"));
          menu.append(Some("Enable at Startup"), Some("context.enable-service"));
          menu.append(Some("Disable at Startup"), Some("context.disable-service"));
          let files = gtk::gio::Menu::new();
          files.append(Some("Open in Default App"), Some("context.open-unit-file"));
          files.append(Some("Show in Files"), Some("context.show-unit-file"));
          files.append(Some("Copy Unit Name"), Some("context.copy-unit-name"));
          menu.append_section(None, &files);
          let popover = gtk::PopoverMenu::from_model(Some(&menu));
          popover.set_parent(&context_cell);
          popover.insert_action_group("context", Some(&setup_actions));
          popover.set_pointing_to(Some(&gtk::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
          popover.connect_closed(|popover| {
            let popover = popover.clone();
            glib::idle_add_local_once(move || popover.unparent());
          });
          popover.popup();
          gesture.set_state(gtk::EventSequenceState::Claimed);
        }
      });
      cell.add_controller(click);
    });
    let bind_rows = rows.clone();
    factory.connect_bind(move |_, item| {
      let item = item.downcast_ref::<gtk::ListItem>().unwrap();
      let Some(row_index) = item.item().as_ref().and_then(row_index) else { return };
      let rows = bind_rows.borrow();
      let unit = &rows[row_index].unit;
      let text = match value {
        0 => unit.short_name(),
        1 => unit.description.as_str(),
        2 => state_label(unit),
        3 => unit.enablement_state.as_deref().unwrap_or("—"),
        4 => unit_origin(unit),
        _ => match unit.scope {
          UnitScope::Global => "system",
          UnitScope::User => "user",
        },
      };
      if value == 0 {
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
        label.set_text(text);
        cell.set_tooltip_text(Some(&format!("{} — {} ({})", unit.name, unit.activation_state, unit.sub_state)));
      } else {
        item.child().and_downcast::<gtk::Label>().unwrap().set_text(text);
      }
    });
    let column = gtk::ColumnViewColumn::new(Some(title), Some(factory));
    let sorter_rows = rows.clone();
    column.set_sorter(Some(&gtk::CustomSorter::new(move |left, right| {
      let Some(left_index) = row_index(left) else { return gtk::Ordering::Equal };
      let Some(right_index) = row_index(right) else { return gtk::Ordering::Equal };
      let rows = sorter_rows.borrow();
      let Some(left) = rows.get(left_index).map(|row| &row.unit) else { return gtk::Ordering::Equal };
      let Some(right) = rows.get(right_index).map(|row| &row.unit) else { return gtk::Ordering::Equal };
      let ordering = match value {
        0 => left.short_name().to_lowercase().cmp(&right.short_name().to_lowercase()),
        1 => left.description.to_lowercase().cmp(&right.description.to_lowercase()),
        2 => state_label(left).to_lowercase().cmp(&state_label(right).to_lowercase()),
        3 => left.enablement_state.as_deref().unwrap_or("").cmp(right.enablement_state.as_deref().unwrap_or("")),
        4 => unit_origin(left).cmp(unit_origin(right)),
        _ => match (left.scope, right.scope) {
          (UnitScope::Global, UnitScope::User) => std::cmp::Ordering::Less,
          (UnitScope::User, UnitScope::Global) => std::cmp::Ordering::Greater,
          _ => std::cmp::Ordering::Equal,
        },
      };
      ordering.then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase())).into()
    })));
    column.set_resizable(true);
    column.set_expand(expand);
    if value == 0 {
      column.set_fixed_width(230);
    }
    view.append_column(&column);
  }

  let sorted = gtk::SortListModel::new(Some(store.clone()), view.sorter());
  let selection = gtk::SingleSelection::new(Some(sorted));
  selection.set_autoselect(true);
  view.set_model(Some(&selection));
  *selection_holder.borrow_mut() = Some(selection.clone());

  let all_filter = gtk::ToggleButton::with_label("All");
  let active_filter = gtk::ToggleButton::with_label("Active");
  let failed_filter = gtk::ToggleButton::with_label("Failed");
  let inactive_filter = gtk::ToggleButton::with_label("Inactive");
  all_filter.set_active(true);
  let filter = Rc::new(Cell::new(StatusFilter::All));
  let scope_filter = Rc::new(Cell::new(ScopeFilter::All));
  let scope = gtk::DropDown::from_strings(&["All scopes", "System", "User"]);
  scope.set_tooltip_text(Some("Filter by service scope"));
  let search = gtk::SearchEntry::builder().placeholder_text("Filter services").hexpand(true).build();
  let start = gtk::Button::builder().icon_name("media-playback-start-symbolic").tooltip_text("Start service").build();
  let stop = gtk::Button::builder().icon_name("media-playback-stop-symbolic").tooltip_text("Stop service").build();
  let restart = gtk::Button::builder().icon_name("view-refresh-symbolic").tooltip_text("Restart service").build();
  let enable = gtk::Button::builder().icon_name("emblem-default-symbolic").tooltip_text("Enable at startup").build();
  let disable =
    gtk::Button::builder().icon_name("action-unavailable-symbolic").tooltip_text("Disable at startup").build();
  let status = gtk::Label::new(Some(if demo { "Demo data" } else { "Loading…" }));
  status.set_xalign(0.0);

  let toolbar = gtk::Box::new(gtk::Orientation::Horizontal, 4);
  toolbar.add_css_class("toolbar");
  toolbar.append(&all_filter);
  toolbar.append(&active_filter);
  toolbar.append(&failed_filter);
  toolbar.append(&inactive_filter);
  toolbar.append(&scope);
  toolbar.append(&search);
  toolbar.append(&start);
  toolbar.append(&stop);
  toolbar.append(&restart);
  toolbar.append(&enable);
  toolbar.append(&disable);
  let scroller = gtk::ScrolledWindow::builder().child(&view).vexpand(true).hexpand(true).build();

  let details_grid = gtk::Grid::builder()
    .column_spacing(12)
    .row_spacing(3)
    .margin_top(6)
    .margin_bottom(6)
    .margin_start(8)
    .margin_end(8)
    .build();
  let detail_names = [
    "Status",
    "Startup",
    "Origin",
    "Scope",
    "PID",
    "Memory",
    "Tasks",
    "Restarts",
    "Since",
    "Result",
    "Unit file",
    "Source",
    "Drop-ins",
    "Preset",
  ];
  let detail_values = detail_names
    .iter()
    .enumerate()
    .map(|(row, name)| {
      let name_label = gtk::Label::new(Some(name));
      name_label.set_xalign(1.0);
      name_label.add_css_class("dim-label");
      let value = gtk::Label::new(Some("—"));
      value.set_xalign(0.0);
      value.set_selectable(true);
      value.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
      value.set_hexpand(true);
      details_grid.attach(&name_label, ((row / 5) * 2) as i32, (row % 5) as i32, 1, 1);
      details_grid.attach(&value, ((row / 5) * 2 + 1) as i32, (row % 5) as i32, 1, 1);
      value
    })
    .collect::<Vec<_>>();

  let logs_view = gtk::TextView::builder()
    .editable(false)
    .cursor_visible(false)
    .monospace(true)
    .wrap_mode(gtk::WrapMode::None)
    .build();
  logs_view.add_css_class("logs");
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
  let inspector = gtk::Notebook::new();
  inspector.append_page(&details_grid, Some(&gtk::Label::new(Some("Details"))));
  inspector.append_page(&logs_page, Some(&gtk::Label::new(Some("Logs (live)"))));
  inspector.append_page(&unit_file_scroller, Some(&gtk::Label::new(Some("Unit File"))));
  inspector.set_size_request(-1, 190);

  let workspace = gtk::Paned::new(gtk::Orientation::Vertical);
  workspace.set_start_child(Some(&scroller));
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
  let selected_summary = gtk::Label::new(Some("No service selected"));
  selected_summary.set_xalign(0.0);
  selected_summary.set_ellipsize(gtk::pango::EllipsizeMode::End);
  selected_summary.set_hexpand(true);
  status.set_xalign(1.0);
  status_bar.append(&selected_summary);
  status_bar.append(&status);
  content.append(&status_bar);

  let window = gtk::ApplicationWindow::builder()
    .application(app)
    .title("Systemctl")
    .default_width(1100)
    .default_height(650)
    .child(&content)
    .build();

  let provider = gtk::CssProvider::new();
  provider.load_from_data(
    ".toolbar { padding: 3px; } .status-bar { padding: 2px 5px; border-top: 1px solid alpha(currentColor, .15); } columnview.view > listview > row { min-height: 18px; padding: 0; } columnview.view > listview > row > cell { padding: 0; } .cell-label { padding: 0 3px; font-size: 12px; } .logs, .unit-file { font-size: 12px; } .status-dot { min-width: 6px; min-height: 6px; border-radius: 999px; margin-left: 3px; } .status-dot.active { background: #2ec27e; } .status-dot.failed { background: #e01b24; } .status-dot.transition { background: #f6d32d; } .status-dot.inactive { background: alpha(currentColor, .32); }",
  );
  gtk::style_context_add_provider_for_display(
    &gtk::prelude::WidgetExt::display(&window),
    &provider,
    gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
  );

  let (tx, rx) = mpsc::channel::<Reply>();
  let rebuild = {
    let rows = rows.clone();
    let store = store.clone();
    let search = search.clone();
    let filter = filter.clone();
    let scope_filter = scope_filter.clone();
    let all_filter = all_filter.clone();
    let active_filter = active_filter.clone();
    let failed_filter = failed_filter.clone();
    let inactive_filter = inactive_filter.clone();
    Rc::new(move || {
      let needle = search.text().to_lowercase();
      let selected_filter = filter.get();
      let selected_scope = scope_filter.get();
      let borrowed_rows = rows.borrow();
      let active = borrowed_rows.iter().filter(|row| row.unit.activation_state == "active").count();
      let failed = borrowed_rows.iter().filter(|row| row.unit.is_failed()).count();
      let inactive = borrowed_rows.iter().filter(|row| row.unit.activation_state == "inactive").count();
      all_filter.set_label(&format!("All {}", borrowed_rows.len()));
      active_filter.set_label(&format!("Active {active}"));
      failed_filter.set_label(&format!("Failed {failed}"));
      inactive_filter.set_label(&format!("Inactive {inactive}"));
      drop(borrowed_rows);
      let indices: Vec<usize> = rows
        .borrow()
        .iter()
        .enumerate()
        .filter(|(_, row)| {
          let matches_text =
            row.unit.name.to_lowercase().contains(&needle) || row.unit.description.to_lowercase().contains(&needle);
          let matches_state = match selected_filter {
            StatusFilter::All => true,
            StatusFilter::Active => row.unit.activation_state == "active",
            StatusFilter::Failed => row.unit.is_failed(),
            StatusFilter::Inactive => row.unit.activation_state == "inactive",
          };
          let matches_scope = match selected_scope {
            ScopeFilter::All => true,
            ScopeFilter::System => row.unit.scope == UnitScope::Global,
            ScopeFilter::User => row.unit.scope == UnitScope::User,
          };
          matches_text && matches_state && matches_scope
        })
        .map(|(i, _)| i)
        .collect();
      store.splice(
        0,
        store.n_items(),
        &indices.iter().map(|i| i.to_string()).collect::<Vec<_>>().iter().map(String::as_str).collect::<Vec<_>>(),
      );
    })
  };

  if demo {
    *rows.borrow_mut() = demo_units().into_iter().map(|unit| Row { unit }).collect();
    rebuild();
  }
  search.connect_search_changed({
    let rebuild = rebuild.clone();
    move |_| rebuild()
  });
  search.connect_stop_search({
    let view = view.clone();
    move |search| {
      search.set_text("");
      view.grab_focus();
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
        let result = tokio::runtime::Runtime::new()
          .unwrap()
          .block_on(gui_backend::load_service_inventory(Scope::All, &["*.service".into()]))
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

  let run_action = Rc::new({
    let rows = rows.clone();
    let selection = selection.clone();
    let tx = tx.clone();
    let status = status.clone();
    move |kind: &'static str| {
      let Some(row_index) = selected_row_index(&selection) else { return };
      let id: UnitId = rows.borrow()[row_index].unit.id();
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
    let rows = rows.clone();
    let selection = selection.clone();
    let clipboard = gtk::prelude::WidgetExt::display(&window).clipboard();
    move |_, _| {
      if let Some(index) = selected_row_index(&selection) {
        clipboard.set_text(&rows.borrow()[index].unit.name);
      }
    }
  });
  context_actions.add_action(&copy_name_action);

  for (name, containing_folder) in [("open-unit-file", false), ("show-unit-file", true)] {
    let action = gtk::gio::SimpleAction::new(name, None);
    action.connect_activate({
      let rows = rows.clone();
      let selection = selection.clone();
      let window = window.clone();
      move |_, _| {
        let path = selected_row_index(&selection)
          .and_then(|index| rows.borrow()[index].unit.file_path.as_ref().and_then(|path| path.as_ref().ok()).cloned());
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

  let log_generation = Rc::new(Cell::new(0_u64));
  let log_cancel = Rc::new(RefCell::new(None::<CancellationToken>));
  let log_current = Rc::new(RefCell::new(None::<UnitId>));
  let update_actions = Rc::new({
    let rows = rows.clone();
    let selection = selection.clone();
    let start = start.clone();
    let stop = stop.clone();
    let restart = restart.clone();
    let enable = enable.clone();
    let disable = disable.clone();
    let selected_summary = selected_summary.clone();
    let tx = tx.clone();
    let log_generation = log_generation.clone();
    let log_cancel = log_cancel.clone();
    let log_current = log_current.clone();
    move || {
      let selected = selected_row_index(&selection);
      let active = selected.map(|index| rows.borrow()[index].unit.is_active());
      start.set_sensitive(active == Some(false));
      stop.set_sensitive(active == Some(true));
      restart.set_sensitive(active.is_some());
      let startup = selected.and_then(|index| rows.borrow()[index].unit.enablement_state.clone());
      enable.set_sensitive(matches!(startup.as_deref(), Some("disabled")));
      disable.set_sensitive(matches!(startup.as_deref(), Some("enabled" | "enabled-runtime")));
      if let Some(index) = selected {
        let rows = rows.borrow();
        let unit = &rows[index].unit;
        selected_summary.set_text(&format!(
          "{}  ·  {}  ·  {}/{}",
          unit.name, unit.description, unit.activation_state, unit.sub_state
        ));
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
            let details = UnitRuntimeInfo {
              fragment_path: format!("/usr/lib/systemd/system/{}", unit_id.name),
              main_pid: unit.is_active().then_some(1234),
              memory_current: unit.is_active().then_some(18 * 1024 * 1024),
              tasks_current: unit.is_active().then_some(7),
              n_restarts: Some(0),
              active_enter_timestamp: Some("Sun 2026-07-12 09:41:03 PDT".into()),
              ..UnitRuntimeInfo::default()
            };
            let _ = tx.send(Reply::Details {
              unit: unit_id.clone(),
              generation,
              details: Box::new(Ok(details)),
            logs: Ok(vec![
              format!("2026-07-12T09:41:03 systemd[1]: Starting {}…", unit_id.name),
              format!("2026-07-12T09:41:03 systemd[1]: Started {}.", unit_id.name),
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
              let result = runtime.block_on(gui_backend::follow_unit_logs(unit_id.clone(), cancel, move |line| {
                tx_lines
                  .send(Reply::LogLine { unit: followed_unit.clone(), generation, line })
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
        selected_summary.set_text("No service selected");
      }
    }
  });
  selection.connect_selected_notify({
    let update_actions = update_actions.clone();
    move |_| update_actions()
  });
  update_actions();

  glib::timeout_add_local(Duration::from_millis(50), {
    let rows = rows.clone();
    let selection = selection.clone();
    let rebuild = rebuild.clone();
    let status = status.clone();
    let load = load.clone();
    let detail_values = detail_values.clone();
    let logs_view = logs_view.clone();
    let unit_file_view = unit_file_view.clone();
    let window = window.clone();
    let update_actions = update_actions.clone();
    let inventory_loading = inventory_loading.clone();
    let log_generation = log_generation.clone();
    move || {
      while let Ok(reply) = rx.try_recv() {
        match reply {
          Reply::Units(Ok(service_list)) => {
            inventory_loading.set(false);
            let count = service_list.units.len();
            *rows.borrow_mut() = service_list.units.into_iter().map(|unit| Row { unit }).collect();
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
            let selected_id =
              selected_row_index(&selection).and_then(|index| rows.borrow().get(index).map(|row| row.unit.id()));
            if selected_id.as_ref() != Some(&unit) || generation != log_generation.get() {
              continue;
            }

            match *details {
              Ok(info) => {
                let selected_index = selected_row_index(&selection);
                let selected_unit =
                  selected_index.and_then(|index| rows.borrow().get(index).map(|row| row.unit.clone()));
                let discovered_path =
                  if info.fragment_path.is_empty() { &info.source_path } else { &info.fragment_path };
                if let Some(index) = selected_index {
                  if !discovered_path.is_empty() {
                    rows.borrow_mut()[index].unit.file_path = Some(Ok(discovered_path.clone()));
                  }
                }
                let values = selected_unit.map(|selected| {
                  let origin = format!("{:?}", systemd::unit_origin(&selected, &info));
                  vec![
                    format!("{} / {}", selected.activation_state, selected.sub_state),
                    selected.enablement_state.unwrap_or_else(|| "—".into()),
                    origin,
                    match selected.scope {
                      UnitScope::Global => "system".into(),
                      UnitScope::User => "user".into(),
                    },
                    info.main_pid.map_or_else(|| "—".into(), |value| value.to_string()),
                    info
                      .memory_current
                      .map_or_else(|| "—".into(), |value| format!("{:.1} MiB", value as f64 / 1_048_576.0)),
                    info.tasks_current.map_or_else(|| "—".into(), |value| value.to_string()),
                    info.n_restarts.map_or_else(|| "—".into(), |value| value.to_string()),
                    info.active_enter_timestamp.or(info.inactive_enter_timestamp).unwrap_or_else(|| "—".into()),
                    info.result.unwrap_or_else(|| "—".into()),
                    if info.fragment_path.is_empty() { "—".into() } else { info.fragment_path },
                    if info.source_path.is_empty() { "—".into() } else { info.source_path },
                    if info.drop_in_paths.is_empty() { "—".into() } else { info.drop_in_paths.join(", ") },
                    info.unit_file_preset.unwrap_or_else(|| "—".into()),
                  ]
                });
                if let Some(values) = values {
                  for (label, value) in detail_values.iter().zip(values) {
                    label.set_text(&value);
                    label.set_tooltip_text(Some(&value));
                  }
                }
              },
              Err(error) => detail_values[0].set_text(&error),
            }
            logs_view.buffer().set_text(&match logs {
              Ok(lines) => lines.join("\n"),
              Err(error) => error,
            });
            unit_file_view.buffer().set_text(&match definition {
              Ok(definition) => definition,
              Err(error) => format!("Could not load the effective unit definition:\n\n{error}"),
            });
          },
          Reply::LogLine { unit, generation, line } => {
            let selected_id =
              selected_row_index(&selection).and_then(|index| rows.borrow().get(index).map(|row| row.unit.id()));
            if selected_id.as_ref() != Some(&unit) || generation != log_generation.get() {
              continue;
            }
            let buffer = logs_view.buffer();
            let mut end = buffer.end_iter();
            if buffer.char_count() > 0 {
              buffer.insert(&mut end, "\n");
            }
            buffer.insert(&mut end, &line);
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
