//! Support for managing systemd on remote hosts over SSH (the `--host` flag).
//!
//! We establish a multiplexed SSH master connection at startup (before entering the TUI, so
//! interactive auth prompts work), then every subsequent SSH invocation — D-Bus bridges and
//! journalctl/systemctl calls — reuses it via ControlPath. That makes each call a cheap channel
//! open instead of a full handshake.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;

use anyhow::{bail, Context, Result};

use crate::systemd::UnitScope;

static REMOTE_HOST: OnceLock<SshHost> = OnceLock::new();

#[derive(Debug)]
pub struct SshHost {
  pub host: String,
  control_path: PathBuf,
}

/// The remote host we're connected to, if any. Set once at startup via [`init`].
pub fn remote_host() -> Option<&'static SshHost> {
  REMOTE_HOST.get()
}

/// Establish a multiplexed SSH master connection to `host` (e.g. `user@hostname`).
/// Must be called before entering the alternate screen so password/2FA prompts work.
pub fn init(host: String) -> Result<()> {
  let runtime_dir = std::env::var("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(|_| std::env::temp_dir());
  // %C expands to a hash of local host, remote host, port, and user
  let control_path = runtime_dir.join("systemctl-tui-ssh-%C");

  let status = Command::new("ssh")
    .args(["-o", "ControlMaster=auto", "-o"])
    .arg(format!("ControlPath={}", control_path.display()))
    .args(["-o", "ControlPersist=60", "-N", "-f", "--", &host])
    .status()
    .context("Failed to run ssh. Is OpenSSH installed?")?;

  if !status.success() {
    bail!("Failed to connect to {host} over SSH");
  }

  REMOTE_HOST.set(SshHost { host, control_path }).expect("ssh::init called twice");
  Ok(())
}

/// Close the master connection. Safe to skip (ControlPersist expires it), but tidy.
pub fn teardown() {
  if let Some(ssh_host) = remote_host() {
    let _ = Command::new("ssh").args(ssh_host.mux_options()).args(["-O", "exit", "--", &ssh_host.host]).output();
  }
}

impl SshHost {
  fn mux_options(&self) -> [String; 4] {
    ["-o".into(), format!("ControlPath={}", self.control_path.display()), "-o".into(), "BatchMode=yes".into()]
  }

  /// Arguments for the `unixexec:` D-Bus transport: run systemd-stdio-bridge on the remote host.
  pub fn bridge_ssh_args(&self, scope: UnitScope) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec!["-xT".into()];
    args.extend(self.mux_options().map(OsString::from));
    args.push("--".into());
    args.push(OsString::from(&self.host));
    match scope {
      UnitScope::Global => {
        args.push("systemd-stdio-bridge".into());
        args.push("--system".into());
      },
      UnitScope::User => {
        // Non-interactive SSH sessions have no XDG_RUNTIME_DIR, so the bridge can't find the
        // user bus without help. Requires a running user manager (active session or lingering).
        // The remote shell word-splits ssh's command string, so the -c payload must be quoted;
        // $(id -u) is expanded by the remote sh, not locally.
        args.push("sh".into());
        args.push("-c".into());
        args.push("'XDG_RUNTIME_DIR=/run/user/$(id -u) exec systemd-stdio-bridge --user'".into());
      },
    }
    args
  }

  /// Build a `std::process::Command` that runs `program args...` on the remote host.
  pub fn command(&self, program: &str, args: &[&str]) -> Command {
    let mut cmd = Command::new("ssh");
    self.add_remote_invocation(&mut cmd, program, args);
    cmd
  }

  /// Like [`SshHost::command`] but for tokio.
  pub fn tokio_command(&self, program: &str, args: &[&str]) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("ssh");
    let mut std_cmd = Command::new("ssh");
    self.add_remote_invocation(&mut std_cmd, program, args);
    cmd.args(std_cmd.get_args());
    cmd
  }

  fn add_remote_invocation(&self, cmd: &mut Command, program: &str, args: &[&str]) {
    cmd.arg("-xT");
    cmd.args(self.mux_options());
    cmd.arg("--");
    cmd.arg(&self.host);
    // ssh concatenates arguments and hands them to the remote shell, so quote each one
    cmd.arg(shell_quote(program));
    for arg in args {
      cmd.arg(shell_quote(arg));
    }
  }
}

/// Single-quote a string for a POSIX shell.
fn shell_quote(s: &str) -> String {
  if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_./=:@%+,".contains(c)) {
    return s.to_string();
  }
  format!("'{}'", s.replace('\'', r"'\''"))
}

/// Run `program args...` locally, or on the remote host if `--host` was given.
pub fn host_command(program: &str, args: &[&str]) -> Command {
  match remote_host() {
    Some(ssh_host) => ssh_host.command(program, args),
    None => {
      let mut cmd = Command::new(program);
      cmd.args(args);
      cmd
    },
  }
}

/// Tokio version of [`host_command`].
pub fn host_tokio_command(program: &str, args: &[&str]) -> tokio::process::Command {
  match remote_host() {
    Some(ssh_host) => ssh_host.tokio_command(program, args),
    None => {
      let mut cmd = tokio::process::Command::new(program);
      cmd.args(args);
      cmd
    },
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_shell_quote_plain() {
    assert_eq!(shell_quote("docker.service"), "docker.service");
    assert_eq!(shell_quote("--lines=500"), "--lines=500");
  }

  #[test]
  fn test_shell_quote_special() {
    assert_eq!(shell_quote("foo bar"), "'foo bar'");
    assert_eq!(shell_quote("it's"), r"'it'\''s'");
    assert_eq!(shell_quote(""), "''");
  }
}
