#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
  Quit,
  Resume,
  Suspend,
  Tick,
  RenderTick,
  Resize(u16, u16),
  ToggleShowLogger,
  EnterNormal,
  EnterSearch,
  EnterProcessing,
  ExitProcessing,
  SetLogs { unit_name: String, logs: String},
  Update,
  Noop,
}
