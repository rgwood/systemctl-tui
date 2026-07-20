//! Pure formatting helpers shared by the TUI and GUI frontends.

/// Parse a journalctl timestamp and return a formatted date string.
///
/// systemd v255 changed the timestamp format from `-0700` to `-07:00` (RFC 3339).
/// See: https://github.com/systemd/systemd/pull/29134
/// Parses a systemd "show" timestamp like "Wed 2026-07-08 10:00:00 PDT" into a compact
/// absolute form and, for local units, a relative one ("2d 4h ago" or "in 2d 4h").
/// Remote timestamps retain their source timezone and omit the relative time because
/// comparing naive wall-clock values from different timezones would be misleading.
pub fn format_systemd_timestamp(timestamp: &str, is_remote: bool) -> Option<(String, Option<String>)> {
  let mut parts = timestamp.split_whitespace();
  let _weekday = parts.next()?;
  let date = parts.next()?;
  let time = parts.next()?;
  let timezone = parts.next();
  let naive = chrono::NaiveDateTime::parse_from_str(&format!("{date} {time}"), "%Y-%m-%d %H:%M:%S").ok()?;
  let mut absolute = naive.format("%Y-%m-%d %H:%M").to_string();
  if is_remote {
    if let Some(timezone) = timezone {
      absolute.push(' ');
      absolute.push_str(timezone);
    }
    return Some((absolute, None));
  }
  let seconds = chrono::Local::now().naive_local().signed_duration_since(naive).num_seconds();
  let relative = if seconds >= 0 {
    format!("{} ago", format_duration(seconds as u64))
  } else {
    format!("in {}", format_duration(seconds.unsigned_abs()))
  };
  Some((absolute, Some(relative)))
}

pub fn format_duration(seconds: u64) -> String {
  match seconds {
    s if s < 60 => format!("{s}s"),
    s if s < 3600 => format!("{}m {}s", s / 60, s % 60),
    s if s < 86400 => format!("{}h {}m", s / 3600, (s % 3600) / 60),
    s => format!("{}d {}h", s / 86400, (s % 86400) / 3600),
  }
}

pub fn format_bytes(bytes: u64) -> String {
  const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];
  let mut value = bytes as f64;
  let mut unit = 0;
  while value >= 1024.0 && unit < UNITS.len() - 1 {
    value /= 1024.0;
    unit += 1;
  }
  if unit == 0 {
    format!("{bytes}B")
  } else {
    format!("{value:.1}{}", UNITS[unit])
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn remote_timestamps_keep_their_timezone_without_a_relative_guess() {
    let formatted = format_systemd_timestamp("Wed 2026-07-08 10:00:00 PDT", true);

    assert_eq!(formatted, Some(("2026-07-08 10:00 PDT".into(), None)));
  }
}
