use crate::parse::{ParseFlags, ParsedSrc, lex::LexFlags};

use super::{Grapheme, Line, Lines, Pos};

#[derive(Default, Clone, Debug)]
pub struct Edit {
  pub old_cursor: Pos,
  pub new_cursor: Pos,
  pub old: Lines,
  pub new: Lines,
  pub merging: bool,
}

impl Edit {
  pub fn is_empty(&self) -> bool {
    self.old == self.new
  }
}

#[derive(Default, Clone, Debug)]
pub struct IndentCtx;

impl IndentCtx {
  pub fn new() -> Self {
    Self
  }

  pub fn check_levels_per_row(&mut self, input: &str) -> (Vec<(usize, usize)>, bool) {
    // byte offset of the start of each row
    let mut row_starts: Vec<usize> = vec![0];
    for (i, ch) in input.char_indices() {
      if ch == '\n' {
        row_starts.push(i + 1);
      }
    }
    let n_rows = row_starts.len();

    // boundaries we need parser depth at: each row_start, plus input.len() for last row's end.
    // \n is a Sep token and doesn't shift depth, so depth-just-before-\n == depth-just-after-\n,
    // which lets depth at row_starts[i+1] double as depth at end of row i.
    let mut boundaries = row_starts;
    boundaries.push(input.len());

    let mut depths: Vec<usize> = Vec::with_capacity(boundaries.len());
    let last_idx = boundaries.len() - 1;
    let mut failed = false;
    for (i, &b) in boundaries.iter().enumerate() {
      // Intermediate prefixes use LEX_UNFINISHED so block_depth tracks
      // through still-open structures. The final prefix parses strictly
      // so unterminated quotes / subshells / etc. flip `failed`.
      let lex_flags = if i == last_idx {
        LexFlags::LEX_UNFINISHED_STRUCTURES
      } else {
        LexFlags::LEX_UNFINISHED
      };
      let mut src = ParsedSrc::new(input[..b].into())
        .with_lex_flags(lex_flags)
        .with_parse_flags(ParseFlags::ERR_RETURN);
      let parse_failed = src.parse_src().is_err();
      if i == last_idx {
        failed = parse_failed;
      }
      depths.push(src.block_depth);
    }

    let levels: Vec<(usize, usize)> = (0..n_rows).map(|i| (depths[i], depths[i + 1])).collect();

    (levels, failed)
  }
}

pub(super) fn extract_range_contiguous(buf: &mut Lines, start: Pos, end: Pos) -> Lines {
  let start_col = start.col.min(buf[start.row].len());
  let end_col = end.col.min(buf[end.row].len());

  if start.row == end.row {
    // single line case
    let line = &mut buf[start.row];
    let removed: Vec<Grapheme> = line.0.drain(start_col..end_col).collect();
    return Lines(vec![Line(removed)]);
  }

  // multi line case
  // tail of first line
  let first_tail: Line = buf[start.row].split_off(start_col);

  // all inbetween lines. extracts nothing if only two rows
  let middle: Lines = buf.drain(start.row + 1..end.row).collect();

  // head of last line
  let last_col = end_col.min(buf[start.row + 1].len());
  let last_head: Line = Line::from(buf[start.row + 1].0.drain(..last_col).collect::<Vec<_>>());

  // tail of last line
  let mut last_remainder = buf.remove(start.row + 1);

  // attach tail of last line to head of first line
  buf[start.row].append(&mut last_remainder);

  // construct vector of extracted content
  let mut extracts = vec![first_tail];
  extracts.extend(middle.0);
  extracts.push(last_head);
  Lines(extracts)
}

impl super::LineBuf {
  /// Provides a public interface for editing the buffer in a way that is recognized by the undo system.
  /// Any change made by the provided function will be tracked in the undo stack.
  pub fn edit<T, F: FnMut(&mut Self) -> T>(&mut self, mut f: F) -> T {
    let before = self.lines.clone();
    let old_cursor = self.cursor.pos;

    let res = f(self);

    if self.is_empty() {
      self.set_hint(None);
    }

    let new_cursor = self.cursor.pos;
    self.handle_edit(before, new_cursor, old_cursor);

    res
  }
  pub fn handle_edit(&mut self, old: Lines, new_cursor: Pos, old_cursor: Pos) {
    let last_edit = self.undo_stack.last();
    let edit_is_merging = last_edit.is_some_and(|edit| edit.merging);
    if edit_is_merging {
      // Update the `new` snapshot on the existing edit
      if let Some(edit) = self.undo_stack.last_mut() {
        edit.new = self.lines.clone();
      }
    } else {
      self.undo_stack.push(Edit {
        new_cursor,
        old_cursor,
        old,
        new: self.lines.clone(),
        merging: false,
      });
    }
  }
}
