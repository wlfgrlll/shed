use nix::sys::signal::Signal;

use std::{
  collections::HashMap,
  fmt::{self, Display},
  path::PathBuf,
  str::FromStr,
};

use crate::{ShResult, eval::execute::exec_nonint, sherr, util};

use super::{
  ShErr,
  eval::{Node, lex::Span},
  expand::as_var_val_display,
  keys::{KeyEvent, KeyMap, KeyMapFlags, KeyMapMatch},
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

#[derive(rust_embed::RustEmbed)]
#[folder = "include"]
#[include = "functions/*"]
struct AutoloadFuncs;

#[derive(rust_embed::RustEmbed)]
#[folder = "include"]
#[include = "completions/*"]
struct AutoloadComps;

/// Shared body for `AutoloadFuncs::get_all` / `AutoloadComps::get_all`.
///
/// Walks the embedded asset paths and the on-disk entries under `env_var`,
/// running each stub through `tag` so completion sources can flip the
/// trigger from `OnCommand` to `OnCompletion`. On-disk entries shadow
/// embedded ones with the same name; that's intentional so users can
/// override bundled scripts.
fn collect_autoload<I>(embedded: I, env_var: &str) -> HashMap<String, AutoloadSrc>
where
  I: Iterator<Item = std::borrow::Cow<'static, str>>,
{
  let mut out: HashMap<String, AutoloadSrc> = embedded
    .filter_map(|path_str| {
      let name = PathBuf::from(path_str.as_ref())
        .file_stem()
        .and_then(|s| s.to_str())
        .map(str::to_string)?;
      if name.is_empty() {
        return None;
      }
      Some((name, AutoloadSrc::Embedded(path_str.to_string())))
    })
    .collect();

  let path_var = std::env::var(env_var).unwrap_or_default();
  for entry in util::path_list_entries(&path_var) {
    let path = entry.path();
    if path.is_dir() {
      continue;
    }
    if let Some(name) = path.file_stem().and_then(|n| n.to_str()) {
      out.insert(name.to_string(), AutoloadSrc::Path(path));
    }
  }

  out
}

impl AutoloadFuncs {
  pub fn get_all() -> HashMap<String, AutoloadSrc> {
    collect_autoload(Self::iter(), "SHED_FUNC_PATH")
  }
}

impl AutoloadComps {
  pub fn get_all() -> HashMap<String, AutoloadSrc> {
    collect_autoload(Self::iter(), "SHED_COMPLETE_PATH")
  }
}

/// A shell function
#[derive(Clone, Debug)]
pub enum ShFunc {
  Defined { logic: Box<Node>, source: Span },
  Autoload(AutoloadSrc),
}

#[derive(Clone, Copy, Debug)]
pub enum AutoloadKind {
  Function,
  Completion,
}

#[derive(Clone, Debug)]
pub enum AutoloadSrc {
  Path(PathBuf),
  Embedded(String),
}

impl AutoloadSrc {
  pub fn source(&self, kind: AutoloadKind) -> ShResult<()> {
    match self {
      Self::Path(p) => super::util::source_file(p.clone()),
      Self::Embedded(s) => {
        let body = match kind {
          AutoloadKind::Function => AutoloadFuncs::get(s)
            .ok_or_else(|| sherr!(NotFound, "Failed to load embedded function: {s}"))?,
          AutoloadKind::Completion => AutoloadComps::get(s)
            .ok_or_else(|| sherr!(NotFound, "Failed to load embedded completion: {s}"))?,
        }
        .data;
        let text = String::from_utf8_lossy(&body).to_string();
        exec_nonint(text, Some(s.clone().into()))
      }
    }
  }
}

impl ShFunc {
  pub fn defined(logic: Node, source: Span) -> Self {
    Self::Defined {
      logic: Box::new(logic),
      source,
    }
  }
  #[allow(dead_code)]
  pub fn autoload_src(&self) -> Option<&AutoloadSrc> {
    match self {
      Self::Autoload(src) => Some(src),
      Self::Defined { .. } => None,
    }
  }
  #[allow(dead_code)]
  pub fn source(&self) -> Option<&Span> {
    match self {
      Self::Defined { source, .. } => Some(source),
      Self::Autoload(_) => None,
    }
  }
  #[allow(dead_code)]
  pub fn logic(&self) -> Option<&Node> {
    match self {
      Self::Defined { logic, .. } => Some(logic),
      Self::Autoload(_) => None,
    }
  }
  #[allow(dead_code)]
  pub fn logic_mut(&mut self) -> Option<&mut Node> {
    match self {
      Self::Defined { logic, .. } => Some(logic),
      Self::Autoload(_) => None,
    }
  }
  #[allow(dead_code)]
  pub fn is_defined(&self) -> bool {
    matches!(self, Self::Defined { .. })
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
  OnCommandNotFound,
}

impl AutoCmdKind {
  pub fn iter() -> impl Iterator<Item = Self> {
    [
      Self::PreCmd,
      Self::PostCmd,
      Self::PreChangeDir,
      Self::PostChangeDir,
      Self::OnJobFinish,
      Self::PrePrompt,
      Self::PostPrompt,
      Self::PreModeChange,
      Self::PostModeChange,
      Self::OnHistoryOpen,
      Self::OnHistoryClose,
      Self::OnHistorySelect,
      Self::OnCompletionStart,
      Self::OnCompletionCancel,
      Self::OnCompletionSelect,
      Self::OnScreensaverExec,
      Self::OnScreensaverReturn,
      Self::OnTimeReport,
      Self::OnExit,
      Self::OnCommandNotFound,
    ]
    .into_iter()
  }
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
  OnCommandNotFound   <=> "on-command-not-found";
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

impl Display for AutoCmd {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    let kind = self.kind.to_string();
    let command = as_var_val_display(&self.command);
    write!(f, "autocmd {kind} {command}")
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
  comp_autoloads: HashMap<String, AutoloadSrc>,
  aliases: HashMap<String, ShAlias>,
  dirty: bool, // flips on alias/function insertion. used for signaling function/alias caching.

  traps: HashMap<TrapTarget, String>,
  keymaps: Vec<KeyMap>,
  autocmds: HashMap<AutoCmdKind, Vec<AutoCmd>>,
}

impl LogTab {
  pub fn new() -> Self {
    let mut new = Self::default();
    for (name, src) in AutoloadFuncs::get_all() {
      new.functions.insert(name, ShFunc::Autoload(src));
    }
    new.comp_autoloads = AutoloadComps::get_all();
    new
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
    self.autocmds.get(&kind).cloned().unwrap_or_default()
  }
  /// Iterate every registered autocmd in `(kind, command)` order. Skips
  /// the `notify_autocmd` side effect that `get_autocmds` performs, since
  /// dumping for `genrc` shouldn't mark autocmds as fired.
  pub fn iter_autocmds(&self) -> impl Iterator<Item = &AutoCmd> {
    let mut kinds: Vec<&AutoCmdKind> = self.autocmds.keys().collect();
    kinds.sort_by_key(ToString::to_string);
    kinds
      .into_iter()
      .flat_map(move |k| self.autocmds.get(k).map(|v| v.iter()).into_iter().flatten())
  }
  pub fn iter_keymaps(&self) -> &[KeyMap] {
    &self.keymaps
  }
  pub fn clear_autocmds(&mut self, kind: AutoCmdKind) {
    self.autocmds.remove(&kind);
  }
  pub fn insert_keymap(&mut self, keymap: KeyMap) {
    for map in &mut self.keymaps {
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
  pub fn has_command_func(&self, name: &str) -> bool {
    self.functions.contains_key(name)
  }
  pub fn get_func(&self, name: &str) -> Option<ShFunc> {
    self.functions.get(name).cloned()
  }
  pub fn funcs(&self) -> &HashMap<String, ShFunc> {
    &self.functions
  }
  pub fn remove_func(&mut self, name: &str) {
    self.functions.remove(name);
    self.dirty = true;
  }
  pub fn take_comp_autoload(&mut self, name: &str) -> Option<AutoloadSrc> {
    self.comp_autoloads.remove(name)
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
