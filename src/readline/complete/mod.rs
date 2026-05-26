use std::{
  collections::HashSet,
  fmt::{Debug, Display},
  os::unix::fs::PermissionsExt,
  path::{Path, PathBuf},
  rc::Rc,
};

use bitflags::bitflags;
use nix::sys::signal::Signal;

mod fuzzy;
mod grid;
#[cfg(test)]
mod tests;

pub(crate) use fuzzy::{FuzzyCompleter, FuzzySelector, ScoredCandidate, SelectorResponse};

pub(crate) use grid::GridCompleter;

use super::{
  builtin::BUILTIN_NAMES,
  context::{CtxTk, CtxTkRule, get_context_tokens},
  editmode,
  eval::{execute::exec_nonint, lex::Span},
  expand::{
    as_var_val_display, escape_glob, escape_str, expand_raw_inner, markers::strip_markers,
    unescape_str,
  },
  key,
  keys::{self, KeyEvent as K},
  linebuf, shopt,
  state::{
    self, Shed,
    meta::Utility,
    vars::{VarFlags, VarKind},
  },
  try_var,
  util::{self, ShResult, ends_with_unescaped, has_unescaped, rfind_unescaped, var_ctx_guard},
  var, write_term,
};

bitflags! {
  #[derive(Default, Debug, Clone, Copy, PartialEq, Eq, Hash)]
  pub struct CompFlags: u32 {
    const FILES    = 1 << 0;
    const DIRS     = 1 << 1;
    const CMDS     = 1 << 2;
    const USERS    = 1 << 3;
    const VARS     = 1 << 4;
    const JOBS     = 1 << 5;
    const ALIAS    = 1 << 6;
    const SIGNALS  = 1 << 7;
    const PRINT    = 1 << 8;
    const REMOVE   = 1 << 9;
    const BUILTINS = 1 << 10;
  }
  #[derive(Default, Debug, Clone, Copy, PartialEq, Eq, Hash)]
  pub struct CompOptFlags: u32 {
    const DEFAULT  = 0b0000000001;
    const DIRNAMES = 0b0000000010;
    const SPACE    = 0b0000000100;
  }
}

#[derive(Default, Debug, Clone)]
pub(crate) struct CompOpts {
  pub func: Option<String>,
  pub wordlist: Option<Vec<String>>,
  pub action: Option<String>,
  pub flags: CompFlags,
  pub opt_flags: CompOptFlags,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CompStrat {
  Var {
    prefix: String,
  },
  Tilde {
    prefix: String,
  },
  Command {
    prefix: String,
  },
  Argument {
    path: String,
  },
  Files {
    path: String,
  },
  /// Semantic dead end, nothing can meaningfully go here. Suggest a `;` so
  /// the user can move on (e.g. inside or right after a closed subshell).
  Separator,
  /// No completion at all (mid-comment, mid-operator, inside a heredoc body).
  Null,
}

impl CompStrat {
  pub fn resolve(tks: &[CtxTk], cursor_pos: usize) -> (Self, Span, usize) {
    // Cursor inside a token's span, complete what's currently being typed.
    let branch = tks.iter().find_map(|t| {
      let branch = t.get_branch(cursor_pos);

      (!branch.is_empty()).then_some(branch)
    });
    log::debug!(
      "Got branch {:?} for cursor position {}",
      branch
        .as_ref()
        .map(|b| b.iter().map(|t| t.class()).collect::<Vec<_>>()),
      cursor_pos
    );

    if let Some(mut branch) = branch {
      while let Some(node) = branch.pop() {
        let res = Self::from_leaf(node, cursor_pos);
        if res.0 == CompStrat::Null && !branch.is_empty() {
          continue;
        }
        return res;
      }
    }

    let Some(prev) = tks.iter().rfind(|t| t.range().end <= cursor_pos) else {
      log::debug!("Cursor in empty input or leading whitespace");
      return (
        Self::Command {
          prefix: String::new(),
        },
        Span::new(cursor_pos..cursor_pos, "".into()),
        0,
      );
    };
    log::debug!(
      "Cursor after {:?} with class {:?}",
      prev.span().as_str(),
      prev.class(),
    );
    (
      Self::from_predecessor(prev),
      Span::new(cursor_pos..cursor_pos, prev.span().get_source()),
      0,
    )
  }

  /// Cursor is *inside* `leaf`. Returns the dispatch strategy *and* the span
  /// to replace when the candidate is selected.
  ///
  /// For Argument leaves: if the cursor is on a structurally-meaningful
  /// sub-token (VarSub, Tilde, CmdSub) we dispatch on that and the
  /// replacement targets just the sub-token's range, so `foo/$FL/bar`
  /// completing `$FL` to `$FLAKEPATH` produces `foo/$FLAKEPATH/bar`.
  /// Otherwise we treat the leaf as path-shaped and target the whole leaf;
  /// `complete_path` decides per-candidate whether to graft (preserving
  /// `$VAR`/`~` in the user's literal text) or wholesale-replace (for glob
  /// patterns where the literal text doesn't appear in the match).
  fn from_leaf(leaf: &CtxTk, cursor_pos: usize) -> (Self, Span, usize) {
    log::debug!(
      "Cursor inside {:?} with class {:?}",
      leaf.span().as_str(),
      leaf.class(),
    );

    let prefix = leaf.prefix_from(cursor_pos).unwrap_or_default().to_string();
    let whole = leaf.span().as_str();
    let cursor_pos = leaf.relative_cursor_pos(cursor_pos);
    let strat = match leaf.class() {
      CtxTkRule::ValidCommand | CtxTkRule::InvalidCommand | CtxTkRule::Keyword => {
        Self::Command { prefix }
      }

      CtxTkRule::AssignmentRight
      | CtxTkRule::CmdSub
      | CtxTkRule::BacktickSub
      | CtxTkRule::ProcSubIn
      | CtxTkRule::ProcSubOut
      | CtxTkRule::DoubleString
      | CtxTkRule::SingleString
      | CtxTkRule::Argument
      | CtxTkRule::ArgumentFile
      | CtxTkRule::DollarString => Self::Argument {
        path: whole.to_string(),
      },
      CtxTkRule::Glob | CtxTkRule::Redirect => Self::Files {
        path: whole.to_string(),
      },
      CtxTkRule::Tilde => Self::Tilde { prefix },

      CtxTkRule::ParamName | CtxTkRule::ArithVar => Self::Var { prefix },

      // Everything else inside a leaf means "no useful completion here".
      CtxTkRule::Comment
      | CtxTkRule::Subshell
      | CtxTkRule::Arithmetic
      | CtxTkRule::VarSub
      | CtxTkRule::HistExp
      | CtxTkRule::Escape
      | CtxTkRule::Separator
      | CtxTkRule::ArithOp
      | CtxTkRule::ArithNumber
      | CtxTkRule::ParamPrefix
      | CtxTkRule::ParamIndex
      | CtxTkRule::ParamOp
      | CtxTkRule::ParamArg
      | CtxTkRule::AssignmentLeft
      | CtxTkRule::AssignmentOp
      | CtxTkRule::Operator
      | CtxTkRule::HereDocStart
      | CtxTkRule::HereDocBody
      | CtxTkRule::ExAddress
      | CtxTkRule::ExBang
      | CtxTkRule::ExPattern
      | CtxTkRule::HereDocEnd => Self::Null,
    };
    // VarSub/ParamName get a narrowed span (`${name`) so trailing param
    // expansion bits (`:-default`, `}`, etc.) are preserved on replace.
    (
      strat,
      leaf.span().clone(),
      cursor_pos.unwrap_or(whole.len()),
    )
  }

  /// Cursor is *past* `prev` (in whitespace or at end of input). The prefix
  /// is empty; what comes next depends on what we just finished.
  fn from_predecessor(prev: &CtxTk) -> Self {
    let prefix = String::new();

    match prev.class() {
      // After a finished command/argument-position token, we're typing args.
      CtxTkRule::ValidCommand
      | CtxTkRule::InvalidCommand
      | CtxTkRule::Argument
      | CtxTkRule::ArgumentFile
      | CtxTkRule::CmdSub
      | CtxTkRule::BacktickSub
      | CtxTkRule::ProcSubIn
      | CtxTkRule::ProcSubOut
      | CtxTkRule::VarSub
      | CtxTkRule::Tilde
      | CtxTkRule::Glob
      | CtxTkRule::DoubleString
      | CtxTkRule::SingleString
      | CtxTkRule::DollarString
      | CtxTkRule::AssignmentRight => Self::Argument {
        path: String::new(),
      },

      // After a separator or operator, we're at the start of a new segment.
      CtxTkRule::Separator | CtxTkRule::Operator => Self::Command { prefix },

      // After a keyword (for/while/if/etc.) the next token is a command head.
      // TODO: split per-keyword once the cases matter (e.g. `for <var>`).
      CtxTkRule::Keyword => Self::Command { prefix },

      // After a redirect we expect a file path.
      CtxTkRule::Redirect => Self::Files {
        path: String::new(),
      },

      // After a closed structural construct, semantically nothing follows
      // until a separator, suggest one.
      CtxTkRule::Subshell | CtxTkRule::Arithmetic => Self::Separator,

      // Past a comment / heredoc / odd internal-only class, no completion.
      CtxTkRule::Comment
      | CtxTkRule::HereDocStart
      | CtxTkRule::HereDocBody
      | CtxTkRule::HereDocEnd
      | CtxTkRule::HistExp
      | CtxTkRule::Escape
      | CtxTkRule::ArithOp
      | CtxTkRule::ArithNumber
      | CtxTkRule::ArithVar
      | CtxTkRule::ParamPrefix
      | CtxTkRule::ParamName
      | CtxTkRule::ParamIndex
      | CtxTkRule::ParamOp
      | CtxTkRule::ParamArg
      | CtxTkRule::AssignmentLeft
      | CtxTkRule::ExAddress
      | CtxTkRule::ExBang
      | CtxTkRule::ExPattern
      | CtxTkRule::AssignmentOp => Self::Null,
    }
  }
}

#[derive(Default, Debug, Clone)]
pub(crate) struct Candidate {
  content: String,
  desc: Option<String>,
  id: Option<usize>, // for stuff like history that cares about the original index
}

impl Eq for Candidate {}

impl PartialEq for Candidate {
  fn eq(&self, other: &Self) -> bool {
    self.content == other.content
  }
}

impl PartialOrd for Candidate {
  fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
    Some(self.cmp(other))
  }
}

impl Ord for Candidate {
  fn cmp(&self, other: &Self) -> std::cmp::Ordering {
    self.content.cmp(&other.content)
  }
}

impl From<PathBuf> for Candidate {
  fn from(value: PathBuf) -> Self {
    let path_raw = value.to_string_lossy().to_string();
    let desc = file_desc(&value);
    Self {
      content: path_raw,
      desc: Some(desc),
      id: None,
    }
  }
}

impl From<String> for Candidate {
  fn from(value: String) -> Self {
    Self {
      content: value,
      desc: None,
      id: None,
    }
  }
}

impl From<Rc<Utility>> for Candidate {
  fn from(value: Rc<Utility>) -> Self {
    From::from(&*value)
  }
}

impl From<&state::meta::Utility> for Candidate {
  fn from(value: &state::meta::Utility) -> Self {
    Self {
      content: value.name().to_string(),
      desc: None,
      id: None,
    }
  }
}

impl From<state::meta::Utility> for Candidate {
  fn from(value: state::meta::Utility) -> Self {
    From::from(&value)
  }
}

impl From<&String> for Candidate {
  fn from(value: &String) -> Self {
    Self {
      content: value.clone(),
      desc: None,
      id: None,
    }
  }
}

impl From<&str> for Candidate {
  fn from(value: &str) -> Self {
    Self {
      content: value.to_string(),
      desc: None,
      id: None,
    }
  }
}

impl From<&&str> for Candidate {
  fn from(value: &&str) -> Self {
    Self::from(*value)
  }
}

impl From<(usize, String)> for Candidate {
  fn from(value: (usize, String)) -> Self {
    Self {
      content: value.1,
      desc: None,
      id: Some(value.0),
    }
  }
}

impl Display for Candidate {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    write!(f, "{}", &self.content)
  }
}

impl AsRef<str> for Candidate {
  fn as_ref(&self) -> &str {
    &self.content
  }
}

impl std::ops::Deref for Candidate {
  type Target = str;
  fn deref(&self) -> &str {
    &self.content
  }
}

impl Candidate {
  pub fn is_match(&self, other: &str) -> bool {
    let ignore_case = shopt!(prompt.completion_ignore_case);
    if ignore_case {
      let other_lower = other.to_lowercase();
      let self_lower = self.content.to_lowercase();
      self_lower.starts_with(&other_lower)
    } else {
      self.content.starts_with(other)
    }
  }
  pub fn content(&self) -> &str {
    &self.content
  }
  #[cfg(test)]
  pub fn desc(&self) -> Option<&str> {
    self.desc.as_deref()
  }
  pub fn id(&self) -> Option<usize> {
    self.id
  }
  pub fn as_str(&self) -> &str {
    &self.content
  }
  pub fn with_desc(mut self, desc: String) -> Self {
    self.desc = Some(desc);
    self
  }
  pub fn display(&self) -> String {
    let mut out = String::with_capacity(self.content.len());
    let mut chars = self.content.chars();
    while let Some(ch) = chars.next() {
      if ch == '\\' {
        if let Some(next) = chars.next() {
          out.push(next);
        }
      } else {
        out.push(ch);
      }
    }
    out
  }

  pub fn strip_prefix(&self, prefix: &str) -> Option<String> {
    let ignore_case = shopt!(prompt.completion_ignore_case);
    if ignore_case {
      let old_len = self.content.len();
      let prefix_lower = prefix.to_lowercase();
      let self_lower = self.content.to_lowercase();
      let stripped = self_lower.strip_prefix(&prefix_lower)?;
      let new_len = stripped.len();
      let delta = old_len - new_len;
      Some(self.content[delta..].to_string())
    } else {
      self.content.strip_prefix(prefix).map(|s| s.to_string())
    }
  }
}

pub(crate) fn complete_signals(start: &str) -> Vec<Candidate> {
  let map_closure = if start.starts_with("SIG") || start.is_empty() {
    |s: Signal| s.to_string()
  } else {
    |s: Signal| {
      s.to_string()
        .strip_prefix("SIG")
        .unwrap_or(s.as_ref())
        .to_string()
    }
  };
  Signal::iterator()
    .map(map_closure)
    .map(Candidate::from)
    .filter(|s| s.is_match(start))
    .collect()
}

pub(crate) fn complete_aliases(start: &str) -> Vec<Candidate> {
  Shed::logic(|l| {
    l.aliases()
      .iter()
      .map(|(a, v)| Candidate::from(a.to_string()).with_desc(v.to_string()))
      .filter(|a| a.is_match(start))
      .collect()
  })
}

pub(crate) fn complete_jobs(start: &str) -> Vec<Candidate> {
  if let Some(prefix) = start.strip_prefix('%') {
    Shed::jobs(|j| {
      j.jobs()
        .iter()
        .filter_map(|j| j.as_ref())
        .filter_map(|j| {
          let name = j.name()?;
          Some(Candidate::from(name.to_string()).with_desc(format!(
            "{} ({})",
            j.pgid(),
            j.get_cmd_line()
          )))
        })
        .filter(|name| name.is_match(prefix))
        .map(|name| format!("%{name}").into())
        .collect()
    })
  } else {
    Shed::jobs(|j| {
      j.jobs()
        .iter()
        .filter_map(|j| j.as_ref())
        .map(|j| Candidate::from(j.pgid().to_string()).with_desc(j.get_cmd_line()))
        .filter(|pgid| pgid.is_match(start))
        .collect()
    })
  }
}

pub(crate) fn complete_users(start: &str) -> Vec<Candidate> {
  let Ok(passwd) = std::fs::read_to_string("/etc/passwd") else {
    return vec![];
  };
  passwd
    .lines()
    .filter_map(|line| line.split(':').next())
    .map(Candidate::from)
    .filter(|username| username.is_match(start))
    .collect()
}

pub(crate) fn complete_vars(start: &str) -> Vec<Candidate> {
  if !var!(start).is_empty() {
    return vec![];
  }
  // if we are here, we have a variable substitution that isn't complete
  // so let's try to complete it
  Shed::vars(|v| {
    v.flatten_vars()
      .keys()
      .map(|s| {
        if let Some(val) = try_var!(s) {
          Candidate::from(s).with_desc(val.escape_debug().collect())
        } else {
          Candidate::from(s)
        }
      })
      .filter(|c| c.is_match(start) && c.content() != start)
      .collect::<Vec<_>>()
  })
}

pub(crate) fn complete_vars_raw(raw: &str) -> Vec<Candidate> {
  if !var!(raw).is_empty() {
    return vec![];
  }
  // if we are here, we have a variable substitution that isn't complete
  // so let's try to complete it
  Shed::vars(|v| {
    v.flatten_vars()
      .keys()
      .map(|k| {
        if let Some(val) = try_var!(k) {
          Candidate::from(k.to_string()).with_desc(val)
        } else {
          Candidate::from(k.to_string())
        }
      })
      .filter(|c| c.is_match(raw) && c.content() != raw)
      .collect::<Vec<_>>()
  })
}

fn complete_builtins(start: &str) -> Vec<Candidate> {
  BUILTIN_NAMES
    .iter()
    .map(Candidate::from)
    .filter(|b| b.is_match(start))
    .collect()
}

fn complete_commands(start: &str, cursor_pos: usize) -> Vec<Candidate> {
  if has_unescaped(start, "/") {
    return complete_path(start, cursor_pos)
      .into_iter()
      .filter(|c| {
        // lets just check the description
        // so we can avoid making another metadata syscall
        let desc = c.desc.as_deref().unwrap_or("");
        desc.starts_with("dir") || desc.starts_with("exec")
      })
      .collect();
  }

  let mut candidates: Vec<Candidate> = Shed::meta(|m| {
    m.cached_utils()
      .map(Candidate::from)
      .filter(|c| c.is_match(start))
      .collect()
  });

  log::debug!("After utilities, candidates are: {:?}", candidates);

  if shopt!(core.autocd) {
    let dirs = complete_dirs(start, cursor_pos);
    candidates.extend(dirs);
  }

  candidates.sort();
  candidates
}

fn complete_dirs(start: &str, cursor_pos: usize) -> Vec<Candidate> {
  let filenames = complete_path(start, cursor_pos);

  filenames
    .into_iter()
    .filter(|f| {
      std::fs::metadata(&f.content)
        .map(|m| m.is_dir())
        .unwrap_or(false)
    })
    .collect()
}

fn unescape_for_completion(raw: &str) -> String {
  let unescaped = unescape_str(raw);
  expand_raw_inner(&mut unescaped.chars().peekable(), true)
    .map(|s| strip_markers(&s))
    .unwrap_or_else(|_| raw.to_string())
}

fn complete_path(path: &str, cursor_pos: usize) -> Vec<Candidate> {
  let (prefix, postfix) = path.split_at_checked(cursor_pos).unwrap_or((path, ""));
  let prefix = if ends_with_unescaped(prefix, "\\") {
    &prefix[..prefix.len() - 1]
  } else {
    prefix
  };

  let unescaped_pre = unescape_for_completion(prefix);
  let unescaped_post = unescape_for_completion(postfix);
  let escaped_pre = escape_glob(&unescaped_pre, false);
  let escaped_post = escape_glob(&unescaped_post, false);

  let ignore_case = shopt!(prompt.completion_ignore_case);
  let pat = format!("{escaped_pre}*{escaped_post}");
  let match_opts = glob::MatchOptions {
    case_sensitive: !ignore_case,
    require_literal_separator: false,
    require_literal_leading_dot: false,
  };
  let candidates: Vec<Candidate> = glob::glob_with(&pat, match_opts)
    .map(|it| it.filter_map(Result::ok).map(|c| c.into()).collect())
    .unwrap_or_default();

  candidates
    .into_iter()
    .map(|mut c| {
      let is_dir = c.desc.as_ref().is_some_and(|d| d.contains("dir"));
      let raw = c.content.clone();

      let mut new_content = if let Some(after_prefix) = raw.strip_prefix(&unescaped_pre) {
        // strip the start and end, escape it for completion.
        // We do this so that something like "$SOME_PATH/foo"
        // is not replaced by "/some/path/foo" (preserves variable names and other stuff)
        let middle = after_prefix
          .strip_suffix(&unescaped_post)
          .unwrap_or(after_prefix);
        let middle_escaped = escape_str(middle, false);
        format!("{prefix}{middle_escaped}{postfix}")
      } else if ignore_case {
        // Glob matched case-insensitively; preserve actual filename casing from `raw`
        // but keep whatever directory prefix the user typed (e.g. ~/ or $VAR/).
        // rfind_unescaped handles escaped slashes in filenames; unescaped_pre needs
        // plain rfind since it is already unescaped.
        let typed_dir_end = rfind_unescaped(prefix, '/').map(|i| i + 1).unwrap_or(0);
        let raw_dir_end = raw.rfind('/').map(|i| i + 1).unwrap_or(0);

        let filename_raw = &raw[raw_dir_end..];
        let middle = filename_raw
          .strip_suffix(&unescaped_post)
          .unwrap_or(filename_raw);
        let middle_escaped = escape_str(middle, false);

        format!("{}{middle_escaped}{postfix}", &prefix[..typed_dir_end])
      } else {
        escape_str(&raw, false)
      };

      // glob strips this, we have to add it back
      if path.starts_with("./") && !new_content.starts_with("./") && !new_content.starts_with('/') {
        new_content = format!("./{new_content}")
      }

      if is_dir {
        new_content.push('/');
      }

      c.content = new_content;
      c
    })
    .collect()
}

fn file_desc<P: AsRef<Path>>(path: P) -> String {
  let path = path.as_ref();
  let Ok(meta) = path.metadata() else {
    return String::new();
  };
  let kind = if meta.is_dir() {
    "dir"
  } else if path
    .symlink_metadata()
    .is_ok_and(|m| m.file_type().is_symlink())
  {
    "link"
  } else if meta.permissions().mode() & 0o111 != 0 {
    "exec"
  } else if meta.is_file() {
    "file"
  } else {
    "?"
  };

  let size = if kind != "dir" {
    util::format_size(meta.len())
  } else {
    String::from("-")
  };
  let mode = util::format_mode(meta.permissions().mode());

  format!("{kind:<4} {size:>6} {mode}")
}

pub(crate) enum CompSpecResult {
  NoSpec, // No compspec registered
  NoMatch {
    flags: CompOptFlags,
  }, /* Compspec found but no candidates matched, returns
           * behavior flags */
  Match {
    result: CompResult,
    flags: CompOptFlags,
  }, // Compspec found and candidates returned
}

#[derive(Default, Debug, Clone)]
pub(crate) struct BashCompSpec {
  /// -F: The name of a function to generate the possible completions.
  pub function: Option<String>,
  /// -W: The list of words
  pub wordlist: Option<Vec<String>>,
  /// -f: complete file names
  pub files: bool,
  /// -d: complete directory names
  pub dirs: bool,
  /// -c: complete command names
  pub commands: bool,
  /// -b: complete builtin names
  pub builtins: bool,
  /// -u: complete user names
  pub users: bool,
  /// -v: complete variable names
  pub vars: bool,
  /// -A signal: complete signal names
  pub signals: bool,
  /// -j: complete job pids or names
  pub jobs: bool,
  /// -a: complete aliases
  pub aliases: bool,

  pub flags: CompOptFlags,
  /// The original command
  pub source: String,
}

#[allow(dead_code)]
impl BashCompSpec {
  pub fn new() -> Self {
    Self::default()
  }
  pub fn with_func(mut self, func: String) -> Self {
    self.function = Some(func);
    self
  }
  pub fn with_wordlist(mut self, wordlist: Vec<String>) -> Self {
    self.wordlist = Some(wordlist);
    self
  }
  pub fn with_source(mut self, source: String) -> Self {
    self.source = source;
    self
  }
  pub fn files(mut self, enable: bool) -> Self {
    self.files = enable;
    self
  }
  pub fn dirs(mut self, enable: bool) -> Self {
    self.dirs = enable;
    self
  }
  pub fn commands(mut self, enable: bool) -> Self {
    self.commands = enable;
    self
  }
  pub fn users(mut self, enable: bool) -> Self {
    self.users = enable;
    self
  }
  pub fn vars(mut self, enable: bool) -> Self {
    self.vars = enable;
    self
  }
  pub fn signals(mut self, enable: bool) -> Self {
    self.signals = enable;
    self
  }
  pub fn jobs(mut self, enable: bool) -> Self {
    self.jobs = enable;
    self
  }
  pub fn aliases(mut self, enable: bool) -> Self {
    self.aliases = enable;
    self
  }
  pub fn builtins(mut self, enable: bool) -> Self {
    self.builtins = enable;
    self
  }
  pub fn from_comp_opts(opts: CompOpts) -> Self {
    let CompOpts {
      func,
      wordlist,
      action: _,
      flags,
      opt_flags,
    } = opts;
    Self {
      function: func,
      wordlist,
      files: flags.contains(CompFlags::FILES),
      dirs: flags.contains(CompFlags::DIRS),
      commands: flags.contains(CompFlags::CMDS),
      users: flags.contains(CompFlags::USERS),
      vars: flags.contains(CompFlags::VARS),
      jobs: flags.contains(CompFlags::JOBS),
      aliases: flags.contains(CompFlags::ALIAS),
      builtins: flags.contains(CompFlags::BUILTINS),
      flags: opt_flags,
      signals: flags.contains(CompFlags::SIGNALS),
      source: String::new(),
    }
  }
  pub fn exec_comp_func(&self, ctx: &CompContext) -> ShResult<Vec<Candidate>> {
    let mut vars_to_unset = HashSet::new();
    for var in [
      "COMP_WORDS",
      "COMP_CWORD",
      "COMP_LINE",
      "COMP_POINT",
      "COMPREPLY",
    ] {
      vars_to_unset.insert(var.to_string());
    }
    let _guard = var_ctx_guard(vars_to_unset);

    let CompContext {
      words,
      cword,
      line,
      cursor_pos,
    } = ctx;

    let raw_words = words.iter().clone().map(|tk| tk.to_string()).collect();
    Shed::vars_mut(|v| {
      v.set_var(
        "COMP_WORDS",
        VarKind::arr_from_vec(raw_words),
        VarFlags::empty(),
      )
    })?;
    Shed::vars_mut(|v| {
      v.set_var(
        "COMP_CWORD",
        VarKind::Str(cword.to_string()),
        VarFlags::empty(),
      )
    })?;
    Shed::vars_mut(|v| {
      v.set_var(
        "COMP_LINE",
        VarKind::Str(line.to_string()),
        VarFlags::empty(),
      )
    })?;
    Shed::vars_mut(|v| {
      v.set_var(
        "COMP_POINT",
        VarKind::Str(cursor_pos.to_string()),
        VarFlags::empty(),
      )
    })?;

    let cmd_name = words.first().map(|s| s.to_string()).unwrap_or_default();

    let cword_str = words.get(*cword).map(|s| s.to_string()).unwrap_or_default();

    let pword_str = if *cword > 0 {
      words
        .get(cword - 1)
        .map(|s| s.to_string())
        .unwrap_or_default()
    } else {
      String::new()
    };

    let input = format!(
      "{} {} {} {}",
      self.function.as_ref().unwrap(),
      as_var_val_display(&cmd_name),
      as_var_val_display(&cword_str),
      as_var_val_display(&pword_str),
    );
    exec_nonint(input, Some("comp_function".into()))?;

    let comp_reply: Vec<Candidate> = Shed::vars(|v| v.get_arr_elems("COMPREPLY"))
      .into_iter()
      .map(Candidate::from)
      .collect();

    let comp_add: Vec<Candidate> = Shed::meta_mut(|m| m.take_comp_candidates())
      .into_iter()
      .filter(|c| {
        log::debug!(
          "Filtering comp_add candidate {:?} against cword_str {:?}",
          c.content,
          cword_str
        );
        c.is_match(&cword_str)
      })
      .collect();

    let candidates: Vec<Candidate> = comp_reply.into_iter().chain(comp_add).collect();

    Ok(candidates)
  }
}

impl CompSpec for BashCompSpec {
  fn complete(&self, ctx: &CompContext) -> ShResult<Vec<Candidate>> {
    let mut candidates: Vec<Candidate> = vec![];
    let prefix = &ctx.words[ctx.cword];

    let unescaped = unescape_str(prefix.as_str());
    let expanded = expand_raw_inner(&mut unescaped.chars().peekable(), false)?;
    let stripped = strip_markers(&expanded);
    if self.files {
      candidates.extend(complete_path(&stripped, ctx.cursor_pos));
    }
    if self.dirs {
      candidates.extend(complete_dirs(&stripped, ctx.cursor_pos));
    }
    if self.commands {
      candidates.extend(complete_commands(&stripped, ctx.cursor_pos));
    }
    if self.vars {
      candidates.extend(complete_vars_raw(&stripped));
    }
    if self.users {
      candidates.extend(complete_users(&stripped));
    }
    if self.jobs {
      candidates.extend(complete_jobs(&stripped));
    }
    if self.aliases {
      candidates.extend(complete_aliases(&stripped));
    }
    if self.signals {
      candidates.extend(complete_signals(&stripped));
    }
    if self.builtins {
      candidates.extend(complete_builtins(&stripped));
    }
    if let Some(words) = &self.wordlist {
      candidates.extend(
        words
          .iter()
          .map(Candidate::from)
          .filter(|w| w.is_match(&stripped)),
      );
    }
    if self.function.is_some() {
      candidates.extend(self.exec_comp_func(ctx)?);
    }
    candidates = candidates
      .into_iter()
      .map(|mut c| {
        let tail = c.strip_prefix(&stripped).unwrap_or_default();
        c.content = format!("{prefix}{tail}");
        c
      })
      .collect();

    candidates.sort_by_key(|c| c.content.len()); // sort by length to prioritize shorter completions, ties are then sorted alphabetically
    candidates.reverse();

    Ok(candidates)
  }

  fn source(&self) -> &str {
    &self.source
  }

  fn get_flags(&self) -> CompOptFlags {
    self.flags
  }
}

pub(crate) trait CompSpec: Debug + CloneCompSpec {
  fn complete(&self, ctx: &CompContext) -> ShResult<Vec<Candidate>>;
  fn source(&self) -> &str;
  fn get_flags(&self) -> CompOptFlags {
    CompOptFlags::empty()
  }
}

pub(crate) trait CloneCompSpec {
  fn clone_box(&self) -> Box<dyn CompSpec>;
}

impl<T: CompSpec + Clone + 'static> CloneCompSpec for T {
  fn clone_box(&self) -> Box<dyn CompSpec> {
    Box::new(self.clone())
  }
}

impl Clone for Box<dyn CompSpec> {
  fn clone(&self) -> Self {
    self.clone_box()
  }
}

#[derive(Debug, Clone)]
pub(crate) struct CompContext {
  pub words: Vec<String>,
  pub cword: usize,
  pub line: String,
  pub cursor_pos: usize,
}

impl CompContext {
  pub fn cmd(&self) -> Option<&str> {
    self.words.first().map(|s| s.as_str())
  }
}

pub(crate) enum CompResult {
  NoMatch,
  Single { result: Candidate },
  Many { candidates: Vec<Candidate> },
}

impl CompResult {
  pub fn from_candidates(mut candidates: Vec<Candidate>) -> Self {
    if candidates.is_empty() {
      Self::NoMatch
    } else if candidates.len() == 1 {
      Self::Single {
        result: candidates.remove(0),
      }
    } else {
      Self::Many { candidates }
    }
  }

  pub fn try_collapse_by_prefix(self, typed: &str) -> Self {
    let Self::Many { candidates } = self else {
      return self;
    };
    let min_end = typed.len();

    let Some(first) = candidates.first() else {
      return Self::Many { candidates };
    };
    let f_content = first.content();
    let mut end = first.len();

    for cand in &candidates[1..] {
      let c = cand.content();
      let common_bytes = first
        .char_indices()
        .zip(c.char_indices())
        .take_while(|((_, c1), (_, c2))| c1 == c2)
        .last()
        .map(|((i, ch), _)| i + ch.len_utf8())
        .unwrap_or(0);
      end = end.min(common_bytes);

      if end == 0 {
        return Self::Many { candidates }; // no common prefix, can't collapse
      }
    }

    if end > min_end {
      Self::Single {
        result: f_content[..end].into(),
      }
    } else {
      Self::Many { candidates }
    }
  }
}

pub(crate) enum CompResponse {
  Accept(Candidate),  // user accepted completion
  Preview(Candidate), // splice candidate into the buffer but keep completer active (Tab-cycle preview)
  Consumed,           // key was handled, but completion remains active
  Passthrough,        // key falls through
  Dismiss,            // user canceled completion
  DismissPassthrough, // dismisses completer, and passes input to the main editor
}

pub(crate) trait Completer {
  fn complete(
    &mut self,
    line: String,
    cursor_pos: usize,
    direction: i32,
  ) -> ShResult<Option<String>>;
  fn reset(&mut self);
  fn reset_stay_active(&mut self);
  fn is_active(&self) -> bool;
  fn all_candidates(&self) -> Vec<Candidate> {
    vec![]
  }
  fn predicted_rows(&self) -> Option<usize> {
    None
  }
  fn selected_candidate(&self) -> Option<Candidate>;
  fn token_span(&self) -> (usize, usize);
  fn original_input(&self) -> &str;
  fn token(&self) -> &str {
    let orig = self.original_input();
    let (s, e) = self.token_span();
    orig.get(s..e).unwrap_or(orig)
  }
  fn draw(&mut self) -> ShResult<usize>;
  fn clear(&mut self) -> ShResult<()> {
    Ok(())
  }
  fn set_prompt_line_context(&mut self, _line_width: usize, _cursor_col: usize) {}
  fn handle_key(&mut self, key: K) -> ShResult<CompResponse>;
  fn get_completed_line(&self, candidate: &str) -> String;
}

#[derive(Default, Debug, Clone)]
pub(crate) struct SimpleCompleter {
  pub candidates: Vec<Candidate>,
  pub selected_idx: usize,
  pub original_input: String,
  pub token_span: (usize, usize),
  pub cursor_pos: usize,
  pub active: bool,
  pub dirs_only: bool,
  pub add_space: bool,
}

impl Completer for SimpleCompleter {
  fn all_candidates(&self) -> Vec<Candidate> {
    self.candidates.clone()
  }
  fn reset_stay_active(&mut self) {
    let active = self.is_active();
    self.reset();
    self.active = active;
  }
  fn get_completed_line(&self, _candidate: &str) -> String {
    self.get_completed_line()
  }
  fn complete(
    &mut self,
    line: String,
    cursor_pos: usize,
    direction: i32,
  ) -> ShResult<Option<String>> {
    if self.active {
      Ok(Some(self.cycle_completion(direction)))
    } else {
      self.start_completion(line, cursor_pos)
    }
  }

  fn reset(&mut self) {
    *self = Self::default();
  }

  fn is_active(&self) -> bool {
    self.active
  }

  fn selected_candidate(&self) -> Option<Candidate> {
    self.candidates.get(self.selected_idx).cloned()
  }

  fn token_span(&self) -> (usize, usize) {
    self.token_span
  }

  fn draw(&mut self) -> ShResult<usize> {
    Ok(0)
  }

  fn original_input(&self) -> &str {
    &self.original_input
  }

  fn handle_key(&mut self, _key: K) -> ShResult<CompResponse> {
    Ok(CompResponse::Passthrough)
  }
}

impl SimpleCompleter {
  pub fn cycle_completion(&mut self, direction: i32) -> String {
    if self.candidates.is_empty() {
      return self.original_input.clone();
    }

    let len = self.candidates.len();
    self.selected_idx = (self.selected_idx as i32 + direction).rem_euclid(len as i32) as usize;

    self.get_completed_line()
  }

  pub fn add_spaces(&mut self) {
    if self.add_space {
      self.candidates = std::mem::take(&mut self.candidates)
        .into_iter()
        .map(|c| {
          if !ends_with_unescaped(&c, "/") 		// directory
					&& !ends_with_unescaped(&c, "=") 		// '='-type arg
					&& !ends_with_unescaped(&c, " ")
          {
            // already has a space
            Candidate::from(format!("{} ", c))
          } else {
            c
          }
        })
        .collect()
    }
  }

  pub fn start_completion(&mut self, line: String, cursor_pos: usize) -> ShResult<Option<String>> {
    let result = self.get_candidates(line.clone(), cursor_pos)?;
    self.cursor_pos = cursor_pos;
    match result {
      CompResult::Many { candidates } => {
        self.candidates = candidates.clone();
        self.add_spaces();
        self.selected_idx = 0;
        self.original_input = line;
        self.active = true;

        Ok(Some(self.get_completed_line()))
      }
      CompResult::Single { result } => {
        self.candidates = vec![result.clone()];
        self.add_spaces();
        self.selected_idx = 0;
        self.original_input = line;
        self.active = false;

        Ok(Some(self.get_completed_line()))
      }
      CompResult::NoMatch => Ok(None),
    }
  }

  pub fn get_completed_line(&self) -> String {
    if self.candidates.is_empty() {
      return self.original_input.clone();
    }
    let selected = &self.candidates[self.selected_idx];
    let (start, end) = self.token_span;
    format!(
      "{}{}{}",
      &self.original_input[..start],
      selected.as_str(),
      &self.original_input[end..],
    )
  }

  pub fn build_comp_ctx(
    &self,
    tks: &[CtxTk],
    line: &str,
    cursor_pos: usize,
  ) -> ShResult<CompContext> {
    let mut ctx = CompContext {
      words: vec![],
      cword: 0,
      line: line.to_string(),
      cursor_pos,
    };

    let segments = tks
      .split(|t| matches!(t.class(), CtxTkRule::Operator | CtxTkRule::Separator))
      .filter(|&s| !s.is_empty())
      .map(|s| s.to_vec())
      .collect::<Vec<_>>();

    if segments.is_empty() {
      return Ok(ctx);
    }

    let relevant_pos = segments
      .iter()
      .position(|tks| {
        tks
          .iter()
          .next()
          .is_some_and(|tk| tk.range().start > cursor_pos)
      })
      .map(|i| i.saturating_sub(1))
      .unwrap_or(segments.len().saturating_sub(1));

    let relevant = segments[relevant_pos].to_vec();
    let mut words = relevant
      .iter()
      .map(|s| s.span().as_str().to_string())
      .collect::<Vec<_>>();

    let cword = if let Some(pos) = relevant
      .iter()
      .position(|tk| tk.range_inclusive().contains(&cursor_pos))
    {
      pos
    } else {
      let insert_pos = relevant
        .iter()
        .position(|tk| tk.range().start > cursor_pos)
        .unwrap_or(relevant.len());
      words.insert(insert_pos, String::new());
      insert_pos
    };

    ctx.words = words;
    ctx.cword = cword;

    Ok(ctx)
  }

  pub fn try_comp_spec(&self, ctx: &CompContext) -> ShResult<CompSpecResult> {
    log::debug!("Trying to find comp spec for context: {:?}", ctx);
    let Some(cmd) = ctx.cmd() else {
      return Ok(CompSpecResult::NoSpec);
    };

    let Some(spec) = Shed::meta(|m| m.get_comp_spec(cmd)) else {
      return Ok(CompSpecResult::NoSpec);
    };

    let candidates = spec.complete(ctx)?;
    if candidates.is_empty() {
      Ok(CompSpecResult::NoMatch {
        flags: spec.get_flags(),
      })
    } else {
      Ok(CompSpecResult::Match {
        result: CompResult::from_candidates(candidates),
        flags: spec.get_flags(),
      })
    }
  }

  pub fn get_candidates(&mut self, line: String, cursor_pos: usize) -> ShResult<CompResult> {
    let tks = get_context_tokens(&line);
    let (strat, replace_span, leaf_cursor_pos) = CompStrat::resolve(&tks, cursor_pos);

    log::debug!(
      "get_candidates: line={line:?} cursor_pos={cursor_pos} strat={strat:?} span={:?} leaf_cursor_pos={leaf_cursor_pos}",
      replace_span.range()
    );
    self.token_span = (replace_span.range().start, replace_span.range().end);
    let mut result = match strat {
      CompStrat::Var { prefix } => CompResult::from_candidates(complete_vars(&prefix)),
      CompStrat::Tilde { prefix } => CompResult::from_candidates(complete_users(&prefix)),
      CompStrat::Command { prefix } => {
        CompResult::from_candidates(complete_commands(&prefix, leaf_cursor_pos))
      }
      CompStrat::Files { path } => {
        if self.dirs_only {
          CompResult::from_candidates(complete_dirs(&path, leaf_cursor_pos))
        } else {
          CompResult::from_candidates(complete_path(&path, leaf_cursor_pos))
        }
      }
      CompStrat::Separator => CompResult::Single {
        result: Candidate::from(";"),
      },
      CompStrat::Null => CompResult::NoMatch,
      CompStrat::Argument { path } => {
        let ctx = self.build_comp_ctx(&tks, &line, cursor_pos)?;
        match self.try_comp_spec(&ctx)? {
          CompSpecResult::Match { result, flags } => {
            if flags.contains(CompOptFlags::SPACE) {
              self.add_space = true;
            }
            result
          }
          CompSpecResult::NoSpec => {
            CompResult::from_candidates(complete_path(&path, leaf_cursor_pos))
          }
          CompSpecResult::NoMatch { flags } => {
            if flags.contains(CompOptFlags::SPACE) {
              self.add_space = true;
            }
            if flags.contains(CompOptFlags::DIRNAMES) || self.dirs_only {
              CompResult::from_candidates(complete_dirs(&path, leaf_cursor_pos))
            } else if flags.contains(CompOptFlags::DEFAULT) {
              CompResult::from_candidates(complete_path(&path, leaf_cursor_pos))
            } else {
              CompResult::NoMatch
            }
          }
        }
      }
    };

    if let CompResult::Many { ref mut candidates } = result {
      candidates.sort_by(|a, b| {
        let a_content = a.content();
        let b_content = b.content();
        let a_len = a_content.len();
        let b_len = b_content.len();

        a_len.cmp(&b_len).then_with(|| a_content.cmp(b_content))
      });
      candidates.dedup();
    }

    // lastly, if all of the candidates contain a common prefix
    // then collapse them into a single candidate with just the prefix
    let typed = &line[self.token_span.0..self.token_span.1];
    Ok(result.try_collapse_by_prefix(typed))
  }
}
