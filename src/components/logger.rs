use anyhow::Result;
use log::LevelFilter;
use ratatui::{
  layout::Rect,
  style::{Color, Style},
  widgets::{Block, Borders},
};
use tokio::sync::mpsc::UnboundedSender;
use tui_logger::{TuiLoggerLevelOutput, TuiLoggerWidget, TuiWidgetState};

use super::{Component, Frame};
use crate::action::Action;

#[derive(Default)]
pub struct Logger {
  state: TuiWidgetState,
}

impl Component for Logger {
  fn init(&mut self, _: UnboundedSender<Action>) -> Result<()> {
    self.state = TuiWidgetState::new().set_default_display_level(LevelFilter::Debug);
    Ok(())
  }

  fn render(&mut self, f: &mut Frame<'_>, rect: Rect) {
    let w = TuiLoggerWidget::default()
      .block(Block::default().title("Log").borders(Borders::ALL))
      .style_error(Style::default().fg(Color::Red))
      .style_debug(Style::default().fg(Color::Green))
      .style_warn(Style::default().fg(Color::Yellow))
      .style_trace(Style::default().fg(Color::Magenta))
      .style_info(Style::default().fg(Color::Cyan))
      .output_separator(':')
      .output_timestamp(Some("%H:%M:%S".to_string()))
      .output_level(Some(TuiLoggerLevelOutput::Long))
      .output_target(false)
      .output_file(true)
      .output_line(true)
      .state(&self.state);
    f.render_widget(w, rect);
  }
}
