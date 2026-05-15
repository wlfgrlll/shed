use nix::sys::signal::Signal;

use std::{
  collections::HashMap,
  fmt::{self, Display},
  str::FromStr,
};

use super::{
  ShErr, Shed,
  keys::{KeyEvent, KeyMap, KeyMapFlags, KeyMapMatch},
  parse::{Node, lex::Span},
  signal::parse_signal,
};

#[derive(Clone, Debug)]
pub(crate) struct ShAlias {
  body: String,
  source: Span,
}

impl ShAlias {
  pub fn new(body: String, source: Span) -> Self {
    Self { body, source }
  }
  pub fn body(&self) -> &str {
    &self.body
  }
  pub fn source(&self) -> &Span {
    &self.source
  }
}

impl Display for ShAlias {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(f, "{}", self.body)
  }
}

/// A shell function
///
/// Wraps the BraceGrp Node that forms the body of the function, and provides some helper methods to extract it from the parse tree
#[derive(Clone, Debug)]
pub struct ShFunc {
  pub body: Node,
  pub source: Span,
}

impl ShFunc {
  pub fn new(body: Node, source: Span) -> Self {
    Self { body, source }
  }
  pub fn body(&self) -> &Node {
    &self.body
  }
  pub fn body_mut(&mut self) -> &mut Node {
    &mut self.body
  }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub(crate) enum AutoCmdKind {
  PreCmd,
  PostCmd,
  PreChangeDir,
  PostChangeDir,
  OnJobFinish,
  PrePrompt,
  PostPrompt,
  PreModeChange,
  PostModeChange,
  OnHistoryOpen,
  OnHistoryClose,
  OnHistorySelect,
  OnCompletionStart,
  OnCompletionCancel,
  OnCompletionSelect,
  OnScreensaverExec,
  OnScreensaverReturn,
  OnTimeReport,
  OnExit,
}

crate::two_way_display!(AutoCmdKind,
  PreCmd              <=> "pre-cmd";
  PostCmd             <=> "post-cmd";
  PreChangeDir        <=> "pre-change-dir";
  PostChangeDir       <=> "post-change-dir";
  OnJobFinish         <=> "on-job-finish";
  PrePrompt           <=> "pre-prompt";
  PostPrompt          <=> "post-prompt";
  PreModeChange       <=> "pre-mode-change";
  PostModeChange      <=> "post-mode-change";
  OnHistoryOpen       <=> "on-history-open";
  OnHistoryClose      <=> "on-history-close";
  OnHistorySelect     <=> "on-history-select";
  OnCompletionStart   <=> "on-completion-start";
  OnCompletionCancel  <=> "on-completion-cancel";
  OnCompletionSelect  <=> "on-completion-select";
  OnScreensaverExec   <=> "on-screensaver-exec";
  OnScreensaverReturn <=> "on-screensaver-return";
  OnTimeReport        <=> "on-time-report";
  OnExit              <=> "on-exit";
);

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AutoCmd {
  kind: AutoCmdKind,
  command: String,
}

impl AutoCmd {
  pub fn new(kind: AutoCmdKind, command: String) -> Self {
    Self { kind, command }
  }
  pub fn command(&self) -> &str {
    &self.command
  }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, PartialOrd, Ord, Hash)]
pub(crate) enum TrapTarget {
  Exit,
  Error,
  Return,
  Signal(Signal),
}

impl FromStr for TrapTarget {
  type Err = ShErr;
  fn from_str(s: &str) -> Result<Self, Self::Err> {
    match s {
      "EXIT" => Ok(TrapTarget::Exit),
      "RETURN" => Ok(TrapTarget::Return),
      "ERR" => Ok(TrapTarget::Error),
      _ => Ok(TrapTarget::Signal(parse_signal(s)?)),
    }
  }
}

impl Display for TrapTarget {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      TrapTarget::Exit => write!(f, "EXIT"),
      TrapTarget::Return => write!(f, "RETURN"),
      TrapTarget::Error => write!(f, "ERR"),
      TrapTarget::Signal(s) => {
        let name = s.to_string();
        write!(f, "{}", name.strip_prefix("SIG").unwrap_or(&name))
      }
    }
  }
}

/// The logic table for the shell
///
/// Contains aliases and functions
#[derive(Default, Clone, Debug)]
pub(crate) struct LogTab {
  functions: HashMap<String, ShFunc>,
  aliases: HashMap<String, ShAlias>,
  dirty: bool, // flips on alias/function insertion. used for signaling function/alias caching.

  traps: HashMap<TrapTarget, String>,
  keymaps: Vec<KeyMap>,
  autocmds: HashMap<AutoCmdKind, Vec<AutoCmd>>,
}

impl LogTab {
  pub fn new() -> Self {
    Self::default()
  }
  pub fn dirty(&self) -> bool {
    self.dirty
  }
  pub fn set_dirty(&mut self, dirty: bool) {
    self.dirty = dirty;
  }
  pub fn insert_autocmd(&mut self, cmd: AutoCmd) {
    let entry = self.autocmds.entry(cmd.kind).or_default();
    if entry.contains(&cmd) {
      return;
    }
    entry.push(cmd);
  }
  pub fn get_autocmds(&self, kind: AutoCmdKind) -> Vec<AutoCmd> {
    Shed::meta_mut(|m| m.notify_autocmd(kind)).ok();
    self.autocmds.get(&kind).cloned().unwrap_or_default()
  }
  pub fn clear_autocmds(&mut self, kind: AutoCmdKind) {
    self.autocmds.remove(&kind);
  }
  pub fn insert_keymap(&mut self, keymap: KeyMap) {
    for map in self.keymaps.iter_mut() {
      if map.keys == keymap.keys {
        // overwrite old keymap with new one
        *map = keymap.clone();
        return;
      }
    }
    self.keymaps.push(keymap);
  }
  pub fn remove_keymap(&mut self, keys: &str) {
    self.keymaps.retain(|km| km.keys != keys);
  }
  pub fn keymaps_filtered(&self, flags: KeyMapFlags, pending: &[KeyEvent]) -> Vec<KeyMap> {
    self
      .keymaps
      .iter()
      .filter(|km| km.flags.intersects(flags) && km.compare(pending) != KeyMapMatch::NoMatch)
      .cloned()
      .collect()
  }
  pub fn insert_func(&mut self, name: &str, src: ShFunc) {
    self.functions.insert(name.into(), src);
    self.dirty = true;
  }
  pub fn insert_trap(&mut self, target: TrapTarget, command: String) {
    self.traps.insert(target, command);
  }
  pub fn get_trap(&self, target: TrapTarget) -> Option<String> {
    self.traps.get(&target).cloned()
  }
  pub fn remove_trap(&mut self, target: TrapTarget) {
    self.traps.remove(&target);
  }
  pub fn traps(&self) -> &HashMap<TrapTarget, String> {
    &self.traps
  }
  pub fn get_func(&self, name: &str) -> Option<ShFunc> {
    self.functions.get(name).cloned()
  }
  pub fn funcs(&self) -> &HashMap<String, ShFunc> {
    &self.functions
  }
  pub fn aliases(&self) -> &HashMap<String, ShAlias> {
    &self.aliases
  }
  pub fn insert_alias(&mut self, name: &str, body: &str, source: Span) {
    self
      .aliases
      .insert(name.into(), ShAlias::new(body.into(), source));
    self.dirty = true;
  }
  pub fn get_alias(&self, name: &str) -> Option<ShAlias> {
    self.aliases.get(name).cloned()
  }
  pub fn remove_alias(&mut self, name: &str) {
    self.aliases.remove(name);
    self.dirty = true;
  }
}
