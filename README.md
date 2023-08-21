# systemctl-tui

A simple little TUI for interacting with systemd services and their logs.

## Install

For now, clone the repo and run `cargo build --release` to get a release binary at `target/release/systemctl-tui`. Later I'll put this on crates

## Future Work

This is a prototype hacked together in a weekend. It currently only supports read operations (so no stopping/starting services). More features to come.

## Credits

- Inspired by the truly excellent [Lazygit](https://github.com/jesseduffield/lazygit).
- Based on the excellent [`ratatui-template`](https://github.com/kdheepak/ratatui-template/)
- systemd code partially taken from [`servicer`](https://github.com/servicer-labs/servicer)