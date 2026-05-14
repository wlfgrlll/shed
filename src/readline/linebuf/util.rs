use nix::{libc::STDIN_FILENO, unistd::isatty};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthChar;

use crate::{
  parse::lex,
  procio::stdin_fileno,
  readline::{
    editcmd::Motion,
    highlight,
    linebuf::{
      CharClass, DEFAULT_VIEWPORT_HEIGHT, Edit, Grapheme, Line, Lines, MotionKind, Pos, SelectMode,
    },
  },
  sherr,
  state::{
    self,
    terminal::get_win_size,
    util::{read_shopts, write_meta},
  },
  status_msg,
  util::{ShResult, ordered},
};

use super::char_class::{CharClassIter, CharClassIterRev};
use super::edit::extract_range_contiguous;

pub fn rot13_char(c: char) -> char {
  let offset = if c.is_ascii_lowercase() {
    b'a'
  } else if c.is_ascii_uppercase() {
    b'A'
  } else {
    return c;
  };
  (((c as u8 - offset + 13) % 26) + offset) as char
}

pub fn toggle_case_char(c: char) -> char {
  if c.is_ascii_lowercase() {
    c.to_ascii_uppercase()
  } else if c.is_ascii_uppercase() {
    c.to_ascii_lowercase()
  } else {
    c
  }
}

impl super::LineBuf {
  pub fn mark_insert_mode_start_pos(&mut self) {
    self.insert_mode_start_pos = Some(self.cursor.pos);
  }

  pub fn clear_insert_mode_start_pos(&mut self) {
    self.insert_mode_start_pos = None;
  }

  pub(super) fn replace_range(&mut self, span: (Pos, Pos), new: &str) -> Lines {
    let s = span.0;
    let e = span.1;
    let motion = MotionKind::Char {
      start: s,
      end: e,
      inclusive: true,
    };
    let content = self.extract_range(&motion);
    self.set_cursor(s);
    self.insert_str(new);
    content
  }
  pub fn get_viewport_height(&self) -> usize {
    let raw = read_shopts(|o| {
      let height = o.line.viewport_height.as_str();
      if let Ok(num) = height.parse::<usize>() {
        num
      } else if let Some(pre) = height.strip_suffix('%')
        && let Ok(num) = pre.parse::<usize>()
      {
        if !isatty(stdin_fileno()).unwrap_or_default() {
          return DEFAULT_VIEWPORT_HEIGHT;
        };
        let (_, rows) = get_win_size(STDIN_FILENO);
        (rows as f64 * (num as f64 / 100.0)).round() as usize
      } else {
        log::warn!(
          "Invalid viewport height shopt value: '{}', using 50% of terminal height as default",
          height
        );
        if !isatty(stdin_fileno()).unwrap_or_default() {
          return DEFAULT_VIEWPORT_HEIGHT;
        };
        let (_, rows) = get_win_size(STDIN_FILENO);
        (rows as f64 * 0.5).round() as usize
      }
    });
    let mut hint_lines = self.hint_lines();
    let mut buf_lines = self.lines.clone();
    buf_lines.attach_lines(&mut hint_lines);
    (raw.min(100)).min(buf_lines.len())
  }
  pub fn update_scroll_offset(&mut self) {
    let height = self.get_viewport_height();
    let scrolloff = read_shopts(|o| o.line.scroll_offset);
    if self.cursor.pos.row < self.scroll_offset + scrolloff {
      self.scroll_offset = self.cursor.pos.row.saturating_sub(scrolloff);
    }
    if self.cursor.pos.row + scrolloff >= self.scroll_offset + height {
      self.scroll_offset = self.cursor.pos.row + scrolloff + 1 - height;
    }

    let max_offset = self.lines.len().saturating_sub(height);
    self.scroll_offset = self.scroll_offset.min(max_offset);
  }
  pub fn display_window_joined(&mut self) -> String {
    let joined = self.joined();
    let do_hl = state::util::read_shopts(|s| s.highlight.enable);
    let palette = if do_hl {
      highlight::Palette::new()
    } else {
      highlight::Palette::neutral()
    };
    let mut select_spans = self.search_match_spans();
    select_spans.extend(self.select_range_byte_pos());

    let highlighted = highlight::highlight(&joined, &palette, self.cursor_to_flat(), select_spans);
    let hint = self.get_hint_text();
    let lines = Lines::to_lines(format!("{highlighted}{hint}"));

    let offset = self.scroll_offset.min(lines.len());
    let (_, mid) = lines.split_at(offset);

    let height = self.get_viewport_height().min(mid.len());
    let (mid, _) = mid.split_at(height);

    Lines(mid.to_vec()).join()
  }
  pub fn trim(&mut self) {
    // trim empty lines
    while self.lines.first().is_some_and(|l| l.0.is_empty()) {
      self.lines.remove(0);
    }
    while self.lines.last().is_some_and(|l| l.0.is_empty()) {
      self.lines.pop();
    }

    // trim whitespace
    for (i, line) in self.lines.iter_mut().enumerate() {
      if i == 0 {
        while line.0.first().is_some_and(|gr| gr.is_ws()) {
          line.0.remove(0);
        }
      }
      while line.0.last().is_some_and(|gr| gr.is_ws()) {
        line.0.pop();
      }
    }

    // trim empty lines again
    while self.lines.first().is_some_and(|l| l.0.is_empty()) {
      self.lines.remove(0);
    }
    while self.lines.last().is_some_and(|l| l.0.is_empty()) {
      self.lines.pop();
    }
  }
  pub fn window_slice_to_cursor(&self) -> Option<String> {
    let mut result = String::new();
    let start_row = self.scroll_offset;

    for i in start_row..self.cursor.pos.row {
      result.push_str(&self.lines[i].to_string());
      result.push('\n');
    }
    let line = &self.lines[self.cursor.pos.row];
    let col = self.cursor.pos.col.min(line.len());
    for g in &line.graphemes()[..col] {
      result.push_str(&g.to_string());
    }
    Some(result)
  }
  pub(super) fn parse_pos(&self, pos: &str) -> ShResult<Pos> {
    if let Some((row, col)) = pos.split_once(':')
      && let Ok(row) = row.parse::<usize>()
      && let Ok(col) = col.parse::<usize>()
    {
      Ok(Pos { row, col })
    } else if let Ok(num) = pos.parse::<usize>() {
      Ok(self.pos_from_flat(num))
    } else {
      Err(sherr!(
        ParseErr,
        "Invalid position format: '{pos}'. Expected 'row:col' or grapheme index.",
      ))
    }
  }
  pub(super) fn insert_lines_at(&mut self, pos: Pos, mut lines: Lines) {
    if lines.is_empty() {
      return;
    }
    let row = pos.row;
    let col = pos.col;

    // Split the current line at the insertion point
    let mut right = self.lines[row].split_off(col);

    let last = lines.len() - 1;

    // First line appends to current line at the split point
    self.lines[row].append(&mut lines[0]);

    // Middle + last lines get inserted after
    for (i, line) in lines.0[1..].iter().cloned().enumerate() {
      self.lines.insert(row + 1 + i, line);
    }

    // Reattach right half to the last inserted line
    self.lines[row + last].append(&mut right);
  }
  pub(super) fn remove_at(&mut self, pos: Pos) -> Option<Grapheme> {
    let Pos { row, col } = pos;
    let line = self.lines.get_mut(row)?;

    line.0.get(col).is_some().then(|| line.0.remove(col))
  }
  pub(super) fn insert_at(&mut self, mut pos: Pos, gr: Grapheme) {
    if gr.is_lf() {
      self.break_line_at(pos);
      pos = pos.row_add(1);
      pos.set(pos.row, 0);
    } else {
      let row = pos.row;
      let col = pos.col;
      self.lines[row].insert(col, gr);
      self.indent_cache = None;
      pos = pos.col_add(1);
    }
    // Cheap test first: only consider dedenting if the line's trimmed content
    // is exactly a closer keyword. Skips the depth query for 99% of typing.
    let line = self.cur_line().to_string();
    let trimmed = line.trim();
    let is_closer = lex::CLOSERS
      .iter()
      .chain(lex::MIDDLES.iter())
      .any(|closer| trimmed == *closer);

    if is_closer {
      let (start, end) = self.indent_levels_for_row(pos.row);
      if start > end {
        let delta = start.saturating_sub(end);
        let line = self.cur_line_mut();
        for _ in 0..delta {
          if line.0.first().is_some_and(|c| c.as_char() == Some('\t')) {
            line.0.remove(0);
          } else {
            break;
          }
        }
      }
    }
  }
  pub(super) fn insert(&mut self, gr: Grapheme) {
    self.insert_at(self.cursor.pos, gr);
  }
  pub(super) fn insert_str(&mut self, s: &str) {
    for gr in s.graphemes(true) {
      let gr = Grapheme::from(gr);
      if gr.is_lf() {
        self.break_line_unchecked();
      } else {
        self.insert(gr);
        self.cursor.pos.col += 1;
      }
    }
  }
  pub fn pop_left(&mut self) -> bool {
    let Some(pos) = self.concat_points.pop_front() else {
      return false;
    };
    self.lines = self.lines.split_lines_at(pos);
    self.fix_cursor();
    true
  }
  pub fn pop_right(&mut self) -> bool {
    let Some(pos) = self.concat_points.pop_back() else {
      return false;
    };
    self.lines.split_lines_at(pos);
    self.fix_cursor();
    true
  }
  pub fn clear_concats(&mut self) {
    self.concat_points.clear();
  }
  /// Concatenate a string onto the left side of the buffer with a separator
  pub fn concat_left(&mut self, sep: &str, other: &str) {
    if self.is_empty() {
      self.lines = Lines::to_lines(other);
      return;
    }
    let joined = self.joined();
    let Some(first) = self.lines.first_mut() else {
      self.lines = Lines::to_lines(other);
      return;
    };
    let mut new_lines = Lines::to_lines(other);
    if new_lines.is_empty() {
      return;
    }
    while first.0.first().is_some_and(|l| l.is_ws()) {
      first.0.remove(0);
    }
    let Some(new_last) = new_lines.last_mut() else {
      unreachable!()
    };
    if !joined.trim_end().ends_with(sep.trim()) {
      new_last.push_str(sep);
    }
    let mut last = new_lines.pop().unwrap();
    let splice_pos = Pos {
      row: new_lines.len(),
      col: last.len(),
    };
    last.append(first);
    self.lines[0] = last;
    if !new_lines.is_empty() {
      for line in new_lines.0.into_iter().rev() {
        self.lines.insert(0, line);
      }
    }
    self.concat_points.push_front(splice_pos);
  }
  /// Concatenate a string onto the right side of the buffer with a separator
  pub fn concat_right(&mut self, sep: &str, other: &str) {
    if self.is_empty() {
      self.lines = Lines::to_lines(other);
      return;
    }
    let joined = self.joined();
    let last_row = self.lines.len() - 1;
    let Some(last) = self.lines.last_mut() else {
      self.lines = Lines::to_lines(other);
      return;
    };
    let mut new_lines = Lines::to_lines(other);
    if new_lines.is_empty() {
      return;
    }
    while last.0.last().is_some_and(|l| l.is_ws()) {
      last.0.pop();
    }
    let Some(new_first) = new_lines.first_mut() else {
      unreachable!()
    };
    if !joined.trim_end().ends_with(sep.trim()) {
      new_first.insert_str(0, sep);
    }
    let splice_pos = Pos {
      row: last_row,
      col: last.len(),
    };
    let mut first = new_lines.remove(0);
    last.append(&mut first);
    self.lines.extend(new_lines.0);
    self.concat_points.push_back(splice_pos);
  }
  pub fn cursor_in_leading_ws(&self) -> bool {
    let line = self.line(self.row());
    let col = self.col();
    line
      .0
      .get(..col)
      .is_none_or(|grs| grs.iter().all(|g| g.is_ws()))
  }

  pub fn cursor_is_escaped(&self) -> bool {
    if self.cursor.pos.col == 0 {
      return false;
    }
    let line = &self.lines[self.cursor.pos.row];
    if self.cursor.pos.col > line.len() {
      return false;
    }
    line
      .graphemes()
      .get(self.cursor.pos.col.saturating_sub(1))
      .is_some_and(|g| g.is_char('\\'))
  }

  pub fn take_buf(&mut self) -> String {
    let result = self.joined();
    self.lines = Lines::default();
    self.cursor.pos = Pos { row: 0, col: 0 };
    result
  }

  pub(super) fn get(&mut self, pos: Pos) -> Option<Grapheme> {
    self
      .lines
      .get(pos.row)
      .and_then(|line| line.graphemes().get(pos.col))
      .cloned()
  }

  pub(super) fn grapheme_before_cursor(&mut self) -> Option<Grapheme> {
    self.get(self.cursor.pos.col_add_signed(-1))
  }

  pub(super) fn pos_to_flat(&self, pos: Pos) -> usize {
    let mut offset = 0;
    let row = pos.row.min(self.lines.len().saturating_sub(1));
    for i in 0..row {
      offset += self.lines[i].len() + 1; // +1 for '\n'
    }
    offset + pos.col.min(self.lines[row].len())
  }

  pub(super) fn pos_from_flat(&self, mut flat: usize) -> Pos {
    for (i, line) in self.lines.iter().enumerate() {
      if flat <= line.len() {
        return Pos { row: i, col: flat };
      }
      flat = flat.saturating_sub(line.len() + 1); // +1 for '\n'
    }
    // If we exceed the total length, clamp to end
    let last_row = self.lines.len().saturating_sub(1);
    let last_col = self.lines[last_row].len();
    Pos {
      row: last_row,
      col: last_col,
    }
  }

  pub fn cursor_to_flat(&self) -> usize {
    self.pos_to_flat(self.cursor.pos)
  }

  pub fn anchor_to_flat(&self) -> Option<usize> {
    self.select_mode.map(|r| match r {
      SelectMode::Char(pos) | SelectMode::Block(pos) | SelectMode::Line(pos) => {
        self.pos_to_flat(pos)
      }
    })
  }

  pub fn set_cursor_from_flat(&mut self, flat: usize) {
    self.cursor.pos = self.pos_from_flat(flat);
    self.fix_cursor();
  }
  pub fn set_anchor_from_flat(&mut self, flat: usize) {
    let new_pos = self.pos_from_flat(flat);
    self.set_anchor(new_pos);
  }
  pub fn set_anchor(&mut self, new_pos: Pos) {
    match self.select_mode.as_mut() {
      Some(SelectMode::Line(pos)) | Some(SelectMode::Block(pos)) | Some(SelectMode::Char(pos)) => {
        *pos = new_pos
      }
      None => unreachable!(),
    }
  }

  pub fn enumerate_graphemes(lines: &Lines) -> Vec<(Pos, Grapheme)> {
    lines
      .iter()
      .enumerate()
      .flat_map(|(row, line)| {
        line
          .graphemes()
          .iter()
          .cloned()
          .enumerate()
          .map(move |(col, g)| (Pos { row, col }, g))
      })
      .collect()
  }

  pub fn with_initial(mut self, s: &str, cursor_pos: usize) -> Self {
    self.set_buffer(s.to_string());
    // In the flat model, cursor_pos was a flat offset. Map to col on row .
    self.cursor.pos = Pos {
      row: 0,
      col: cursor_pos.min(s.len()),
    };
    self
  }

  pub fn move_cursor_to_end(&mut self) {
    self.set_cursor(Pos::MAX);
  }

  pub fn cursor_max(&self) -> usize {
    // In single-line mode this is the length of the first line
    // In multi-line mode this returns total grapheme count (for flat compat)
    if self.lines.len() == 1 {
      self.lines[0].len()
    } else {
      self.count_graphemes()
    }
  }

  pub fn cursor_at_max(&self) -> bool {
    let last_row = self.lines.len().saturating_sub(1);
    let max = if self.cursor.exclusive {
      self.lines[last_row].len().saturating_sub(1)
    } else {
      self.lines[last_row].len()
    };
    self.cursor.pos.row == last_row && self.cursor.pos.col >= max
  }

  pub fn set_cursor_clamp(&mut self, exclusive: bool) {
    self.cursor.exclusive = exclusive;
  }

  pub fn start_of_line(&self) -> usize {
    // Return 0-based flat offset of start of current row
    let mut offset = 0;
    for i in 0..self.cursor.pos.row {
      offset += self.lines[i].len() + 1; // +1 for '\n'
    }
    offset
  }

  pub fn on_last_line(&self) -> bool {
    self.cursor.pos.row == self.lines.len().saturating_sub(1)
      && self.hint.as_ref().is_none_or(|h| h.lines().len() <= 1)
  }

  pub fn slice(&self, range: std::ops::Range<usize>) -> Option<String> {
    let joined = self.joined();
    let graphemes: Vec<&str> = joined.graphemes(true).collect();
    if range.start > graphemes.len() || range.end > graphemes.len() {
      return None;
    }
    Some(graphemes[range].join(""))
  }

  pub fn cursor_byte_pos(&self) -> usize {
    let mut pos = 0;
    for i in 0..self.cursor.pos.row {
      pos += self.lines[i].to_string().len() + 1; // +1 for '\n'
    }
    let line_str = self.lines[self.cursor.pos.row].to_string();
    let col = self
      .cursor
      .pos
      .col
      .min(self.lines[self.cursor.pos.row].len());
    // Sum bytes of graphemes up to col
    let mut byte_count = 0;
    for (i, g) in line_str.graphemes(true).enumerate() {
      if i >= col {
        break;
      }
      byte_count += g.len();
    }
    pos + byte_count
  }
  pub fn clear_buffer(&mut self) {
    self.lines = Lines::default();
    self.clear_concats();
    self.fix_cursor();
  }
  pub fn set_buffer(&mut self, s: String) {
    self.lines = Lines::to_lines(&s);
    if self.lines.is_empty() {
      self.lines.push(Line::default());
    }
    self.clear_concats();
    self.fix_cursor();
  }
  pub fn joined(&self) -> String {
    let mut lines = vec![];
    for line in &self.lines.0 {
      lines.push(line.to_string());
    }
    lines.join("\n")
  }
  pub fn fix_cursor(&mut self) {
    // we are now going to enforce some invariants and do some bookkeeping
    if self.lines.is_empty() {
      // self.lines must always have at least one line
      self.lines.push(Line::default());
    }
    if self.cursor.pos.row >= self.lines.len() {
      // clamp this now so self.cur_line() cannot panic
      self.cursor.pos.row = self.lines.len().saturating_sub(1);
    }
    if self.cursor.exclusive {
      let line = self.cur_line();
      let col = self.col();
      if col > 0 && col >= line.len() {
        self.cursor.pos.col = line.len().saturating_sub(1);
      }
    } else {
      let line = self.cur_line();
      let col = self.col();
      if col > 0 && col > line.len() {
        self.cursor.pos.col = line.len();
      }
    }

    // update viewport scroll offset
    self.update_scroll_offset();
  }
  pub fn stop_undo_merge(&mut self) {
    self.merging_undos = false;
    if let Some(edit) = self.undo_stack.last_mut() {
      edit.merging = false;
    }

    self.undo_stack.push(Edit {
      old_cursor: self.cursor.pos,
      new_cursor: self.cursor.pos,
      old: self.lines.clone(),
      new: self.lines.clone(),
      merging: false,
    });
  }
  pub fn start_undo_merge(&mut self) {
    self.merging_undos = true;
    if let Some(edit) = self.undo_stack.last_mut() {
      edit.merging = true;
    }
  }
  pub fn equalize_rows(&mut self, line_nums: Vec<usize>) {
    for row in line_nums {
      let (start, end) = self.indent_levels_for_row(row);
      let num_tabs = start.min(end);

      let line = self.line_mut(row);
      while line.0.first().is_some_and(|c| c.is_ws()) {
        line.0.remove(0);
      }
      for tab in std::iter::repeat_n(Grapheme::from('\t'), num_tabs) {
        line.insert(0, tab);
      }
    }
  }
  pub fn indent_levels_for_row(&mut self, row: usize) -> (usize, usize) {
    self.indent_levels().get(row).cloned().unwrap_or_default()
  }
  /// Returns (depth-at-cursor, parse-failed). Computed from the prefix
  /// up to the cursor — reflects whether we're inside an open block.
  pub fn cursor_indent_level(&mut self) -> (usize, bool) {
    let (to_cursor, _) = self.lines.clone().split_lines(self.cursor.pos);
    let raw = to_cursor.join();
    let (levels, failed) = self.indent_levels_for(&raw);
    let depth = levels.last().cloned().unwrap_or_default().1;
    (depth, failed)
  }
  pub fn indent_levels_for(&mut self, buf: &str) -> (Vec<(usize, usize)>, bool) {
    self.indent_ctx.check_levels_per_row(buf)
  }
  pub fn indent_levels(&mut self) -> &[(usize, usize)] {
    let has_cache = self.indent_cache.is_some();
    if !has_cache {
      let joined = self.joined();
      let (levels, status) = self.indent_ctx.check_levels_per_row(&joined);
      self.indent_cache = Some(levels);
      self.parse_status = status;
    }
    self.indent_cache.as_ref().unwrap()
  }
  pub(super) fn delete_range(&mut self, motion: &MotionKind) -> Lines {
    self.extract_range(motion)
  }
  pub(super) fn yank_range(&self, motion: &MotionKind) -> Lines {
    let mut tmp = Self {
      lines: self.lines.clone(),
      cursor: self.cursor,
      ..Default::default()
    };
    tmp.extract_range(motion)
  }
  pub(super) fn extract_range(&mut self, motion: &MotionKind) -> Lines {
    let extracted = match motion {
      MotionKind::Char {
        start,
        end,
        inclusive,
      } => self.extract_span((*start, *end), *inclusive),
      MotionKind::Line {
        start,
        end,
        inclusive,
      } => {
        let end = if *inclusive {
          *end
        } else {
          end.saturating_sub(1)
        };
        self.lines.drain(*start..=end).collect()
      }
      MotionKind::Block { start, end } => {
        let (s, e) = ordered(*start, *end);
        (s.row..=e.row)
          .map(|row| {
            let sc = s.col.min(self.lines[row].len());
            let ec = (e.col + 1).min(self.lines[row].len());
            Line(self.lines[row].0.drain(sc..ec).collect())
          })
          .collect()
      }
    };
    if self.lines.is_empty() {
      self.lines.push(Line::default());
    }
    extracted
  }
  pub(super) fn yank_span(&self, span: (Pos, Pos), inclusive: bool) -> Lines {
    let mut tmp = Self {
      lines: self.lines.clone(),
      cursor: self.cursor,
      ..Default::default()
    };
    tmp.extract_span(span, inclusive)
  }
  pub(super) fn extract_span(&mut self, span: (Pos, Pos), inclusive: bool) -> Lines {
    let (s, e) = ordered(span.0, span.1);
    let end = if inclusive {
      Pos {
        row: e.row,
        col: e.col + 1,
      }
    } else {
      e
    };
    let mut buf = std::mem::take(&mut self.lines);
    let extracted = extract_range_contiguous(&mut buf, s, end);
    self.lines = buf;
    extracted
  }
  pub(super) fn move_to_start(&mut self, motion: MotionKind) {
    match motion {
      MotionKind::Char { start, end, .. } => {
        let (s, _) = ordered(start, end);
        self.set_cursor(s);
      }
      MotionKind::Line { start, end, .. } => {
        let (s, _) = ordered(start, end);
        self.set_cursor(Pos { row: s, col: 0 });
      }
      MotionKind::Block { .. } => unimplemented!(),
    }
  }
  pub fn get_matching_lines(
    &self,
    constraint: &Motion,
    re: &str,
    negated: bool,
  ) -> ShResult<Vec<usize>> {
    let (s, e) = match constraint {
      Motion::LineRange(s, e) => {
        let Some(s) = self.resolve_line_addr(s)? else {
          return Ok(vec![]);
        };
        let Some(e) = self.resolve_line_addr(e)? else {
          return Ok(vec![]);
        };
        ordered(s, e)
      }
      Motion::Line(addr) => {
        let Some(line) = self.resolve_line_addr(addr)? else {
          return Ok(vec![]);
        };
        (line, line)
      }
      _ => (0, self.lines.len().saturating_sub(1)),
    };

    let re = match write_meta(|m| m.get_regex(re.to_string())) {
      Ok(re) => re,
      Err(e) => {
        status_msg!("{e}");
        return Ok(vec![]);
      }
    };
    let mut acc = 0;
    let mut lines = vec![];

    loop {
      if !(s..=e).contains(&acc) {
        acc += 1 % self.lines.len();
        continue;
      }
      let Some(line) = self.get_row(acc) else { break };
      let line_str = line.to_string();
      if re.is_match(&line_str) != negated {
        lines.push(acc);
      }

      if acc == self.lines.len().saturating_sub(1) {
        break;
      }
      acc += 1 % self.lines.len();
    }

    Ok(lines)
  }
  pub(super) fn calc_cursor_display_col(&self) -> usize {
    self.calc_display_col_for(self.cursor.pos)
  }
  pub(super) fn calc_display_col_for(&self, pos: Pos) -> usize {
    let tab_width = read_shopts(|o| o.line.tab_width);
    let line = self.line(pos.row);
    let mut col = 0;
    for gr in &line.0[..pos.col] {
      let Some(ch) = gr.as_char() else {
        col += gr.width();
        continue;
      };

      match ch {
        '\t' => {
          col += tab_width - (col % tab_width);
        }
        c => {
          col += c.width().unwrap_or(0);
        }
      }
    }

    col
  }
  pub fn pos_to_byte(&mut self, pos: Pos) -> Option<usize> {
    if let Some(positions) = &self.byte_positions {
      positions
        .iter()
        .find_map(|(b, p)| (*p >= pos).then_some(*b))
    } else {
      self.byte_positions = Some(self.byte_positions());
      self.pos_to_byte(pos)
    }
  }
  pub fn byte_to_pos(&mut self, byte_offset: usize) -> Option<Pos> {
    if let Some(positions) = &self.byte_positions {
      positions
        .iter()
        .find_map(|(b, p)| (*b >= byte_offset).then_some(*p))
    } else {
      self.byte_positions = Some(self.byte_positions());
      self.byte_to_pos(byte_offset)
    }
  }
  /// map every valid Pos in the buffer to a corresponding byte position in the string
  pub(super) fn byte_positions(&self) -> Vec<(usize, Pos)> {
    let mut positions = vec![];
    let mut acc = 0;

    for (row, line) in self.lines.iter().enumerate() {
      for (col, gr) in line.0.iter().enumerate() {
        positions.push((acc, Pos { row, col }));
        acc += gr.len_utf8();
      }
      positions.push((
        acc,
        Pos {
          row,
          col: line.0.len(),
        },
      ));
      acc += 1; // for the newline
    }

    positions
  }
  pub(super) fn display_col_to_index(&self, row: usize, target: usize) -> usize {
    let tab_width = read_shopts(|o| o.line.tab_width);
    let line = self.line(row);
    let mut col = 0;
    for (i, gr) in line.0.iter().enumerate() {
      if col >= target {
        return i;
      }
      let Some(ch) = gr.as_char() else {
        col += gr.width();
        continue;
      };

      match ch {
        '\t' => {
          col += tab_width - (col % tab_width);
        }
        c => {
          col += c.width().unwrap_or(0);
        }
      }
    }

    line.0.len()
  }
  pub(super) fn get_row(&self, row: usize) -> Option<&Line> {
    self.lines.get(row)
  }
  pub(super) fn pos_slice_str(&self, s: Pos, e: Pos) -> String {
    let (s, e) = ordered(s, e);
    if s.row == e.row {
      self.lines[s.row].0[s.col..=e.col]
        .iter()
        .map(|g| g.to_string())
        .collect()
    } else {
      let mut result = String::new();
      // First line from s.col to end
      for g in &self.lines[s.row].0[s.col..] {
        result.push_str(&g.to_string());
      }
      // Middle lines
      for line in &self.lines.0[s.row + 1..e.row] {
        result.push('\n');
        result.push_str(&line.to_string());
      }
      // Last line from start to e.col
      result.push('\n');
      for g in &self.lines[e.row].0[..=e.col] {
        result.push_str(&g.to_string());
      }
      result
    }
  }
  /// Returns the start/end span of a number at a given position, if any
  pub(super) fn number_at(&self, mut pos: Pos) -> Option<(Pos, Pos)> {
    let is_number_char = |gr: &Grapheme| {
      gr.as_char()
        .is_some_and(|c| c == '.' || c == '-' || c.is_ascii_digit())
    };
    let is_digit = |gr: &Grapheme| gr.as_char().is_some_and(|c| c.is_ascii_digit());

    pos = self.clamp_pos(pos);
    if !is_number_char(self.gr_at(pos)?) {
      return None;
    }

    // If cursor is on '-', advance to the first digit
    if self.gr_at(pos)?.as_char() == Some('-') {
      pos = pos.col_add(1);
    }

    let mut start = self
      .scan_backward_from(pos, |g| !is_digit(g))
      .map(|pos| Pos {
        row: pos.row,
        col: pos.col + 1,
      })
      .unwrap_or(Pos::MIN);
    let end = self
      .scan_forward_from(pos, |g| !is_digit(g))
      .map(|pos| Pos {
        row: pos.row,
        col: pos.col.saturating_sub(1),
      })
      .unwrap_or(Pos {
        row: pos.row,
        col: self.lines[pos.row].len().saturating_sub(1),
      });

    if start > Pos::MIN && self.lines[start.row][start.col.saturating_sub(1)].as_char() == Some('-')
    {
      start.col -= 1;
    }

    Some((start, end))
  }
  pub(super) fn number_at_cursor(&self) -> Option<(Pos, Pos)> {
    self.number_at(self.cursor.pos)
  }
  pub(super) fn clamp_pos(&self, mut pos: Pos) -> Pos {
    pos.clamp_row(&self.lines);
    pos.clamp_col(&self.lines[pos.row].0, false);
    pos
  }
  pub(super) fn gr_at(&self, pos: Pos) -> Option<&Grapheme> {
    self.lines.get(pos.row)?.0.get(pos.col)
  }
  pub(super) fn end_pos(&self) -> Pos {
    let mut pos = Pos::MAX;
    pos.clamp_row(&self.lines);
    pos.clamp_col(&self.lines[pos.row].0, self.cursor.exclusive);
    pos
  }
  pub(super) fn char_classes_backward_from(
    &self,
    pos: Pos,
  ) -> impl Iterator<Item = (Pos, CharClass)> {
    CharClassIterRev::new(&self.lines, pos)
  }
  pub(super) fn char_classes_forward_from(
    &self,
    pos: Pos,
  ) -> impl Iterator<Item = (Pos, CharClass)> {
    CharClassIter::new(&self.lines, pos)
  }
  pub(super) fn scan_backward_from<F: FnMut(&Grapheme) -> bool>(
    &self,
    mut pos: Pos,
    mut f: F,
  ) -> Option<Pos> {
    pos.clamp_row(&self.lines);
    pos.clamp_col(&self.lines[pos.row].0, false);
    let Pos { mut row, mut col } = pos;

    loop {
      let line = &self.lines[row];
      if !line.is_empty() && f(&line[col]) {
        return Some(Pos { row, col });
      }
      if col > 0 {
        col -= 1;
      } else if row > 0 {
        row -= 1;
        col = self.lines[row].len().saturating_sub(1);
      } else {
        return None;
      }
    }
  }
  pub(super) fn scan_backward<F: FnMut(&Grapheme) -> bool>(&self, f: F) -> Option<Pos> {
    self.scan_backward_from(self.cursor.pos.col_add_signed(-1), f)
  }
  pub(super) fn scan_forward_from<F: FnMut(&Grapheme) -> bool>(
    &self,
    mut pos: Pos,
    mut f: F,
  ) -> Option<Pos> {
    pos.clamp_row(&self.lines);
    pos.clamp_col(&self.lines[pos.row].0, false);
    let Pos { mut row, mut col } = pos;

    loop {
      let line = &self.lines[row];
      if col >= line.len() {
        if row < self.lines.len() - 1 {
          row += 1;
          col = 0;
          continue;
        } else {
          return None;
        }
      }
      if !line.is_empty() && f(&line[col]) {
        return Some(Pos { row, col });
      }
      if col < self.lines[row].len().saturating_sub(1) {
        col += 1;
      } else if row < self.lines.len().saturating_sub(1) {
        row += 1;
        col = 0;
      } else {
        return None;
      }
    }
  }
  pub(super) fn scan_forward<F: FnMut(&Grapheme) -> bool>(&self, f: F) -> Option<Pos> {
    self.scan_forward_from(self.cursor.pos, f)
  }
  pub(super) fn line_to_pos(&self, pos: Pos) -> &[Grapheme] {
    let line = &self.lines[pos.row];
    let col = pos.col.min(line.len());
    &line[..col]
  }
  pub(super) fn line_from_pos(&self, pos: Pos) -> &[Grapheme] {
    let line = &self.lines[pos.row];
    let col = pos.col.min(line.len());
    &line[col..]
  }
  pub fn row(&self) -> usize {
    self.cursor.pos.row
  }
  pub(super) fn offset_row(&self, offset: isize) -> usize {
    let mut row = self.cursor.pos.row.saturating_add_signed(offset);
    row = row.clamp(0, self.lines.len().saturating_sub(1));
    row
  }
  pub(super) fn col(&self) -> usize {
    self.cursor.pos.col
  }
  pub(super) fn offset_col(&self, row: usize, offset: isize) -> usize {
    let mut col = self.cursor.pos.col.saturating_add_signed(offset);
    let max = if self.cursor.exclusive {
      self.lines[row].len().saturating_sub(1)
    } else {
      self.lines[row].len()
    };
    col = col.clamp(0, max);
    col
  }
  pub(super) fn offset_col_wrapping_at(
    &self,
    row: usize,
    offset: isize,
    pos: Pos,
  ) -> (usize, usize) {
    let mut row = row;
    let mut col = pos.col as isize + offset;

    while col < 0 {
      if row == 0 {
        col = 0;
        break;
      }
      row -= 1;
      col += self.lines[row].len() as isize + 1;
    }
    while col > self.lines[row].len() as isize {
      if row >= self.lines.len() - 1 {
        col = self.lines[row].len() as isize;
        break;
      }
      col -= self.lines[row].len() as isize + 1;
      row += 1;
    }

    (row, col as usize)
  }
  pub(super) fn offset_col_wrapping(&self, row: usize, offset: isize) -> (usize, usize) {
    self.offset_col_wrapping_at(row, offset, self.cursor.pos)
  }
  pub(super) fn cursor_on_ws(&self) -> bool {
    let line = self.cur_line();
    let col = self.cursor.pos.col;
    line.graphemes().get(col).is_some_and(|g| g.is_ws())
  }
  pub fn set_cursor(&mut self, mut pos: Pos) {
    pos.clamp_row(&self.lines);
    pos.clamp_col(&self.lines[pos.row].0, self.cursor.exclusive);
    self.cursor.pos = pos;
  }
  pub(super) fn set_row(&mut self, row: usize) {
    self.set_cursor(Pos {
      row,
      col: self.saved_col.unwrap_or(self.cursor.pos.col),
    });
  }
  pub(super) fn offset_cursor(&self, row_offset: isize, col_offset: isize) -> Pos {
    let row = self.offset_row(row_offset);
    let col = self.offset_col(row, col_offset);
    Pos { row, col }
  }
  pub(super) fn offset_cursor_wrapping(&self, row_offset: isize, col_offset: isize) -> Pos {
    let row = self.offset_row(row_offset);
    let (row, col) = self.offset_col_wrapping(row, col_offset);
    Pos { row, col }
  }
  pub(super) fn break_line_unchecked(&mut self) {
    self.break_line_at_unchecked(self.cursor.pos);
  }
  pub(super) fn break_line_at(&mut self, pos: Pos) {
    self.break_line_at_inner(pos, true);
  }
  pub(super) fn break_line_at_unchecked(&mut self, pos: Pos) {
    self.break_line_at_inner(pos, false);
  }
  pub(super) fn break_line_at_inner(&mut self, pos: Pos, invalidate_cache: bool) {
    let Pos { row, col } = pos;
    let rest = self.lines[row].split_off(col);

    self.lines.insert(row + 1, rest);
    if invalidate_cache {
      self.indent_cache = None;
    }
    let (_, end) = self.indent_levels_for_row(row + 1);
    let new_line = self.lines.get_mut(row + 1).unwrap();

    let mut col = 0;
    for tab in std::iter::repeat_n(Grapheme::from('\t'), end) {
      new_line.insert(0, tab);
      col += 1;
    }

    self.cursor.pos.set(row + 1, col);
  }
  pub(super) fn line_iter_mut(&mut self, span: (usize, usize)) -> impl Iterator<Item = &mut Line> {
    let (start, end) = ordered(span.0, span.1);
    self.lines.iter_mut().take(end + 1).skip(start)
  }
  pub(super) fn line_mut(&mut self, row: usize) -> &mut Line {
    &mut self.lines[row]
  }
  pub(super) fn line(&self, row: usize) -> &Line {
    &self.lines[row]
  }
  pub(super) fn cur_line_mut(&mut self) -> &mut Line {
    &mut self.lines[self.cursor.pos.row]
  }
  #[track_caller]
  pub(super) fn cur_line(&self) -> &Line {
    let caller = std::panic::Location::caller();
    log::trace!("cur_line called from {}:{}", caller.file(), caller.line());
    &self.lines[self.cursor.pos.row]
  }
  pub fn count_graphemes(&self) -> usize {
    self.lines.iter().map(|line| line.len()).sum()
  }
  pub fn is_empty(&self) -> bool {
    self.lines.len() == 0 || (self.lines.len() == 1 && self.count_graphemes() == 0)
  }
  pub fn clear_pending_search(&mut self) {
    self.pending_search = None;
  }
  pub fn update_pending_search(&mut self, new: Option<String>) {
    let Some(new) = new else { return };
    self.pending_search = (!new.is_empty()).then_some(new);
  }
}
