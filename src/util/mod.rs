mod error;
mod guards;
mod macros;
mod strops;
mod ui;

use std::os::fd::BorrowedFd;
use crate::state;

pub use ui::{
  VERT_LINE,
  HOR_LINE,
  BOT_LEFT,
  BOT_RIGHT,
  TOP_LEFT,
  TOP_RIGHT,
  TREE_LEFT,
  TREE_RIGHT,
  PaletteEntry,
  pad_line,
  pad_line_into,
  ansi_from_description,
  style_from_description
};

pub use guards::{
  var_ctx_guard,
  scope_guard,
  shared_scope_guard,
};

pub use error::{
  get_context,
  clear_color,
  ShResult,
  ShErr,
  ShErrKind,
  ShResultExt
};

pub use strops::{
  expand_ansi_c,
  QuoteState,
  split_tk,
  split_tk_at,
  has_unescaped,
  ends_with_unescaped,
  split_at_unescaped,
  split_case_pat,
  scan_braces,
  scan_parens,
  format_size,
  format_mode,
};

pub struct FdWriter<'a>(pub BorrowedFd<'a>);

impl<'a> std::io::Write for FdWriter<'a> {
  fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
    nix::unistd::write(self.0, buf).map_err(|e| std::io::Error::from_raw_os_error(e as i32))
  }
  fn flush(&mut self) -> std::io::Result<()> {
    Ok(())
  }
}

/// Sets status code and always returns Ok(())
///
/// It's easy to forget to set the status code, this helps with that
pub fn with_status(code: i32) -> ShResult<()> {
  state::set_status(code);
  Ok(())
}
