// File initially taken from https://github.com/servicer-labs/servicer/blob/master/src/utils/systemd.rs, since modified

use core::str;

use anyhow::{bail, Context, Result};
use log::error;
use tokio_util::sync::CancellationToken;
use tracing::info;
use zbus::{proxy, zvariant, Connection};

use crate::ssh;

#[derive(Debug, Clone)]
pub struct UnitWithStatus {
  pub name: String,                              // The primary unit name as string
  pub scope: UnitScope,                          // System or user?
  pub description: String,                       // The human readable description string
  pub file_path: Option<Result<String, String>>, // The unit file path - populated later on demand

  pub load_state: String, // The load state (i.e. whether the unit file has been loaded successfully)

  // Some comments re: state from this helpful comment: https://www.reddit.com/r/linuxquestions/comments/r58dvz/comment/hmlemfk/
  /// One state, called the "activation state", essentially describes what the unit is doing now. The two most common values for this state are active and inactive, though there are a few other possibilities. (Each unit type has its own set of "substates" that map to these activation states. For instance, service units can be running or stopped. Again, there's a variety of other substates, and the list differs for each unit type.)
  pub activation_state: String,
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
    self.activation_state == "active"
  }

  pub fn is_failed(&self) -> bool {
    self.activation_state == "failed"
  }

  pub fn is_not_found(&self) -> bool {
    self.load_state == "not-found"
  }

  pub fn is_enabled(&self) -> bool {
    self.load_state == "loaded" && self.activation_state == "active"
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
  let (name, description, load_state, active_state, sub_state, _followed, _path, _job_id, _job_type, _job_path) =
    raw_unit;

  UnitWithStatus {
    name,
    scope,
    description,
    file_path: None,
    enablement_state: None,
    load_state,
    activation_state: active_state,
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

/// Represents a unit file from ListUnitFiles (includes disabled units not returned by ListUnits)
#[derive(Debug, Clone)]
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

/// Get unit files for all services, INCLUDING DISABLED ONES (ListUnits doesn't include those)
/// This is slower than get_all_services. Takes about 100ms (user) and 300ms (global) on 13th gen Intel i7
pub async fn get_unit_files(scope: Scope, services: &[String]) -> Result<Vec<UnitFile>> {
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
  let is_root = nix::unistd::geteuid().is_root();
  info!("get_unit_files: is_root={}, scope={:?}", is_root, scope);

  for unit_scope in unit_scopes {
    info!("get_unit_files: fetching {:?} unit files", unit_scope);
    let connection = match get_connection(unit_scope).await {
      Ok(conn) => conn,
      Err(e) => {
        error!("get_unit_files: failed to get {:?} connection: {:?}", unit_scope, e);
        if (is_root || ssh::remote_host().is_some()) && unit_scope == UnitScope::User {
          info!("get_unit_files: skipping user scope");
          continue;
        }
        return Err(e);
      },
    };
    let manager_proxy = ManagerProxy::new(&connection).await?;
    let unit_files =
      match manager_proxy.list_unit_files_by_patterns(vec![], services.iter().map(|s| s.to_string()).collect()).await {
        Ok(files) => {
          info!("get_unit_files: got {} {:?} unit files", files.len(), unit_scope);
          files
        },
        Err(e) => {
          error!("get_unit_files: list_unit_files_by_patterns failed for {:?}: {:?}", unit_scope, e);
          if (is_root || ssh::remote_host().is_some()) && unit_scope == UnitScope::User {
            info!("get_unit_files: ignoring user scope error");
            vec![]
          } else {
            return Err(e.into());
          }
        },
      };

    let services = unit_files
      .into_iter()
      .filter_map(|(path, state)| {
        let rust_path = std::path::Path::new(&path);
        let file_name = rust_path.file_name()?.to_str()?;
        Some(UnitFile { name: file_name.to_string(), scope: unit_scope, enablement_state: state, path })
      })
      .collect::<Vec<_>>();
    ret.extend(services);
  }

  info!("Loaded {} unit files in {:?}", ret.len(), start.elapsed());
  Ok(ret)
}

// this takes like 5-10 ms on 13th gen Intel i7 (scope=all)
/// The result of fetching units. `refreshed_scopes` lists the scopes that were fetched
/// successfully: a previously-seen unit that's absent from a refreshed scope is genuinely
/// no longer loaded (e.g. a disabled unit gets unloaded when it stops), as opposed to a
/// scope we couldn't query (e.g. user units when running as root).
#[derive(Debug, Clone)]
pub struct ServiceList {
  pub units: Vec<UnitWithStatus>,
  pub refreshed_scopes: Vec<UnitScope>,
}

pub async fn get_all_services(scope: Scope, services: &[String]) -> Result<ServiceList> {
  let start = std::time::Instant::now();

  let mut units = vec![];
  let mut refreshed_scopes = vec![];

  let is_root = nix::unistd::geteuid().is_root();

  match scope {
    Scope::Global => {
      let system_units = get_services(UnitScope::Global, services).await?;
      units.extend(system_units);
      refreshed_scopes.push(UnitScope::Global);
    },
    Scope::User => {
      let user_units = get_services(UnitScope::User, services).await?;
      units.extend(user_units);
      refreshed_scopes.push(UnitScope::User);
    },
    Scope::All => {
      let (system_units, user_units) =
        tokio::join!(get_services(UnitScope::Global, services), get_services(UnitScope::User, services));
      units.extend(system_units?);
      refreshed_scopes.push(UnitScope::Global);

      // Should always be able to get user units, but it may fail when running as root
      // or when the remote host has no running user manager
      match user_units {
        Ok(user_units) => {
          units.extend(user_units);
          refreshed_scopes.push(UnitScope::User);
        },
        Err(e) if is_root => error!("Failed to get user units, ignoring because we're running as root: {e:?}"),
        Err(e) if ssh::remote_host().is_some() => {
          error!(
            "Failed to get user units from remote host: {e:?}. \
             This requires an active session or lingering (loginctl enable-linger) on the remote host."
          )
        },
        Err(e) => return Err(e),
      }
    },
  }

  // sort by name case-insensitive
  units.sort_by_key(|a| a.name.to_lowercase());

  info!("Loaded systemd services in {:?}", start.elapsed());

  Ok(ServiceList { units, refreshed_scopes })
}

async fn get_services(scope: UnitScope, services: &[String]) -> Result<Vec<UnitWithStatus>, anyhow::Error> {
  let connection = get_connection(scope).await?;
  let manager_proxy = ManagerProxy::new(&connection).await?;
  let units = manager_proxy.list_units_by_patterns(vec![], services.to_vec()).await?;
  let units: Vec<_> = units.into_iter().map(|u| to_unit_status(u, scope)).collect();
  Ok(units)
}

/// Runtime properties for a single unit, fetched on demand when it's selected.
/// All fields are optional because they vary by unit type (services have PIDs,
/// timers have a next elapse, etc.) and by systemd version.
#[derive(Debug, Clone, Default)]
pub struct UnitRuntimeInfo {
  pub fragment_path: String,
  pub main_pid: Option<u32>,
  pub memory_current: Option<u64>,
  pub tasks_current: Option<u64>,
  pub n_restarts: Option<u32>,
  pub cpu_usage_nsec: Option<u64>,
  pub active_enter_timestamp: Option<String>,
  pub inactive_enter_timestamp: Option<String>,
  pub result: Option<String>,
  pub exec_main_status: Option<i32>,
  pub next_elapse: Option<String>,
}

/// Fetches runtime properties (including the unit file path) for a unit in a single
/// `systemctl show` invocation. Uses systemctl rather than D-Bus so it works in remote mode too.
/// Properties that don't apply to the unit type are simply omitted from the output.
pub fn get_unit_runtime_info(service: &UnitId) -> Result<UnitRuntimeInfo> {
  const PROPERTIES: &str = "FragmentPath,MainPID,MemoryCurrent,TasksCurrent,NRestarts,CPUUsageNSec,\
                            ActiveEnterTimestamp,InactiveEnterTimestamp,Result,ExecMainStatus,NextElapseUSecRealtime";
  let mut args = vec!["--quiet", "show", "-p", PROPERTIES];
  args.push(&service.name);

  if service.scope == UnitScope::User {
    args.insert(0, "--user");
  }

  let output = ssh::host_command("systemctl", &args).output()?;

  if !output.status.success() {
    let stderr = String::from_utf8(output.stderr)?;
    bail!(stderr);
  }

  fn non_empty(value: &str) -> Option<String> {
    match value {
      "" | "[not set]" | "n/a" => None,
      v => Some(v.to_string()),
    }
  }

  let mut info = UnitRuntimeInfo::default();
  for line in str::from_utf8(&output.stdout)?.lines() {
    let Some((key, value)) = line.split_once('=') else { continue };
    let value = value.trim();
    match key {
      "FragmentPath" => info.fragment_path = value.to_string(),
      "MainPID" => info.main_pid = value.parse().ok().filter(|&pid| pid != 0),
      // systemd prints u64::MAX (or "[not set]") for unavailable accounting values
      "MemoryCurrent" => info.memory_current = value.parse().ok().filter(|&v| v != u64::MAX),
      "TasksCurrent" => info.tasks_current = value.parse().ok().filter(|&v| v != u64::MAX),
      "NRestarts" => info.n_restarts = value.parse().ok(),
      "CPUUsageNSec" => info.cpu_usage_nsec = value.parse().ok().filter(|&v| v != u64::MAX),
      "ActiveEnterTimestamp" => info.active_enter_timestamp = non_empty(value),
      "InactiveEnterTimestamp" => info.inactive_enter_timestamp = non_empty(value),
      "Result" => info.result = non_empty(value),
      "ExecMainStatus" => info.exec_main_status = value.parse().ok(),
      "NextElapseUSecRealtime" => info.next_elapse = non_empty(value),
      _ => {},
    }
  }

  Ok(info)
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
        anyhow::bail!("cancelled");
    }
    result = stop_service(service) => {
        result
    }
  }
}

pub async fn reload(scope: UnitScope, cancel_token: CancellationToken) -> Result<()> {
  async fn reload_(scope: UnitScope) -> Result<()> {
    let connection = get_connection(scope).await?;
    let manager_proxy: ManagerProxy<'_> = ManagerProxy::new(&connection).await?;
    let error_message = match scope {
      UnitScope::Global => "Failed to reload units, probably because superuser permissions are needed. Try running `sudo systemctl daemon-reload`",
      UnitScope::User => "Failed to reload units. Try running `systemctl --user daemon-reload`",
    };
    manager_proxy.reload().await.context(error_message)?;
    Ok(())
  }

  // god these select macros are ugly, is there really no better way to select?
  tokio::select! {
    _ = cancel_token.cancelled() => {
        anyhow::bail!("cancelled");
    }
    result = reload_(scope) => {
        result
    }
  }
}

async fn get_connection(scope: UnitScope) -> Result<Connection, anyhow::Error> {
  // Cache connections; this matters a lot for remote hosts, where each new connection
  // spawns an ssh + systemd-stdio-bridge pair
  static GLOBAL_CONNECTION: tokio::sync::OnceCell<Connection> = tokio::sync::OnceCell::const_new();
  static USER_CONNECTION: tokio::sync::OnceCell<Connection> = tokio::sync::OnceCell::const_new();

  let cell = match scope {
    UnitScope::Global => &GLOBAL_CONNECTION,
    UnitScope::User => &USER_CONNECTION,
  };

  let connection = cell
    .get_or_try_init(|| async {
      match ssh::remote_host() {
        Some(ssh_host) => {
          let unixexec = zbus::address::transport::Unixexec::new(
            std::path::PathBuf::from("ssh"),
            Some(std::ffi::OsString::from("ssh")),
            ssh_host.bridge_ssh_args(scope),
          );
          let connection =
            zbus::connection::Builder::address(zbus::address::Transport::Unixexec(unixexec))?.build().await?;
          info!("Established remote D-Bus connection to {} for {:?} scope", ssh_host.host, scope);
          Ok::<Connection, anyhow::Error>(connection)
        },
        None => match scope {
          UnitScope::Global => Ok(Connection::system().await?),
          UnitScope::User => Ok(Connection::session().await?),
        },
      }
    })
    .await?;
  Ok(connection.clone())
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

pub async fn enable_service(service: UnitId, cancel_token: CancellationToken) -> Result<()> {
  async fn enable(service: UnitId) -> Result<()> {
    let connection = get_connection(service.scope).await?;
    let manager_proxy = ManagerProxy::new(&connection).await?;
    let files = vec![service.name];
    let (_, changes) = manager_proxy.enable_unit_files(files, false, false).await?;

    for (change_type, name, destination) in changes {
      info!("{}: {} -> {}", change_type, name, destination);
    }
    // Enabling without reloading puts things in a weird state where `systemctl status foo` tells you to run daemon-reload
    manager_proxy.reload().await?;
    Ok(())
  }

  tokio::select! {
    _ = cancel_token.cancelled() => {
        anyhow::bail!("cancelled");
    }
    result = enable(service) => {
        result
    }
  }
}

pub async fn disable_service(service: UnitId, cancel_token: CancellationToken) -> Result<()> {
  async fn disable(service: UnitId) -> Result<()> {
    let connection = get_connection(service.scope).await?;
    let manager_proxy = ManagerProxy::new(&connection).await?;
    let files = vec![service.name];
    let changes = manager_proxy.disable_unit_files(files, false).await?;

    for (change_type, name, destination) in changes {
      info!("{}: {} -> {}", change_type, name, destination);
    }
    manager_proxy.reload().await?;
    Ok(())
  }

  tokio::select! {
    _ = cancel_token.cancelled() => {
        anyhow::bail!("cancelled");
    }
    result = disable(service) => {
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

pub async fn kill_service(service: UnitId, signal: String, cancel_token: CancellationToken) -> Result<()> {
  async fn kill(service: UnitId, signal: String) -> Result<()> {
    let mut args = vec!["kill", "--signal", &signal];
    if service.scope == UnitScope::User {
      args.push("--user");
    }
    args.push(&service.name);

    let output = ssh::host_command("systemctl", &args).output()?;

    if output.status.success() {
      info!("Successfully sent signal {} to srvice {}", signal, service.name);
      Ok(())
    } else {
      let stderr = String::from_utf8(output.stderr)?;
      bail!("Failed to send signal {} to service {}: {}", signal, service.name, stderr);
    }
  }

  tokio::select! {
      _ = cancel_token.cancelled() => {
          bail!("cancelled");
      }
      result = kill(service, signal) => {
          result
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
  #[zbus(name = "StartUnit", allow_interactive_auth)]
  fn start_unit(&self, name: String, mode: String) -> zbus::Result<zvariant::OwnedObjectPath>;

  /// [📖](https://www.freedesktop.org/software/systemd/man/systemd.directives.html#StopUnit()) Call interface method `StopUnit`.
  #[zbus(name = "StopUnit", allow_interactive_auth)]
  fn stop_unit(&self, name: String, mode: String) -> zbus::Result<zvariant::OwnedObjectPath>;

  /// [📖](https://www.freedesktop.org/software/systemd/man/systemd.directives.html#ReloadUnit()) Call interface method `ReloadUnit`.
  #[zbus(name = "ReloadUnit", allow_interactive_auth)]
  fn reload_unit(&self, name: String, mode: String) -> zbus::Result<zvariant::OwnedObjectPath>;

  /// [📖](https://www.freedesktop.org/software/systemd/man/systemd.directives.html#RestartUnit()) Call interface method `RestartUnit`.
  #[zbus(name = "RestartUnit", allow_interactive_auth)]
  fn restart_unit(&self, name: String, mode: String) -> zbus::Result<zvariant::OwnedObjectPath>;

  /// [📖](https://www.freedesktop.org/software/systemd/man/systemd.directives.html#EnableUnitFiles()) Call interface method `EnableUnitFiles`.
  #[zbus(name = "EnableUnitFiles", allow_interactive_auth)]
  fn enable_unit_files(
    &self,
    files: Vec<String>,
    runtime: bool,
    force: bool,
  ) -> zbus::Result<(bool, Vec<(String, String, String)>)>;

  /// [📖](https://www.freedesktop.org/software/systemd/man/systemd.directives.html#DisableUnitFiles()) Call interface method `DisableUnitFiles`.
  #[zbus(name = "DisableUnitFiles", allow_interactive_auth)]
  fn disable_unit_files(&self, files: Vec<String>, runtime: bool) -> zbus::Result<Vec<(String, String, String)>>;

  /// [📖](https://www.freedesktop.org/software/systemd/man/systemd.directives.html#ListUnits()) Call interface method `ListUnits`.
  #[zbus(name = "ListUnits")]
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
  #[zbus(name = "ListUnitsByPatterns")]
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
  #[zbus(name = "Reload", allow_interactive_auth)]
  fn reload(&self) -> zbus::Result<()>;

  /// [📖](https://www.freedesktop.org/software/systemd/man/latest/systemd.directives.html#ListUnitFilesByPatterns()) Call interface method `ListUnitFilesByPatterns`.
  #[zbus(name = "ListUnitFilesByPatterns")]
  fn list_unit_files_by_patterns(
    &self,
    states: Vec<String>,
    patterns: Vec<String>,
  ) -> zbus::Result<Vec<(String, String)>>;
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
  #[zbus(property)]
  fn active_state(&self) -> zbus::Result<String>;

  /// Get property `LoadState`.
  #[zbus(property)]
  fn load_state(&self) -> zbus::Result<String>;

  /// Get property `UnitFileState`.
  #[zbus(property)]
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
  #[zbus(property, name = "MainPID")]
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

/// Diagnostic result explaining why logs might be missing
#[derive(Debug, Clone)]
pub enum LogDiagnostic {
  /// Unit has never been activated (ActiveEnterTimestamp is 0)
  NeverRun { unit_name: String },
  /// Journal is not accessible (likely permissions)
  JournalInaccessible { error: String },
  /// Unit-specific permission issue
  PermissionDenied { error: String },
  /// Journal is available but no logs exist for this unit
  NoLogsRecorded { unit_name: String },
  /// The user can only see their own journal entries, not system-wide ones
  LimitedJournalAccess,
  /// journalctl command failed with an error
  JournalctlError { stderr: String },
}

impl LogDiagnostic {
  /// Returns a human-readable message for display
  pub fn message(&self) -> String {
    match self {
      Self::NeverRun { unit_name } => format!("No logs: {} has never been started", unit_name),
      Self::JournalInaccessible { error } => {
        format!("Cannot access journal: {}\n\nCheck that systemd-journald is running", error)
      },
      Self::PermissionDenied { error } => format!("Permission denied: {}\n\nTry: sudo systemctl-tui", error),
      Self::NoLogsRecorded { unit_name } => {
        format!("No logs recorded for {} (unit has run but produced no journal output)", unit_name)
      },
      Self::LimitedJournalAccess => {
        let place = match crate::ssh::remote_host() {
          Some(ssh_host) => format!(" on {}", ssh_host.host),
          None => String::new(),
        };
        format!(
          "No access to system logs: this user can only see its own journal entries.\n\nTo fix, run{place}: sudo usermod -aG systemd-journal $USER\n(takes effect on next login)"
        )
      },
      Self::JournalctlError { stderr } => format!("journalctl error: {}", stderr),
    }
  }
}

/// Check if a unit has ever been activated using systemctl show
pub fn check_unit_has_run(unit: &UnitId) -> bool {
  let mut args = vec!["show", "-P", "ActiveEnterTimestampMonotonic"];
  if unit.scope == UnitScope::User {
    args.insert(0, "--user");
  }
  args.push(&unit.name);

  ssh::host_command("systemctl", &args)
    .output()
    .ok()
    .and_then(
      |output| if output.status.success() { std::str::from_utf8(&output.stdout).ok().map(String::from) } else { None },
    )
    .map(|s| s.trim().parse::<u64>().unwrap_or(0) > 0)
    .unwrap_or(false)
}

/// Check whether the journal is readable and we can see system-wide entries.
/// Returns a diagnostic if something is wrong. Deliberately not using --quiet:
/// journalctl succeeds with empty output when the user can only see their own
/// entries, and the warning on stderr is the only signal.
/// Cached per scope: access won't change mid-session (group changes need a new
/// login), and on remote hosts each check costs an SSH round trip.
fn check_journal_access(scope: UnitScope) -> Option<LogDiagnostic> {
  static GLOBAL_ACCESS: std::sync::OnceLock<Option<LogDiagnostic>> = std::sync::OnceLock::new();
  static USER_ACCESS: std::sync::OnceLock<Option<LogDiagnostic>> = std::sync::OnceLock::new();

  let cell = match scope {
    UnitScope::Global => &GLOBAL_ACCESS,
    UnitScope::User => &USER_ACCESS,
  };
  cell.get_or_init(|| check_journal_access_uncached(scope)).clone()
}

fn check_journal_access_uncached(scope: UnitScope) -> Option<LogDiagnostic> {
  let mut args = vec!["--lines=1"];
  if scope == UnitScope::User {
    args.push("--user");
  }

  match ssh::host_command("journalctl", &args).output() {
    Ok(output) => {
      let stderr = String::from_utf8_lossy(&output.stderr);
      if scope == UnitScope::Global && journalctl_warns_about_limited_access(&stderr) {
        return Some(LogDiagnostic::LimitedJournalAccess);
      }
      if output.status.success() {
        None
      } else {
        Some(parse_journalctl_error(stderr.trim()))
      }
    },
    Err(e) => Some(LogDiagnostic::JournalInaccessible { error: e.to_string() }),
  }
}

/// Does this journalctl stderr contain the warning about only seeing your own messages?
pub fn journalctl_warns_about_limited_access(stderr: &str) -> bool {
  stderr.contains("Users in groups") || stderr.contains("not seeing messages from other users")
}

/// Parse journalctl stderr to determine the specific error type
pub fn parse_journalctl_error(stderr: &str) -> LogDiagnostic {
  let stderr_lower = stderr.to_lowercase();

  if stderr_lower.contains("insufficient permissions") {
    // older systemd (e.g. 239 on EL8): journalctl exits nonzero with "No journal files
    // were opened due to insufficient permissions" instead of succeeding with a warning
    LogDiagnostic::LimitedJournalAccess
  } else if stderr_lower.contains("permission denied") || stderr_lower.contains("access denied") {
    LogDiagnostic::PermissionDenied { error: stderr.trim().to_string() }
  } else if stderr_lower.contains("no such file") || stderr_lower.contains("failed to open") {
    LogDiagnostic::JournalInaccessible { error: stderr.trim().to_string() }
  } else {
    LogDiagnostic::JournalctlError { stderr: stderr.trim().to_string() }
  }
}

/// Diagnose why logs are missing for a unit
pub fn diagnose_missing_logs(unit: &UnitId) -> LogDiagnostic {
  // Check 1: Can we read the journal at all? Do this first: without access,
  // every unit looks like it has no logs and other checks are misleading
  if let Some(diagnostic) = check_journal_access(unit.scope) {
    return diagnostic;
  }

  // Check 2: Has the unit ever run?
  if !check_unit_has_run(unit) {
    return LogDiagnostic::NeverRun { unit_name: unit.name.clone() };
  }

  // If we get here, journal is accessible but no logs for this specific unit
  LogDiagnostic::NoLogsRecorded { unit_name: unit.name.clone() }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_get_unit_path() {
    assert_eq!(get_unit_path("test.service"), "/org/freedesktop/systemd1/unit/test_2eservice");
  }

  #[test]
  fn test_encode_as_dbus_object_path() {
    assert_eq!(encode_as_dbus_object_path("test.service"), "test_2eservice");
    assert_eq!(encode_as_dbus_object_path("test-with-hyphen.service"), "test_2dwith_2dhyphen_2eservice");
  }

  #[test]
  fn test_parse_journalctl_error_permission() {
    let diagnostic = parse_journalctl_error("Failed to get journal access: Permission denied");
    assert!(matches!(diagnostic, LogDiagnostic::PermissionDenied { .. }));
  }

  #[test]
  fn test_parse_journalctl_error_no_file() {
    let diagnostic = parse_journalctl_error("No such file or directory");
    assert!(matches!(diagnostic, LogDiagnostic::JournalInaccessible { .. }));
  }

  #[test]
  fn test_limited_access_warning_detection() {
    // real journalctl output from a user not in adm/systemd-journal (systemd 249)
    let stderr = "Hint: You are currently not seeing messages from other users and the system.\n      \
                  Users in groups 'adm', 'systemd-journal' can see all messages.\n      \
                  Pass -q to turn off this notice.";
    assert!(journalctl_warns_about_limited_access(stderr));
    assert!(!journalctl_warns_about_limited_access("-- No entries --"));
    assert!(!journalctl_warns_about_limited_access(""));
  }

  #[test]
  fn test_parse_journalctl_error_old_systemd_insufficient_permissions() {
    // real journalctl output from a user in no journal groups on systemd 239 (EL8),
    // which exits nonzero instead of succeeding with a hint like newer versions
    let diagnostic = parse_journalctl_error("No journal files were opened due to insufficient permissions.");
    assert!(matches!(diagnostic, LogDiagnostic::LimitedJournalAccess));
  }

  #[test]
  fn test_parse_journalctl_error_generic() {
    let diagnostic = parse_journalctl_error("Something unexpected happened");
    assert!(matches!(diagnostic, LogDiagnostic::JournalctlError { .. }));
  }
}
