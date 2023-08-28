# systemctl-tui

[![Crates.io](https://img.shields.io/crates/v/systemctl-tui.svg)](https://crates.io/crates/systemctl-tui)

A fast, simple TUI for interacting with systemd services and their logs.

![image](https://github.com/rgwood/systemctl-tui/assets/26268125/da1d4f06-ea8d-4ea0-805e-d0e26e641cd6)


## Install

This project only works on Linux. Currently you need to build from scratch with [Rust installed](https://rustup.rs/). Either:

1. Run `cargo install systemctl-tui`
2. Clone the repo and run `cargo build --release` to get a release binary at `target/release/systemctl-tui`

Optional: alias `systemctl-tui` to `st` for quick access

Once the project has matured a bit I'll look into other package managers.

## Help
![image](https://github.com/rgwood/systemctl-tui/assets/26268125/512f269d-e221-4fa0-9479-a48f1b1a3f8d)

## Credits

- Inspired by the truly excellent [Lazygit](https://github.com/jesseduffield/lazygit).
- Used [`ratatui-template`](https://github.com/kdheepak/ratatui-template/) to get started
- systemd code partially taken from [`servicer`](https://github.com/servicer-labs/servicer)
