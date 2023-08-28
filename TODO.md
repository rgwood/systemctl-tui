## TODO

- [x] Overhaul rendering for lower CPU usage
Calling render on an interval uses a fair bit of CPU. The app can idle at low-double-digits CPU % on a slower machines, which sucks
Did some profiling and it seems like a lot of this is Ratatui's diffing; we are making it diff a lot of content with the logs etc.
I think we should only refresh on demand to fix this. Remove the render_tick interval completely.
Will need to do some testing after this.
Longer-term, probably want to do some more perf work on the render() function; might be able to speed it up

- [ ] display error (like when start/stop fails)
- [ ] unit files
  - [ ] figure out path to unit file
  - [ ] show/preview contents
  - [ ] command to open service file in text editor
- [ ] action to reload (do this automatically?)
- [ ] use journalctl -f to follow logs for instant refresh
- [x] display spinner while starting up service
  - [x] generalize spinner logic to all actions
- [x] refresh logs on a timer
- [x] refresh services on a timer
- [x] put on crates.io
- [x] Implement scrolling with pgup/pgdown
- [x] try adding a modal help menu/command picker like x/? in lazygit
- [x] when searching, auto-select the first result
- [x] select first item by default
- [x] add color for stopped/running status
- [x] add some color (for dates maybe?)
- [x] ctrl-f for find
- [x] move logs to their own pane
