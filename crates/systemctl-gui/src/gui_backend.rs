//! Backend operations shared by graphical frontends.
//!
//! This module deliberately has no GTK dependencies. Slow command-based operations run on
//! Tokio's blocking pool so callers can use them without freezing their UI thread.

use std::{collections::HashMap, process::Stdio};

use anyhow::{anyhow, Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio_util::sync::CancellationToken;

use systemctl_ui_core::{
  ssh,
  systemd::{self, LogDiagnostic, Scope, ServiceList, UnitFile, UnitId, UnitRuntimeInfo, UnitWithStatus},
};

const MAX_UNIT_DEFINITION_BYTES: usize = 4 * 1024 * 1024;

/// Load both systemd's runtime unit list and its unit-file list.
///
/// `ListUnits` only contains loaded units and does not include enablement state. Combining it
/// with `ListUnitFilesByPatterns` produces the complete inventory expected by a service manager,
/// including disabled, static, and masked services.
pub async fn load_service_inventory(scope: Scope, patterns: &[String]) -> Result<ServiceList> {
  let (services, unit_files) =
    tokio::try_join!(systemd::get_all_services(scope, patterns), systemd::get_unit_files(scope, patterns),)?;
  Ok(merge_service_inventory(services, unit_files))
}

/// Merge unit-file metadata into the runtime unit list.
///
/// Generated units are transient implementation details and aliases duplicate their canonical
/// units, so neither is added when it is absent from the runtime list.
pub fn merge_service_inventory(mut services: ServiceList, unit_files: Vec<UnitFile>) -> ServiceList {
  let mut positions: HashMap<UnitId, usize> =
    services.units.iter().enumerate().map(|(position, unit)| (unit.id(), position)).collect();

  for unit_file in unit_files {
    let id = unit_file.id();
    if let Some(position) = positions.get(&id).copied() {
      let unit = &mut services.units[position];
      unit.enablement_state = Some(unit_file.enablement_state);
      unit.file_path = Some(Ok(unit_file.path));
    } else if unit_file.enablement_state != "generated" && unit_file.enablement_state != "alias" {
      let unit = UnitWithStatus {
        name: unit_file.name,
        scope: unit_file.scope,
        description: String::new(),
        file_path: Some(Ok(unit_file.path)),
        load_state: "not-loaded".into(),
        activation_state: "inactive".into(),
        sub_state: "dead".into(),
        enablement_state: Some(unit_file.enablement_state),
      };
      positions.insert(id, services.units.len());
      services.units.push(unit);
    }
  }

  services.units.sort_by_key(|unit| unit.name.to_lowercase());
  services
}

/// Fetch runtime properties for a unit without blocking an async UI backend.
pub async fn load_unit_details(unit: UnitId) -> Result<UnitRuntimeInfo> {
  tokio::task::spawn_blocking(move || systemd::get_unit_runtime_info(&unit))
    .await
    .context("unit details worker panicked")?
}

/// Load the effective unit definition as rendered by `systemctl cat`.
///
/// Unlike reading `FragmentPath` directly, this includes drop-ins in systemd's application order
/// and also works for generated and transient units. The command is routed through the existing
/// SSH transport when the application is connected to a remote host.
pub async fn load_unit_definition(unit: UnitId) -> Result<String> {
  tokio::task::spawn_blocking(move || load_unit_definition_blocking(&unit))
    .await
    .context("unit definition worker panicked")?
}

fn unit_definition_args(unit: &UnitId) -> Vec<&str> {
  let mut args = vec!["--no-pager", "cat", unit.name.as_str()];
  if unit.scope == systemd::UnitScope::User {
    args.push("--user");
  }
  args
}

fn load_unit_definition_blocking(unit: &UnitId) -> Result<String> {
  let output = ssh::host_command("systemctl", &unit_definition_args(unit))
    .output()
    .with_context(|| format!("failed to run systemctl cat for {}", unit.name))?;
  parse_unit_definition_output(unit, output.status.success(), &output.stdout, &output.stderr)
}

fn parse_unit_definition_output(unit: &UnitId, success: bool, stdout: &[u8], stderr: &[u8]) -> Result<String> {
  if !success {
    let diagnostic = String::from_utf8_lossy(stderr);
    let diagnostic = diagnostic.trim();
    let diagnostic = if diagnostic.is_empty() { "systemctl cat exited unsuccessfully" } else { diagnostic };
    return Err(anyhow!("could not load {}: {diagnostic}", unit.name));
  }
  if stdout.len() > MAX_UNIT_DEFINITION_BYTES {
    return Err(anyhow!(
      "unit definition for {} is unreasonably large ({} bytes; limit is {} bytes)",
      unit.name,
      stdout.len(),
      MAX_UNIT_DEFINITION_BYTES
    ));
  }

  let definition = std::str::from_utf8(stdout)
    .with_context(|| format!("unit definition for {} is not valid UTF-8", unit.name))?
    .to_owned();
  if definition.trim().is_empty() {
    return Err(anyhow!("systemctl returned an empty definition for {}", unit.name));
  }
  Ok(definition)
}

/// Fetch recent journal entries for a unit without blocking an async UI backend.
///
/// Missing logs and journal permission failures are returned as a human-readable diagnostic line,
/// matching the TUI's behaviour. Process-launch and output-decoding failures remain errors.
pub async fn load_recent_logs(unit: UnitId, line_count: usize) -> Result<Vec<String>> {
  tokio::task::spawn_blocking(move || load_recent_logs_blocking(&unit, line_count))
    .await
    .context("journal worker panicked")?
}

/// Follow new journal entries for a unit until `cancel` is triggered.
///
/// The callback runs once for each complete line emitted by `journalctl`. Dropping the returned
/// future also kills the child process, so replacing a selected unit cannot leave a follower
/// behind. Cancellation is a normal shutdown and returns `Ok(())`.
pub async fn follow_unit_logs<F>(unit: UnitId, cancel: CancellationToken, mut on_line: F) -> Result<()>
where
  F: FnMut(String) -> Result<()> + Send,
{
  let args = journal_follow_args(&unit);
  let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
  let mut command = ssh::host_tokio_command("journalctl", &arg_refs);
  command.stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);

  let mut child = command.spawn().with_context(|| format!("failed to start journalctl for {}", unit.name))?;
  let stdout = child.stdout.take().context("journalctl stdout was not captured")?;
  let mut stderr = child.stderr.take().context("journalctl stderr was not captured")?;
  let stderr_task = tokio::spawn(async move {
    let mut output = String::new();
    stderr.read_to_string(&mut output).await.context("failed to read journalctl stderr")?;
    Ok::<_, anyhow::Error>(output)
  });
  let mut lines = BufReader::new(stdout).lines();

  loop {
    tokio::select! {
      _ = cancel.cancelled() => {
        // `kill` can fail if journalctl exited between the select and this call. `wait` below is
        // still authoritative, and cancellation itself should never become a UI error.
        let _ = child.kill().await;
        let _ = child.wait().await;
        stderr_task.await.context("journalctl stderr reader panicked")??;
        return Ok(());
      }
      line = lines.next_line() => match line.context("failed to read journalctl output")? {
        Some(line) => on_line(line).context("journal line callback failed")?,
        None => break,
      },
    }
  }

  let status = child.wait().await.context("failed waiting for journalctl")?;
  let stderr = stderr_task.await.context("journalctl stderr reader panicked")??;
  if !status.success() {
    let message = stderr.trim();
    let message = if message.is_empty() {
      format!("journalctl exited with {status}")
    } else {
      systemd::parse_journalctl_error(message).message()
    };
    return Err(anyhow!(message)).with_context(|| format!("failed to follow logs for {}", unit.name));
  }

  Ok(())
}

fn journal_follow_args(unit: &UnitId) -> Vec<String> {
  let mut args = vec![
    "--follow".into(),
    "--lines=0".into(),
    "--quiet".into(),
    "--output=short-iso".into(),
    "-u".into(),
    unit.name.clone(),
  ];
  if unit.scope == systemd::UnitScope::User {
    args.push("--user".into());
  }
  args
}

fn load_recent_logs_blocking(unit: &UnitId, line_count: usize) -> Result<Vec<String>> {
  let line_count = line_count.to_string();
  let mut args = vec!["--quiet", "--output=short-iso", "--lines", line_count.as_str(), "-u", unit.name.as_str()];
  if unit.scope == systemd::UnitScope::User {
    args.push("--user");
  }

  let output = ssh::host_command("journalctl", &args).output()?;
  if !output.status.success() {
    let stderr = String::from_utf8_lossy(&output.stderr);
    return Ok(vec![systemd::parse_journalctl_error(&stderr).message()]);
  }

  let stdout = std::str::from_utf8(&output.stdout)?;
  let lines: Vec<String> = stdout.lines().map(String::from).collect();
  if lines.is_empty() {
    let diagnostic: LogDiagnostic = systemd::diagnose_missing_logs(unit);
    Ok(vec![diagnostic.message()])
  } else {
    Ok(lines)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use systemctl_ui_core::systemd::UnitScope;

  fn runtime_unit(name: &str) -> UnitWithStatus {
    UnitWithStatus {
      name: name.into(),
      scope: UnitScope::Global,
      description: "A loaded service".into(),
      file_path: None,
      load_state: "loaded".into(),
      activation_state: "active".into(),
      sub_state: "running".into(),
      enablement_state: None,
    }
  }

  fn unit_file(name: &str, state: &str) -> UnitFile {
    UnitFile {
      name: name.into(),
      scope: UnitScope::Global,
      enablement_state: state.into(),
      path: format!("/usr/lib/systemd/system/{name}"),
    }
  }

  #[test]
  fn merge_enriches_loaded_units() {
    let services =
      ServiceList { units: vec![runtime_unit("loaded.service")], refreshed_scopes: vec![UnitScope::Global] };
    let merged = merge_service_inventory(services, vec![unit_file("loaded.service", "enabled")]);

    assert_eq!(merged.units.len(), 1);
    assert_eq!(merged.units[0].enablement_state.as_deref(), Some("enabled"));
    assert_eq!(merged.units[0].file_path.as_ref().unwrap().as_deref(), Ok("/usr/lib/systemd/system/loaded.service"));
    assert_eq!(merged.units[0].activation_state, "active");
  }

  #[test]
  fn merge_adds_unloaded_manageable_units() {
    let services = ServiceList { units: vec![], refreshed_scopes: vec![UnitScope::Global] };
    let merged = merge_service_inventory(services, vec![unit_file("disabled.service", "disabled")]);

    assert_eq!(merged.units.len(), 1);
    let unit = &merged.units[0];
    assert_eq!(unit.name, "disabled.service");
    assert_eq!(unit.load_state, "not-loaded");
    assert_eq!(unit.activation_state, "inactive");
    assert_eq!(unit.sub_state, "dead");
  }

  #[test]
  fn merge_does_not_add_generated_or_alias_units() {
    let services = ServiceList { units: vec![], refreshed_scopes: vec![UnitScope::Global] };
    let merged = merge_service_inventory(
      services,
      vec![unit_file("generated.service", "generated"), unit_file("alias.service", "alias")],
    );

    assert!(merged.units.is_empty());
  }

  #[test]
  fn merge_keys_units_by_name_and_scope() {
    let mut user = runtime_unit("same.service");
    user.scope = UnitScope::User;
    let services = ServiceList {
      units: vec![runtime_unit("same.service"), user],
      refreshed_scopes: vec![UnitScope::Global, UnitScope::User],
    };
    let mut user_file = unit_file("same.service", "disabled");
    user_file.scope = UnitScope::User;
    let merged = merge_service_inventory(services, vec![user_file]);

    assert_eq!(merged.units.len(), 2);
    assert_eq!(merged.units.iter().find(|unit| unit.scope == UnitScope::Global).unwrap().enablement_state, None);
    assert_eq!(
      merged.units.iter().find(|unit| unit.scope == UnitScope::User).unwrap().enablement_state.as_deref(),
      Some("disabled")
    );
  }

  #[test]
  fn journal_follow_arguments_select_unit_and_output_format() {
    let args = journal_follow_args(&UnitId { name: "demo.service".into(), scope: UnitScope::Global });

    assert_eq!(args, ["--follow", "--lines=0", "--quiet", "--output=short-iso", "-u", "demo.service"]);
  }

  #[test]
  fn journal_follow_arguments_support_user_scope() {
    let args = journal_follow_args(&UnitId { name: "demo.service".into(), scope: UnitScope::User });

    assert_eq!(args.last().map(String::as_str), Some("--user"));
  }

  #[test]
  fn unit_definition_arguments_select_unit() {
    let unit = UnitId { name: "demo.service".into(), scope: UnitScope::Global };

    assert_eq!(unit_definition_args(&unit), ["--no-pager", "cat", "demo.service"]);
  }

  #[test]
  fn unit_definition_arguments_support_user_scope() {
    let unit = UnitId { name: "demo.service".into(), scope: UnitScope::User };

    assert_eq!(unit_definition_args(&unit), ["--no-pager", "cat", "demo.service", "--user"]);
  }

  #[test]
  fn unit_definition_parser_preserves_effective_definition() {
    let unit = UnitId { name: "demo.service".into(), scope: UnitScope::Global };
    let output = b"# /usr/lib/systemd/system/demo.service\n[Service]\nExecStart=/usr/bin/demo\n\n# /etc/systemd/system/demo.service.d/local.conf\n[Service]\nRestart=always\n";

    assert_eq!(parse_unit_definition_output(&unit, true, output, b"").unwrap().as_bytes(), output);
  }

  #[test]
  fn unit_definition_parser_reports_systemctl_diagnostic() {
    let unit = UnitId { name: "missing.service".into(), scope: UnitScope::Global };

    let error = parse_unit_definition_output(&unit, false, b"", b"No files found for missing.service.\n").unwrap_err();

    assert_eq!(error.to_string(), "could not load missing.service: No files found for missing.service.");
  }

  #[test]
  fn unit_definition_parser_rejects_empty_and_oversized_output() {
    let unit = UnitId { name: "odd.service".into(), scope: UnitScope::Global };

    assert!(parse_unit_definition_output(&unit, true, b" \n", b"").unwrap_err().to_string().contains("empty"));
    let oversized = vec![b'x'; MAX_UNIT_DEFINITION_BYTES + 1];
    assert!(parse_unit_definition_output(&unit, true, &oversized, b"")
      .unwrap_err()
      .to_string()
      .contains("unreasonably large"));
  }
}
