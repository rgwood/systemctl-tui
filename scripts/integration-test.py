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
    ./scripts/integration-test.py                        # local
    ./scripts/integration-test.py --host user@hostname   # remote

Requires: tmux and a debug build (`cargo build`). Remote mode needs
passwordless ssh to the target.
"""

import argparse
import subprocess
import sys
import time

SESSION = "sctui-integration-test"

passed: list[str] = []
failed: list[str] = []


def run(cmd: list[str], **kwargs) -> subprocess.CompletedProcess:
    return subprocess.run(cmd, capture_output=True, text=True, timeout=30, **kwargs)


def tmux(*args: str) -> subprocess.CompletedProcess:
    return run(["tmux", *args])


def capture() -> str:
    return tmux("capture-pane", "-t", SESSION, "-p").stdout


def send_keys(*keys: str, delay: float = 0.0) -> None:
    for key in keys:
        tmux("send-keys", "-t", SESSION, key)
        if delay:
            time.sleep(delay)


def start_app(binary: str, host: str | None, extra_args: list[str] | None = None) -> None:
    tmux("kill-session", "-t", SESSION)
    host_args = ["--host", host] if host else []
    args = [binary, *host_args, *(extra_args or [])]
    cmd = " ".join(f"'{a}'" for a in args)
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
    # dbus.service exists and has run on any systemd machine
    start_app(binary, host, ["--limit-units", "dbus.service"])
    wait_for(lambda: "Details" in capture())
    send_keys("Down")

    def logs_or_diagnostic() -> bool:
        screen = capture()
        rendered_logs = "systemd[1]" in screen or "dbus" in screen.lower()
        diagnostic = "No access to system logs" in screen or "No logs" in screen
        return rendered_logs or diagnostic

    check("logs pane shows logs or diagnostic", wait_for(logs_or_diagnostic), capture())
    check("app still alive", app_alive())


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
    args = parser.parse_args()

    problem = check_prerequisites(args.binary, args.host)
    if problem:
        print(f"SKIP: {problem}")
        return 2

    print(f"testing against: {args.host or 'local systemd'}\n")
    try:
        test_startup_and_browse(args.binary, args.host)
        test_logs(args.binary, args.host)
        test_no_dropped_keystrokes(args.binary, args.host)
        test_clean_exit(args.binary, args.host)
    finally:
        stop_app()

    print(f"\n{len(passed)} passed, {len(failed)} failed")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
