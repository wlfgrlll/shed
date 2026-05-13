use std::ops::Range;

use crate::readline::editcmd::{LineAddr, Motion};

use super::{MotionKind, Pos, SignedPos, ordered};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SelectMode {
  Char(Pos),
  Line(Pos),
  Block(Pos),
}

impl SelectMode {
  pub fn shape(&self, other: Pos) -> SelectShape {
    match self {
      SelectMode::Char(pos) => {
        let (s, e) = ordered(*pos, other);
        // offset points from lower end (s) to upper end (e) - always non-negative
        SelectShape::Char(e.difference(&s))
      }
      SelectMode::Line(pos) => {
        let (s, e) = ordered(*pos, other);
        SelectShape::Line(e.difference(&s))
      }
      SelectMode::Block(pos) => {
        let (s, e) = ordered(*pos, other);
        SelectShape::Block(e.difference(&s))
      }
    }
  }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SelectShape {
  Char(SignedPos),
  Line(SignedPos),
  Block(SignedPos),
}

impl SelectShape {
  pub fn pos(&self) -> SignedPos {
    match self {
      SelectShape::Char(pos) | SelectShape::Line(pos) | SelectShape::Block(pos) => *pos,
    }
  }

  pub fn into_select_mode(self, resolved: Pos) -> SelectMode {
    match self {
      SelectShape::Char(_) => SelectMode::Char(resolved),
      SelectShape::Line(_) => SelectMode::Line(resolved),
      SelectShape::Block(_) => SelectMode::Block(resolved),
    }
  }
}

impl super::LineBuf {
  pub fn start_char_select(&mut self) {
    self.select_mode = Some(SelectMode::Char(self.cursor.pos));
  }

  pub fn start_line_select(&mut self) {
    self.select_mode = Some(SelectMode::Line(self.cursor.pos));
  }

  pub fn start_block_select(&mut self) {
    self.select_mode = Some(SelectMode::Block(self.cursor.pos));
  }

  pub fn stop_selecting(&mut self) {
    if self.select_mode.is_some() {
      self.last_selection = self.select_mode.map(|m| {
        let anchor = match m {
          SelectMode::Char(a) | SelectMode::Block(a) | SelectMode::Line(a) => a,
        };
        (m, anchor)
      });
    }
    self.select_mode = None;
  }

  pub fn select_motion(&self) -> Option<MotionKind> {
    let range = self.select_range()?;
    match range {
      Motion::CharRange(s, e) => {
        let (s, e) = ordered(s, e);
        Some(MotionKind::Char {
          start: s,
          end: e,
          inclusive: true,
        })
      }
      Motion::LineRange(s, e) => {
        let s = self.resolve_line_addr(&s).ok()??;
        let e = self.resolve_line_addr(&e).ok()??;
        let (s, e) = ordered(s, e);
        Some(MotionKind::Line {
          start: s,
          end: e,
          inclusive: true,
        })
      }
      Motion::BlockRange(..) => todo!(),
      _ => unreachable!(),
    }
  }

  /// Absolute values of currently selected range
  pub fn select_range(&self) -> Option<Motion> {
    let mode = self.select_mode.as_ref()?;
    self.evaluate_selection(mode)
  }

  pub fn select_range_byte_pos(&mut self) -> Option<Range<usize>> {
    match self.select_range()? {
      Motion::CharRange(s, e) => {
        let s = self.pos_to_byte(s)?;
        let e = self.pos_to_byte(e)?;
        let (s, e) = ordered(s, e);
        Some(s..e)
      }
      Motion::LineRange(s, e) => {
        let s = self.resolve_line_addr(&s).ok()??;
        let e = self.resolve_line_addr(&e).ok()??;
        let s = self.pos_to_byte(Pos { row: s, col: 0 })?;
        let e = self.pos_to_byte(Pos {
          row: e,
          col: self.lines[e].len(),
        })?;
        let (s, e) = ordered(s, e);
        Some(s..e)
      }
      Motion::BlockRange(..) => todo!(),
      _ => unreachable!(),
    }
  }

  pub fn evaluate_selection(&self, mode: &SelectMode) -> Option<Motion> {
    match mode {
      SelectMode::Char(pos) => {
        let (s, e) = ordered(self.cursor.pos, *pos);
        Some(Motion::CharRange(s, e))
      }
      SelectMode::Line(pos) => {
        let (s, e) = ordered(self.row() + 1, pos.row + 1);
        Some(Motion::LineRange(LineAddr::Number(s), LineAddr::Number(e)))
      }
      SelectMode::Block(pos) => {
        let (s, e) = ordered(self.cursor.pos, *pos);
        Some(Motion::BlockRange(s, e))
      }
    }
  }

  pub fn evaluate_select_shape(&self, shape: &SelectShape) -> Option<Motion> {
    let offset = shape.pos();
    let anchor = self.cursor.pos.add_signed(offset);
    assert!(anchor > self.cursor.pos);
    let mode = shape.into_select_mode(anchor);
    self.evaluate_selection(&mode)
  }

  pub fn select_mode(&self) -> Option<Motion> {
    self
      .select_mode
      .as_ref()
      .map(|m| Motion::Selection(m.shape(self.cursor.pos)))
  }

  pub fn is_selecting(&self) -> bool {
    self.select_mode.is_some()
  }
}
