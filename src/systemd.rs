// File initially taken from https://github.com/servicer-labs/servicer/blob/master/src/utils/systemd.rs, since modified

use anyhow::Result;
use duct::cmd;
use log::error;
use tokio_util::sync::CancellationToken;
use tracing::info;
use zbus::{proxy, zvariant, Connection};

#[derive(Debug, Clone)]
pub struct UnitWithStatus {
  pub name: String,              // The primary unit name as string
  pub scope: UnitScope,          // System or user?
  pub description: String,       // The human readable description string
  pub file_path: Option<String>, // The unit file path - populated later on demand
  pub load_state: String,        // The load state (i.e. whether the unit file has been loaded successfully)
  pub active_state: String,      // The active state (i.e. whether the unit is currently started or not)
  pub sub_state: String, // The sub state (a more fine-grained version of the active state that is specific to the unit type, which the active state is not)
                         // We don't use any of these right now, might as well skip'em so there's less data to clone
                         // pub followed: String, // A unit that is being followed in its state by this unit, if there is any, otherwise the empty string.
                         // pub path: String,     // The unit object path
                         // pub job_id: u32,      // If there is a job queued for the job unit the numeric job id, 0 otherwise
                         // pub job_type: String, // The job type as string
                         // pub job_path: String, // The job object path
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UnitScope {
  Global,
  User,
}

/// Just enough info to fully identify a unit
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct UnitId {
  pub name: String,
  pub scope: UnitScope,
}

impl UnitWithStatus {
  pub fn is_active(&self) -> bool {
    self.active_state == "active"
  }

  pub fn is_failed(&self) -> bool {
    self.active_state == "failed"
  }

  pub fn is_not_found(&self) -> bool {
    self.load_state == "not-found"
  }

  pub fn is_enabled(&self) -> bool {
    self.load_state == "loaded" && self.active_state == "active"
  }

  pub fn short_name(&self) -> &str {
    if self.name.ends_with(".service") {
      &self.name[..self.name.len() - 8]
    } else {
      &self.name
    }
  }

  // TODO: should we have a non-allocating version of this?
  pub fn id(&self) -> UnitId {
    UnitId { name: self.name.clone(), scope: self.scope }
  }

  // useful for updating without wiping out the file path
  pub fn update(&mut self, other: UnitWithStatus) {
    self.description = other.description;
    self.load_state = other.load_state;
    self.active_state = other.active_state;
    self.sub_state = other.sub_state;
  }
}

type RawUnit =
  (String, String, String, String, String, String, zvariant::OwnedObjectPath, u32, String, zvariant::OwnedObjectPath);

fn to_unit_status(raw_unit: RawUnit, scope: UnitScope) -> UnitWithStatus {
  let (name, description, load_state, active_state, sub_state, _followed, _path, _job_id, _job_type, _job_path) =
    raw_unit;

  UnitWithStatus { name, scope, description, file_path: None, load_state, active_state, sub_state }
}

// Different from UnitScope in that this is not for 1 specific unit (i.e. it can include multiple scopes)
#[derive(Clone, Copy, Default, Debug)]
pub enum Scope {
  Global,
  User,
  #[default]
  All,
}

// this takes like 5-10 ms on 13th gen Intel i7 (scope=all)
pub async fn get_all_services(scope: Scope) -> Result<Vec<UnitWithStatus>> {
  let start = std::time::Instant::now();

  let mut units = vec![];

  let is_root = nix::unistd::geteuid().is_root();

  match scope {
    Scope::Global => {
      let system_units = get_services(UnitScope::Global).await?;
      units.extend(system_units);
    },
    Scope::User => {
      let user_units = get_services(UnitScope::User).await?;
      units.extend(user_units);
    },
    Scope::All => {
      let (system_units, user_units) = tokio::join!(get_services(UnitScope::Global), get_services(UnitScope::User));
      units.extend(system_units?);

      // Should always be able to get user units, but it may fail when running as root
      if let Ok(user_units) = user_units {
        units.extend(user_units);
      } else if is_root {
        error!("Failed to get user units, ignoring because we're running as root")
      } else {
        user_units?;
      }
    },
  }

  // sort by name case-insensitive
  units.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));

  info!("Loaded systemd services in {:?}", start.elapsed());

  Ok(units)
}

async fn get_services(scope: UnitScope) -> Result<Vec<UnitWithStatus>, anyhow::Error> {
  let connection = get_connection(scope).await?;
  let manager_proxy = ManagerProxy::new(&connection).await?;
  let units = manager_proxy.list_units_by_patterns(vec![], vec!["*.service".into()]).await?;
  let units: Vec<_> = units.into_iter().map(|u| to_unit_status(u, scope)).collect();
  Ok(units)
}

pub fn get_unit_file_location(service: &UnitId) -> Result<String> {
  // show -P FragmentPath reitunes.service
  let mut args = vec!["--quiet", "show", "-P", "FragmentPath"];
  args.push(&service.name);

  if service.scope == UnitScope::User {
    args.insert(0, "--user");
  }

  match cmd("systemctl", args).read() {
    Ok(output) => Ok(output.trim().to_string()),
    Err(e) => anyhow::bail!("Failed to get unit file location: {}", e),
  }
}

pub async fn start_service(service: UnitId, cancel_token: CancellationToken) -> Result<()> {
  async fn start_service(service: UnitId) -> Result<()> {
    let connection = get_connection(service.scope).await?;
    let manager_proxy = ManagerProxy::new(&connection).await?;
    manager_proxy.start_unit(service.name.clone(), "replace".into()).await?;
    Ok(())
  }

  // god these select macros are ugly, is there really no better way to select?
  tokio::select! {
    _ = cancel_token.cancelled() => {
        // The token was cancelled
        anyhow::bail!("cancelled");
    }
    result = start_service(service) => {
        result
    }
  }
}

pub async fn stop_service(service: UnitId, cancel_token: CancellationToken) -> Result<()> {
  async fn stop_service(service: UnitId) -> Result<()> {
    let connection = get_connection(service.scope).await?;
    let manager_proxy = ManagerProxy::new(&connection).await?;
    manager_proxy.stop_unit(service.name, "replace".into()).await?;
    Ok(())
  }

  // god these select macros are ugly, is there really no better way to select?
  tokio::select! {
    _ = cancel_token.cancelled() => {
        // The token was cancelled
        anyhow::bail!("cancelled");
    }
    result = stop_service(service) => {
        result
    }
  }
}

async fn get_connection(scope: UnitScope) -> Result<Connection, anyhow::Error> {
  match scope {
    UnitScope::Global => Ok(Connection::system().await?),
    UnitScope::User => Ok(Connection::session().await?),
  }
}

pub async fn restart_service(service: UnitId, cancel_token: CancellationToken) -> Result<()> {
  async fn restart(service: UnitId) -> Result<()> {
    let connection = get_connection(service.scope).await?;
    let manager_proxy = ManagerProxy::new(&connection).await?;
    manager_proxy.restart_unit(service.name, "replace".into()).await?;
    Ok(())
  }

  // god these select macros are ugly, is there really no better way to select?
  tokio::select! {
    _ = cancel_token.cancelled() => {
        // The token was cancelled
        anyhow::bail!("cancelled");
    }
    result = restart(service) => {
        result
    }
  }
}

// useless function only added to test that cancellation works
pub async fn sleep_test(_service: String, cancel_token: CancellationToken) -> Result<()> {
  // god these select macros are ugly, is there really no better way to select?
  tokio::select! {
      _ = cancel_token.cancelled() => {
          // The token was cancelled
          anyhow::bail!("cancelled");
      }
      _ = tokio::time::sleep(std::time::Duration::from_secs(2)) => {
          Ok(())
      }
  }
}

/// Proxy object for `org.freedesktop.systemd1.Manager`.
/// Partially taken from https://github.com/lucab/zbus_systemd/blob/main/src/systemd1/generated.rs
#[proxy(
  interface = "org.freedesktop.systemd1.Manager",
  default_service = "org.freedesktop.systemd1",
  default_path = "/org/freedesktop/systemd1",
  gen_blocking = false
)]
pub trait Manager {
  /// [📖](https://www.freedesktop.org/software/systemd/man/systemd.directives.html#StartUnit()) Call interface method `StartUnit`.
  #[dbus_proxy(name = "StartUnit")]
  fn start_unit(&self, name: String, mode: String) -> zbus::Result<zvariant::OwnedObjectPath>;

  /// [📖](https://www.freedesktop.org/software/systemd/man/systemd.directives.html#StopUnit()) Call interface method `StopUnit`.
  #[dbus_proxy(name = "StopUnit")]
  fn stop_unit(&self, name: String, mode: String) -> zbus::Result<zvariant::OwnedObjectPath>;

  /// [📖](https://www.freedesktop.org/software/systemd/man/systemd.directives.html#RestartUnit()) Call interface method `RestartUnit`.
  #[dbus_proxy(name = "RestartUnit")]
  fn restart_unit(&self, name: String, mode: String) -> zbus::Result<zvariant::OwnedObjectPath>;

  /// [📖](https://www.freedesktop.org/software/systemd/man/systemd.directives.html#EnableUnitFiles()) Call interface method `EnableUnitFiles`.
  #[dbus_proxy(name = "EnableUnitFiles")]
  fn enable_unit_files(
    &self,
    files: Vec<String>,
    runtime: bool,
    force: bool,
  ) -> zbus::Result<(bool, Vec<(String, String, String)>)>;

  /// [📖](https://www.freedesktop.org/software/systemd/man/systemd.directives.html#DisableUnitFiles()) Call interface method `DisableUnitFiles`.
  #[dbus_proxy(name = "DisableUnitFiles")]
  fn disable_unit_files(&self, files: Vec<String>, runtime: bool) -> zbus::Result<Vec<(String, String, String)>>;

  /// [📖](https://www.freedesktop.org/software/systemd/man/systemd.directives.html#ListUnits()) Call interface method `ListUnits`.
  #[dbus_proxy(name = "ListUnits")]
  fn list_units(
    &self,
  ) -> zbus::Result<
    Vec<(
      String,
      String,
      String,
      String,
      String,
      String,
      zvariant::OwnedObjectPath,
      u32,
      String,
      zvariant::OwnedObjectPath,
    )>,
  >;

  /// [📖](https://www.freedesktop.org/software/systemd/man/systemd.directives.html#ListUnitsByPatterns()) Call interface method `ListUnitsByPatterns`.
  #[dbus_proxy(name = "ListUnitsByPatterns")]
  fn list_units_by_patterns(
    &self,
    states: Vec<String>,
    patterns: Vec<String>,
  ) -> zbus::Result<
    Vec<(
      String,
      String,
      String,
      String,
      String,
      String,
      zvariant::OwnedObjectPath,
      u32,
      String,
      zvariant::OwnedObjectPath,
    )>,
  >;

  /// [📖](https://www.freedesktop.org/software/systemd/man/systemd.directives.html#Reload()) Call interface method `Reload`.
  #[dbus_proxy(name = "Reload")]
  fn reload(&self) -> zbus::Result<()>;
}

/// Proxy object for `org.freedesktop.systemd1.Unit`.
/// Taken from https://github.com/lucab/zbus_systemd/blob/main/src/systemd1/generated.rs
#[proxy(
  interface = "org.freedesktop.systemd1.Unit",
  default_service = "org.freedesktop.systemd1",
  assume_defaults = false,
  gen_blocking = false
)]
pub trait Unit {
  /// Get property `ActiveState`.
  #[dbus_proxy(property)]
  fn active_state(&self) -> zbus::Result<String>;

  /// Get property `LoadState`.
  #[dbus_proxy(property)]
  fn load_state(&self) -> zbus::Result<String>;

  /// Get property `UnitFileState`.
  #[dbus_proxy(property)]
  fn unit_file_state(&self) -> zbus::Result<String>;
}

/// Proxy object for `org.freedesktop.systemd1.Service`.
/// Taken from https://github.com/lucab/zbus_systemd/blob/main/src/systemd1/generated.rs
#[proxy(
  interface = "org.freedesktop.systemd1.Service",
  default_service = "org.freedesktop.systemd1",
  assume_defaults = false,
  gen_blocking = false
)]
trait Service {
  /// Get property `MainPID`.
  #[dbus_proxy(property, name = "MainPID")]
  fn main_pid(&self) -> zbus::Result<u32>;
}

/// Returns the load state of a systemd unit
///
/// Returns `invalid-unit-path` if the path is invalid
///
/// # Arguments
///
/// * `connection`: zbus connection
/// * `full_service_name`: Full name of the service name with '.service' in the end
///
pub async fn get_active_state(connection: &Connection, full_service_name: &str) -> String {
  let object_path = get_unit_path(full_service_name);

  match zvariant::ObjectPath::try_from(object_path) {
    Ok(path) => {
      let unit_proxy = UnitProxy::new(connection, path).await.unwrap();
      unit_proxy.active_state().await.unwrap_or("invalid-unit-path".into())
    },
    Err(_) => "invalid-unit-path".to_string(),
  }
}

/// Returns the unit file state of a systemd unit. If the state is `enabled`, the unit loads on every boot
///
/// Returns `invalid-unit-path` if the path is invalid
///
/// # Arguments
///
/// * `connection`: zbus connection
/// * `full_service_name`: Full name of the service name with '.service' in the end
///
pub async fn get_unit_file_state(connection: &Connection, full_service_name: &str) -> String {
  let object_path = get_unit_path(full_service_name);

  match zvariant::ObjectPath::try_from(object_path) {
    Ok(path) => {
      let unit_proxy = UnitProxy::new(connection, path).await.unwrap();
      unit_proxy.unit_file_state().await.unwrap_or("invalid-unit-path".into())
    },
    Err(_) => "invalid-unit-path".to_string(),
  }
}

/// Returns the PID of a systemd service
///
/// # Arguments
///
/// * `connection`: zbus connection
/// * `full_service_name`: Full name of the service name with '.service' in the end
///
pub async fn get_main_pid(connection: &Connection, full_service_name: &str) -> Result<u32, zbus::Error> {
  let object_path = get_unit_path(full_service_name);

  let validated_object_path = zvariant::ObjectPath::try_from(object_path).unwrap();

  let service_proxy = ServiceProxy::new(connection, validated_object_path).await.unwrap();
  service_proxy.main_pid().await
}

/// Encode into a valid dbus string
///
/// # Arguments
///
/// * `input_string`
///
fn encode_as_dbus_object_path(input_string: &str) -> String {
  input_string
    .chars()
    .map(|c| if c.is_ascii_alphanumeric() || c == '/' || c == '_' { c.to_string() } else { format!("_{:x}", c as u32) })
    .collect()
}

/// Unit file path for a service
///
/// # Arguments
///
/// * `full_service_name`
///
pub fn get_unit_path(full_service_name: &str) -> String {
  format!("/org/freedesktop/systemd1/unit/{}", encode_as_dbus_object_path(full_service_name))
}
