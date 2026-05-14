use std::sync::Arc;

use crate::util::ShResult;

// Credit to Rustyline for the design ideas in this module
// https://github.com/kkawakam/rustyline
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct KeyEvent(pub KeyCode, pub ModKeys);

impl KeyEvent {
  pub fn as_vim_seq(&self) -> ShResult<String> {
    let mut seq = String::new();
    let KeyEvent(event, mods) = self;
    let mut needs_angle_bracket = false;

    if mods.contains(ModKeys::CTRL) {
      seq.push_str("C-");
      needs_angle_bracket = true;
    }
    if mods.contains(ModKeys::ALT) {
      seq.push_str("A-");
      needs_angle_bracket = true;
    }
    if mods.contains(ModKeys::SHIFT) {
      seq.push_str("S-");
      needs_angle_bracket = true;
    }

    match event {
      KeyCode::ExMode => {
        seq.push_str("CMD");
        needs_angle_bracket = true;
      }
      KeyCode::Backspace => {
        seq.push_str("BS");
        needs_angle_bracket = true;
      }
      KeyCode::BracketedPasteStart => todo!(),
      KeyCode::BracketedPasteEnd => todo!(),
      KeyCode::Delete => {
        seq.push_str("Del");
        needs_angle_bracket = true;
      }
      KeyCode::Down => {
        seq.push_str("Down");
        needs_angle_bracket = true;
      }
      KeyCode::End => {
        seq.push_str("End");
        needs_angle_bracket = true;
      }
      KeyCode::Enter => {
        seq.push_str("Enter");
        needs_angle_bracket = true;
      }
      KeyCode::Esc => {
        seq.push_str("Esc");
        needs_angle_bracket = true;
      }

      KeyCode::F(f) => {
        seq.push_str(&format!("F{}", f));
        needs_angle_bracket = true;
      }
      KeyCode::Home => {
        seq.push_str("Home");
        needs_angle_bracket = true;
      }
      KeyCode::Insert => {
        seq.push_str("Insert");
        needs_angle_bracket = true;
      }
      KeyCode::Left => {
        seq.push_str("Left");
        needs_angle_bracket = true;
      }
      KeyCode::PageDown => {
        seq.push_str("PgDn");
        needs_angle_bracket = true;
      }
      KeyCode::PageUp => {
        seq.push_str("PgUp");
        needs_angle_bracket = true;
      }
      KeyCode::Right => {
        seq.push_str("Right");
        needs_angle_bracket = true;
      }
      KeyCode::Tab => {
        seq.push_str("Tab");
        needs_angle_bracket = true;
      }
      KeyCode::Up => {
        seq.push_str("Up");
        needs_angle_bracket = true;
      }
      KeyCode::Char(ch) => {
        seq.push(*ch);
      }
      KeyCode::Verbatim(s) => seq.push_str(s),
      clk @ (KeyCode::MiddleClick(x, y) | KeyCode::RightClick(x, y) | KeyCode::LeftClick(x, y)) => {
        let name = match clk {
          KeyCode::MiddleClick(_, _) => "MiddleClick",
          KeyCode::RightClick(_, _) => "RightClick",
          KeyCode::LeftClick(_, _) => "LeftClick",
          _ => unreachable!(),
        };
        seq.push_str(&format!("{name}({x},{y})"));
        needs_angle_bracket = true;
      }
      KeyCode::ScrollUp => {
        seq.push_str("ScrollUp");
        needs_angle_bracket = true;
      }
      KeyCode::ScrollDown => {
        seq.push_str("ScrollDown");
        needs_angle_bracket = true;
      }
      KeyCode::Back => {
        seq.push_str("Back");
        needs_angle_bracket = true;
      }
      KeyCode::Forward => {
        seq.push_str("Forward");
        needs_angle_bracket = true;
      }
      KeyCode::MousePos(x, y) => {
        seq.push_str(&format!("MousePos({x},{y})"));
        needs_angle_bracket = true;
      }
    }

    if needs_angle_bracket {
      Ok(format!("<{}>", seq))
    } else {
      Ok(seq)
    }
  }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum KeyCode {
  Backspace,
  BracketedPasteStart,
  BracketedPasteEnd,
  Char(char),
  Verbatim(Arc<str>), // For sequences that should be treated as literal input, not parsed into a KeyCode
  Delete,
  Down,
  End,
  Enter,
  Esc,
  F(u8),
  Home,
  Insert,
  Left,
  PageDown,
  PageUp,
  Right,
  Tab,
  Up,

  // mouse events
  ScrollUp,
  ScrollDown,
  MousePos(usize, usize),
  LeftClick(usize, usize),
  RightClick(usize, usize),
  MiddleClick(usize, usize),
  Back,
  Forward,

  // weird stuff
  ExMode, // keycode emitted by the <cmd> byte alias in vim keymaps
}

bitflags::bitflags! {
  #[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
  pub struct ModKeys: u8 {
    /// Control modifier
    const CTRL  = 1<<3;
    /// Escape or Alt modifier
    const ALT  = 1<<2;
    /// Shift modifier
    const SHIFT = 1<<1;

    /// No modifier
    const NONE = 0;
    /// Ctrl + Shift
    const CTRL_SHIFT = Self::CTRL.bits() | Self::SHIFT.bits();
    /// Alt + Shift
    const ALT_SHIFT = Self::ALT.bits() | Self::SHIFT.bits();
    /// Ctrl + Alt
    const CTRL_ALT = Self::CTRL.bits() | Self::ALT.bits();
    /// Ctrl + Alt + Shift
    const CTRL_ALT_SHIFT = Self::CTRL.bits() | Self::ALT.bits() | Self::SHIFT.bits();
  }
}

impl From<u16> for ModKeys {
  fn from(param: u16) -> Self {
    // CSI modifiers: param = 1 + (shift) + (alt*2) + (ctrl*4) + (meta*8)
    let bits = param.saturating_sub(1);
    let mut mods = ModKeys::empty();
    if bits & 1 != 0 {
      mods |= ModKeys::SHIFT;
    }
    if bits & 2 != 0 {
      mods |= ModKeys::ALT;
    }
    if bits & 4 != 0 {
      mods |= ModKeys::CTRL;
    }
    mods
  }
}

impl From<&u16> for ModKeys {
  fn from(value: &u16) -> Self {
    ModKeys::from(*value)
  }
}
