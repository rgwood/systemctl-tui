//! Baked-in plain-English explanations of common systemd units.
//!
//! The database lives in `src/assets/unit-descriptions.tsv` and is compiled into the
//! binary. Templated instances (`getty@tty1.service`) fall back to their template
//! entry (`getty@.service`). Entries are scoped to the system or user manager so a
//! locally defined unit with the same name in the other manager does not match.

use std::collections::HashMap;
use std::sync::LazyLock;

use crate::systemd::UnitScope;

static DESCRIPTIONS: LazyLock<HashMap<(&'static str, UnitScope), &'static str>> = LazyLock::new(|| {
  let mut descriptions = HashMap::new();
  for line in
    include_str!("assets/unit-descriptions.tsv").lines().filter(|line| !line.is_empty() && !line.starts_with('#'))
  {
    let mut fields = line.splitn(3, '\t');
    let (Some(unit_name), Some(scope), Some(description)) = (fields.next(), fields.next(), fields.next()) else {
      continue;
    };
    match scope {
      "system" => {
        descriptions.insert((unit_name, UnitScope::Global), description);
      },
      "user" => {
        descriptions.insert((unit_name, UnitScope::User), description);
      },
      "both" => {
        descriptions.insert((unit_name, UnitScope::Global), description);
        descriptions.insert((unit_name, UnitScope::User), description);
      },
      _ => {},
    }
  }
  descriptions
});

/// Look up the baked-in explanation for a unit, if we have one.
pub fn explain(unit_name: &str, scope: UnitScope) -> Option<&'static str> {
  if let Some(desc) = DESCRIPTIONS.get(&(unit_name, scope)) {
    return Some(desc);
  }
  // getty@tty1.service -> getty@.service
  if let (Some(at), Some(dot)) = (unit_name.find('@'), unit_name.rfind('.')) {
    if at < dot {
      let template = format!("{}@{}", &unit_name[..at], &unit_name[dot..]);
      return DESCRIPTIONS.get(&(template.as_str(), scope)).copied();
    }
  }
  None
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn exact_match() {
    assert!(explain("systemd-journald.service", UnitScope::Global).unwrap().contains("log"));
  }

  #[test]
  fn template_instance_falls_back_to_template() {
    assert_eq!(explain("getty@tty1.service", UnitScope::Global), explain("getty@.service", UnitScope::Global));
    assert!(explain("getty@tty1.service", UnitScope::Global).is_some());
  }

  #[test]
  fn unknown_unit_returns_none() {
    assert_eq!(explain("no-such-unit.service", UnitScope::Global), None);
  }

  #[test]
  fn lookup_respects_scope() {
    assert!(explain("docker.service", UnitScope::Global).is_some());
    assert_eq!(explain("docker.service", UnitScope::User), None);
    assert!(explain("basic.target", UnitScope::Global).is_some());
    assert!(explain("basic.target", UnitScope::User).is_some());
  }

  #[test]
  fn database_parses_fully() {
    let mut parsed_entries = 0;
    let mut scoped_keys = std::collections::HashSet::new();
    for line in include_str!("assets/unit-descriptions.tsv").lines() {
      if !line.is_empty() && !line.starts_with('#') {
        let fields = line.split('\t').collect::<Vec<_>>();
        assert_eq!(fields.len(), 3, "malformed line: {line}");
        assert!(matches!(fields[1], "system" | "user" | "both"), "invalid scope: {line}");
        let scopes: &[UnitScope] = match fields[1] {
          "system" => &[UnitScope::Global],
          "user" => &[UnitScope::User],
          "both" => &[UnitScope::Global, UnitScope::User],
          _ => unreachable!(),
        };
        for scope in scopes {
          assert!(scoped_keys.insert((fields[0], *scope)), "duplicate unit and scope: {line}");
        }
        parsed_entries += 1;
      }
    }
    assert!(parsed_entries > 50);
  }
}
