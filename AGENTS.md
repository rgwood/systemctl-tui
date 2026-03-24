When testing systemctl-tui, run it in debug mode (i.e. do not specify `--release`).

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

Note: `tmux capture-pane -p` returns empty content for alternate-screen TUI apps. To verify rendering, check stderr (where the TUI escape codes go) or just confirm the process hasn't crashed. The primary signals for a passing test are:

1. Process is still alive after each interaction
2. No panics or errors in stderr
3. Process exits cleanly on `q`

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
