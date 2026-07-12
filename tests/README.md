# Tests

systemctl-tui is a full-screen terminal app, so its integration tests drive the real binary inside tmux and assert on the rendered screen. (Rust unit tests live in `src/` next to the code they test, as usual.)

## integration-test.py

The main test suite. Launches the app in a tmux session, sends keystrokes, and checks the captured screen: startup, navigation, search, the actions menu, log rendering, resize handling, a keystroke-drop regression test, and clean exit.

```
./tests/integration-test.py                        # against local systemd
./tests/integration-test.py --host user@hostname   # against a remote machine over ssh
```

Needs tmux and a debug build (`cargo build`). `--remote-suite` adds tests that assume the fixture units and users baked into the containers below — use it via `remote-matrix.py`, not against a real host.

## remote-matrix.py

Runs the remote-mode suite against containers running real systemd versions — 239 (Rocky 8) through current Fedora — plus two "hostile" hosts where remote mode cannot work and must fail with a clear error instead of hanging: `no-systemd` (alpine) and `dead-systemd` (systemd installed but not booted).

```
./tests/remote-matrix.py                        # full matrix (~10-20 min cold)
./tests/remote-matrix.py --distro ubuntu-24.04  # one distro
./tests/remote-matrix.py --keep                 # keep failed containers for debugging
```

Needs podman or docker. `remote-matrix/` holds the Containerfiles and fixture units it builds from. Each container gets systemd as PID 1, sshd, and three test users with different permission levels; the harness reaches it through an `ssh` shim on `PATH`, so your real ssh config and known_hosts are never touched.

CI runs everything here on every PR. See AGENTS.md for more detail on the tmux testing approach.
