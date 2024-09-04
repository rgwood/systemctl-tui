use crate::{
  components::home::Mode,
  systemd::{UnitId, UnitWithStatus},
};

#[derive(Debug, Clone)]
pub enum Action {
  Quit,
  Resume,
  Suspend,
  Render,
  DebouncedRender,
  SpinnerTick,
  Resize(u16, u16),
  ToggleShowLogger,
  RefreshServices,
  SetServices(Vec<UnitWithStatus>),
  EnterMode(Mode),
  EnterError { err: String },
  CancelTask,
  ToggleHelp,
  SetUnitFilePath { unit: UnitId, path: String },
  CopyUnitFilePath,
  SetUnitDescription { unit: UnitId, description: String },
  SetLogs { unit: UnitId, logs: Vec<String> },
  AppendLogLine { unit: UnitId, line: String },
  StartService(UnitId),
  StopService(UnitId),
  RestartService(UnitId),
  ReloadService(UnitId),
  EnableService(UnitId),
  DisableService(UnitId),
  ScrollUp(u16),
  ScrollDown(u16),
  ScrollToTop,
  ScrollToBottom,
  Noop,
}
