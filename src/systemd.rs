// File initially taken from https://github.com/servicer-labs/servicer/blob/master/src/utils/systemd.rs, since modified

use anyhow::{bail, Context, Result};
use core::str;
use log::error;
use std::process::Command;
use tokio_util::sync::CancellationToken;
use tracing::info;
use zbus::{zvariant, Connection};
use zbus_systemd::systemd1::{ManagerProxy, ServiceProxy, UnitProxy};

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

// this takes like 5-10 ms on 13th gen Intel i7 (scope=all)
pub async fn get_all_services(scope: Scope, services: &[String]) -> Result<Vec<UnitWithStatus>> {
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

async fn get_services(scope: UnitScope, services: &[String]) -> Result<Vec<UnitWithStatus>, anyhow::Error> {
  let connection = get_connection(scope).await?;
  let manager_proxy = ManagerProxy::new(&connection).await?;
  let units = manager_proxy.list_units_by_patterns(vec![], services.to_vec()).await?;

  Ok(units.into_iter().map(|u| to_unit_status(u, scope)).collect())
}

pub fn get_unit_file_location(service: &UnitId) -> Result<String> {
  // show -P FragmentPath reitunes.service
  let mut args = vec!["--quiet", "show", "-P", "FragmentPath"];
  args.push(&service.name);

  if service.scope == UnitScope::User {
    args.insert(0, "--user");
  }

  let output = Command::new("systemctl").args(&args).output()?;

  if output.status.success() {
    let path = str::from_utf8(&output.stdout)?.trim();
    if path.is_empty() {
      bail!("No unit file found for {}", service.name);
    }
    Ok(path.trim().to_string())
  } else {
    let stderr = String::from_utf8(output.stderr)?;
    bail!(stderr);
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
}
