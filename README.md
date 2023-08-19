# systemctl-tui

## Credits

Based on the excellent [`ratatui-template`](https://github.com/kdheepak/ratatui-template/)

<img width="1512" alt="image" src="https://github.com/kdheepak/ratatui-template/assets/1813121/61d9f3a4-14d7-4bb8-85be-771fd5da4c0f">

### Features

- Uses [tokio](https://tokio.rs/) for async events
- Logs with [tui-logger](https://github.com/gin66/tui-logger) and [tracing](https://github.com/tokio-rs/tracing)
- [better-panic](https://github.com/mitsuhiko/better-panic)
- Clap for command line argument parsing
- App with `Component` trait, with [`Home`](./src/components/home.rs) and [`Logger`](./src/components/logger.rs) components as examples

### Usage

- Clone the repository
- Rename `ratatui-template` and `RATATUI_TEMPLATE` with your `app-name` and `APP_NAME` in the following files:
  - `src/main.rs`
  - `src/utils.rs`
  - `build.rs`
  - `Cargo.toml`
  - `README.md`
  - `.github/workflows/cd.yml`
  - `.envrc`
