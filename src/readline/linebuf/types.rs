use std::{
  fmt::{self, Display},
  ops::{Deref, DerefMut, Index, IndexMut},
  slice::SliceIndex,
};

use smallvec::SmallVec;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthChar;

use super::{CharClass, Pos};
/// A single grapheme. Graphemes can be composed of multiple chars, but are always treated as a single unit for display and editing purposes.
/// Using a SmallVec<[char; 4]> allows us to organize most multi-byte codepoints while maintaining both ownership and stack allocation.
/// If we ever run into a Grapheme made of more than 4 chars, just that Grapheme will gracefully spill over onto the heap
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Grapheme(pub(super) SmallVec<[char; 4]>);

impl Grapheme {
  /// Returns the display width of the Grapheme. ASCII control bytes (other
  /// than `\n` and `\t`) and DEL render as 2-column caret notation (`^[`,
  /// `^M`, `^?`, etc.) in the highlighter, so their width must match.
  /// Other unprintable codepoints fall back to 0.
  pub fn width(&self) -> usize {
    self
      .0
      .iter()
      .map(|c| {
        if Self::is_visualized_control(*c) {
          2
        } else {
          c.width().unwrap_or(0)
        }
      })
      .sum()
  }

  fn is_visualized_control(c: char) -> bool {
    matches!(c, '\x00'..='\x08' | '\x0b'..='\x1f' | '\x7f')
  }
  pub fn len_utf8(&self) -> usize {
    self.0.iter().map(|c| c.len_utf8()).sum()
  }
  /// Returns true if the Grapheme is wrapping a linefeed ('\n')
  pub fn is_lf(&self) -> bool {
    self.is_char('\n')
  }
  /// Returns true if the Grapheme consists of exactly one char and that char is equal to `c`
  pub fn is_char(&self, c: char) -> bool {
    self.0.len() == 1 && self.0[0] == c
  }
  /// Returns the CharClass of the Grapheme, which is determined by the properties of its chars
  /// Used for things like word motions
  pub fn class(&self) -> CharClass {
    CharClass::from(self)
  }

  /// If the Grapheme consists of exactly one char, returns that char. Otherwise, returns None.
  /// All callsites that use this method operate on ascii, so never returning anything for multibyte sequences is fine.
  pub fn as_char(&self) -> Option<char> {
    if self.0.len() == 1 {
      Some(self.0[0])
    } else {
      None
    }
  }

  /// Returns true if the Grapheme is classified as whitespace
  pub fn is_ws(&self) -> bool {
    self.class() == CharClass::Whitespace
  }
}

impl From<char> for Grapheme {
  fn from(value: char) -> Self {
    let mut new = SmallVec::<[char; 4]>::new();
    new.push(value);
    Self(new)
  }
}

impl From<&str> for Grapheme {
  fn from(value: &str) -> Self {
    assert_eq!(value.graphemes(true).count(), 1);
    let mut new = SmallVec::<[char; 4]>::new();
    for char in value.chars() {
      new.push(char);
    }
    Self(new)
  }
}

impl From<String> for Grapheme {
  fn from(value: String) -> Self {
    Into::<Self>::into(value.as_str())
  }
}

impl From<&String> for Grapheme {
  fn from(value: &String) -> Self {
    Into::<Self>::into(value.as_str())
  }
}

impl Display for Grapheme {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    for ch in &self.0 {
      write!(f, "{ch}")?;
    }
    Ok(())
  }
}

pub fn to_graphemes(s: impl ToString) -> Vec<Grapheme> {
  let s = s.to_string();
  s.graphemes(true).map(Grapheme::from).collect()
}

#[derive(Default, Debug, Clone, PartialEq, Eq, PartialOrd)]
pub struct Line(pub(super) Vec<Grapheme>);

impl Line {
  pub fn graphemes(&self) -> &[Grapheme] {
    &self.0
  }
  pub fn len(&self) -> usize {
    self.0.len()
  }
  pub fn is_empty(&self) -> bool {
    self.len() == 0
  }
  pub fn push_str(&mut self, s: &str) {
    for g in s.graphemes(true) {
      self.0.push(Grapheme::from(g));
    }
  }
  pub fn split_off(&mut self, at: usize) -> Line {
    if at > self.0.len() {
      return Line::default();
    }
    Line(self.0.split_off(at))
  }
  pub fn append(&mut self, other: &mut Line) {
    self.0.append(&mut other.0);
  }
  pub fn insert_str(&mut self, at: usize, other: &str) {
    if other.contains('\n') {
      log::warn!(
        "Inserting string with newlines into a single line. Newlines will be treated as literal characters."
      );
    }
    let start = at.min(self.0.len());
    for (at, g) in (start..).zip(other.graphemes(true)) {
      self.0.insert(at, Grapheme::from(g));
    }
  }
  pub fn insert_char(&mut self, at: usize, c: char) {
    let at = at.min(self.0.len());
    self.0.insert(at, Grapheme::from(c));
  }
  pub fn insert(&mut self, at: usize, g: Grapheme) {
    let at = at.min(self.0.len());
    self.0.insert(at, g);
  }
  pub fn trim_start(&mut self) -> Line {
    let mut clone = self.clone();
    while clone.0.first().is_some_and(|g| g.is_ws()) {
      clone.0.remove(0);
    }
    clone
  }
}

impl IndexMut<usize> for Line {
  fn index_mut(&mut self, index: usize) -> &mut Self::Output {
    &mut self.0[index]
  }
}

impl<T: SliceIndex<[Grapheme]>> Index<T> for Line {
  type Output = T::Output;
  fn index(&self, index: T) -> &Self::Output {
    &self.0[index]
  }
}

impl From<Vec<Grapheme>> for Line {
  fn from(value: Vec<Grapheme>) -> Self {
    Self(value)
  }
}

impl Display for Line {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    for gr in &self.0 {
      write!(f, "{gr}")?;
    }
    Ok(())
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lines(pub(super) Vec<Line>);
impl Lines {
  pub fn to_lines(s: impl ToString) -> Lines {
    let s = s.to_string();
    let mut new: Lines = s.split("\n").map(to_graphemes).map(Line::from).collect();
    new.push_empty();
    new
  }

  /// Ensure that the underlying Vec<Line> is not empty
  fn push_empty(&mut self) {
    if self.is_empty() {
      self.push(Line::default());
    }
  }

  pub fn join(&self) -> String {
    self
      .0
      .iter()
      .map(|line| line.to_string())
      .collect::<Vec<String>>()
      .join("\n")
  }

  pub fn split_lines_at(&mut self, pos: Pos) -> Lines {
    let tail = self[pos.row].split_off(pos.col);
    let mut rest: Lines = self.drain(pos.row + 1..).collect();
    rest.insert(0, tail);
    self.push_empty();
    rest
  }

  pub fn split_lines(mut self, pos: Pos) -> (Lines, Lines) {
    let tail = self[pos.row].split_off(pos.col);
    let mut rest: Lines = self.drain(pos.row + 1..).collect();
    self.push_empty();
    rest.insert(0, tail);
    (self, rest)
  }

  pub fn attach_lines(&mut self, other: &mut Lines) {
    if other.is_empty() {
      return;
    }
    if self.is_empty() {
      self.append(other);
      self.push_empty();
      return;
    }
    let mut head = other.remove(0);
    let mut tail = self.pop().unwrap();
    tail.append(&mut head);
    self.push(tail);
    self.append(other);
    self.push_empty();
  }

  pub fn is_prefix_lines(&self, other: &Lines) -> bool {
    if self.is_empty() {
      return false;
    }

    let all_but_last = self.0[..self.len().saturating_sub(1)]
      .iter()
      .zip(other.iter())
      .all(|(l, r)| *l == *r);

    if !all_but_last {
      return false;
    }

    let last = self.len().saturating_sub(1);
    let Some(other_line) = other.get(last).map(|l| &l.0) else {
      return false;
    };
    let this_line = &self[last].0;

    this_line.len() <= other_line.len()
      && this_line.iter().zip(other_line.iter()).all(|(l, r)| l == r)
  }

  pub fn strip_prefix_lines(mut self, other: &Lines) -> Option<Lines> {
    if self.is_empty() {
      return None;
    }

    let common_lines = self
      .0
      .iter()
      .zip(other.iter())
      .take_while(|(l, r)| *l == *r)
      .count();

    // drain equal lines
    self.drain(..common_lines);

    if self.is_empty() {
      self.push_empty();
      return None;
    }

    if let Some(other_line) = other.get(common_lines) {
      let common_chars = self[0]
        .0
        .iter()
        .zip(other_line.0.iter())
        .take_while(|(l, r)| l == r)
        .count();

      // drain common characters
      self[0].0.drain(..common_chars);
    } else if common_lines > 0 {
      // every line matched on the hint's prefix, and there is no partial match at the boundary.
      // the remaining content is just complete lines, so we have to add an empty line for those
      // to attach to, otherwise it attaches to our last line.
      self.0.insert(0, Line::default());
    }

    if self.iter().all(|l| l.is_empty()) {
      None
    } else {
      Some(self)
    }
  }
}

impl Default for Lines {
  fn default() -> Self {
    Self(vec![Line::default()])
  }
}

impl std::iter::FromIterator<Line> for Lines {
  fn from_iter<T: IntoIterator<Item = Line>>(iter: T) -> Self {
    Self(iter.into_iter().collect())
  }
}

impl From<Vec<Line>> for Lines {
  fn from(value: Vec<Line>) -> Self {
    Self(value)
  }
}

impl Deref for Lines {
  type Target = Vec<Line>;
  fn deref(&self) -> &Self::Target {
    &self.0
  }
}

impl DerefMut for Lines {
  fn deref_mut(&mut self) -> &mut Vec<Line> {
    &mut self.0
  }
}

impl Index<usize> for Lines {
  type Output = Line;
  fn index(&self, index: usize) -> &Self::Output {
    let index = index.min(self.0.len().saturating_sub(1));
    &self.0[index]
  }
}

impl IndexMut<usize> for Lines {
  fn index_mut(&mut self, index: usize) -> &mut Line {
    let index = index.min(self.0.len().saturating_sub(1));
    &mut self.0[index]
  }
}
