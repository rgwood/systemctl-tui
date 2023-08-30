# systemctl-tui

[![Crates.io](https://img.shields.io/crates/v/systemctl-tui.svg)](https://crates.io/crates/systemctl-tui)

A fast, simple TUI for interacting with [systemd](https://en.wikipedia.org/wiki/Systemd) services and their logs.

![image](https://github.com/rgwood/systemctl-tui/assets/26268125/da1d4f06-ea8d-4ea0-805e-d0e26e641cd6)

`systemctl-tui` can quickly browse service status and logs, as well as start/stop/restart services. It aims to do a small number of things well.

## Install

This project only works on Linux. Currently you need to build from scratch with [Rust installed](https://rustup.rs/). Either:

1. Run `cargo install systemctl-tui`
2. Clone the repo and run `cargo build --release` to get a release binary at `target/release/systemctl-tui`

#### Optional:

1. Alias `systemctl-tui` to `st` for quick access
2. Create a symlink so `systemctl=tui` can be used with sudo:
```sh
sudo ln -s ~/.cargo/bin/systemctl-tui /usr/bin/systemctl-tui
```

## Help
![image](https://github.com/rgwood/systemctl-tui/assets/26268125/512f269d-e221-4fa0-9479-a48f1b1a3f8d)

## Credits

- Inspired by the truly excellent [Lazygit](https://github.com/jesseduffield/lazygit).
- Used [`ratatui-template`](https://github.com/kdheepak/ratatui-template/) to get started
- systemd code partially taken from [`servicer`](https://github.com/servicer-labs/servicer)
