// File initially taken from https://github.com/servicer-labs/servicer/blob/master/src/utils/systemd.rs, since modified

use core::{fmt, str};

use anyhow::{bail, Result};
use std::fs::File;
use std::io::{BufRead, BufReader};
use tracing::{info, warn};
use zbus::{proxy, zvariant, Connection};

// TODO: start representing more of these fields with enums instead of strings
#[derive(Debug, Clone)]
pub struct UnitWithStatus {
  pub name: String,              // The primary unit name as string
  pub scope: UnitScope,          // System or user?
  pub description: String,       // The human readable description string
  pub file_path: Option<String>, // The unit file path - populated later on demand

  pub load_state: String, // The load state (i.e. whether the unit file has been loaded successfully)

  // Some comments re: state from this helpful comment: https://www.reddit.com/r/linuxquestions/comments/r58dvz/comment/hmlemfk/
  /// One state, called the "activation state", essentially describes what the unit is doing now. The two most common values for this state are active and inactive, though there are a few other possibilities. (Each unit type has its own set of "substates" that map to these activation states. For instance, service units can be running or stopped. Again, there's a variety of other substates, and the list differs for each unit type.)
  pub activation_state: ActivationState,
  /// The sub state (a more fine-grained version of the active state that is specific to the unit type, which the active state is not)
  pub sub_state: String,

  /// The other state all units have is called the "enablement state". It describes how the unit might be automatically started in the future. A unit is enabled if it has been added to the requirements list of any other unit though symlinks in the filesystem. The set of symlinks to be created when enabling a unit is described by the unit's [Install] section. A unit is disabled if no symlinks are present. Again there's a variety of other values other than these two (e.g. not all units even have [Install] sections).
  /// Only populated when needed b/c this is much slower to get
  pub enablement_state: Option<String>,
  // We don't use any of these right now, might as well skip'em so there's less data to clone
  // pub followed: String, // A unit that is being followed in its state by this unit, if there is any, otherwise the empty string.
  // pub path: String,     // The unit object path
  // pub job_id: u32,      // If there is a job queued for the job unit the numeric job id, 0 otherwise
  // pub job_type: String, // The job type as string
  // pub job_path: String, // The job object path
}

#[derive(Debug, Clone)]
pub enum ActivationState {
  Active,
  Inactive,
  Failed,
  Unknown,
  Other(String),
}

impl From<String> for ActivationState {
  fn from(s: String) -> Self {
    match s.as_str() {
      "active" => ActivationState::Active,
      "inactive" => ActivationState::Inactive,
      "failed" => ActivationState::Failed,
      "unknown" => ActivationState::Unknown,
      _ => ActivationState::Other(s),
    }
  }
}

impl fmt::Display for ActivationState {
  fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
    match self {
      ActivationState::Active => write!(f, "active"),
      ActivationState::Inactive => write!(f, "inactive"),
      ActivationState::Failed => write!(f, "failed"),
      ActivationState::Unknown => write!(f, "unknown"),
      ActivationState::Other(s) => write!(f, "{}", s),
    }
  }
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
    matches!(self.activation_state, ActivationState::Active)
  }

  pub fn is_failed(&self) -> bool {
    matches!(self.activation_state, ActivationState::Failed)
  }

  pub fn is_not_found(&self) -> bool {
    self.load_state == "not-found"
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
    self.activation_state = other.activation_state;
    self.sub_state = other.sub_state;
  }
}

type RawUnit =
  (String, String, String, String, String, String, zvariant::OwnedObjectPath, u32, String, zvariant::OwnedObjectPath);

fn to_unit_status(raw_unit: RawUnit, scope: UnitScope) -> UnitWithStatus {
  let (name, description, load_state, activation_state, sub_state, _followed, _path, _job_id, _job_type, _job_path) =
    raw_unit;

  UnitWithStatus {
    name,
    scope,
    description,
    file_path: None,
    enablement_state: None,
    load_state,
    activation_state: activation_state.into(),
    sub_state,
  }
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
pub async fn get_services_from_list_units(scope: Scope, services: &[String]) -> Result<Vec<UnitWithStatus>> {
  let start = std::time::Instant::now();

  let mut units = vec![];

  let is_root = nix::unistd::geteuid().is_root();

  match scope {
    Scope::Global => {
      let system_units = get_services(UnitScope::Global, services).await?;
      units.extend(system_units);
    },
    Scope::User => {
      let user_units = get_services(UnitScope::User, services).await?;
      units.extend(user_units);
    },
    Scope::All => {
      let (system_units, user_units) =
        tokio::join!(get_services(UnitScope::Global, services), get_services(UnitScope::User, services));
      units.extend(system_units?);

      // Should always be able to get user units, but it may fail when running as root
      if let Ok(user_units) = user_units {
        units.extend(user_units);
      } else if is_root {
        warn!("Failed to get user units, ignoring because we're running as root and that's kinda expected");
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

async fn get_services(scope: UnitScope, services: &[String]) -> Result<Vec<UnitWithStatus>, anyhow::Error> {
  let connection = get_connection(scope).await?;
  let manager_proxy = ManagerProxy::new(&connection).await?;
  let units = manager_proxy.list_units_by_patterns(vec![], services.to_vec()).await?;
  let units: Vec<_> = units.into_iter().map(|u| to_unit_status(u, scope)).collect();
  Ok(units)
}

pub struct UnitFile {
  pub name: String,
  pub scope: UnitScope,
  pub enablement_state: String,
  pub path: String,
}

impl UnitFile {
  pub fn id(&self) -> UnitId {
    UnitId { name: self.name.clone(), scope: self.scope }
  }
}

/// Uses ListUnitFiles to get info for all services, including disabled ones
/// The tradeoff is that this is slow, we don't get as much info as from ListUnits,
/// and this returns a ton of static/masked/generated services that are not super interesting (at least to me)
pub async fn get_unit_files(scope: Scope) -> Result<Vec<UnitFile>> {
  let start = std::time::Instant::now();
  let mut unit_scopes = vec![];
  match scope {
    Scope::Global => unit_scopes.push(UnitScope::Global),
    Scope::User => unit_scopes.push(UnitScope::User),
    Scope::All => {
      unit_scopes.push(UnitScope::Global);
      unit_scopes.push(UnitScope::User);
    },
  }

  let mut ret = vec![];

  for unit_scope in unit_scopes {
    let unit_files = match get_service_unit_files(unit_scope).await {
      Ok(unit_files) => unit_files,
      Err(e) => {
        if nix::unistd::geteuid().is_root() {
          warn!("Failed to get user units, ignoring because we're running as root and that's kinda expected");
          vec![]
        } else {
          return Err(e);
        }
      },
    };

    let services = unit_files
      .iter()
      .map(|(path, state)| {
        // get the service name, e.g. foo.bar.baz.service from /somewhere/foo.bar.baz.service
        let rust_path = std::path::Path::new(path);
        let file_stem = rust_path.file_name().unwrap_or_default().to_str().unwrap_or_default();
        (file_stem.to_string(), state.to_string(), path.to_string())
      })
      .map(|(name, state, path)| UnitFile { name, scope: unit_scope, enablement_state: state, path })
      .collect::<Vec<_>>();
    ret.extend(services);
  }

  info!("Loaded {} unit files in {:?}", ret.len(), start.elapsed());

  Ok(ret)
}

/// Get unit files for all services, INCLUDING DISABLED ONES (the normal systemd APIs don't include those, which is annoying)
/// This is slow. Takes about 100ms (user) and 300ms (global) on 13th gen Intel i7
/// Returns a list of (path, state)s
pub async fn get_service_unit_files(scope: UnitScope) -> Result<Vec<(String, String)>> {
  let connection = get_connection(scope).await?;
  let manager_proxy = ManagerProxy::new(&connection).await?;
  let unit_files = manager_proxy.list_unit_files_by_patterns(vec![], vec!["*.service".into()]).await?;
  Ok(unit_files)
}

pub async fn start_service(service: UnitId) -> Result<()> {
  let connection = get_connection(service.scope).await?;
  let manager_proxy = ManagerProxy::new(&connection).await?;
  manager_proxy.start_unit(service.name.clone(), "replace".into()).await?;
  Ok(())
}

pub async fn stop_service(service: UnitId) -> Result<()> {
  let connection = get_connection(service.scope).await?;
  let manager_proxy = ManagerProxy::new(&connection).await?;
  manager_proxy.stop_unit(service.name, "replace".into()).await?;
  Ok(())
}

async fn get_connection(scope: UnitScope) -> Result<Connection, anyhow::Error> {
  match scope {
    UnitScope::Global => Ok(Connection::system().await?),
    UnitScope::User => Ok(Connection::session().await?),
  }
}

pub async fn restart_service(service: UnitId) -> Result<()> {
  let connection = get_connection(service.scope).await?;
  let manager_proxy = ManagerProxy::new(&connection).await?;
  manager_proxy.restart_unit(service.name, "replace".into()).await?;
  Ok(())
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

  /// [📖](https://www.freedesktop.org/software/systemd/man/latest/systemd.directives.html#ListUnitFiles()) Call interface method `ListUnitFiles`.
  fn list_unit_files(&self) -> zbus::Result<Vec<(String, String)>>;

  /// [📖](https://www.freedesktop.org/software/systemd/man/latest/systemd.directives.html#ListUnitFilesByPatterns()) Call interface method `ListUnitFilesByPatterns`.
  fn list_unit_files_by_patterns(
    &self,
    states: Vec<String>,
    patterns: Vec<String>,
  ) -> zbus::Result<Vec<(String, String)>>;

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

pub fn get_description_from_unit_file(path: &str) -> Result<String> {
  let file = File::open(path)?;
  let reader = BufReader::new(file);

  for line in reader.lines() {
    let line = line?;
    if line.trim().starts_with("Description=") {
      return Ok(line.trim_start_matches("Description=").trim().to_string());
    }
  }

  bail!("Description not found in unit file")
}
