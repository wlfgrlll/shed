mod error;
mod guards;
mod macros;
mod strops;
mod ui;

use crate::state;
use std::os::fd::BorrowedFd;

use super::{expand, match_loop, parse, sherr};

pub(super) use ui::{
  BOT_LEFT, BOT_RIGHT, HOR_LINE, PaletteEntry, TOP_LEFT, TOP_RIGHT, TREE_LEFT, TREE_RIGHT,
  VERT_LINE, ansi_from_description, pad_line, pad_line_into, style_from_description,
};

pub(super) use guards::{scope_guard, shared_scope_guard, var_ctx_guard};

pub(super) use error::{ShErr, ShErrKind, ShResult, ShResultExt, get_context};

pub(super) use strops::{
  QuoteState, ends_with_unescaped, expand_ansi_c, format_mode, format_size, has_unescaped,
  rfind_unescaped, scan_braces, scan_parens, split_at_unescaped, split_case_pat, split_tk,
};

pub(super) struct FdWriter<'a>(pub BorrowedFd<'a>);

impl<'a> std::io::Write for FdWriter<'a> {
  fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
    nix::unistd::write(self.0, buf).map_err(|e| std::io::Error::from_raw_os_error(e as i32))
  }
  fn flush(&mut self) -> std::io::Result<()> {
    Ok(())
  }
}

/// Given two things that implement Ord, make sure that the left is less than the right
pub(super) fn ordered<T: Ord>(start: T, end: T) -> (T, T) {
  if start > end {
    (end, start)
  } else {
    (start, end)
  }
}

/// Sets status code and always returns Ok(())
///
/// It's easy to forget to set the status code, this helps with that
pub(super) fn with_status(code: i32) -> ShResult<()> {
  state::Shed::set_status(code);
  Ok(())
}
