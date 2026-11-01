//! Parsing of ANSI escape sequences in log messages.
//!
//! Services often log with colors. We remove every escape sequence from the text (so the
//! text is safe to display and export) and keep any SGR styling as UI-neutral byte ranges.
//! Other sequences (cursor movement, hyperlinks, window titles) are dropped.

use std::ops::Range;
use vte::{Params, Parser, Perform};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnsiColor {
  /// Palette color: 0-15 are the terminal's standard+bright colors, 16-255 the extended palette.
  Indexed(u8),
  Rgb(u8, u8, u8),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AnsiStyle {
  pub fg: Option<AnsiColor>,
  pub bg: Option<AnsiColor>,
  pub bold: bool,
  pub dim: bool,
  pub italic: bool,
  pub underline: bool,
  pub reverse: bool,
  pub strikethrough: bool,
}

/// A styled part of a string. Ranges are byte offsets, in order, and don't overlap; text
/// outside of any run has no ANSI styling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StyledRun {
  pub range: Range<usize>,
  pub style: AnsiStyle,
}

/// Split a string into its text (with all escape sequences removed) and styled runs.
pub fn parse_ansi(input: &str) -> (String, Vec<StyledRun>) {
  if !input.contains('\x1b') {
    return (input.to_string(), Vec::new());
  }

  let mut performer = AnsiText { text: String::with_capacity(input.len()), ..Default::default() };
  Parser::new().advance(&mut performer, input.as_bytes());
  performer.finish_run();
  (performer.text, performer.runs)
}

#[derive(Default)]
struct AnsiText {
  text: String,
  runs: Vec<StyledRun>,
  style: AnsiStyle,
  run_start: usize,
}

impl AnsiText {
  fn finish_run(&mut self) {
    if self.style != AnsiStyle::default() && self.text.len() > self.run_start {
      self.runs.push(StyledRun { range: self.run_start..self.text.len(), style: self.style });
    }
  }
}

impl Perform for AnsiText {
  fn print(&mut self, c: char) {
    self.text.push(c);
  }

  fn execute(&mut self, byte: u8) {
    // The other control characters don't mean anything in a single line of text
    if byte == b'\t' {
      self.text.push('\t');
    }
  }

  fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], ignore: bool, action: char) {
    // Private sequences that end in `m`, like `ESC[>4;2m`, have their marker in `intermediates`
    if action != 'm' || ignore || !intermediates.is_empty() {
      return;
    }
    let mut new_style = self.style;
    apply_sgr(&mut new_style, params);
    if new_style != self.style {
      self.finish_run();
      self.style = new_style;
      self.run_start = self.text.len();
    }
  }
}

fn apply_sgr(style: &mut AnsiStyle, params: &Params) {
  let mut params = params.iter();
  while let Some(param) = params.next() {
    match param {
      [0] => *style = AnsiStyle::default(),
      [1] => style.bold = true,
      [2] => style.dim = true,
      [3] => style.italic = true,
      [4, 0] => style.underline = false,
      // Includes styled underlines like `4:3`
      [4] | [4, _] => style.underline = true,
      [7] => style.reverse = true,
      [9] => style.strikethrough = true,
      [22] => {
        style.bold = false;
        style.dim = false;
      },
      [23] => style.italic = false,
      [24] => style.underline = false,
      [27] => style.reverse = false,
      [29] => style.strikethrough = false,
      [c @ 30..=37] => style.fg = Some(AnsiColor::Indexed(*c as u8 - 30)),
      [38, subparams @ ..] => style.fg = extended_color(subparams, &mut params),
      [39] => style.fg = None,
      [c @ 40..=47] => style.bg = Some(AnsiColor::Indexed(*c as u8 - 40)),
      [48, subparams @ ..] => style.bg = extended_color(subparams, &mut params),
      [49] => style.bg = None,
      [c @ 90..=97] => style.fg = Some(AnsiColor::Indexed(*c as u8 - 90 + 8)),
      [c @ 100..=107] => style.bg = Some(AnsiColor::Indexed(*c as u8 - 100 + 8)),
      _ => {},
    }
  }
}

/// The rest of a 256-color or truecolor code, which can be `38;5;n` and `38;2;r;g;b`, or
/// `38:5:n` and `38:2:r:g:b` (optionally with a colorspace, `38:2:cs:r:g:b`).
fn extended_color<'a>(subparams: &[u16], params: &mut impl Iterator<Item = &'a [u16]>) -> Option<AnsiColor> {
  let byte = |value: &u16| u8::try_from(*value).ok();
  match subparams {
    [5, n] => Some(AnsiColor::Indexed(byte(n)?)),
    [2, r, g, b] | [2, _, r, g, b] => Some(AnsiColor::Rgb(byte(r)?, byte(g)?, byte(b)?)),
    [] => {
      let mut next = || params.next()?.first().and_then(byte);
      match next()? {
        5 => Some(AnsiColor::Indexed(next()?)),
        2 => Some(AnsiColor::Rgb(next()?, next()?, next()?)),
        _ => None,
      }
    },
    _ => None,
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn text_of(input: &str) -> String {
    parse_ansi(input).0
  }

  #[test]
  fn plain_text_is_untouched() {
    assert_eq!(parse_ansi("no escapes here"), ("no escapes here".to_string(), vec![]));
  }

  #[test]
  fn parses_the_line_from_the_bug_report() {
    let (text, runs) = parse_ansi("\x1b[2m15:26:32\x1b[0m \x1b[92mINF\x1b[0m Script exited OK");
    assert_eq!(text, "15:26:32 INF Script exited OK");
    assert_eq!(
      runs,
      vec![
        StyledRun { range: 0..8, style: AnsiStyle { dim: true, ..Default::default() } },
        StyledRun { range: 9..12, style: AnsiStyle { fg: Some(AnsiColor::Indexed(10)), ..Default::default() } },
      ]
    );
  }

  #[test]
  fn parses_extended_colors() {
    let (text, runs) = parse_ansi("\x1b[38;5;208ma\x1b[38;2;1;2;3mb\x1b[48;5;4mc");
    assert_eq!(text, "abc");
    assert_eq!(runs[0].style.fg, Some(AnsiColor::Indexed(208)));
    assert_eq!(runs[1].style.fg, Some(AnsiColor::Rgb(1, 2, 3)));
    assert_eq!(runs[2].style.fg, Some(AnsiColor::Rgb(1, 2, 3)));
    assert_eq!(runs[2].style.bg, Some(AnsiColor::Indexed(4)));
  }

  #[test]
  fn parses_colon_separated_colors() {
    let (text, runs) = parse_ansi("\x1b[38:5:208ma\x1b[38:2:1:2:3mb\x1b[38:2::4:5:6mc");
    assert_eq!(text, "abc");
    assert_eq!(runs[0].style.fg, Some(AnsiColor::Indexed(208)));
    assert_eq!(runs[1].style.fg, Some(AnsiColor::Rgb(1, 2, 3)));
    assert_eq!(runs[2].style.fg, Some(AnsiColor::Rgb(4, 5, 6)));
  }

  #[test]
  fn private_sequences_ending_in_m_are_not_styles() {
    // xterm's modifyOtherKeys. The `2` here is not "dim".
    assert_eq!(parse_ansi("\x1b[>4;2mx"), ("x".to_string(), vec![]));
    assert_eq!(parse_ansi("\x1b[?4;2mx"), ("x".to_string(), vec![]));
  }

  #[test]
  fn tabs_are_kept() {
    assert_eq!(text_of("\x1b[0ma\tb"), "a\tb");
  }

  #[test]
  fn styles_accumulate_and_reset_individually() {
    let (_, runs) = parse_ansi("\x1b[1;36ma\x1b[39mb\x1b[22mc");
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[0].style, AnsiStyle { bold: true, fg: Some(AnsiColor::Indexed(6)), ..Default::default() });
    assert_eq!(runs[1].style, AnsiStyle { bold: true, ..Default::default() });
  }

  #[test]
  fn empty_sgr_is_a_reset() {
    let (text, runs) = parse_ansi("\x1b[31mred\x1b[m plain");
    assert_eq!(text, "red plain");
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].range, 0..3);
  }

  #[test]
  fn unterminated_style_runs_to_the_end() {
    let (_, runs) = parse_ansi("a\x1b[31mbcd");
    assert_eq!(runs[0].range, 1..4);
  }

  #[test]
  fn offsets_are_bytes_with_multibyte_text() {
    let (text, runs) = parse_ansi("→\x1b[31mé\x1b[0m!");
    assert_eq!(text, "→é!");
    assert_eq!(&text[runs[0].range.clone()], "é");
  }

  #[test]
  fn non_style_sequences_are_removed_without_losing_text() {
    assert_eq!(text_of("\x1b[2K\x1b[1Gprogress"), "progress");
    assert_eq!(text_of("\x1b[?25lhide"), "hide");
    assert_eq!(text_of("\x1b]8;;https://example.com\x1b\\link\x1b]8;;\x1b\\ done"), "link done");
    assert_eq!(text_of("\x1b]0;title\x07after"), "after");
    assert_eq!(text_of("\x1b(B\x1b[mafter"), "after");
    assert_eq!(text_of("\x1bcafter"), "after");
  }

  #[test]
  fn malformed_sequences_lose_little_text() {
    assert_eq!(text_of("trailing\x1b"), "trailing");
    assert_eq!(text_of("trunc\x1b[3"), "trunc");
    // A new escape sequence ends an unterminated OSC, so the text after it survives
    assert_eq!(text_of("\x1b]title\x1b[31mred"), "red");
    // Non-ASCII inside an escape sequence is invalid. vte ignores everything up to the next final byte.
    assert_eq!(text_of("\x1b[1é after"), "fter");
    assert_eq!(text_of("\x1bé after"), "fter");
    assert_eq!(text_of("\x1b\x1b[0mdouble"), "double");
    // Bad colors don't take the following codes down with them, or leave a style behind
    assert_eq!(parse_ansi("\x1b[38;5mx"), ("x".to_string(), vec![]));
    assert_eq!(parse_ansi("\x1b[999mx"), ("x".to_string(), vec![]));
  }

  #[test]
  fn unterminated_osc_swallows_the_rest_of_the_string() {
    // This matches what a real terminal does. Each log line is parsed separately, so the damage
    // stops at the end of the line.
    assert_eq!(text_of("before\x1b]no terminator"), "before");
  }
}
