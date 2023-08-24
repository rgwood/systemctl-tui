use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone)]
pub enum Action {
  Quit,
  Resume,
  Suspend,
  RenderTick,
  Resize(u16, u16),
  ToggleShowLogger,
  EnterNormal,
  EnterSearch,
  EnterActionMenu,
  EnterProcessing,
  SetCancellationToken(CancellationToken),
  CancelTask,
  ToggleHelp,
  SetLogs { unit_name: String, logs: String },
  StartService(String),
  ScrollUp(u16),
  ScrollDown(u16),
  ScrollToTop,
  ScrollToBottom,
  Update,
  Noop,
}
