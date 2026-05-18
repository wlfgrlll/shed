use std::{collections::VecDeque, fmt::Display, ops::Range};

use ariadne::Span as AriadneSpan;
use unicode_segmentation::UnicodeSegmentation;

use super::{
  Shed, autocmd,
  context::{CtxTkRule, get_context_tokens},
  editcmd,
  editcmd::{EditCmd, Motion, Verb},
  editmode, eval,
  eval::lex::{LexFlags, LexStream, TkFlags, TkRule},
  expand::{expand_alias_with_pos, markers},
  highlight,
  history::History,
  match_loop, motion, procio, sherr, shopt, stash, state, status_msg, system_msg, try_var,
  util::{QuoteState, ShResult, ordered},
};

mod char_class;
mod edit;
mod excmd;
mod hint;
mod killring;
mod motion;
mod pos;
mod select;
mod types;
mod util;
mod verb;

pub(crate) use super::util::{Pos, SignedPos};
pub use char_class::CharClass;
pub use edit::{Edit, IndentCtx};
pub use hint::Hint;
pub use killring::KillRing;
pub use pos::{Cursor, MotionKind};
pub use select::{SelectMode, SelectShape};
pub use types::{Grapheme, Line, Lines};
pub use util::{rot13_char, toggle_case_char};

pub(crate) const DEFAULT_VIEWPORT_HEIGHT: usize = 40;

#[derive(Debug, Clone)]
pub struct LineBuf {
  lines: Lines,
  byte_positions: Option<Vec<(usize, Pos)>>,
  hint: Option<Hint>,
  cursor: Cursor,

  select_mode: Option<SelectMode>,
  last_selection: Option<(SelectMode, Pos)>,

  last_substitute: Option<EditCmd>,
  last_global: Option<EditCmd>,
  last_search: Option<Motion>,
  pending_search: Option<String>,

  insert_mode_start_pos: Option<Pos>,
  saved_col: Option<usize>,
  indent_ctx: IndentCtx,

  scroll_offset: usize,

  undo_stack: Vec<Edit>,
  redo_stack: Vec<Edit>,
  merging_undos: bool,

  kill_ring: KillRing,

  concat_points: VecDeque<Pos>,
  indent_cache: Option<Vec<(usize, usize)>>,
  parse_status: bool,
}

impl Default for LineBuf {
  fn default() -> Self {
    Self {
      lines: Lines::default(),
      hint: None,
      byte_positions: None,
      cursor: Cursor {
        pos: Pos { row: 0, col: 0 },
        exclusive: false,
      },
      select_mode: None,
      last_selection: None,
      last_substitute: None,
      last_global: None,
      last_search: None,
      pending_search: None,
      insert_mode_start_pos: None,
      saved_col: None,
      indent_ctx: IndentCtx::new(),
      scroll_offset: 0,
      undo_stack: vec![],
      redo_stack: vec![],
      merging_undos: false,
      kill_ring: KillRing::new(),
      concat_points: VecDeque::new(),
      indent_cache: None,
      parse_status: true,
    }
  }
}

#[allow(dead_code, unused_variables)]
impl LineBuf {
  pub fn new() -> Self {
    Self::default()
  }
  pub fn cursor(&self) -> Pos {
    self.cursor.pos
  }
  pub fn lines(&self) -> &Lines {
    &self.lines
  }
  pub fn scroll_offset(&self) -> usize {
    self.scroll_offset
  }
  pub(super) fn exec_cmd(&mut self, cmd: EditCmd) -> ShResult<()> {
    let is_char_insert = cmd.verb.as_ref().is_some_and(|v| v.1.is_char_insert());
    let is_kill = cmd.verb.as_ref().is_some_and(|v| v.1 == Verb::Kill);
    let is_killring_op = cmd
      .verb
      .as_ref()
      .is_some_and(|v| matches!(v.1, Verb::KillCycle | Verb::KillPut));
    let starts_merge = cmd
      .verb
      .as_ref()
      .is_some_and(|v| matches!(v.1, Verb::Change));
    let is_line_motion = cmd.is_line_motion()
      || cmd
        .verb
        .as_ref()
        .is_some_and(|v| v.1 == Verb::AcceptLineOrNewline);
    let is_undo_op = cmd.is_undo_op();
    let is_vertical = matches!(
      cmd.motion().map(|m| &m.1),
      Some(Motion::LineUp | Motion::LineDown)
    );
    let is_separator = cmd.is_separator_insert();
    let is_edit = cmd.is_edit();

    if !is_vertical {
      self.saved_col = None;
    }

    if is_edit {
      self.indent_cache = None;
    }

    let before = self.lines.clone();
    let old_cursor = self.cursor.pos;

    if is_separator
      && !self.grapheme_before_cursor().is_none_or(|gr| gr.is_ws())
      && shopt!(prompt.expand_aliases)
    {
      self.attempt_alias_expansion();
    }

    // Execute the command
    let res = self.exec_verb(&cmd);

    if self.is_empty() {
      self.set_hint(None);
    }

    let new_cursor = self.cursor.pos;

    // Stop merging on any non-char-insert command, even if buffer didn't change
    if !self.merging_undos
      && !is_char_insert
      && !is_undo_op
      && let Some(edit) = self.undo_stack.last_mut()
    {
      edit.merging = false;
    }
    let changed = self.lines != before;

    if changed && !is_undo_op {
      self.redo_stack.clear();
      if is_char_insert {
        // Merge consecutive char inserts into one undo entry
        if let Some(edit) = self.undo_stack.last_mut().filter(|e| e.merging) {
          edit.new = self.lines.clone();
          edit.new_cursor = new_cursor;
        } else {
          self.undo_stack.push(Edit {
            old_cursor,
            new_cursor,
            old: before,
            new: self.lines.clone(),
            merging: true,
          });
        }
      } else {
        self.handle_edit(before, new_cursor, old_cursor);
        // Change starts a new merge chain so subsequent InsertChars merge into it
        if (starts_merge || self.merging_undos)
          && let Some(edit) = self.undo_stack.last_mut()
        {
          edit.merging = true;
        }
      }

      if self.undo_stack.last().is_some_and(|e| e.is_empty()) {
        self.undo_stack.pop();
      }
    }

    self.fix_cursor();

    if !is_kill {
      self.kill_ring.merging = false;
    }

    if !is_killring_op {
      self.kill_ring.reset();
    }

    if let Some(Hint::Override(hint_lines)) = self.hint.as_ref()
      && !self.lines.is_prefix_lines(hint_lines)
    {
      self.clear_hint();
    }

    self.byte_positions = None;

    res
  }

  pub fn attempt_inline_expansion(&mut self, history: &History) -> bool {
    let hist_res = self.attempt_history_expansion(history);
    let alias_res = self.attempt_alias_expansion();

    hist_res || alias_res
  }

  pub fn attempt_alias_expansion_all(&mut self) -> bool {
    let raw = self.joined();
    let (result, first_pos) = expand_alias_with_pos(raw);
    if first_pos.is_some() {
      self.lines = Lines::to_lines(result);
      true
    } else {
      false
    }
  }

  pub fn attempt_alias_expansion(&mut self) -> bool {
    let (to_cursor, mut after_cursor) = self.lines.clone().split_lines(self.cursor.pos);
    let raw = to_cursor.join();
    let mut tokens = LexStream::new(raw.clone().into(), LexFlags::empty())
      .filter_map(Result::ok)
      .filter(|tk| !matches!(tk.class, TkRule::Soi | TkRule::Eoi | TkRule::Null))
      .collect::<Vec<_>>();
    while tokens
      .last()
      .is_some_and(|tk| !tk.flags.contains(TkFlags::IS_CMD))
    {
      tokens.pop();
    }

    let Some(last) = tokens.pop() else {
      return false;
    };
    if !last.flags.contains(TkFlags::IS_CMD) {
      return false;
    }
    let tk_start = last.span.start();
    let word = last.as_str();

    if let Some(alias) = Shed::logic(|l| l.aliases().get(word).cloned())
      && let alias = alias.to_string()
      && !raw[tk_start..].starts_with(&alias)
    {
      let delta = alias.graphemes(true).count() as isize - word.graphemes(true).count() as isize;
      let expanded = last.replaced(&alias);

      self.lines = Lines::to_lines(expanded);
      self.lines.attach_lines(&mut after_cursor);
      self.cursor.pos = self.cursor.pos.col_add_signed(delta);

      true
    } else {
      false
    }
  }

  /// The inner logic of `attempt_history_expansion()`. This function calls itself recursively when it encounters command substitutions.
  /// This is necessary because of the following nasty edge case:
  /// ```bash
  /// echo "foo $(echo 'bar!') biz"
  /// ```
  /// The exclamation point is inside of both double and single quotes here. According to shell language though, it's really just in single quotes because the command substitution is it's own parsing context.
  /// Pressing enter on this case with a normal flat parse will attempt history expansion. But a parse that recurses into command subs will not.
  /// The easiest way to handle this is to simply do lightweight recursive descent whenever we see the start of a command sub.
  pub fn find_history_expansions(
    &mut self,
    changes: &mut Vec<((Pos, Pos), String)>,
    positions: impl Iterator<Item = (Pos, Grapheme)>,
    history: &History,
    offset: Pos,
  ) -> bool {
    let mut positions = positions.peekable();
    let mut qt_state = QuoteState::default();

    // Map a sub-buffer position to the original buffer's coordinate space.
    // On row 0 of the sub-buffer, columns are offset from the anchor point.
    // On subsequent rows, columns map directly (same line structure).
    let map_pos = |slf: &Self, sub_pos: Pos, offset: Pos| -> Pos {
      if sub_pos.row == 0 {
        let (r, c) = slf.offset_col_wrapping_at(offset.row, sub_pos.col as isize, offset);
        Pos::new(r, c)
      } else {
        Pos::new(offset.row + sub_pos.row, sub_pos.col)
      }
    };

    while let Some((pos, gr)) = positions.next() {
      let Some(ch) = gr.as_char() else { continue };
      match ch {
        symbol @ ('$' | '`') if qt_state.in_double() => {
          let mut lines = vec![];
          let mut cur_line = vec![];
          if let Some((_, gr2)) = positions.peek()
            && let Some('(') = gr2.as_char()
          {
            // command substitution. read until we find matching paren.
            let mut paren_depth = 1;
            match_loop!(positions.next() => (_,gr) => gr.as_char(), {
              Some('\\') => {
                if let Some((_,gr2)) = positions.next() {
                  cur_line.push(gr2.clone());
                }
              }
              Some('$') => {
                let Some((pos,gr2)) = positions.peek() else {
                  cur_line.push(gr.clone());
                  continue
                };
                let Some('(') = gr2.as_char() else {
                  cur_line.push(gr.clone());
                  continue
                };

                positions.next();
                paren_depth += 1;
                cur_line.push(Grapheme::from('$'));
                cur_line.push(Grapheme::from('('));
              }
              Some(')') => {
                paren_depth -= 1;
                if paren_depth == 0 {
                  break;
                }
                cur_line.push(Grapheme::from(')'));
              }

              _ if gr.is_lf() => lines.push(Line(std::mem::take(&mut cur_line))),
              _ => cur_line.push(gr.clone()),
            });

            lines.push(Line(cur_line));
            let sub_positions = Self::enumerate_graphemes(&Lines(lines)).into_iter();
            // offset past "$(" - 2 chars from the '$' position
            let sub_offset = map_pos(self, pos.col_add(2), offset);

            // now we recurse.
            self.find_history_expansions(changes, sub_positions, history, sub_offset);
          } else if symbol == '`' {
            // also command substitution.
            match_loop!(positions.next() => (_,gr) => gr.as_char(), {
              Some('\\') => {
                if let Some((_,gr2)) = positions.next() {
                  cur_line.push(gr2.clone());
                }
              }
              Some('`') => break,

              _ if gr.is_lf() => lines.push(Line(std::mem::take(&mut cur_line))),
              _ => cur_line.push(gr.clone()),
            });

            lines.push(Line(cur_line));
            let sub_positions = Self::enumerate_graphemes(&Lines(lines)).into_iter();
            // offset past "`" - 1 char from the backtick position
            let sub_offset = map_pos(self, pos.col_add(1), offset);

            // now we recurse.
            self.find_history_expansions(changes, sub_positions, history, sub_offset);
          } else {
            positions.next();
            continue;
          };
        }
        '\\' | '$' => {
          positions.next();
        }
        '\'' => qt_state.toggle_single(),
        '"' => qt_state.toggle_double(),
        '!' if !qt_state.in_single() => {
          let start = pos;
          let Some((pos2, gr2)) = positions.next() else {
            continue;
          };
          let Some(ch) = gr2.as_char() else {
            continue;
          };
          match ch {
            '!' => {
              if let Some(prev) = history.last() {
                let raw = prev.command();
                let start = map_pos(self, start, offset);
                changes.push(((start, start.col_add(1)), raw.to_string()));
              }
            }
            '$' => {
              if let Some(prev) = history.last() {
                let raw = prev.command();
                let start = map_pos(self, start, offset);
                if let Some(last_word) = raw.split_whitespace().last() {
                  changes.push(((start, start.col_add(1)), last_word.to_string()));
                }
              }
            }
            ch if !ch.is_whitespace() => {
              if ch == '"' && qt_state.in_double() {
                qt_state.toggle_double();
                continue;
              }
              let mut end = pos2;
              let cur_row = end.row;
              while let Some((pos3, gr3)) = positions.next() {
                if pos3.row > cur_row {
                  break;
                }; // break on linefeed
                let Some(ch) = gr3.as_char() else { break }; // break on non-ascii
                if ch.is_whitespace() {
                  break; // break on whitespace
                } else if matches!(ch, ';' | '&' | '|' | '(' | ')' | '<' | '>') {
                  break; // break on shell metacharacters
                } else if ch == '"' && qt_state.in_double() {
                  qt_state.toggle_double();
                  break;
                };
                end = pos3;
              }
              let pos2 = map_pos(self, pos2, offset);
              let start = map_pos(self, start, offset);
              let end = map_pos(self, end, offset);

              let span = self.yank_span((pos2, end), true);
              let token = span.join();
              let cmd = history.resolve_hist_token(&token).unwrap_or(token);

              changes.push(((start, end), cmd));
            }
            _ => {}
          }
        }
        _ => {}
      }
    }

    !changes.is_empty()
  }

  pub fn attempt_history_expansion(&mut self, history: &History) -> bool {
    let buf = self.joined();
    let tks = get_context_tokens(&buf);
    let mut hist_expansions = vec![];
    for tk in &tks {
      hist_expansions.extend(tk.find_nodes(|n| *n.class() == CtxTkRule::HistExp));
    }
    hist_expansions.sort_by_key(|n| n.span().start());

    let mut any_changes = false;
    let mut changes: Vec<((Pos, Pos), String)> = vec![];
    for exp in hist_expansions {
      let span = exp.span().clone();
      let Some(start) = self.byte_to_pos(span.range().start) else {
        continue;
      };
      let Some(mut end) = self.byte_to_pos(span.range().end) else {
        continue;
      };
      end = end.col_sub(1); // exclusive range
      let change = match history.resolve_hist_token(exp.span().as_str()) {
        Some(s) => {
          any_changes = true;
          s.to_string()
        }
        None => {
          any_changes = true;
          let raw = exp.span().as_str();
          raw
            .strip_prefix('!')
            .map(|s| s.to_string())
            .unwrap_or_else(|| raw.to_string())
        }
      };

      changes.push(((start, end), change));
    }

    for (range, change) in changes.into_iter().rev() {
      let old_len = self.count_graphemes();
      self.replace_range(range, &change);
      let new_len = self.count_graphemes();
      let delta = new_len as isize - old_len as isize;
      let (nr, nc) = self.offset_col_wrapping(self.row(), delta);
      self.cursor.pos.set(nr, nc);
    }

    any_changes
  }

  pub fn search_match_spans(&self) -> Vec<Range<usize>> {
    if let Some(pat) = self.pending_search.as_ref()
      && !pat.is_empty()
      && let Ok(re) = Shed::meta_mut(|m| m.get_regex(pat.clone()))
    {
      let buf = self.joined();
      let positions = self.byte_positions();
      let lookup = |b: usize| -> Option<usize> {
        positions
          .iter()
          .find_map(|(off, p)| (*off >= b).then_some(*off))
      };
      re.find_iter(&buf)
        .filter_map(|m| Some(lookup(m.start())?..lookup(m.end())?))
        .collect()
    } else {
      vec![]
    }
  }
}

impl Display for LineBuf {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    let mut cloned = self.lines.clone();

    // Layer 1: search match highlighting
    if let Some(pat) = self.pending_search.as_ref()
      && !pat.is_empty()
      && let Ok(re) = Shed::meta_mut(|m| m.get_regex(pat.clone()))
    {
      let buf = self.joined();
      // Collect (start_pos, end_pos) pairs first, then insert in reverse
      // so earlier insertions don't shift later byte offsets
      // Build a one-shot byte-to-pos index since we can't mutate the cache
      // through &self.
      let positions = self.byte_positions();
      let lookup = |b: usize| -> Option<Pos> {
        positions
          .iter()
          .find_map(|(off, p)| (*off >= b).then_some(*p))
      };
      let mut spans: Vec<(Pos, Pos)> = re
        .find_iter(&buf)
        .filter_map(|m| Some((lookup(m.start())?, lookup(m.end())?)))
        .collect();
      // Sort by start descending so later positions are inserted first
      spans.sort_by(|a, b| b.0.cmp(&a.0));
      for (s, e) in spans {
        // Insert end marker first (still on its row), then start marker
        if e.col >= cloned[e.row].len() {
          cloned[e.row].push_char(markers::MATCH_END);
        } else {
          cloned[e.row].insert(e.col, markers::MATCH_END.into());
        }
        cloned[s.row].insert(s.col, markers::MATCH_START.into());
      }
    }

    // Layer 2: visual mode selection highlighting
    if let Some(select) = self.select_mode.as_ref() {
      match select {
        SelectMode::Char(pos) => {
          let (s, e) = ordered(self.cursor.pos, *pos);
          if s.row == e.row {
            // Same line: insert end first to avoid shifting start index
            let line = &mut cloned[s.row];
            if e.col + 1 >= line.len() {
              line.push_char(markers::VISUAL_MODE_END);
            } else {
              line.insert(e.col + 1, markers::VISUAL_MODE_END.into());
            }
            line.insert(s.col, markers::VISUAL_MODE_START.into());
          } else {
            // Start line: highlight from s.col to end
            cloned[s.row].insert(s.col, markers::VISUAL_MODE_START.into());
            cloned[s.row].push_char(markers::VISUAL_MODE_END);

            // Middle lines: fully highlighted
            for row in cloned.iter_mut().skip(s.row + 1).take(e.row - s.row - 1) {
              row.insert(0, markers::VISUAL_MODE_START.into());
              row.push_char(markers::VISUAL_MODE_END);
            }

            // End line: highlight from start to e.col
            let end_line = &mut cloned[e.row];
            if e.col + 1 >= end_line.len() {
              end_line.push_char(markers::VISUAL_MODE_END);
            } else {
              end_line.insert(e.col + 1, markers::VISUAL_MODE_END.into());
            }
            end_line.insert(0, markers::VISUAL_MODE_START.into());
          }
        }
        SelectMode::Line(pos) => {
          let (s, e) = ordered(self.row(), pos.row);
          for row in cloned.iter_mut().take(e + 1).skip(s) {
            row.insert(0, markers::VISUAL_MODE_START.into());
          }
          cloned[e].push_char(markers::VISUAL_MODE_END);
        }
        SelectMode::Block(_pos) => unimplemented!(),
      }
    }

    let lines: Vec<String> = cloned.0.iter().map(|line| line.to_string()).collect();
    write!(f, "{}", lines.join("\n"))
  }
}
