#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
  Quit,
  Resume,
  Suspend,
  RenderTick,
  Resize(u16, u16),
  ToggleShowLogger,
  EnterNormal,
  EnterSearch,
  EnterHelp,
  ExitHelp,
  SetLogs { unit_name: String, logs: String },
  ScrollUp(u16),
  ScrollDown(u16),
  Update,
  Noop,
}
