use std::fmt::{self, Display};
use std::str::FromStr;

use super::{
  KeyCode as K, KeyEvent as E, ModKeys as M, ShResult, Shed, SimpleEditor,
  editcmd::{self, CmdFlags, Direction, EditCmd, Motion, To, Verb, Word},
  history,
  history::History,
  key,
  linebuf::LineBuf,
  motion, register, state, status_msg,
  util::ShErr,
  verb,
};

mod emacs;
mod ex;
mod insert;
mod normal;
mod parse;
mod remote;
mod replace;
mod search;
mod verbatim;
mod visual;

pub(super) use emacs::Emacs;
pub(super) use ex::{AddressRange, ExNdRule, ExNode, SubFlags, ViEx};
pub(super) use insert::ViInsert;
pub(super) use normal::ViNormal;
pub(super) use parse::{ParseResult, ViParser};
pub(super) use remote::RemoteMode;
pub(super) use replace::ViReplace;
pub(super) use search::{ViSearch, ViSearchRev};
pub(super) use verbatim::ViVerbatim;
pub(super) use visual::ViVisual;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ModeReport {
  Insert,
  Normal,
  Ex,
  Visual,
  Replace,
  Verbatim,
  Emacs,
  Remote,
  Search,
  RevSearch,
}

impl ModeReport {
  pub(super) fn as_edit_mode(&self) -> Box<dyn EditMode> {
    match self {
      ModeReport::Insert => Box::new(ViInsert::new()) as Box<dyn EditMode>,
      ModeReport::Normal => Box::new(ViNormal::new()) as Box<dyn EditMode>,
      ModeReport::Ex => Box::new(ViEx::default()) as Box<dyn EditMode>,
      ModeReport::Visual => Box::new(ViVisual::new()) as Box<dyn EditMode>,
      ModeReport::Replace => Box::new(ViReplace::new()) as Box<dyn EditMode>,
      ModeReport::Verbatim => Box::new(ViVerbatim::new()) as Box<dyn EditMode>,
      ModeReport::Emacs => Box::new(Emacs::new()) as Box<dyn EditMode>,
      ModeReport::Remote => Box::new(RemoteMode) as Box<dyn EditMode>,
      ModeReport::Search => Box::new(ViSearch::new(1)) as Box<dyn EditMode>,
      ModeReport::RevSearch => Box::new(ViSearchRev::new(1)) as Box<dyn EditMode>,
    }
  }
}

impl Display for ModeReport {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::Insert => write!(f, "INSERT"),
      Self::Normal => write!(f, "NORMAL"),
      Self::Ex => write!(f, "COMMAND"),
      Self::Visual => write!(f, "VISUAL"),
      Self::Replace => write!(f, "REPLACE"),
      Self::Verbatim => write!(f, "VERBATIM"),
      Self::Emacs => write!(f, "EMACS"),
      Self::Remote => write!(f, "REMOTE"),
      Self::Search | Self::RevSearch => write!(f, "SEARCH"),
    }
  }
}
impl FromStr for ModeReport {
  type Err = ShErr;
  fn from_str(s: &str) -> Result<Self, Self::Err> {
    match s.to_uppercase().as_str() {
      "INSERT" => Ok(Self::Insert),
      "NORMAL" => Ok(Self::Normal),
      "COMMAND" => Ok(Self::Ex),
      "VISUAL" => Ok(Self::Visual),
      "REPLACE" => Ok(Self::Replace),
      "VERBATIM" => Ok(Self::Verbatim),
      "REMOTE" => Ok(Self::Remote),
      "EMACS" => Ok(Self::Emacs),
      "SEARCH" => Ok(Self::Search),
      "REVSEARCH" => Ok(Self::RevSearch),
      _ => Err(crate::sherr!(ParseErr, "Invalid ModeReport kind: {s}")),
    }
  }
}

#[derive(Debug, Clone)]
pub(super) enum CmdReplay {
  ModeReplay { cmds: Vec<EditCmd>, repeat: u16 },
  Single(Box<EditCmd>),
}

impl CmdReplay {
  pub fn mode(cmds: Vec<EditCmd>, repeat: u16) -> Self {
    Self::ModeReplay { cmds, repeat }
  }
}

pub(super) enum CmdState {
  Pending,
  Complete,
  Invalid,
}

pub(super) trait EditMode {
  fn handle_key_fallible(&mut self, key: E) -> ShResult<Option<EditCmd>> {
    Ok(self.handle_key(key))
  }
  fn handle_key(&mut self, key: E) -> Option<EditCmd>;
  fn is_repeatable(&self) -> bool;
  fn as_replay(&self) -> Option<CmdReplay>;
  fn cursor_style(&self) -> String;
  fn pending_seq(&self) -> Option<String>;
  fn pending_cursor(&self) -> Option<usize> {
    None
  }
  fn editor(&mut self) -> Option<&mut LineBuf> {
    None
  }
  fn history(&mut self) -> Option<&mut History> {
    None
  }
  fn is_input_mode(&self) -> bool {
    false
  }
  fn clamp_cursor(&self) -> bool;
  fn report_mode(&self) -> ModeReport;
}

pub fn common_cmds(key: E) -> Option<EditCmd> {
  let mut pending_cmd = EditCmd::new();
  match key {
    key!(Home) => pending_cmd.set_motion(motion!(Motion::StartOfLine)),
    key!(End) => pending_cmd.set_motion(motion!(Motion::EndOfLine)),
    key!(Tab) => pending_cmd.set_verb(verb!(Verb::InsertChar('\t'))),
    key!(Shift + Enter) => pending_cmd.set_verb(verb!(Verb::InsertChar('\n'))),
    key!(Enter) => pending_cmd.set_verb(verb!(Verb::AcceptLineOrNewline)),
    key!(Left) => pending_cmd.set_motion(motion!(Motion::BackwardChar)),
    key!(Ctrl + 'd') => pending_cmd.set_verb(verb!(Verb::EndOfFile)),
    key!(Ctrl + 'c') => pending_cmd.set_verb(verb!(Verb::Interrupt)),
    key!(Ctrl + 'p') => pending_cmd.set_verb(verb!(Verb::HistoryUp)),
    key!(Ctrl + 'n') => pending_cmd.set_verb(verb!(Verb::HistoryDown)),
    key!(Ctrl + 'l') => pending_cmd.set_verb(verb!(Verb::ClearScreen)),
    key!(Ctrl + 's') => pending_cmd.set_verb(verb!(Verb::AcceptHint)),
    key!(Right) => pending_cmd.set_motion(motion!(Motion::ForwardChar)),
    key!(Ctrl + Left) => pending_cmd.set_motion(motion!(Motion::WordMotion(
      To::Start,
      Word::Normal,
      Direction::Backward
    ))),
    key!(Ctrl + Right) => pending_cmd.set_motion(motion!(Motion::WordMotion(
      To::Start,
      Word::Normal,
      Direction::Forward
    ))),
    key!(Delete) => {
      pending_cmd.set_verb(verb!(Verb::Delete));
      pending_cmd.set_motion(motion!(Motion::ForwardCharForced));
    }
    key!(Backspace) | key!(Ctrl + 'h') => {
      pending_cmd.set_verb(verb!(Verb::Delete));
      pending_cmd.set_motion(motion!(Motion::BackwardCharForced));
    }
    E(K::Up, mods) => {
      pending_cmd.set_motion(motion!(Motion::LineUp));
      if mods.contains(M::SHIFT) {
        pending_cmd.flags |= CmdFlags::HAS_SHIFT;
      } else if mods.contains(M::CTRL) {
        pending_cmd.flags |= CmdFlags::HAS_CTRL;
      }
    }
    E(K::Down, mods) => {
      pending_cmd.set_motion(motion!(Motion::LineDown));
      if mods.contains(M::SHIFT) {
        pending_cmd.flags |= CmdFlags::HAS_SHIFT;
      } else if mods.contains(M::CTRL) {
        pending_cmd.flags |= CmdFlags::HAS_CTRL;
      }
    }
    _ => return None,
  }
  Some(pending_cmd)
}
