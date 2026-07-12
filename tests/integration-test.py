#!/usr/bin/env -S uv run --script --quiet
# /// script
# requires-python = ">=3.12"
# dependencies = []
# ///
"""Integration tests for systemctl-tui.

Drives the real binary in a tmux session and asserts on rendered output.
By default it tests against local systemd; pass --host to run the same tests
against a remote machine over SSH.

Usage:
    ./tests/integration-test.py                        # local
    ./tests/integration-test.py --host user@hostname   # remote

Requires: tmux and a debug build (`cargo build`). Remote mode needs
passwordless ssh to the target.
"""

import argparse
import os
import re
import subprocess
import sys
import time

# unique per run so concurrent invocations (e.g. remote-matrix.py against several
# containers) don't clobber each other's tmux sessions
SESSION = f"sctui-integration-test-{os.getpid()}"

passed: list[str] = []
failed: list[str] = []


def run(cmd: list[str], **kwargs) -> subprocess.CompletedProcess:
    return subprocess.run(cmd, capture_output=True, text=True, timeout=30, **kwargs)


def tmux(*args: str) -> subprocess.CompletedProcess:
    return run(["tmux", *args])


def capture() -> str:
    return tmux("capture-pane", "-t", SESSION, "-p").stdout


def capture_esc() -> str:
    """Like capture(), but keeps ANSI escape sequences (e.g. to detect highlight colors)."""
    return tmux("capture-pane", "-t", SESSION, "-e", "-p").stdout


def send_keys(*keys: str, delay: float = 0.0) -> None:
    for key in keys:
        tmux("send-keys", "-t", SESSION, key)
        if delay:
            time.sleep(delay)


def send_mouse(kind: str, col: int, row: int) -> None:
    """Inject an SGR mouse event (1-based col/row) - the app enables SGR mouse tracking
    (crossterm's EnableMouseCapture), so tmux can feed it raw escape sequences as if a real
    mouse driver produced them.

    kind: press|release|drag|move|wheel_up|wheel_down
    """
    code = {"press": "0", "release": "0", "drag": "32", "move": "35", "wheel_up": "64", "wheel_down": "65"}[kind]
    suffix = "m" if kind == "release" else "M"
    tmux("send-keys", "-t", SESSION, "-l", f"\x1b[<{code};{col};{row}{suffix}")


def click(col: int, row: int) -> None:
    """Press + release the left mouse button at a 1-based (col, row)."""
    send_mouse("press", col, row)
    time.sleep(0.1)
    send_mouse("release", col, row)


def start_app(binary: str, host: str | None, extra_args: list[str] | None = None) -> None:
    tmux("kill-session", "-t", SESSION)
    host_args = ["--host", host] if host else []
    args = [binary, *host_args, *(extra_args or [])]
    cmd = " ".join(f"'{a}'" for a in args)
    # tmux's server may have started before a shim was prepended to PATH (e.g. by
    # remote-matrix.py); wrap with `env` so the pane picks up the current PATH.
    cmd = f"env 'PATH={os.environ.get('PATH', '')}' {cmd}"
    tmux("new-session", "-d", "-s", SESSION, "-x", "120", "-y", "40", cmd)


def stop_app() -> None:
    tmux("kill-session", "-t", SESSION)


def app_alive() -> bool:
    result = tmux("list-panes", "-t", SESSION, "-F", "#{pane_dead}")
    return result.returncode == 0 and result.stdout.strip() == "0"


def check(name: str, condition: bool, context: str = "") -> None:
    if condition:
        passed.append(name)
        print(f"  PASS  {name}")
    else:
        failed.append(name)
        print(f"  FAIL  {name}")
        if context:
            print(f"        {context}")


def wait_for(predicate, timeout: float = 15.0, interval: float = 0.5) -> bool:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return True
        time.sleep(interval)
    return False


def test_startup_and_browse(binary: str, host: str | None) -> None:
    print("startup and browsing:")
    start_app(binary, host)
    check("app launches and renders", wait_for(lambda: "Services" in capture()))
    check("services listed", wait_for(lambda: ".service" in capture() or "Details" in capture()))

    # navigate: initial mode is Search, Down selects the first unit
    send_keys("Down")
    time.sleep(1)
    check("selection shows details", "Description:" in capture(), capture())

    # help screen
    send_keys("?")
    time.sleep(1)
    check("help screen opens", "Shortcuts" in capture())
    send_keys("Escape")
    time.sleep(1)

    # action menu
    send_keys("Enter")
    time.sleep(1)
    check("action menu opens", "Actions for" in capture(), capture())
    send_keys("Escape")
    time.sleep(1)

    # resize, including a very small terminal
    tmux("resize-window", "-t", SESSION, "-x", "40", "-y", "15")
    time.sleep(1)
    alive_when_small = app_alive()
    tmux("resize-window", "-t", SESSION, "-x", "120", "-y", "40")
    time.sleep(1)
    check("survives resizing", alive_when_small and app_alive())

    check("app still alive after interactions", app_alive())


def test_logs(binary: str, host: str | None) -> None:
    """Logs should either render, or show an actionable diagnostic - never a crash."""
    print("logs:")
    # systemd-journald.service exists as a real (non-alias) unit and has logged on any
    # systemd machine; dbus.service is only an alias on dbus-broker distros like Fedora,
    # and aliases don't match unit patterns
    start_app(binary, host, ["--limit-units", "systemd-journald.service"])
    wait_for(lambda: "Details" in capture())
    send_keys("Down")

    def logs_or_diagnostic() -> bool:
        screen = capture()
        # match log-line shapes, not the unit name (which is always on screen)
        rendered_logs = "systemd[1]" in screen or "systemd-journald[" in screen or "Journal started" in screen
        diagnostic = "No access to system logs" in screen or "No logs" in screen
        return rendered_logs or diagnostic

    check("logs pane shows logs or diagnostic", wait_for(logs_or_diagnostic), capture())
    check("app still alive", app_alive())


def test_mouse(binary: str, host: str | None) -> None:
    """Mouse handling is client-side (crossterm SGR parsing + ratatui rect hit-testing), so this
    runs identically in local and remote mode - unlike most of the remote suite it isn't testing
    anything ssh-specific.
    """
    print("mouse interactions:")
    start_app(binary, host)
    wait_for(lambda: "Services" in capture())
    # initial mode is Search; leave it so plain keys like 'f' below aren't typed into the box
    send_keys("Escape")
    time.sleep(0.3)

    def highlighted_row(screen_esc: str) -> int | None:
        """Row (0-based, matching capture().splitlines()) carrying the list-selection background."""
        for i, line in enumerate(screen_esc.splitlines()):
            if "48;5;8" in line:
                return i
        return None

    def service_rows() -> list[int]:
        """0-based row indices of visible, non-empty service list entries (left ~29 cols)."""
        rows = []
        lines = capture().splitlines()
        in_list = False
        for i, line in enumerate(lines):
            if "Services" in line[:30]:
                in_list = True
                continue
            if not in_list:
                continue
            if line.lstrip().startswith("╰"):
                break
            name = line[1:29].strip()
            if name:
                rows.append(i)
        return rows

    # 1. click selects a service
    rows = service_rows()
    before = highlighted_row(capture_esc())
    target = next((r for r in rows if r != before), rows[0] if rows else None)
    check("found a clickable service row", target is not None)
    if target is not None:
        click(10, target + 1)
        time.sleep(0.4)
        after = highlighted_row(capture_esc())
        check("click selects a service", after == target, f"before={before} target={target} after={after}")

    # For the rest of this test we want a unit with a guaranteed non-empty description and
    # plenty of log history to scroll/select, so the click/wheel/drag checks below aren't at the
    # mercy of whichever unit happened to be under the cursor in step 1. systemd-journald.service
    # is a real (non-alias) unit that has logged on any systemd machine (see test_logs).
    send_keys("C-f")
    time.sleep(0.2)
    type_text("systemd-journald")
    time.sleep(0.3)
    send_keys("Down")
    time.sleep(0.5)
    send_keys("Escape")
    time.sleep(0.3)
    check("selected systemd-journald.service for the remaining checks", "systemd-journald" in capture(), capture())

    # 2. click on a details field copies the value and shows a toast
    desc_line = next((i for i, l in enumerate(capture().splitlines()) if "Description:" in l), None)
    check("details pane shows a Description field", desc_line is not None, capture())
    if desc_line is not None:
        line = capture().splitlines()[desc_line]
        idx = line.index("Description:") + len("Description:")
        match = re.search(r"\S", line[idx:])
        value_col = idx + match.start() + 3 if match else idx + 3
        click(value_col, desc_line + 1)
        check("click on details field copies + shows toast", wait_for(lambda: re.search(r"Copied \d+ chars", capture()) is not None), capture())
        time.sleep(2.1)  # let the toast expire before the next assertion that checks for its absence

    # 3. wheel scrolls logs
    logs_before = capture()
    for _ in range(3):
        send_mouse("wheel_down", 70, 20)
        time.sleep(0.3)
    logs_after = capture()
    check("wheel scrolls logs", logs_after != logs_before, f"before:\n{logs_before}\nafter:\n{logs_after}")

    # 4. drag-selecting log text copies it and shows a toast
    send_mouse("press", 40, 11)
    time.sleep(0.1)
    send_mouse("drag", 44, 11)
    time.sleep(0.1)
    send_mouse("drag", 48, 11)
    time.sleep(0.1)
    send_mouse("release", 48, 11)
    check("drag selection copies log text", wait_for(lambda: re.search(r"Copied \d+ chars", capture()) is not None), capture())
    time.sleep(2.1)

    # 5. clicking outside the action menu closes it
    send_keys("Enter")
    check("action menu opens", wait_for(lambda: "Actions for" in capture()), capture())
    click(5, 5)
    time.sleep(0.4)
    check("click outside closes action menu", "Actions for" not in capture(), capture())
    check("app alive after closing menu", app_alive())

    # 6. clicking a status filter entry toggles it
    send_keys("Escape")
    time.sleep(0.3)
    send_keys("f")
    check("status filter popup opens", wait_for(lambda: "Status filter" in capture()), capture())
    filter_line = next((i for i, l in enumerate(capture().splitlines()) if "✓ active" in l), None)
    check("status filter has a checked entry", filter_line is not None, capture())
    if filter_line is not None:
        line = capture().splitlines()[filter_line]
        col = line.index("✓ active") + 2  # land inside "active", not on the border/checkmark
        click(col, filter_line + 1)
        time.sleep(0.4)
        toggled = capture().splitlines()[filter_line]
        check("clicking a status filter entry toggles its checkmark", "✓ active" not in toggled, toggled)
    click(5, 5)
    time.sleep(0.4)
    check("click outside closes status filter popup", "Status filter" not in capture(), capture())

    check("app alive after mouse interactions", app_alive())
    send_keys("q")


def test_no_dropped_keystrokes(binary: str, host: str | None) -> None:
    """Regression test: ssh children used to inherit (and eat) the TUI's stdin.

    Typing changes the selection, which kicks off log fetches; in remote mode,
    keys pressed while a fetch was in flight were forwarded to ssh instead of
    the app. 30ms is fast human typing; before the fix this dropped keys in
    ~80% of remote runs. Cheap enough to run locally too.
    """
    print("keystroke drops:")
    text = "systemdnetworkd"
    for ms in (30, 10):
        start_app(binary, host)
        wait_for(lambda: "Services" in capture())
        send_keys(*text, delay=ms / 1000)
        time.sleep(2)
        search_line = capture().splitlines()[1] if len(capture().splitlines()) > 1 else ""
        check(
            f"no dropped keys at {ms}ms intervals",
            text in search_line.replace("│", "").strip(),
            f"search box: {search_line!r}",
        )


def test_clean_exit(binary: str, host: str | None) -> None:
    print("clean exit:")
    start_app(binary, host)
    wait_for(lambda: "Services" in capture())
    # initial mode is Search where q would be typed into the box; Escape first
    send_keys("Escape")
    time.sleep(0.5)
    send_keys("q")
    # the pane dies when the app exits, so the session disappears
    check("q exits the app", wait_for(lambda: not app_alive(), timeout=10))


def addr_of(host: str) -> str:
    """Strip the user@ prefix off a --host value, e.g. 'testuser@127.0.0.1' -> '127.0.0.1'."""
    return host.split("@", 1)[1] if "@" in host else host


def type_text(text: str) -> None:
    send_keys(*list(text))


def test_user_bus_is_user_bus(binary: str, host: str) -> None:
    """Regression: systemd-stdio-bridge --user can silently serve the system bus."""
    print("user bus scope:")
    start_app(binary, host, ["--scope", "all", "--limit-units", "sctui-*"])
    # the services column truncates long names, so match on the visible prefix rather
    # than the full ".service" suffix
    screen = wait_for(lambda: "sctui-test" in capture() and "sctui-user-test" in capture())
    check("both sctui units listed", screen, capture())

    type_text("user")
    time.sleep(1)
    send_keys("Down")
    time.sleep(1)
    result_screen = capture()
    check("sctui-user-test.service selected", "sctui-user-test.service" in result_screen, result_screen)
    # scope is shown on the Enablement line, e.g. "disabled · user"
    check("scope shows user", "· user" in result_screen, result_screen)


def test_root_action_round_trip(binary: str, root_host: str) -> None:
    print("root action round-trip:")
    # make the test idempotent: earlier tests/runs may have left the unit running
    run(["ssh", root_host, "systemctl", "stop", "sctui-test.service"])
    start_app(binary, root_host, ["--scope", "global", "--limit-units", "sctui-test.service"])
    wait_for(lambda: "Description:" in capture())
    send_keys("Down")
    check("starts inactive", wait_for(lambda: "inactive" in capture()), capture())

    send_keys("Enter")
    check("actions menu opens", wait_for(lambda: "Actions for" in capture()), capture())
    send_keys("s")
    check("start succeeds", wait_for(lambda: "active (running)" in capture(), timeout=20), capture())

    send_keys("Enter")
    check("actions menu opens again", wait_for(lambda: "Actions for" in capture()), capture())
    send_keys("t")
    check("stop succeeds", wait_for(lambda: "inactive (dead)" in capture(), timeout=20), capture())


def test_polkit_rejection(binary: str, host: str) -> None:
    print("polkit/permission rejection:")
    start_app(binary, host, ["--scope", "global", "--limit-units", "sctui-test.service"])
    wait_for(lambda: "Description:" in capture())
    send_keys("Down")
    send_keys("Enter")
    wait_for(lambda: "Actions for" in capture())
    send_keys("s")

    def error_shown() -> bool:
        screen = capture().lower()
        return any(term in screen for term in ("authentication", "denied", "failed", "error"))

    check("error surfaces instead of hanging", wait_for(error_shown, timeout=20), capture())
    check("app still alive", app_alive())
    check("unit did not become active", "active (running)" not in capture(), capture())
    send_keys("Escape")


def test_user_scope_action_succeeds(binary: str, host: str) -> None:
    print("user-scope action without root:")
    start_app(binary, host, ["--scope", "user", "--limit-units", "sctui-user-test.service"])
    wait_for(lambda: "Description:" in capture())
    send_keys("Down")
    send_keys("Enter")
    wait_for(lambda: "Actions for" in capture())
    send_keys("r")
    check("restart succeeds as user", wait_for(lambda: "active (running)" in capture(), timeout=20), capture())


def test_log_rendering(binary: str, host: str) -> None:
    print("log rendering:")
    start_app(binary, host, ["--limit-units", "sctui-marker.service", "--scope", "global"])
    wait_for(lambda: "Description:" in capture())
    send_keys("Down")
    check("boot marker appears in logs", wait_for(lambda: "sctui-boot-marker" in capture()), capture())


def run_markers(screen: str) -> set[str]:
    """Extract the unique per-start markers the dummy unit logs (sctui-run-<nanos>)."""
    return set(re.findall(r"sctui-run-\d+", screen))


def test_follow_mode_streams(binary: str, root_host: str) -> None:
    print("follow mode streaming:")
    run(["ssh", root_host, "systemctl", "start", "sctui-test.service"])
    start_app(binary, root_host, ["--limit-units", "sctui-test.service", "--scope", "global"])
    wait_for(lambda: "Description:" in capture())
    send_keys("Down")
    wait_for(lambda: run_markers(capture()), timeout=15)
    # the app attaches `journalctl --follow` only after the batch fetch returns; give it a
    # moment so the restart's log lines aren't dropped in the gap between the two
    time.sleep(3)
    seen = run_markers(capture())

    # restarting logs a fresh marker; it must stream in live via `journalctl --follow`
    run(["ssh", root_host, "systemctl", "restart", "sctui-test.service"])
    check(
        "new log lines stream in over follow",
        wait_for(lambda: run_markers(capture()) - seen, timeout=20),
        capture(),
    )
    # don't leave the unit running for later tests / reruns
    run(["ssh", root_host, "systemctl", "stop", "sctui-test.service"])


def test_journal_access_diagnostic(binary: str, nojournal_host: str) -> None:
    print("journal-access diagnostic:")
    # sctui-marker.service is a real system-scope unit with boot logs; reading them
    # requires systemd-journal group membership, which nojournal lacks. (dbus.service
    # doesn't work here: it's only an alias on dbus-broker distros like Fedora.)
    start_app(binary, nojournal_host, ["--scope", "global", "--limit-units", "sctui-marker.service"])
    wait_for(lambda: "Description:" in capture())
    send_keys("Down")
    check(
        "no-access diagnostic shown",
        wait_for(lambda: "No access to system logs" in capture() or "No logs" in capture()),
        capture(),
    )


def test_missing_bridge_error_path(binary: str, root_host: str) -> None:
    """Must run last: it temporarily breaks systemd-stdio-bridge on the remote host."""
    print("missing-bridge error path:")
    which = run(["ssh", root_host, "command", "-v", "systemd-stdio-bridge"])
    bridge_path = which.stdout.strip()
    if which.returncode != 0 or not bridge_path:
        check("systemd-stdio-bridge locatable on remote host", False, which.stdout + which.stderr)
        return

    moved = run(["ssh", root_host, "mv", bridge_path, "/root/bridge.bak"])
    if moved.returncode != 0:
        check("moved systemd-stdio-bridge out of the way", False, moved.stderr)
        return

    try:
        result = subprocess.run(
            [binary, "--host", root_host],
            capture_output=True,
            text=True,
            timeout=20,
        )
        check("nonzero exit when bridge is missing", result.returncode != 0, f"exit={result.returncode}")
        check(
            "clear error message in stderr",
            "systemd-stdio-bridge not found" in result.stderr,
            result.stderr,
        )
    finally:
        run(["ssh", root_host, "mv", "/root/bridge.bak", bridge_path])


def check_prerequisites(binary: str, host: str | None) -> str | None:
    if run(["which", "tmux"]).returncode != 0:
        return "tmux is not installed"
    if run(["test", "-x", binary]).returncode != 0:
        return f"binary not found at {binary} (run `cargo build` first)"
    if host:
        ssh_check = run(["ssh", "-o", "BatchMode=yes", "-o", "ConnectTimeout=5", host, "true"])
        if ssh_check.returncode != 0:
            return f"cannot ssh to {host} without a password: {ssh_check.stderr.strip()}"
    return None


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--host", default=None, help="ssh target for remote mode (default: test locally)")
    parser.add_argument("--binary", default="./target/debug/systemctl-tui")
    parser.add_argument(
        "--remote-suite",
        action="store_true",
        help="also run the remote-mode regression suite (requires --host)",
    )
    args = parser.parse_args()

    if args.remote_suite and not args.host:
        print("SKIP: --remote-suite requires --host")
        return 2

    problem = check_prerequisites(args.binary, args.host)
    if problem:
        print(f"SKIP: {problem}")
        return 2

    print(f"testing against: {args.host or 'local systemd'}\n")
    try:
        test_startup_and_browse(args.binary, args.host)
        test_logs(args.binary, args.host)
        test_mouse(args.binary, args.host)
        if not args.remote_suite:
            test_no_dropped_keystrokes(args.binary, args.host)
        test_clean_exit(args.binary, args.host)

        if args.remote_suite:
            addr = addr_of(args.host)
            root_host = f"root@{addr}"
            nojournal_host = f"nojournal@{addr}"

            test_user_bus_is_user_bus(args.binary, args.host)
            test_root_action_round_trip(args.binary, root_host)
            test_polkit_rejection(args.binary, args.host)
            test_user_scope_action_succeeds(args.binary, args.host)
            test_log_rendering(args.binary, args.host)
            test_follow_mode_streams(args.binary, root_host)
            test_journal_access_diagnostic(args.binary, nojournal_host)
            # last: this one breaks systemd-stdio-bridge on the remote host
            test_missing_bridge_error_path(args.binary, root_host)
    finally:
        stop_app()

    print(f"\n{len(passed)} passed, {len(failed)} failed")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
