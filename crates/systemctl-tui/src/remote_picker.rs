use std::{collections::HashSet, fs, path::PathBuf};

use anyhow::{bail, Context, Result};
use dialoguer::{Input, Select};

const MANUAL_ENTRY: &str = "Enter a host manually…";

/// Prompt for a remote host before the main TUI starts.
pub fn choose_remote_host() -> Result<String> {
  let mut hosts = ssh_config_hosts();
  deduplicate(&mut hosts);

  let mut choices = hosts.clone();
  choices.push(MANUAL_ENTRY.to_string());

  let selection = Select::new()
    .with_prompt("Remote host")
    .items(&choices)
    .default(0)
    .interact_opt()
    .context("Failed to read remote host selection")?;

  let Some(selection) = selection else {
    bail!("Remote host selection cancelled");
  };

  let host = if selection == hosts.len() {
    Input::<String>::new()
      .with_prompt("SSH host")
      .validate_with(|value: &String| -> std::result::Result<(), &str> {
        if value.trim().is_empty() {
          Err("host cannot be empty")
        } else {
          Ok(())
        }
      })
      .interact_text()
      .context("Failed to read remote host")?
      .trim()
      .to_string()
  } else {
    hosts[selection].clone()
  };

  Ok(host)
}

fn ssh_config_hosts() -> Vec<String> {
  let Some(home) = std::env::var_os("HOME") else {
    return vec![];
  };
  let config_path = PathBuf::from(home).join(".ssh/config");
  fs::read_to_string(config_path).map(|contents| parse_ssh_config_hosts(&contents)).unwrap_or_default()
}

fn parse_ssh_config_hosts(config: &str) -> Vec<String> {
  config
    .lines()
    .filter_map(|line| {
      let line = line.split('#').next()?.trim();
      let (keyword, value) = line.split_once(char::is_whitespace)?;
      if !keyword.eq_ignore_ascii_case("host") {
        return None;
      }
      Some(
        value.split_whitespace().filter(|host| !host.contains(['*', '?', '!'])).map(str::to_string).collect::<Vec<_>>(),
      )
    })
    .flatten()
    .collect()
}

fn deduplicate(hosts: &mut Vec<String>) {
  let mut seen = HashSet::new();
  hosts.retain(|host| seen.insert(host.clone()));
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn parses_literal_ssh_host_aliases() {
    let config = r#"
Host prod web
  HostName prod.example.com

host *.internal !bastion
  User deploy

HOST staging # a comment
  HostName staging.example.com
"#;

    assert_eq!(parse_ssh_config_hosts(config), ["prod", "web", "staging"]);
  }

  #[test]
  fn deduplicates_hosts_without_reordering_them() {
    let mut hosts = vec!["recent".into(), "prod".into(), "recent".into(), "staging".into()];
    deduplicate(&mut hosts);
    assert_eq!(hosts, ["recent", "prod", "staging"]);
  }
}
