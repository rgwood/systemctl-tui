# systemctl-tui

[![Crates.io](https://img.shields.io/crates/v/systemctl-tui.svg)](https://crates.io/crates/systemctl-tui)

A simple little TUI for interacting with systemd services and their logs.

![image](https://github.com/rgwood/systemctl-tui/assets/26268125/0202cec5-45a9-4cef-977e-323ca66faf7d)

## Install

Currently you need [Rust installed](https://rustup.rs/). Either:

1. Run `cargo install systemctl-tui`
2. Clone the repo `cargo build --release` to get a release binary at `target/release/systemctl-tui`

## Future Work

This is a prototype hacked together in a weekend. It currently only supports read operations (so no stopping/starting services). More features to come.

## Credits

- Inspired by the truly excellent [Lazygit](https://github.com/jesseduffield/lazygit).
- Based on the excellent [`ratatui-template`](https://github.com/kdheepak/ratatui-template/)
- systemd code partially taken from [`servicer`](https://github.com/servicer-labs/servicer)
