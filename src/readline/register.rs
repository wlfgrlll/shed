use std::{cell::RefCell, collections::HashMap, fmt::Display};

use itertools::Itertools;

use super::{expand::expand_keymap, keys::KeyEvent, linebuf::Line};

thread_local! {
  pub static REGISTERS: RefCell<Registers> = RefCell::new(Registers::new());

  #[cfg(test)]
  pub static SAVED_REGISTERS: RefCell<Option<Registers>> = const { RefCell::new(None) };
}

#[cfg(test)]
pub fn save_registers() {
  SAVED_REGISTERS.with(|saved| {
    let mut saved = saved.borrow_mut();
    *saved = Some(REGISTERS.with(|regs| regs.borrow().clone()));
  });
}

#[cfg(test)]
pub fn restore_registers() {
  SAVED_REGISTERS.with(|saved| {
    let mut saved = saved.borrow_mut();
    if let Some(regs) = saved.take() {
      REGISTERS.with(|r| *r.borrow_mut() = regs);
    }
  });
}

pub(super) fn read_register(ch: Option<char>) -> Option<RegisterContent> {
  REGISTERS.with(|regs| regs.borrow().get_reg(ch).map(|r| r.content().clone()))
}

pub(super) fn write_register(ch: Option<char>, buf: RegisterContent) {
  REGISTERS.with(|regs| {
    if let Some(r) = regs.borrow_mut().get_reg_mut(ch) {
      r.write(buf);
    }
  });
}

pub(super) fn append_register(ch: Option<char>, buf: RegisterContent) {
  REGISTERS.with(|regs| {
    if let Some(r) = regs.borrow_mut().get_reg_mut(ch) {
      r.append(buf);
    }
  });
}

#[derive(Clone, Copy, Debug)]
pub(super) struct RegisterName {
  name: Option<char>,
  append: bool,
}

impl RegisterName {
  pub fn new(name: Option<char>) -> Self {
    let Some(ch) = name else {
      return Self::default();
    };

    let append = ch.is_uppercase();
    let name = ch.to_ascii_lowercase();
    Self {
      name: Some(name),
      append,
    }
  }
  pub fn name(&self) -> Option<char> {
    self.name
  }
  pub fn display(&self) -> Option<char> {
    let name = self.name?;
    if self.append {
      Some(name.to_ascii_uppercase())
    } else {
      Some(name)
    }
  }
  pub fn is_none(&self) -> bool {
    self.name.is_none()
  }
  pub fn write_to_register(&self, buf: RegisterContent) {
    if self.append {
      append_register(self.name, buf);
    } else {
      write_register(self.name, buf);
    }
  }
  pub fn read_from_register(&self) -> Option<RegisterContent> {
    read_register(self.name)
  }
}

impl Default for RegisterName {
  fn default() -> Self {
    Self {
      name: None,
      append: false,
    }
  }
}

impl From<char> for RegisterName {
  fn from(value: char) -> Self {
    Self::new(Some(value))
  }
}

#[derive(Default, Clone, Debug)]
pub(super) enum RegisterContent {
  Span(Vec<Line>),
  Line(Vec<Line>),
  Block(Vec<Line>),
  Macro(Vec<KeyEvent>),
  #[default]
  Empty,
}

impl Display for RegisterContent {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::Block(s) | Self::Line(s) | Self::Span(s) => {
        let joined = s
          .iter()
          .map(|l| l.to_string())
          .collect::<Vec<_>>()
          .join("\n");

        write!(f, "{joined}")
      }
      Self::Macro(keys) => {
        let expanded = keys
          .iter()
          .map(|k| k.as_vim_seq().unwrap_or_default())
          .join("");
        write!(f, "{expanded}")
      }
      Self::Empty => write!(f, ""),
    }
  }
}

#[derive(Default, Clone, Debug)]
pub struct Registers(HashMap<char, Register>);

impl Registers {
  pub fn new() -> Self {
    let mut regs = HashMap::new();
    for c in 'a'..='z' {
      regs.insert(c, Register::default());
    }
    regs.insert('"', Register::default()); // 'default' register
    Self(regs)
  }
  pub fn get_reg(&self, name: Option<char>) -> Option<&Register> {
    let key = match name {
      None | Some('"') => '"',
      Some(c) if c.is_ascii_alphabetic() => c.to_ascii_lowercase(),
      _ => return None,
    };

    self.0.get(&key.to_ascii_lowercase())
  }
  pub fn get_reg_mut(&mut self, name: Option<char>) -> Option<&mut Register> {
    let key = match name {
      None | Some('"') => '"',
      Some(c) if c.is_ascii_alphabetic() => c.to_ascii_lowercase(),
      _ => return None,
    };

    self.0.get_mut(&key.to_ascii_lowercase())
  }
}

#[derive(Clone, Default, Debug)]
pub struct Register {
  content: RegisterContent,
}

impl Register {
  pub fn content(&self) -> &RegisterContent {
    &self.content
  }
  pub fn write(&mut self, buf: RegisterContent) {
    self.content = buf
  }
  pub fn append(&mut self, buf: RegisterContent) {
    if matches!(buf, RegisterContent::Empty) {
      return;
    }
    if matches!(self.content, RegisterContent::Empty) {
      self.content = buf;
      return;
    }

    use RegisterContent as C;
    match (&mut self.content, buf) {
      // same-shape text-into-text: extend in place
      (
        C::Span(a) | C::Line(a) | C::Block(a),
        C::Span(mut b) | C::Line(mut b) | C::Block(mut b),
      ) => {
        a.append(&mut b);
      }
      // macro-into-macro: extend in place
      (C::Macro(a), C::Macro(mut b)) => {
        a.append(&mut b);
      }

      (
        // text-into-macro: parse the text as a key sequence
        C::Macro(a),
        C::Span(b) | C::Line(b) | C::Block(b),
      ) => {
        let text = b
          .iter()
          .map(|l| l.to_string())
          .collect::<Vec<_>>()
          .join("\n");
        a.extend(expand_keymap(&text));
      }

      (
        // macro-into-text: render keys as a vim-style string, push as one Line
        C::Span(a) | C::Line(a) | C::Block(a),
        C::Macro(b),
      ) => {
        let rendered: String = b.iter().filter_map(|k| k.as_vim_seq().ok()).collect();
        let mut line = crate::readline::linebuf::Line::default();
        line.push_str(&rendered);
        a.push(line);
      }
      // both Empty cases handled above
      (C::Empty, _) | (_, C::Empty) => unreachable!(),
    }
  }
}
