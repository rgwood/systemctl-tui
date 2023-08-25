use crate::systemd::UnitStatus;

#[derive(Debug, Clone)]
pub enum Action {
  Quit,
  Resume,
  Suspend,
  RenderTick,
  Resize(u16, u16),
  ToggleShowLogger,
  EnterNormal,
  RefreshServices,
  SetServices(Vec<UnitStatus>),
  EnterSearch,
  EnterActionMenu,
  EnterProcessing,
  CancelTask,
  ToggleHelp,
  SetLogs { unit_name: String, logs: String },
  StartService(String),
  StopService(String),
  RestartService(String),
  ReloadService(String),
  EnableService(String),
  DisableService(String),
  ScrollUp(u16),
  ScrollDown(u16),
  ScrollToTop,
  ScrollToBottom,
  Update,
  Noop,
}
