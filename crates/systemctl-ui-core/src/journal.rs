//! Parsing of `journalctl --output=json` lines into structured log entries.
//!
//! We use JSON output (rather than `short-iso`) so we get the `PRIORITY` field, which
//! lets us colorize errors/warnings the way `journalctl` does in a terminal.

use chrono::{Local, TimeZone};
use serde_json::Value;

/// One displayable log line. Multi-line journal messages are split into multiple
/// entries (continuation lines have no timestamp).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogEntry {
  /// Local time, pre-formatted for display ("2026-07-15 10:04"). `None` for
  /// continuation lines and non-journal messages (diagnostics, errors).
  pub timestamp: Option<String>,
  /// The rest of the line: "identifier[pid]: message" for journal entries.
  pub content: String,
  /// syslog priority 0-7 (0=emerg ... 7=debug), if the journal entry had one
  pub priority: Option<u8>,
}

impl LogEntry {
  /// A plain text line with no timestamp or priority (diagnostics, error messages).
  pub fn plain(content: impl Into<String>) -> Self {
    Self { timestamp: None, content: content.into(), priority: None }
  }

  /// Plain-text form, used when exporting logs to a pager.
  pub fn to_plain_string(&self) -> String {
    match &self.timestamp {
      Some(ts) => format!("{} {}", ts, self.content),
      None => self.content.clone(),
    }
  }
}

/// Parse one line of `journalctl --output=json` output. Returns one entry per line of
/// the message (journal messages can be multi-line). Falls back to a plain entry with
/// the raw line if it isn't valid JSON.
pub fn parse_json_log_line(line: &str) -> Vec<LogEntry> {
  let Ok(value) = serde_json::from_str::<Value>(line) else {
    return vec![LogEntry::plain(line)];
  };

  let timestamp = value["__REALTIME_TIMESTAMP"]
    .as_str()
    .and_then(|us| us.parse::<i64>().ok())
    .and_then(|us| Local.timestamp_micros(us).single())
    .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string());

  let priority = value["PRIORITY"].as_str().and_then(|p| p.parse::<u8>().ok());

  let message = field_as_string(&value["MESSAGE"]).unwrap_or_default();

  // "identifier[pid]: " prefix, like journalctl's short output
  let identifier = value["SYSLOG_IDENTIFIER"].as_str().or_else(|| value["_COMM"].as_str());
  let pid = value["_PID"].as_str();
  let prefix = match (identifier, pid) {
    (Some(id), Some(pid)) => format!("{id}[{pid}]: "),
    (Some(id), None) => format!("{id}: "),
    (None, _) => String::new(),
  };

  let mut lines = message.lines();
  let first = lines.next().unwrap_or_default();
  let mut entries = vec![LogEntry { timestamp, content: format!("{prefix}{first}"), priority }];
  for continuation in lines {
    entries.push(LogEntry { timestamp: None, content: format!("  {continuation}"), priority });
  }
  entries
}

/// Journal fields are usually JSON strings, but journalctl encodes non-UTF8 values as
/// arrays of bytes. Handle both.
fn field_as_string(value: &Value) -> Option<String> {
  match value {
    Value::String(s) => Some(s.clone()),
    Value::Array(bytes) => {
      let bytes: Vec<u8> = bytes.iter().filter_map(|b| b.as_u64().map(|b| b as u8)).collect();
      Some(String::from_utf8_lossy(&bytes).into_owned())
    },
    _ => None,
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn parses_a_typical_entry() {
    let line = r#"{"__REALTIME_TIMESTAMP":"1752598800000000","PRIORITY":"6","MESSAGE":"Started Docker.","SYSLOG_IDENTIFIER":"systemd","_PID":"1"}"#;
    let entries = parse_json_log_line(line);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].priority, Some(6));
    assert_eq!(entries[0].content, "systemd[1]: Started Docker.");
    assert!(entries[0].timestamp.is_some());
  }

  #[test]
  fn splits_multiline_messages() {
    let line = r#"{"__REALTIME_TIMESTAMP":"1752598800000000","PRIORITY":"3","MESSAGE":"first\nsecond","SYSLOG_IDENTIFIER":"app"}"#;
    let entries = parse_json_log_line(line);
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].content, "app: first");
    assert_eq!(entries[1].content, "  second");
    assert_eq!(entries[1].timestamp, None);
    assert_eq!(entries[1].priority, Some(3));
  }

  #[test]
  fn handles_non_utf8_message_byte_arrays() {
    let line = r#"{"MESSAGE":[104,105,255],"PRIORITY":"4"}"#;
    let entries = parse_json_log_line(line);
    assert_eq!(entries.len(), 1);
    assert!(entries[0].content.starts_with("hi"));
    assert_eq!(entries[0].priority, Some(4));
  }

  #[test]
  fn falls_back_to_plain_on_invalid_json() {
    let entries = parse_json_log_line("not json at all");
    assert_eq!(entries, vec![LogEntry::plain("not json at all")]);
  }

  #[test]
  fn missing_fields_are_tolerated() {
    let entries = parse_json_log_line(r#"{"MESSAGE":"hello"}"#);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].content, "hello");
    assert_eq!(entries[0].priority, None);
    assert_eq!(entries[0].timestamp, None);
  }
}
