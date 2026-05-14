use crate::{
  readline::editcmd::{
    Bound, Cmd, Dest, Direction, EditCmd, LineAddr, Motion, TextObj, To, Verb, Word,
  },
  state::util::write_meta,
  status_msg,
  util::ShResult,
};

use super::{CharClass, Grapheme, MotionKind, Pos, ordered};

impl super::LineBuf {
  fn find_delim_match(&mut self) -> Option<MotionKind> {
    let is_opener = |g: &Grapheme| matches!(g.as_char(), Some(c) if "([{<".contains(c));
    let is_closer = |g: &Grapheme| matches!(g.as_char(), Some(c) if ")]}>".contains(c));
    let is_delim = |g: &Grapheme| is_opener(g) || is_closer(g);
    let first = self.scan_forward(is_delim)?;

    let delim_match = if is_closer(self.gr_at(first)?) {
      let mut depth = 0;
      let opener = match self.gr_at(first)?.as_char()? {
        ')' => '(',
        ']' => '[',
        '}' => '{',
        '>' => '<',
        _ => unreachable!(),
      };
      self.scan_backward_from(first, |g| {
        if g.as_char() == self.gr_at(first).and_then(|c| c.as_char()) {
          depth += 1;
        } else if g.as_char() == Some(opener) {
          depth -= 1;
        }
        depth == 0
      })?
    } else if is_opener(self.gr_at(first)?) {
      let mut depth = 0;
      let closer = match self.gr_at(first)?.as_char()? {
        '(' => ')',
        '[' => ']',
        '{' => '}',
        '<' => '>',
        _ => unreachable!(),
      };
      self.scan_forward_from(first, |g| {
        if g.as_char() == self.gr_at(first).and_then(|c| c.as_char()) {
          depth += 1;
        } else if g.as_char() == Some(closer) {
          depth -= 1;
        }
        depth == 0
      })?
    } else {
      unreachable!()
    };

    Some(MotionKind::Char {
      start: self.cursor.pos,
      end: delim_match,
      inclusive: true,
    })
  }
  /// Given a LineAddr, resolve it to an absolute line number.
  ///
  /// This is used for commands like `:3` or `:'a` where we need to convert the address into a line number in the buffer.
  pub fn resolve_line_addr(&self, addr: &LineAddr) -> ShResult<Option<usize>> {
    match addr {
      LineAddr::Number(n) => Ok(Some(
        (n.saturating_sub(1)).min(self.lines.len().saturating_sub(1)),
      )),
      LineAddr::Current => Ok(Some(self.row())),
      LineAddr::Last => Ok(Some(self.lines.len().saturating_sub(1))),
      LineAddr::Offset(i) => Ok(Some(self.row().saturating_add_signed(*i))),
      dir @ (LineAddr::Pattern(re) | LineAddr::PatternRev(re)) => {
        let reg = match write_meta(|m| m.get_regex(re.clone())) {
          Ok(re) => re,
          Err(e) => {
            status_msg!("{e}");
            return Ok(None);
          }
        };
        let off = if matches!(dir, LineAddr::Pattern(_)) {
          1
        } else {
          -1
        };
        let inc_acc =
          |acc: usize| (acc as isize + off).rem_euclid(self.lines.len() as isize) as usize;
        let mut acc = inc_acc(self.row());

        while let Some(row) = self.get_row(acc) {
          let row_str = row.to_string();
          if reg.is_match(&row_str) {
            return Ok(Some(acc));
          }

          if acc == self.row() {
            break;
          }
          acc = inc_acc(acc);
        }

        Ok(None)
      }
      LineAddr::Mark(ch) => {
        match ch {
          anchor @ ('<' | '>') => {
            let Some(select_range) = self.select_range() else {
              return Ok(None);
            };
            let (s, e) = match select_range {
              Motion::CharRange(s, e) => (s.row, e.row),
              Motion::LineRange(s, e) => {
                let Some(s) = self.resolve_line_addr(&s)? else {
                  return Ok(None);
                };
                let Some(e) = self.resolve_line_addr(&e)? else {
                  return Ok(None);
                };
                (s, e)
              }
              _ => unreachable!(),
            };
            match anchor {
              '<' => Ok(Some(s)),
              '>' => Ok(Some(e)),
              _ => unreachable!(),
            }
          }
          _ => Ok(None), // TODO: implement marks
        }
      }
    }
  }

  fn search(&mut self, motion: &Motion, save: bool, count: usize) -> Option<MotionKind> {
    let Motion::Search(pat, dir) = motion else {
      return None;
    };
    let re = match write_meta(|m| m.get_regex(pat.clone())) {
      Ok(re) => re,
      Err(e) => {
        status_msg!("{e}");
        return None;
      }
    };
    let buf = self.joined();
    let mut offset = self.pos_to_byte(self.cursor.pos)?;
    let mut target_byte = None;

    for _ in 0..count {
      target_byte = match dir {
        Direction::Forward => re
          .find_at(&buf, offset + 1)
          .or_else(|| re.find(&buf))
          .map(|m| m.start()),
        Direction::Backward => {
          let matches: Vec<_> = re.find_iter(&buf).collect();
          matches
            .iter()
            .rev()
            .find(|m| m.start() < offset)
            .or_else(|| matches.last())
            .map(|m| m.start())
        }
      };
      offset = target_byte?;
    }

    target_byte.and_then(|b| self.byte_to_pos(b)).map(|target| {
      if save {
        self.last_search = Some(motion.clone());
      }
      MotionKind::Char {
        start: self.cursor.pos,
        end: target,
        inclusive: false,
      }
    })
  }
  /// Wrapper for eval_motion_inner that calls it with `check_hint: false`
  pub(super) fn eval_motion(&mut self, cmd: &EditCmd) -> ShResult<Option<MotionKind>> {
    self.eval_motion_inner(cmd, false)
  }
  pub(super) fn eval_motion_with_hint(&mut self, cmd: &EditCmd) -> ShResult<Option<MotionKind>> {
    self.eval_motion_inner(cmd, true)
  }
  fn eval_motion_inner(&mut self, cmd: &EditCmd, check_hint: bool) -> ShResult<Option<MotionKind>> {
    let EditCmd { verb, motion, .. } = cmd;
    let Some(Cmd(count, motion)) = motion.as_ref() else {
      return Ok(None);
    };
    let mut motion = motion.clone();

    if let Motion::Selection(mode) = motion
      && let Some(new) = self.evaluate_select_shape(&mode)
    {
      motion = new;
    }

    let eval = |this: &mut Self| -> ShResult<Option<MotionKind>> {
      let kind = match &motion {
        Motion::WholeLine => {
          let start = this.row();
          let end =
            (this.row() + (count.saturating_sub(1))).min(this.lines.len().saturating_sub(1));
          Some(MotionKind::Line {
            start,
            end,
            inclusive: true,
          })
        }
        Motion::TextObj(text_obj) => this.dispatch_text_obj(text_obj.clone()),
        Motion::EndOfLastWord => {
          let row = this.row() + (count.saturating_sub(1));
          let line = this.line_mut(row);
          let mut target = Pos { row, col: 0 };
          for (i, gr) in line.0.iter().enumerate() {
            if !gr.is_ws() {
              target.col = i;
            }
          }

          (target != this.cursor.pos).then_some(MotionKind::Char {
            start: this.cursor.pos,
            end: target,
            inclusive: true,
          })
        }
        Motion::StartOfFirstWord => {
          let row = this.row() + count.saturating_sub(1);
          let mut target = Pos { row, col: 0 };
          let line = this.line(row);
          for (i, gr) in line.0.iter().enumerate() {
            target.col = i;
            if !gr.is_ws() {
              break;
            }
          }

          (target != this.cursor.pos).then_some(MotionKind::Char {
            start: this.cursor.pos,
            end: target,
            inclusive: true,
          })
        }
        dir @ (Motion::StartOfLine | Motion::EndOfLine) => {
          let (inclusive, off) = match dir {
            Motion::StartOfLine => (false, isize::MIN),
            Motion::EndOfLine => (true, isize::MAX),
            _ => unreachable!(),
          };
          let row_offset = count.saturating_sub(1);
          let target = this.offset_cursor(row_offset as isize, off);
          (target != this.cursor.pos).then_some(MotionKind::Char {
            start: this.cursor.pos,
            end: target,
            inclusive,
          })
        }
        Motion::WordMotion(to, word, dir) => {
          // 'cw' is a weird case
          // if you are on the word's left boundary, it will not delete whitespace after
          // the end of the word
          let ignore_trailing_ws = matches!(verb, Some(Cmd(_, Verb::Change)),)
            && matches!(
              motion,
              Motion::WordMotion(To::Start, _, Direction::Forward,)
            );
          let inclusive = verb.is_none();

          this.eval_word_motion(*count, to, word, dir, ignore_trailing_ws, inclusive)
        }
        Motion::CharSearch(dir, dest, char) => {
          let off = this.search_char(dir, dest, char, *count);
          let target = this.offset_cursor(0, off);
          let inclusive = matches!(dir, Direction::Forward);
          (target != this.cursor.pos).then_some(MotionKind::Char {
            start: this.cursor.pos,
            end: target,
            inclusive,
          })
        }
        dir @ (Motion::BackwardChar | Motion::ForwardChar)
        | dir @ (Motion::BackwardCharForced | Motion::ForwardCharForced) => {
          let (off, wrap) = match dir {
            Motion::BackwardChar => (-(*count as isize), false),
            Motion::ForwardChar => (*count as isize, false),
            Motion::BackwardCharForced => (-(*count as isize), true),
            Motion::ForwardCharForced => (*count as isize, true),
            _ => unreachable!(),
          };
          let target = if wrap {
            this.offset_cursor_wrapping(0, off)
          } else {
            this.offset_cursor(0, off)
          };

          (target != this.cursor.pos).then_some(MotionKind::Char {
            start: this.cursor.pos,
            end: target,
            inclusive: false,
          })
        }
        dir @ (Motion::LineDown | Motion::LineUp) => {
          let off = match dir {
            Motion::LineUp => -(*count as isize),
            Motion::LineDown => *count as isize,
            _ => unreachable!(),
          };
          if verb.is_some() {
            let row = this.row();
            let target_row = this.offset_row(off);
            let (s, e) = ordered(row, target_row);
            Some(MotionKind::Line {
              start: s,
              end: e,
              inclusive: true,
            })
          } else {
            if this.saved_col.is_none() {
              this.saved_col = Some(this.calc_cursor_display_col());
            }
            let row = this.offset_row(off);
            let limit = if this.cursor.exclusive {
              this.lines[row].len().saturating_sub(1)
            } else {
              this.lines[row].len()
            };
            let target_col = this.saved_col.unwrap();
            let col = this.display_col_to_index(row, target_col).min(limit);
            let target = Pos { row, col };
            (target != this.cursor.pos).then_some(MotionKind::Char {
              start: this.cursor.pos,
              end: target,
              inclusive: true,
            })
          }
        }
        dir @ (Motion::EndOfBuffer | Motion::StartOfBuffer) => {
          let off = match dir {
            Motion::StartOfBuffer => isize::MIN,
            Motion::EndOfBuffer => isize::MAX,
            _ => unreachable!(),
          };
          if verb.is_some() {
            let row = this.row();
            let target_row = this.offset_row(off);
            let (s, e) = ordered(row, target_row);
            Some(MotionKind::Line {
              start: s,
              end: e,
              inclusive: true,
            })
          } else {
            let target = this.offset_cursor(off, 0);
            (target != this.cursor.pos).then_some(MotionKind::Char {
              start: this.cursor.pos,
              end: target,
              inclusive: true,
            })
          }
        }
        Motion::ToColumn => {
          let row = this.row();
          let end = Pos {
            row,
            col: count.saturating_sub(1),
          };
          Some(MotionKind::Char {
            start: this.cursor.pos,
            end,
            inclusive: end > this.cursor.pos,
          })
        }

        Motion::Search(..) => this.search(&motion, true, *count),

        Motion::RepeatSearch => {
          if let Some(search) = this.last_search.clone() {
            this.search(&search, false, *count)
          } else {
            None
          }
        }

        Motion::RepeatSearchRev => {
          if let Some(search) = &this.last_search {
            let rev_search = match search {
              Motion::Search(pat, dir) => {
                let rev_dir = match dir {
                  Direction::Forward => Direction::Backward,
                  Direction::Backward => Direction::Forward,
                };
                Motion::Search(pat.clone(), rev_dir)
              }
              _ => unreachable!(),
            };
            this.search(&rev_search, false, *count)
          } else {
            None
          }
        }

        Motion::ToDelimMatch => this.find_delim_match(),
        Motion::ToParen(direction) | Motion::ToBrace(direction) => {
          let (opener, closer) = match motion {
            Motion::ToParen(_) => ('(', ')'),
            Motion::ToBrace(_) => ('{', '}'),
            _ => unreachable!(),
          };
          match direction {
            Direction::Forward => {
              let mut depth = 0;
              let Some(target_pos) = this.scan_forward(|g| {
                if g.as_char() == Some(opener) {
                  depth += 1;
                }
                if g.as_char() == Some(closer) {
                  depth -= 1;
                  if depth <= 0 {
                    return true;
                  }
                }
                false
              }) else {
                return Ok(None);
              };
              return Ok(Some(MotionKind::Char {
                start: this.cursor.pos,
                end: target_pos,
                inclusive: true,
              }));
            }
            Direction::Backward => {
              let mut depth = 0;
              let Some(target_pos) = this.scan_backward(|g| {
                if g.as_char() == Some(closer) {
                  depth += 1;
                }
                if g.as_char() == Some(opener) {
                  depth -= 1;
                  if depth <= 0 {
                    return true;
                  }
                }
                false
              }) else {
                return Ok(None);
              };
              return Ok(Some(MotionKind::Char {
                start: this.cursor.pos,
                end: target_pos,
                inclusive: true,
              }));
            }
          }
        }

        Motion::CharRange(s, e) => {
          let (s, e) = ordered(*s, *e);
          Some(MotionKind::Char {
            start: s,
            end: e,
            inclusive: true,
          })
        }
        Motion::Line(l) => {
          let Some(l) = this.resolve_line_addr(l)? else {
            return Ok(None);
          };
          Some(MotionKind::Line {
            start: l,
            end: l + 1,
            inclusive: false,
          })
        }
        Motion::LineRange(s, e) => {
          let Some(s) = this.resolve_line_addr(s)? else {
            return Ok(None);
          };
          let Some(e) = this.resolve_line_addr(e)? else {
            return Ok(None);
          };
          let (s, e) = ordered(s, e);
          Some(MotionKind::Line {
            start: s,
            end: e,
            inclusive: true,
          })
        }
        Motion::BlockRange(s, e) => {
          let (s, e) = ordered(*s, *e);
          Some(MotionKind::Block { start: s, end: e })
        }
        dir @ (Motion::HalfScreenUp | Motion::HalfScreenDown) => {
          let off = match dir {
            Motion::HalfScreenUp => -(this.get_viewport_height() as isize / 2),
            Motion::HalfScreenDown => this.get_viewport_height() as isize / 2,
            _ => unreachable!(),
          };
          let row = this.row();
          let target_row = this.offset_row(off);
          Some(MotionKind::Line {
            start: target_row,
            end: row,
            inclusive: false,
          })
        }
        Motion::RepeatMotion | Motion::RepeatMotionRev => None,
        Motion::Null => None,
        Motion::Selection(_) => {
          unreachable!()
        }
      };
      Ok(kind)
    };

    if check_hint {
      self.with_hint(eval)
    } else {
      eval(self)
    }
  }
  /// Wrapper for apply_motion_inner that calls it with `accept_hint: false`
  pub(super) fn apply_motion(&mut self, motion: MotionKind) -> ShResult<()> {
    self.apply_motion_inner(motion, false)
  }
  pub(super) fn apply_motion_with_hint(&mut self, motion: MotionKind) -> ShResult<()> {
    self.apply_motion_inner(motion, true)
  }
  fn apply_motion_inner(&mut self, motion: MotionKind, accept_hint: bool) -> ShResult<()> {
    let apply = |this: &mut Self| -> ShResult<()> {
      match motion {
        MotionKind::Char { end, .. } => {
          this.set_cursor(end);
        }
        MotionKind::Line { start, .. } => {
          this.set_row(start);
        }
        MotionKind::Block { .. } => unimplemented!(),
      }
      Ok(())
    };

    if accept_hint {
      self.with_hint(apply)
    } else {
      apply(self)
    }
  }
  pub(super) fn motion_mutation(
    &mut self,
    motion: &MotionKind,
    mut f: impl FnMut(&Grapheme) -> Grapheme,
  ) {
    match motion {
      MotionKind::Char {
        start,
        end,
        inclusive,
      } => {
        let (s, e) = ordered(start, end);
        if s.row == e.row {
          let range = if *inclusive {
            s.col..e.col + 1
          } else {
            s.col..e.col
          };
          for col in range {
            if col >= self.lines[s.row].len() {
              break;
            }
            self.lines[s.row][col] = f(&self.lines[s.row][col]);
          }
          return;
        }
        let end = if *inclusive { e.col + 1 } else { e.col };

        for col in s.col..self.lines[s.row].len() {
          self.lines[s.row][col] = f(&self.lines[s.row][col]);
        }
        for row in s.row + 1..e.row {
          for col in 0..self.lines[row].len() {
            self.lines[row][col] = f(&self.lines[row][col]);
          }
        }
        for col in 0..end {
          if col >= self.lines[e.row].len() {
            break;
          }
          self.lines[e.row][col] = f(&self.lines[e.row][col]);
        }
      }
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
        for row in *start..=end {
          let line = self.line_mut(row);
          for col in 0..line.len() {
            line[col] = f(&line[col]);
          }
        }
      }
      MotionKind::Block { .. } => unimplemented!(),
    }
  }
  fn search_char(&self, dir: &Direction, dest: &Dest, char: &Grapheme, count: usize) -> isize {
    let mut off: isize = 0;
    'outer: for it in 0..count {
      let pos = self.offset_cursor(0, off);
      match dir {
        Direction::Forward => {
          let slice = self.line_from_pos(pos);
          for (i, gr) in slice.iter().enumerate().skip(1) {
            let i = i as isize;
            if gr != char {
              continue;
            }
            match dest {
              Dest::On => {
                off += i;
                continue 'outer;
              }
              Dest::Before => {
                if it != count.saturating_sub(1) {
                  // there are more iterations to go
                  // if we land before, we will stop in the same
                  // place next time around
                  off += i;
                } else {
                  off += (i - 1).max(0);
                }
                continue 'outer;
              }
            }
          }
          return 0; // not found
        }
        Direction::Backward => {
          let slice = self.line_to_pos(pos);
          for (i, gr) in slice.iter().rev().enumerate() {
            let i = i as isize;
            if gr != char {
              continue;
            }
            match dest {
              Dest::On => {
                off -= i + 1;
                continue 'outer;
              }
              Dest::Before => {
                if it != count.saturating_sub(1) {
                  // there are more iterations to go
                  // if we land before, we will stop in the same
                  // place next time around
                  off -= i + 1;
                } else {
                  off -= i;
                }
                continue 'outer;
              }
            }
          }
          return 0; // not found
        }
      }
    }

    off
  }
  pub(super) fn eval_word_motion(
    &self,
    count: usize,
    to: &To,
    word: &Word,
    dir: &Direction,
    ignore_trailing_ws: bool,
    mut inclusive: bool,
  ) -> Option<MotionKind> {
    let mut target = self.cursor.pos;

    for i in 0..count {
      let last = i == count - 1;
      let iws = ignore_trailing_ws && last; // only ignore on the last iteration
      match (to, dir) {
        (To::Start, Direction::Forward) => {
          // 'w' is a special snowflake motion so we need these two extra arguments
          // if we hit the ignore_trailing_ws path in the function,
          // inclusive is flipped to true.
          target = self
            .word_motion_w(word, target, iws, &mut inclusive)
            .unwrap_or_else(|| {
              // we set inclusive to true so that we catch the entire word
              // instead of ignoring the last character
              inclusive = true;
              Pos::MAX
            });
        }
        (To::End, Direction::Forward) => {
          inclusive = true;
          target = self.word_motion_e(word, target).unwrap_or(Pos::MAX);
        }
        (To::Start, Direction::Backward) => {
          target = self.word_motion_b(word, target).unwrap_or(Pos::MIN);
        }
        (To::End, Direction::Backward) => {
          inclusive = true;
          target = self.word_motion_ge(word, target).unwrap_or(Pos::MIN);
        }
      }
    }

    target.clamp_row(&self.lines);
    target.clamp_col(&self.lines[target.row].0, self.cursor.exclusive);

    Some(MotionKind::Char {
      start: self.cursor.pos,
      end: target,
      inclusive,
    })
  }
  fn word_motion_w(
    &self,
    word: &Word,
    start: Pos,
    ignore_trailing_ws: bool,
    inclusive: &mut bool,
  ) -> Option<Pos> {
    use CharClass as C;

    // get our iterator of char classes
    // we dont actually care what the chars are
    // just what they look like.
    // we are going to use .find() a lot to advance the iterator
    let mut classes = self.char_classes_forward_from(start).peekable();

    match word {
      Word::Big => {
        if let Some((_, C::Whitespace)) = classes.peek() {
          // we are on whitespace. advance to the next non-ws char class
          return classes.find(|(_, c)| !c.is_ws()).map(|(p, _)| p);
        }

        let last_non_ws = classes.find(|(_, c)| c.is_ws());
        if ignore_trailing_ws {
          return last_non_ws.map(|(p, _)| p);
        }
        classes.find(|(_, c)| !c.is_ws()).map(|(p, _)| p)
      }
      Word::Normal => {
        if let Some((_, C::Whitespace)) = classes.peek() {
          // we are on whitespace. advance to the next non-ws char class
          return classes.find(|(_, c)| !c.is_ws()).map(|(p, _)| p);
        }

        // go forward until we find some char class that isnt this one
        let mut last = classes.next()?;
        let first_c = last.1;
        while let Some((p, c)) = classes.next() {
          match c {
            C::Whitespace => {
              if ignore_trailing_ws {
                *inclusive = true;
                return Some(last.0);
              } else {
                break;
              }
            }
            c if !c.is_other_class_or_ws(&first_c) => {
              last = (p, c);
            }
            _ => return Some(p),
          }
        }

        // we found whitespace previously, look for the next non-whitespace char class
        classes.find(|(_, c)| !c.is_ws()).map(|(p, _)| p)
      }
    }
  }
  fn word_motion_b(&self, word: &Word, start: Pos) -> Option<Pos> {
    use CharClass as C;
    // get our iterator again
    let mut classes = self.char_classes_backward_from(start).peekable();

    match word {
      Word::Big => {
        classes.next();
        // for 'b', we handle starting on whitespace differently than 'w'
        // we don't return immediately if find() returns Some() here.
        let first_non_ws = if let Some((_, C::Whitespace)) = classes.peek() {
          // we use find() to advance the iterator as usual
          // but we can also be clever and use the question mark
          // to return early if we don't find a word backwards
          classes.find(|(_, c)| !c.is_ws())?
        } else {
          classes.next()?
        };

        // ok now we are off that whitespace
        // now advance backwards until we find more whitespace, or next() is None

        let mut last = first_non_ws;
        while let Some((_, c)) = classes.peek() {
          if c.is_ws() {
            break;
          }
          last = classes.next()?;
        }
        Some(last.0)
      }
      Word::Normal => {
        classes.next();
        let first_non_ws = if let Some((_, C::Whitespace)) = classes.peek() {
          classes.find(|(_, c)| !c.is_ws())?
        } else {
          classes.next()?
        };

        // ok, off the whitespace
        // now advance until we find any different char class at all
        let mut last = first_non_ws;
        while let Some((_, c)) = classes.peek() {
          if c.is_other_class(&last.1) {
            break;
          }
          last = classes.next()?;
        }

        Some(last.0)
      }
    }
  }
  fn word_motion_e(&self, word: &Word, start: Pos) -> Option<Pos> {
    use CharClass as C;
    let mut classes = self.char_classes_forward_from(start).peekable();

    match word {
      Word::Big => {
        classes.next(); // unconditionally skip first position for 'e'
        let first_non_ws = if let Some((_, C::Whitespace)) = classes.peek() {
          classes.find(|(_, c)| !c.is_ws())?
        } else {
          classes.next()?
        };

        let mut last = first_non_ws;
        while let Some((_, c)) = classes.peek() {
          if c.is_ws() {
            return Some(last.0);
          }
          last = classes.next()?;
        }
        None
      }
      Word::Normal => {
        classes.next();
        let first_non_ws = if let Some((_, C::Whitespace)) = classes.peek() {
          classes.find(|(_, c)| !c.is_ws())?
        } else {
          classes.next()?
        };

        let mut last = first_non_ws;
        while let Some((_, c)) = classes.peek() {
          if c.is_other_class_or_ws(&first_non_ws.1) {
            return Some(last.0);
          }
          last = classes.next()?;
        }
        None
      }
    }
  }
  fn word_motion_ge(&self, word: &Word, start: Pos) -> Option<Pos> {
    use CharClass as C;
    let mut classes = self.char_classes_backward_from(start).peekable();

    match word {
      Word::Big => {
        classes.next(); // unconditionally skip first position for 'ge'
        if matches!(classes.peek(), Some((_, c)) if !c.is_ws()) {
          classes.find(|(_, c)| c.is_ws());
        }

        classes.find(|(_, c)| !c.is_ws()).map(|(p, _)| p)
      }
      Word::Normal => {
        classes.next();
        if let Some((_, C::Whitespace)) = classes.peek() {
          return classes.find(|(_, c)| !c.is_ws()).map(|(p, _)| p);
        }

        let cur_class = classes.peek()?.1;
        let bound = classes.find(|(_, c)| c.is_other_class(&cur_class))?;

        if bound.1.is_ws() {
          classes.find(|(_, c)| !c.is_ws()).map(|(p, _)| p)
        } else {
          Some(bound.0)
        }
      }
    }
  }
  fn dispatch_text_obj(&mut self, obj: TextObj) -> Option<MotionKind> {
    match obj {
      // text structures
      TextObj::Word(word, bound) => self.text_obj_word(self.cursor.pos, word, bound),
      TextObj::Sentence(_)
      | TextObj::Paragraph(_)
      | TextObj::WholeSentence(_)
      | TextObj::WholeParagraph(_) => {
        log::warn!("{:?} text objects are not implemented yet", obj);
        None
      }

      // quote stuff
      TextObj::DoubleQuote(bound) | TextObj::SingleQuote(bound) | TextObj::BacktickQuote(bound) => {
        self.text_obj_quote(obj, bound)
      }

      // delimited blocks
      TextObj::Paren(bound)
      | TextObj::Bracket(bound)
      | TextObj::Brace(bound)
      | TextObj::Angle(bound) => self.text_obj_delim(obj, bound),
    }
  }
  pub(super) fn text_obj_word(
    &mut self,
    from: Pos,
    word: Word,
    bound: Bound,
  ) -> Option<MotionKind> {
    use CharClass as C;
    let mut fwd_classes = self.char_classes_forward_from(from);
    let first_class = fwd_classes.next()?;
    match first_class {
      (pos, C::Whitespace) => match bound {
        Bound::Inside => {
          let mut fwd_classes = self.char_classes_forward_from(pos).peekable();
          let mut bkwd_classes = self.char_classes_backward_from(pos).peekable();
          let mut first = (pos, C::Whitespace);
          let mut last = (pos, C::Whitespace);
          while let Some((_, c)) = bkwd_classes.peek() {
            if !c.is_ws() {
              break;
            }
            first = bkwd_classes.next()?;
          }

          while let Some((_, c)) = fwd_classes.peek() {
            if !c.is_ws() {
              break;
            }
            last = fwd_classes.next()?;
          }

          Some(MotionKind::Char {
            start: first.0,
            end: last.0,
            inclusive: true,
          })
        }
        Bound::Around => {
          let mut fwd_classes = self.char_classes_forward_from(pos).peekable();
          let mut bkwd_classes = self.char_classes_backward_from(pos).peekable();
          let mut first = (pos, C::Whitespace);
          let mut last = (pos, C::Whitespace);
          while let Some((_, cl)) = bkwd_classes.peek() {
            if !cl.is_ws() {
              break;
            }
            first = bkwd_classes.next()?;
          }

          while let Some((_, cl)) = fwd_classes.peek() {
            if !cl.is_ws() {
              break;
            }
            last = fwd_classes.next()?;
          }
          let word_class = fwd_classes.next()?.1;
          while let Some((_, cl)) = fwd_classes.peek() {
            match word {
              Word::Big => {
                if cl.is_ws() {
                  break;
                }
              }
              Word::Normal => {
                if cl.is_other_class_or_ws(&word_class) {
                  break;
                }
              }
            }
            last = fwd_classes.next()?;
          }

          Some(MotionKind::Char {
            start: first.0,
            end: last.0,
            inclusive: true,
          })
        }
      },
      (pos, c) => {
        let break_cond = |cl: &C, c: &C| -> bool {
          match word {
            Word::Big => cl.is_ws(),
            Word::Normal => cl.is_other_class(c),
          }
        };
        match bound {
          Bound::Inside => {
            let mut fwd_classes = self.char_classes_forward_from(pos).peekable();
            let mut bkwd_classes = self.char_classes_backward_from(pos).peekable();
            let mut first = (pos, c);
            let mut last = (pos, c);

            while let Some((_, cl)) = bkwd_classes.peek() {
              if break_cond(cl, &c) {
                break;
              }
              first = bkwd_classes.next()?;
            }

            while let Some((_, cl)) = fwd_classes.peek() {
              if break_cond(cl, &c) {
                break;
              }
              last = fwd_classes.next()?;
            }

            Some(MotionKind::Char {
              start: first.0,
              end: last.0,
              inclusive: true,
            })
          }
          Bound::Around => {
            let mut fwd_classes = self.char_classes_forward_from(pos).peekable();
            let mut bkwd_classes = self.char_classes_backward_from(pos).peekable();
            let mut first = (pos, c);
            let mut last = (pos, c);

            while let Some((_, cl)) = bkwd_classes.peek() {
              if break_cond(cl, &c) {
                break;
              }
              first = bkwd_classes.next()?;
            }

            while let Some((_, cl)) = fwd_classes.peek() {
              if break_cond(cl, &c) {
                break;
              }
              last = fwd_classes.next()?;
            }

            // Include trailing whitespace
            while let Some((_, cl)) = fwd_classes.peek() {
              if !cl.is_ws() {
                break;
              }
              last = fwd_classes.next()?;
            }

            Some(MotionKind::Char {
              start: first.0,
              end: last.0,
              inclusive: true,
            })
          }
        }
      }
    }
  }
  fn text_obj_quote(&mut self, obj: TextObj, bound: Bound) -> Option<MotionKind> {
    let q_ch = match obj {
      TextObj::DoubleQuote(_) => '"',
      TextObj::SingleQuote(_) => '\'',
      TextObj::BacktickQuote(_) => '`',
      _ => unreachable!(),
    };

    let start_pos = self
      .scan_backward(|g| g.as_char() == Some(q_ch))
      .or_else(|| self.scan_forward(|g| g.as_char() == Some(q_ch)))?;

    let mut scan_start_pos = start_pos;
    let line_len = self.lines[scan_start_pos.row].len();
    scan_start_pos.col = (scan_start_pos.col + 1).min(line_len.saturating_sub(1));

    let mut end_pos = self.scan_forward_from(scan_start_pos, |g| g.as_char() == Some(q_ch))?;

    match bound {
      Bound::Around => {
        // Around for quoted structures is weird. We have to include any trailing whitespace in the range.
        end_pos.col += 1;
        let mut classes = self.char_classes_forward_from(end_pos);
        end_pos = classes
          .find(|(_, c)| !c.is_ws())
          .map(|(p, _)| p)
          .unwrap_or(self.end_pos());

        (start_pos <= end_pos).then_some(MotionKind::Char {
          start: start_pos,
          end: end_pos,
          inclusive: false,
        })
      }
      Bound::Inside => {
        let mut start_pos = start_pos;
        start_pos.col += 1;
        (start_pos <= end_pos).then_some(MotionKind::Char {
          start: start_pos,
          end: end_pos,
          inclusive: false,
        })
      }
    }
  }
  fn text_obj_delim(&mut self, obj: TextObj, bound: Bound) -> Option<MotionKind> {
    let (opener, closer) = match obj {
      TextObj::Paren(_) => ('(', ')'),
      TextObj::Bracket(_) => ('[', ']'),
      TextObj::Brace(_) => ('{', '}'),
      TextObj::Angle(_) => ('<', '>'),
      _ => unreachable!(),
    };
    let mut depth = 0;
    let start_pos = self
      .scan_backward(|g| {
        if g.as_char() == Some(closer) {
          depth += 1;
        }
        if g.as_char() == Some(opener) {
          if depth == 0 {
            return true;
          }
          depth -= 1;
        }
        false
      })
      .or_else(|| self.scan_forward(|g| g.as_char() == Some(opener)))?;

    depth = 0;
    let end_pos = self.scan_forward_from(start_pos, |g| {
      if g.as_char() == Some(opener) {
        depth += 1;
      }
      if g.as_char() == Some(closer) {
        depth -= 1;
      }
      depth == 0
    })?;

    match bound {
      Bound::Around => Some(MotionKind::Char {
        start: start_pos,
        end: end_pos,
        inclusive: true,
      }),
      Bound::Inside => {
        let mut start_pos = start_pos;
        start_pos.col += 1;
        (start_pos <= end_pos).then_some(MotionKind::Char {
          start: start_pos,
          end: end_pos,
          inclusive: false,
        })
      }
    }
  }
}
