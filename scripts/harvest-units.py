#!/usr/bin/env -S uv run --script --quiet
# /// script
# requires-python = ">=3.12"
# dependencies = []
# ///
"""Harvest candidate unit names for the baked-in descriptions database.

Collects unit names from the local machine (system + user scope), collapses
templated instances down to their template, and filters out machine-specific
or auto-generated noise. Prints the surviving candidates with the systemd
Description= for reference, plus a rejects list so we can sanity-check the
filters.
"""

import re
import subprocess
import sys
from collections import defaultdict

# Unit types that are inherently machine-specific or auto-generated.
# devices/mounts/swaps/scopes/slices come from hardware or the fstab/cgroup
# layout; there's nothing useful to pre-describe.
SKIP_SUFFIXES = (".device", ".swap", ".scope", ".snapshot")

# Mounts are mostly machine-specific, but a handful of API-filesystem mounts
# are universal and genuinely confusing ("what is proc-sys-fs-binfmt_misc?").
KEEP_MOUNTS = {
    "proc-sys-fs-binfmt_misc.mount",
    "sys-fs-fuse-connections.mount",
    "sys-kernel-config.mount",
    "sys-kernel-debug.mount",
    "sys-kernel-tracing.mount",
    "dev-hugepages.mount",
    "dev-mqueue.mount",
    "tmp.mount",
    "boot.mount",
    "run-user-1000.mount",  # placeholder; template-collapsed below
}

# Patterns that mark a unit as machine/instance-specific even after
# template collapsing.
NOISE_PATTERNS = [
    re.compile(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}"),  # UUIDs
    re.compile(r"[0-9a-f]{32}"),  # long hex ids
    re.compile(r"\\x[0-9a-f]{2}"),  # escaped paths (dev-disk-by\x2d...)
    re.compile(r"^(dev|sys|proc|run|home|media|mnt|var|srv|snap)-.*\.mount$"),
    re.compile(r"^dbus-:"),  # dbus-activated transient names
    re.compile(r"^dbus-org\."),  # alias units for the real service
    re.compile(r"^app-"),  # per-app desktop launcher wrappers
    re.compile(r"^snap\."),  # per-snap-app units (snapd.* itself is kept)
]

# Units specific to this machine's owner or to systemctl-tui development.
LOCAL_ONLY = {
    "handy-ptt.service",
    "reitunes.service",
    "reitunez.service",
    "systemctl-tui-polkit-test.service",
    "test-dummy.service",
    "ttyd.service",
}


def list_units(scope_args: list[str]) -> dict[str, str]:
    """Return {unit_name: description} from list-units + list-unit-files."""
    units: dict[str, str] = {}
    out = subprocess.run(
        ["systemctl", *scope_args, "list-units", "--all", "--no-legend", "--plain", "--full"],
        capture_output=True, text=True,
    ).stdout
    for line in out.splitlines():
        parts = line.split(None, 4)
        if len(parts) >= 5:
            units[parts[0]] = parts[4]
        elif parts:
            units[parts[0]] = ""
    out = subprocess.run(
        ["systemctl", *scope_args, "list-unit-files", "--no-legend", "--plain", "--full"],
        capture_output=True, text=True,
    ).stdout
    for line in out.splitlines():
        parts = line.split()
        if parts:
            units.setdefault(parts[0], "")
    return units


def collapse_template(name: str) -> str:
    """getty@tty1.service -> getty@.service"""
    m = re.match(r"^([^@]+)@(.+)(\.[a-z]+)$", name)
    if m:
        return f"{m.group(1)}@{m.group(3)}"
    return name


def wanted(name: str) -> bool:
    if name.endswith(SKIP_SUFFIXES):
        return False
    if name.endswith(".mount") and name not in KEEP_MOUNTS:
        return False
    if name in LOCAL_ONLY:
        return False
    return not any(p.search(name) for p in NOISE_PATTERNS)


def main() -> None:
    scopes = [("system", ["--system"]), ("user", ["--user"])]
    kept: dict[str, str] = {}
    rejected: set[str] = set()
    per_scope_counts: dict[str, int] = defaultdict(int)

    for scope_name, args in scopes:
        for name, desc in list_units(args).items():
            per_scope_counts[scope_name] += 1
            collapsed = collapse_template(name)
            if wanted(collapsed):
                # Prefer a non-empty description; template desc may vary per
                # instance, first one wins which is fine for reference.
                if collapsed not in kept or (not kept[collapsed] and desc):
                    kept[collapsed] = desc
            else:
                rejected.add(collapsed)

    print(f"# Harvested from local machine: "
          f"{per_scope_counts['system']} system + {per_scope_counts['user']} user units")
    print(f"# Kept {len(kept)} candidates, rejected {len(rejected)} as noise\n")
    for name in sorted(kept):
        desc = kept[name]
        print(f"{name}\t{desc}" if desc else name)

    if "--show-rejects" in sys.argv:
        print("\n# Rejected:")
        for name in sorted(rejected):
            print(name)


if __name__ == "__main__":
    main()
