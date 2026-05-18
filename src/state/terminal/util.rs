use std::{fmt::Display, os::fd::RawFd};

use nix::{
  libc,
  sys::termios::{self, Termios},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::try_var;

pub(crate) fn calc_str_width(s: &str) -> usize {
  let mut esc_seq = 0;
  s.graphemes(true).map(|g| width(g, &mut esc_seq)).sum()
}

pub(crate) fn truncate_visual(s: &str, max_width: usize) -> String {
  let mut out = String::new();
  let mut visible = 0;
  let mut esc_seq = 0u8;
  let mut wrote_anything_visible = false;

  for g in s.graphemes(true) {
    let w = width(g, &mut esc_seq);
    if esc_seq == 0 && visible + w > max_width {
      break;
    }
    out.push_str(g);
    visible += w;
    if w > 0 {
      wrote_anything_visible = true;
    }
  }

  if wrote_anything_visible {
    out.push_str("\x1b[0m");
  }
  out
}

pub(crate) fn truncate_with_ellipsis(s: &str, max_width: usize) -> String {
  if calc_str_width(s) <= max_width {
    return s.to_string();
  }
  if max_width <= 3 {
    // Not enough room even for the ellipsis itself; just hard-truncate.
    return truncate_visual(s, max_width);
  }
  let mut out = truncate_visual(s, max_width - 3);
  out.push_str("...");
  out
}

// Big credit to rustyline for this
pub(crate) fn width(s: &str, esc_seq: &mut u8) -> usize {
  if *esc_seq == 1 {
    if s == "[" {
      // CSI
      *esc_seq = 2;
    } else {
      // two-character sequence
      *esc_seq = 0;
    }
    0
  } else if *esc_seq == 2 {
    if s == ";" || (s.as_bytes()[0] >= b'0' && s.as_bytes()[0] <= b'9') {
      /*} else if s == "m" {
      // last
       *esc_seq = 0;*/
    } else {
      // not supported
      *esc_seq = 0;
    }

    0
  } else if s == "\x1b" {
    *esc_seq = 1;
    0
  } else if s == "\n" {
    0
  } else {
    get_width_calculator().width(s)
  }
}

pub(crate) trait WidthCalculator: Send + Sync {
  fn width(&self, text: &str) -> usize;
}

static WIDTH_CALC: std::sync::OnceLock<Box<dyn WidthCalculator>> = std::sync::OnceLock::new();

pub(crate) fn get_width_calculator() -> &'static dyn WidthCalculator {
  WIDTH_CALC.get_or_init(width_calculator).as_ref()
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct UnicodeWidth;

impl WidthCalculator for UnicodeWidth {
  fn width(&self, text: &str) -> usize {
    text.width()
  }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct WcWidth;

impl WcWidth {
  pub fn cwidth(&self, ch: char) -> usize {
    ch.width().unwrap()
  }
}

impl WidthCalculator for WcWidth {
  fn width(&self, text: &str) -> usize {
    let mut width = 0;
    for ch in text.chars() {
      width += self.cwidth(ch)
    }
    width
  }
}

const ZWJ: char = '\u{200D}';
#[derive(Clone, Copy, Debug)]
pub(crate) struct NoZwj;

impl WidthCalculator for NoZwj {
  fn width(&self, text: &str) -> usize {
    if text.contains(ZWJ) {
      // ZWJ sequence renders as a single glyph on supported terminals
      2
    } else {
      UnicodeWidth.width(text)
    }
  }
}

pub(crate) fn width_calculator() -> Box<dyn WidthCalculator> {
  match try_var!("TERM_PROGRAM").as_deref() {
    Some("Apple_Terminal") => Box::new(UnicodeWidth),
    Some("iTerm.app") => Box::new(UnicodeWidth),
    Some("WezTerm") => Box::new(UnicodeWidth),
    Some(_) => Box::new(WcWidth),
    None => match try_var!("TERM").as_deref() {
      Some("xterm-kitty") => Box::new(NoZwj),
      _ => Box::new(WcWidth),
    },
  }
}

pub(crate) enum ColorMode {
  Truecolor,
  Palette256,
  Palette16,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) enum CursorStyle {
  #[default]
  Default,
  Block(bool),
  Underline(bool),
  Beam(bool),
}

impl Display for CursorStyle {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      CursorStyle::Default => write!(f, "\x1b[0 q"),
      CursorStyle::Block(blink) => write!(f, "\x1b[{} q", if *blink { 1 } else { 2 }),
      CursorStyle::Underline(blink) => write!(f, "\x1b[{} q", if *blink { 3 } else { 4 }),
      CursorStyle::Beam(blink) => write!(f, "\x1b[{} q", if *blink { 5 } else { 6 }),
    }
  }
}

// I'd like to thank rustyline for this idea
nix::ioctl_read_bad!(win_size, libc::TIOCGWINSZ, libc::winsize);

/// Get the dimensions of thejterminal.
///
/// Returned as (cols,rows)
pub(crate) fn get_win_size(fd: RawFd) -> (u16, u16) {
  use std::mem::zeroed;

  if cfg!(test) {
    return (80, 24);
  }

  unsafe {
    let mut size: libc::winsize = zeroed();
    match win_size(fd, &mut size) {
      Ok(0) => {
        /* rustyline code says:
         In linux pseudo-terminals are created with dimensions of
         zero. If host application didn't initialize the correct
         size before start we treat zero size as 80 columns and
         infinite rows
        */
        let cols = if size.ws_col == 0 { 80 } else { size.ws_col };
        let rows = if size.ws_row == 0 {
          u16::MAX
        } else {
          size.ws_row
        };
        (cols, rows)
      }
      _ => (80, 24),
    }
  }
}

pub(super) fn enable_raw_mode(term: &mut Termios) {
  termios::cfmakeraw(term);
  // Keep ISIG enabled so Ctrl+C/Ctrl+Z still generate signals
  term.local_flags |= termios::LocalFlags::ISIG;
  // Keep OPOST enabled so \n is translated to \r\n on output
  term.output_flags |= termios::OutputFlags::OPOST;
}

pub(super) fn enable_cooked_mode(term: &mut Termios) {
  term.local_flags |= termios::LocalFlags::ICANON
    | termios::LocalFlags::ECHO
    | termios::LocalFlags::ECHOE
    | termios::LocalFlags::ECHOK
    | termios::LocalFlags::ECHONL
    | termios::LocalFlags::ISIG
    | termios::LocalFlags::IEXTEN;
  term.input_flags |= termios::InputFlags::ICRNL | termios::InputFlags::IXON;
  term.output_flags |= termios::OutputFlags::OPOST;
  // Restore VMIN/VTIME to canonical mode defaults
  term.control_chars[termios::SpecialCharacterIndices::VMIN as usize] = 1;
  term.control_chars[termios::SpecialCharacterIndices::VTIME as usize] = 0;
}
