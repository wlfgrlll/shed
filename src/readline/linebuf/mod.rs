use std::{collections::VecDeque, ops::Range};

use ariadne::Span as AriadneSpan;
use unicode_segmentation::UnicodeSegmentation;

use super::{
  Shed, autocmd,
  context::{CtxTkRule, get_context_tokens},
  editcmd,
  editcmd::{EditCmd, Motion, Verb},
  editmode, eval,
  eval::lex::{LexFlags, LexStream, TkFlags, TkRule},
  expand::expand_alias_with_pos,
  highlight,
  history::History,
  motion, procio, register, sherr, shopt, stash, state, status_msg, system_msg, try_var,
  util::{ShResult, ordered},
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

/// Holds and manages edits for the current in-progress command.
/// This struct is the beating heart of `shed`'s line editor.
///
/// Consumes `EditCmd`s to perform edits.
///
/// # Structure
///
/// As opposed to traditional flat-string style approaches for line editors
/// like `zsh`'s `zle` or `bash`'s `readline`, we instead use a `Lines`, which nests data
/// three layers deep:
/// 1. `Lines(Vec<Line>)` - has methods for operating on ranges of lines
/// 2. `Line(Vec<Grapheme>)` - has methods for operating on ranges of graphemes
/// 3. `Grapheme(SmallVec<[char;4]>)` - has methods for closely inspecting UTF-8 grapheme clusters
///
/// This results in a 2D grid of graphemes. Linewise operations become very simple;
/// lookup is an O(1) index into a vector, operations on whole lines can just use a range like
/// `self.lines[0..5]`, etc. Cursor columns are also simpler in this case; an emoji with
/// several zero-width-joiners is the exact same size as an ascii character. We can perform
/// operations without needing to tip-toe around `char` boundaries.
///
/// # Tradeoffs
///
/// The tradeoff is that contiguous operations spanning multiple lines become somewhat complex to handle.
/// With a flat string you just include the newline in the operation. With our model, we don't have newlines.
///
/// Personally I think the tradeoff is worth it, after working with both the flat string model and the 2D grid model.
/// Scanning for newlines has proven to be an exceptionally fragile method of performing linewise operations, which
/// is what necessitated this design in the first place. In order to have robust support for many of `vim`'s more in-depth
/// features such as line-addressed ex mode commands, this design was what I landed on.
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
