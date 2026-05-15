use std::cmp::Ordering;

use super::{Lines, Pos, Shed};

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum Hint {
  Override(Lines),
  History(Lines),
}

impl Hint {
  pub fn set_lines(&mut self, new_lines: Lines) {
    match self {
      Self::Override(lines) | Self::History(lines) => {
        *lines = new_lines;
      }
    }
  }
  pub fn lines(&self) -> &Lines {
    match self {
      Self::Override(lines) | Self::History(lines) => lines,
    }
  }
  pub fn raw(&self) -> String {
    self.lines().join()
  }
  pub fn take_lines(&mut self) -> Lines {
    match self {
      Self::Override(lines) | Self::History(lines) => std::mem::take(lines),
    }
  }
  pub fn display(&self, prefix: Option<&str>) -> String {
    let mut text = self.raw();
    if let Some(prefix) = prefix
      && let Some(rest) = text.strip_prefix(prefix)
    {
      text = rest.to_string();
    }

    format!("\x1b[90m{text}\x1b[0m").replace("\n", "\n\x1b[90m")
  }
  pub fn is_empty(&self) -> bool {
    self.lines().is_empty() || (self.lines().len() == 1 && self.lines()[0].is_empty())
  }
}

impl PartialOrd for Hint {
  fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
    Some(self.cmp(other))
  }
}

impl Ord for Hint {
  /// Defines priority for hint types.
  ///
  /// If a new hint would overwrite an old hint, the 'lesser' hint loses
  ///
  /// 'greater' hints and 'equal' hints both overwrite.
  fn cmp(&self, other: &Self) -> Ordering {
    match self {
      Self::Override(_) => {
        if matches!(other, Self::Override(_)) {
          Ordering::Equal
        } else {
          Ordering::Greater
        }
      }
      Self::History(_) => match other {
        Self::Override(_) => Ordering::Less,
        Self::History(_) => Ordering::Equal,
      },
    }
  }
}

impl super::LineBuf {
  /// Perform an operation that incrementally accepts the hint if the cursor moves into it
  ///
  /// Process:
  /// * take the lines out of self.hint directly
  /// * mark end of buffer position, append hint lines to self.lines
  /// * call the function
  /// * split the buffer at `old_end_pos.max(new_cursor_pos)`
  ///
  /// Notes:
  /// * The size of the hint can never grow as a result of this function. It will only ever stay the same size or shrink.
  pub fn with_hint<F, T>(&mut self, f: F) -> T
  where
    F: FnOnce(&mut Self) -> T,
  {
    let mut hint = self.hint.take();
    let mut old_end_pos = self.end_pos();

    if self.cursor.exclusive {
      old_end_pos = old_end_pos.col_add(1);
    }

    if let Some(h) = hint.as_mut()
      && let Some(mut hint_lines) = h.take_lines().strip_prefix_lines(&self.lines)
    {
      self.lines.attach_lines(&mut hint_lines);
    }
    let old_cursor_pos = self.cursor.pos;

    let result = f(self);

    let new_cursor_pos = self.cursor.pos;

    if let Some(mut hint) = hint {
      let is_past_end = if self.cursor.exclusive {
        new_cursor_pos >= old_end_pos
      } else {
        new_cursor_pos > old_end_pos
      };
      let split_pos = if new_cursor_pos > old_cursor_pos && is_past_end {
        // our cursor moved into the hint.
        let old_len = self.count_graphemes();
        self.attempt_alias_expansion();
        let new_len = self.count_graphemes();
        let delta = new_len as isize - old_len as isize;
        new_cursor_pos.col_add_signed(delta + 1)
      } else {
        old_end_pos
      };

      let hint_lines = self.lines.split_lines_at(split_pos);
      if !hint_lines.is_empty() {
        hint.set_lines(hint_lines);
        self.hint = Some(hint);
      }
    }

    result
  }

  pub fn clear_hint(&mut self) {
    self.hint = None;
  }

  pub fn set_hint(&mut self, hint: Option<Hint>) {
    if self.is_empty() {
      self.hint = None;
      return;
    }

    let Some(hint) = hint else {
      if !matches!(&self.hint, Some(Hint::Override(_))) {
        self.hint = None;
      }
      return;
    };

    if let Some(old_hint) = self.hint.as_ref()
      && *old_hint > hint
    {
      // order comparisons on hints are priority checks
      // if older hint has higher priority, keep it instead of replacing with lower-priority new hint
      return;
    }

    if !Shed::shopts(|o| o.line.auto_suggest) {
      self.hint = None;
      return;
    }

    self.hint = (!hint.is_empty()).then_some(hint);
  }

  pub fn has_hint(&self) -> bool {
    self
      .hint
      .as_ref()
      .is_some_and(|h| !h.lines().is_empty() && h.lines().iter().any(|l| !l.is_empty()))
  }

  pub fn hint_lines(&self) -> Lines {
    Lines(
      self
        .hint
        .as_ref()
        .map(|h| h.lines().to_vec())
        .unwrap_or_default(),
    )
  }

  pub fn get_hint_text(&self) -> String {
    self.try_get_hint_text().unwrap_or_default()
  }

  pub fn try_get_hint_text(&self) -> Option<String> {
    self.hint.as_ref().map(|h| h.display(Some(&self.joined())))
  }

  pub fn try_join_hint(&self) -> Option<String> {
    self.hint.as_ref().map(|h| h.raw())
  }

  pub fn accept_hint(&mut self) {
    let Some(mut hint) = self.hint.take() else {
      return;
    };
    let Some(mut hint_lines) = hint.take_lines().strip_prefix_lines(&self.lines) else {
      return;
    };
    self.lines.attach_lines(&mut hint_lines);
    self.attempt_alias_expansion_all();

    self.set_cursor(Pos::MAX);
    self.fix_cursor();
  }
}
