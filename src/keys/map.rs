use bitflags::bitflags;

use super::{KeyEvent, expand::expand_keymap};

bitflags! {
  #[derive(Debug, Clone, Copy, PartialEq, Eq)]
  pub struct KeyMapFlags: u32 {
    const NORMAL 			= 1<<0;
    const INSERT 			= 1<<1;
    const VISUAL 			= 1<<2;
    const EX 					= 1<<3;
    const OP_PENDING 	= 1<<4;
    const REPLACE 		= 1<<5;
    const VERBATIM 		= 1<<6;
    const EMACS   		= 1<<7;
    const REMOTE   		= 1<<8;
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyMapMatch {
  NoMatch,
  IsPrefix,
  IsExact,
}

#[derive(Debug, Clone)]
pub struct KeyMap {
  pub flags: KeyMapFlags,
  pub keys: String,
  pub action: String,
}

impl KeyMap {
  pub fn keys_expanded(&self) -> Vec<KeyEvent> {
    expand_keymap(&self.keys)
  }
  pub fn action_expanded(&self) -> Vec<KeyEvent> {
    expand_keymap(&self.action)
  }
  pub fn compare(&self, other: &[KeyEvent]) -> KeyMapMatch {
    let ours = self.keys_expanded();
    if other == ours {
      KeyMapMatch::IsExact
    } else if ours.starts_with(other) {
      KeyMapMatch::IsPrefix
    } else {
      KeyMapMatch::NoMatch
    }
  }
}
