//! Parsing of ANSI escape sequences in log messages.
//!
//! Services often log with colors. We remove every escape sequence from the text (so the
//! text is safe to display and export) and keep any SGR styling as UI-neutral byte ranges.
//! Other sequences (cursor movement, hyperlinks, window titles) are dropped.

use std::ops::Range;

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

  let mut text = String::with_capacity(input.len());
  let mut runs = Vec::new();
  let mut style = AnsiStyle::default();
  let mut run_start = 0;
  let mut rest = input;

  while let Some(esc) = rest.find('\x1b') {
    text.push_str(&rest[..esc]);
    let sequence = &rest[esc + 1..];
    let len = sequence_len(sequence);

    if let Some(params) = sequence[..len].strip_prefix('[').and_then(|csi| csi.strip_suffix('m')) {
      let mut new_style = style;
      apply_sgr(&mut new_style, params);
      if new_style != style {
        if style != AnsiStyle::default() && text.len() > run_start {
          runs.push(StyledRun { range: run_start..text.len(), style });
        }
        style = new_style;
        run_start = text.len();
      }
    }
    rest = &sequence[len..];
  }
  text.push_str(rest);

  if style != AnsiStyle::default() && text.len() > run_start {
    runs.push(StyledRun { range: run_start..text.len(), style });
  }
  (text, runs)
}

/// The length in bytes of the escape sequence whose ESC has just been consumed. Everything
/// that gets counted is ASCII, so the result is always a char boundary.
fn sequence_len(sequence: &str) -> usize {
  let bytes = sequence.as_bytes();
  match bytes.first() {
    // CSI: parameter and intermediate bytes, then a final byte
    Some(b'[') => {
      let body = bytes[1..].iter().take_while(|b| (0x20..=0x3f).contains(*b)).count();
      let has_final = bytes.get(1 + body).is_some_and(|b| (0x40..=0x7e).contains(b));
      1 + body + usize::from(has_final)
    },
    // OSC, DCS, SOS, PM, APC: a string ended by BEL or ST (ESC \). If it's never ended,
    // only remove the introducer; better to show junk than to lose log text.
    Some(b']' | b'P' | b'X' | b'^' | b'_') => {
      let bel = sequence.find('\x07').map(|i| i + 1);
      let st = sequence.find("\x1b\\").map(|i| i + 2);
      bel.into_iter().chain(st).min().unwrap_or(1)
    },
    // nF escapes like `ESC ( B` (which is part of what `tput sgr0` emits), and other 2-byte escapes
    Some(_) => {
      let intermediates = bytes.iter().take_while(|b| (0x20..=0x2f).contains(*b)).count();
      let has_final = bytes.get(intermediates).is_some_and(|b| (0x30..=0x7e).contains(b));
      intermediates + usize::from(has_final)
    },
    None => 0,
  }
}

fn apply_sgr(style: &mut AnsiStyle, params: &str) {
  // An empty parameter means 0. Anything we can't parse (like colon-separated colors) is ignored.
  let mut codes = params.split(';').map(|p| if p.is_empty() { Some(0) } else { p.parse::<u8>().ok() });
  while let Some(code) = codes.next() {
    match code {
      Some(0) => *style = AnsiStyle::default(),
      Some(1) => style.bold = true,
      Some(2) => style.dim = true,
      Some(3) => style.italic = true,
      Some(4) => style.underline = true,
      Some(7) => style.reverse = true,
      Some(9) => style.strikethrough = true,
      Some(22) => {
        style.bold = false;
        style.dim = false;
      },
      Some(23) => style.italic = false,
      Some(24) => style.underline = false,
      Some(27) => style.reverse = false,
      Some(29) => style.strikethrough = false,
      Some(c @ 30..=37) => style.fg = Some(AnsiColor::Indexed(c - 30)),
      Some(38) => style.fg = extended_color(&mut codes),
      Some(39) => style.fg = None,
      Some(c @ 40..=47) => style.bg = Some(AnsiColor::Indexed(c - 40)),
      Some(48) => style.bg = extended_color(&mut codes),
      Some(49) => style.bg = None,
      Some(c @ 90..=97) => style.fg = Some(AnsiColor::Indexed(c - 90 + 8)),
      Some(c @ 100..=107) => style.bg = Some(AnsiColor::Indexed(c - 100 + 8)),
      _ => {},
    }
  }
}

/// The rest of a `38;5;n` or `38;2;r;g;b` color.
fn extended_color(codes: &mut impl Iterator<Item = Option<u8>>) -> Option<AnsiColor> {
  match codes.next()?? {
    5 => Some(AnsiColor::Indexed(codes.next()??)),
    2 => Some(AnsiColor::Rgb(codes.next()??, codes.next()??, codes.next()??)),
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
  fn malformed_sequences_dont_panic_or_lose_text() {
    assert_eq!(text_of("trailing\x1b"), "trailing");
    assert_eq!(text_of("trunc\x1b[3"), "trunc");
    assert_eq!(text_of("\x1b]no terminator"), "no terminator");
    assert_eq!(text_of("\x1b[1é"), "é");
    assert_eq!(text_of("\x1bé"), "é");
    assert_eq!(text_of("\x1b\x1b[0mdouble"), "double");
    // Bad colors don't take the following codes down with them, or leave a style behind
    assert_eq!(parse_ansi("\x1b[38;5mx"), ("x".to_string(), vec![]));
    assert_eq!(parse_ansi("\x1b[999mx"), ("x".to_string(), vec![]));
    assert_eq!(parse_ansi("\x1b[38:5:208mx"), ("x".to_string(), vec![]));
  }
}
