When testing systemctl-tui, run it in debug mode (i.e. do not specify `--release`).

## Repo layout

This is a Cargo workspace with three crates:

- `crates/systemctl-core` — shared systemd/journald/SSH plumbing, no UI dependencies
- `crates/systemctl-tui` — the ratatui TUI (the main published binary)
- `crates/systemctl-gui` — a GTK 4 GUI. Excluded from workspace default-members because it needs GTK system libraries (`libgtk-4-dev`); build it explicitly with `cargo build -p systemctl-gui`.

A plain `cargo build`/`cargo test`/`cargo clippy` covers core + TUI only.

## Before committing or publishing

Always run `cargo fmt` and `cargo clippy` before committing changes or creating a PR. Both must pass cleanly.

## Testing with tmux

The full tmux test battery below is slow — only run it when explicitly asked or before creating a PR. For routine changes, fmt+clippy+build is sufficient.

Since this is a TUI app, automated testing requires tmux to provide a virtual terminal. The general pattern:

```bash
# Launch in a detached tmux session
tmux new-session -d -s sctui-test -x 120 -y 40 \
  './target/debug/systemctl-tui 2>/tmp/sctui-stderr.log'
sleep 2

# Send keystrokes
tmux send-keys -t sctui-test 'j'       # navigate down
tmux send-keys -t sctui-test 'C-f'     # ctrl+f to open search
tmux send-keys -t sctui-test 'docker'  # type search query
tmux send-keys -t sctui-test 'Escape'  # close search/dialog
tmux send-keys -t sctui-test 'Enter'   # open actions dialog
tmux send-keys -t sctui-test '?'       # help screen
tmux send-keys -t sctui-test 'q'       # quit

# Check the process is still alive (hasn't crashed)
tmux list-panes -t sctui-test -F '#{pane_pid} #{pane_dead}'

# Test terminal resize handling
tmux resize-pane -t sctui-test -x 80 -y 24

# Clean up
tmux kill-session -t sctui-test 2>/dev/null
```

Use `tmux capture-pane -t sctui-test -p` to read the rendered screen and assert on its contents (it captures alternate-screen apps fine). The primary signals for a passing test are:

1. Process is still alive after each interaction
2. `capture-pane` shows the expected content (services list, dialogs, logs)
3. No panics or errors in stderr
4. Process exits cleanly on `q` (note: press `Escape` first if in search mode, where `q` is just text input)

`tests/integration-test.py` automates this checklist end-to-end (local by default, or `--host user@hostname` for remote mode; it includes a keystroke-drop regression test that manual testing tends to miss). Prefer running it over hand-rolling tmux commands.

Note: the TUI renders to **stderr**, so don't redirect `2>` when driving it in tmux — you'll get a blank-looking pane and think the app is hung.

### systemd version matrix

`tests/remote-matrix.py` runs the remote-mode test suite against containers running real systemd versions (239→current: Rocky 8, Ubuntu 20.04/22.04/24.04, Debian 12, Fedora), plus two "hostile" hosts where remote mode must fail fast with a clear error: `no-systemd` (alpine) and `dead-systemd` (systemd installed but not booted). Pre-239 systemd can't boot in containers on cgroup-v2 hosts (and pre-230 lacks `ListUnitsByPatterns`), so graceful failure is the only testable contract for genuinely old hosts. It builds systemd+sshd container images, waits for system and user managers, and runs `integration-test.py --host ... --remote-suite` against each. Needs podman or docker. `--distro ubuntu-24.04` to run one; `--keep` leaves a failed container up for debugging. Slow (~10-20 min for the full matrix) — run it when touching remote-mode code (`crates/systemctl-core/src/ssh.rs`, journalctl/D-Bus plumbing) or before a release, not for routine changes. CI runs it on every PR.

### Test checklist

When validating changes, run through these scenarios:

- **Launch**: app starts and renders without crashing
- **Navigation**: `j`/`k`/`G`/`g` move the selection
- **Search**: `Ctrl+f` or `/` opens search, typing filters the list, `Escape` clears
- **Help screen**: `?` or `F1` opens, `Escape` closes
- **Actions dialog**: `Enter` opens, `Escape` closes
- **Resize**: shrink and grow the terminal (including very small sizes like 40x15)
- **CLI flags**: `--help`, `--version`, `--scope user`, `--limit-units "*.timer"`
- **Clean exit**: `q` exits with code 0
