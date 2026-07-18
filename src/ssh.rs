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
  // %C expands to a hash of local host, remote host, port, and user. Include our PID so that
  // concurrent instances don't share a master (teardown would kill it out from under the other)
  let control_path = runtime_dir.join(format!("systemctl-tui-ssh-{}-%C", std::process::id()));

  let status = Command::new("ssh")
    .args(["-o", "ControlMaster=auto", "-o"])
    .arg(format!("ControlPath={}", control_path.display()))
    .args([
      "-o",
      "ControlPersist=60",
      "-o",
      "ConnectTimeout=10",
      "-o",
      "ServerAliveInterval=15",
      "-o",
      "ServerAliveCountMax=3",
      "-N",
      "-f",
      "--",
      &host,
    ])
    .status()
    .context("Failed to run ssh. Is OpenSSH installed?")?;

  if !status.success() {
    bail!("Failed to connect to {host} over SSH");
  }

  let ssh_host = SshHost { host, control_path };

  // Fail now with a clear message rather than later with an opaque D-Bus error
  let bridge_check = ssh_host.command("command", &["-v", "systemd-stdio-bridge"]).output();
  if !bridge_check.map(|o| o.status.success()).unwrap_or(false) {
    let host = &ssh_host.host;
    ssh_host.close_master();
    bail!("systemd-stdio-bridge not found on {host}. It ships with systemd; is {host} running a systemd distro?");
  }

  REMOTE_HOST.set(ssh_host).expect("ssh::init called twice");
  Ok(())
}

/// Close the master connection. Safe to skip (ControlPersist expires it), but tidy.
pub fn teardown() {
  if let Some(ssh_host) = remote_host() {
    ssh_host.close_master();
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
        // Not `--system`: that flag doesn't exist in older systemd (245 rejects it, 249 has
        // it), but `-p` with the (documented, stable) default system bus path works on every
        // version back to at least 239. Found by the container matrix on rocky-8/ubuntu-20.04.
        args.push("systemd-stdio-bridge".into());
        args.push("-p".into());
        args.push("unix:path=/run/dbus/system_bus_socket".into());
      },
      UnitScope::User => {
        // `systemd-stdio-bridge --user` is unreliable in non-interactive SSH sessions (it can
        // silently serve the system bus or fail outright), so point it at the user bus socket
        // explicitly. Requires a running user manager (active session or lingering).
        // The remote shell word-splits ssh's command string, so the -c payload must be quoted;
        // $(id -u) is expanded by the remote sh, not locally.
        args.push("sh".into());
        args.push("-c".into());
        args.push("'exec systemd-stdio-bridge -p unix:path=/run/user/$(id -u)/bus'".into());
      },
    }
    args
  }

  fn close_master(&self) {
    let _ = Command::new("ssh").args(self.mux_options()).args(["-O", "exit", "--", &self.host]).output();
  }

  /// Build a `std::process::Command` that runs `program args...` on the remote host.
  pub fn command(&self, program: &str, args: &[&str]) -> Command {
    let mut cmd = Command::new("ssh");
    cmd.args(self.remote_invocation_args(program, args));
    // ssh forwards its stdin to the remote command; if it inherits the TUI's stdin it
    // steals keystrokes from the terminal
    cmd.stdin(std::process::Stdio::null());
    cmd
  }

  /// Like [`SshHost::command`] but for tokio.
  pub fn tokio_command(&self, program: &str, args: &[&str]) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("ssh");
    cmd.args(self.remote_invocation_args(program, args));
    // same stdin-stealing hazard as in `command` above
    cmd.stdin(std::process::Stdio::null());
    cmd
  }

  fn remote_invocation_args(&self, program: &str, args: &[&str]) -> Vec<String> {
    let mut ssh_args = vec!["-xT".to_string()];
    ssh_args.extend(self.mux_options());
    ssh_args.push("--".into());
    ssh_args.push(self.host.clone());
    // ssh concatenates arguments and hands them to the remote shell, so quote each one
    ssh_args.push(shell_quote(program));
    ssh_args.extend(args.iter().map(|arg| shell_quote(arg)));
    ssh_args
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

  fn test_host() -> SshHost {
    SshHost { host: "user@example".into(), control_path: PathBuf::from("/tmp/ctl-%C") }
  }

  fn args_of(cmd: &Command) -> Vec<String> {
    cmd.get_args().map(|a| a.to_string_lossy().into_owned()).collect()
  }

  #[test]
  fn test_remote_command_args() {
    let host = test_host();
    let cmd = host.command("journalctl", &["--lines=500", "-u", "foo bar.service"]);
    assert_eq!(cmd.get_program(), "ssh");
    assert_eq!(
      args_of(&cmd),
      vec![
        "-xT",
        "-o",
        "ControlPath=/tmp/ctl-%C",
        "-o",
        "BatchMode=yes",
        "--",
        "user@example",
        "journalctl",
        "--lines=500",
        "-u",
        // quoted so the remote shell doesn't word-split it
        "'foo bar.service'",
      ]
    );
    // NOTE: stdin must be null for remote commands (ssh steals terminal input otherwise),
    // but std::process::Command has no getter for stdio config. The remote integration
    // test's typing check covers that regression.
  }

  #[test]
  fn test_tokio_command_matches_std_command() {
    let host = test_host();
    let std_cmd = host.command("systemctl", &["show", "-P", "FragmentPath", "foo.service"]);
    let tokio_cmd = host.tokio_command("systemctl", &["show", "-P", "FragmentPath", "foo.service"]);
    assert_eq!(args_of(tokio_cmd.as_std()), args_of(&std_cmd));
  }

  #[test]
  fn test_bridge_args_global() {
    let host = test_host();
    let args: Vec<String> =
      host.bridge_ssh_args(UnitScope::Global).iter().map(|a| a.to_string_lossy().into_owned()).collect();
    assert_eq!(
      args,
      vec![
        "-xT",
        "-o",
        "ControlPath=/tmp/ctl-%C",
        "-o",
        "BatchMode=yes",
        "--",
        "user@example",
        "systemd-stdio-bridge",
        "-p",
        // not --system, which old systemd versions (e.g. 245) don't support
        "unix:path=/run/dbus/system_bus_socket",
      ]
    );
  }

  #[test]
  fn test_bridge_args_user() {
    let host = test_host();
    let args: Vec<String> =
      host.bridge_ssh_args(UnitScope::User).iter().map(|a| a.to_string_lossy().into_owned()).collect();
    // the -c payload must stay single-quoted: the remote shell word-splits ssh's
    // command string, and $(id -u) must be expanded remotely, not locally
    assert_eq!(args[args.len() - 3..], ["sh", "-c", "'exec systemd-stdio-bridge -p unix:path=/run/user/$(id -u)/bus'"]);
    assert_eq!(args[args.len() - 4], "user@example");
  }

  #[test]
  fn test_local_command_passthrough() {
    // no remote host initialized in tests, so host_command should run the program directly
    let cmd = host_command("journalctl", &["--lines=1"]);
    assert_eq!(cmd.get_program(), "journalctl");
    assert_eq!(args_of(&cmd), vec!["--lines=1"]);
  }
}
