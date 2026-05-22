use std::{
  collections::HashSet,
  fmt::{Debug, Display},
  os::unix::fs::PermissionsExt,
  path::{Path, PathBuf},
  rc::Rc,
};

use bitflags::bitflags;
use nix::sys::signal::Signal;

use super::{
  builtin::BUILTIN_NAMES,
  context::{CtxTk, CtxTkRule, get_context_tokens},
  editmode::{EditMode, ViInsert},
  eval::{execute::exec_nonint, lex::Span},
  expand::{
    as_var_val_display, escape_glob, escape_str, expand_raw_inner, markers::strip_markers,
    unescape_str,
  },
  key,
  keys::{KeyCode as C, KeyEvent as K},
  linebuf::LineBuf,
  shopt,
  state::{
    self, Shed,
    meta::Utility,
    terminal::{ColorMode, Cols, Rows, TermGuard, calc_str_width},
    vars::{VarFlags, VarKind},
  },
  try_var,
  util::{self, ShResult, ends_with_unescaped, rfind_unescaped, var_ctx_guard},
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

#[derive(Clone, Default, Debug)]
pub(crate) struct ClampedUsize {
  val: usize,
  max: usize,
  wrap: bool,
}

impl ClampedUsize {
  pub fn new(val: usize, max: usize, wrap: bool) -> Self {
    Self { val, max, wrap }
  }
  pub fn get(&self) -> usize {
    self.val
  }
  pub fn set_max(&mut self, max: usize) {
    self.max = max;
    if self.val >= self.max && self.max > 0 {
      self.val = self.max - 1;
    }
  }
  pub fn wrap_add(&mut self, n: usize) {
    if self.max == 0 {
      return;
    }
    if self.wrap {
      self.val = (self.val + n) % self.max;
    } else {
      self.val = (self.val + n).min(self.max.saturating_sub(1));
    }
  }
  pub fn wrap_sub(&mut self, n: usize) {
    if self.max == 0 {
      return;
    }
    if self.wrap {
      self.val = (self.val + self.max - (n % self.max)) % self.max;
    } else {
      self.val = self.val.saturating_sub(n);
    }
  }

  pub fn sub(&mut self, n: usize) {
    self.val = self.val.saturating_sub(n);
  }
  pub fn add(&mut self, n: usize) {
    self.val = self.val.saturating_add(n).min(self.max.saturating_sub(1));
  }
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
          Candidate::from(s).with_desc(val)
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
  if let Some(stripped) = start.strip_prefix("./") {
    return Shed::meta(|m| {
      m.cached_utils()
        .filter(|u| matches!(u.kind(), state::meta::UtilKind::File(_)))
        .map(Candidate::from)
        .filter(|c| c.is_match(stripped))
        .map(|mut c| {
          c.content = format!("./{}", c.content);
          c
        })
        .collect()
    });
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
}

pub(crate) enum CompResponse {
  Passthrough,       // key falls through
  Accept(Candidate), // user accepted completion
  Dismiss,           // user canceled completion
  Consumed,          // key was handled, but completion remains active
}

pub(crate) enum SelectorResponse {
  Accept(Candidate),
  Dismiss,
  Consumed,
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
pub(crate) struct ScoredCandidate {
  pub candidate: Candidate,
  pub score: Option<i32>,
  pub penalize_len_diff: bool,
}

impl ScoredCandidate {
  const BONUS_BOUNDARY: i32 = 10;
  const BONUS_CONSECUTIVE: i32 = 8;
  const BONUS_FIRST_CHAR: i32 = 5;
  const PENALTY_GAP_START: i32 = 3;
  const PENALTY_GAP_EXTEND: i32 = 1;

  pub fn new(candidate: Candidate) -> Self {
    Self {
      candidate,
      score: None,
      penalize_len_diff: false,
    }
  }
  pub fn with_len_penalty(mut self, enable: bool) -> Self {
    self.penalize_len_diff = enable;
    self
  }
  fn is_word_bound(prev: char, curr: char) -> bool {
    match prev {
      '/' | '_' | '-' | '.' | ' ' => true,
      c if c.is_lowercase() && curr.is_uppercase() => true, // camelCase boundary
      _ => false,
    }
  }
  pub fn fuzzy_score(&mut self, other: &str) -> i32 {
    if other.is_empty() {
      self.score = Some(0);
      return 0;
    }

    let query_chars: Vec<char> = other.chars().collect();
    let candidate_chars: Vec<char> = self.candidate.chars().collect();
    let mut indices = vec![];
    let mut qi = 0;
    for (ci, c_ch) in self.candidate.chars().enumerate() {
      if qi < query_chars.len() && c_ch.eq_ignore_ascii_case(&query_chars[qi]) {
        indices.push(ci);
        qi += 1;
      }
    }

    if indices.len() != query_chars.len() {
      self.score = Some(i32::MIN);
      return i32::MIN;
    }

    let mut score: i32 = 0;

    for (i, &idx) in indices.iter().enumerate() {
      if idx == 0 {
        score += Self::BONUS_FIRST_CHAR;
      }

      if idx == 0 || Self::is_word_bound(candidate_chars[idx - 1], candidate_chars[idx]) {
        score += Self::BONUS_BOUNDARY;
      }

      if i > 0 {
        let gap = idx - indices[i - 1] - 1;
        if gap == 0 {
          score += Self::BONUS_CONSECUTIVE;
        } else {
          score -= Self::PENALTY_GAP_START + (gap as i32 - 1) * Self::PENALTY_GAP_EXTEND;
        }
      }
    }

    if self.penalize_len_diff {
      let len_diff = (candidate_chars.len() as isize - query_chars.len() as isize).unsigned_abs();
      let len_penalty = (len_diff as i32) * 2;
      score -= len_penalty;
    }

    self.score = Some(score);
    score
  }
}

impl From<String> for ScoredCandidate {
  fn from(content: String) -> Self {
    Self {
      candidate: content.into(),
      score: None,
      penalize_len_diff: false,
    }
  }
}

impl From<Candidate> for ScoredCandidate {
  fn from(candidate: Candidate) -> Self {
    Self {
      candidate,
      score: None,
      penalize_len_diff: false,
    }
  }
}

#[derive(Debug, Clone)]
pub(crate) struct FuzzyLayout {
  top_left: usize,
  rows: usize,
  cols: usize,
  cursor_col: usize,
  /// Width of the prompt line above the `\n` that starts the fuzzy window.
  /// If PSR was drawn, this is `t_cols`; otherwise the content width.
  preceding_line_width: usize,
  /// Cursor column on the prompt line before the fuzzy window was drawn.
  preceding_cursor_col: usize,
}

#[derive(Default, Debug)]
pub(crate) struct QueryEditor {
  mode: ViInsert,
  scroll_offset: usize,
  available_width: usize,
  linebuf: LineBuf,
}

impl QueryEditor {
  pub fn clear(&mut self) {
    self.linebuf = LineBuf::new();
    self.mode = ViInsert::default();
    self.scroll_offset = 0;
  }
  pub fn set_available_width(&mut self, width: usize) {
    self.available_width = width;
  }
  pub fn update_scroll_offset(&mut self) {
    let cursor_pos = self.linebuf.cursor_to_flat();
    if cursor_pos < self.scroll_offset + 1 {
      self.scroll_offset = self.linebuf.cursor_to_flat().saturating_sub(1)
    }
    if cursor_pos >= self.scroll_offset + self.available_width.saturating_sub(1) {
      self.scroll_offset = self
        .linebuf
        .cursor_to_flat()
        .saturating_sub(self.available_width.saturating_sub(1));
    }
    let max_offset = self
      .linebuf
      .count_graphemes()
      .saturating_sub(self.available_width);
    self.scroll_offset = self.scroll_offset.min(max_offset);
  }
  pub fn get_window(&mut self) -> String {
    let buf_len = self.linebuf.count_graphemes();
    if buf_len <= self.available_width {
      return self.linebuf.joined();
    }
    let start = self
      .scroll_offset
      .min(buf_len.saturating_sub(self.available_width));
    let end = (start + self.available_width).min(buf_len);
    self.linebuf.slice(start..end).unwrap_or_default()
  }
  pub fn handle_key(&mut self, key: K) -> ShResult<()> {
    let Some(cmd) = self.mode.handle_key(key) else {
      return Ok(());
    };
    self.linebuf.exec_cmd(cmd)
  }
}

#[derive(Default, Debug)]
pub(crate) struct FuzzySelector {
  query: QueryEditor,
  filtered: Vec<ScoredCandidate>,
  candidates: Vec<Candidate>,
  cursor: ClampedUsize,
  number_candidates: bool,
  old_layout: Option<FuzzyLayout>,
  max_height: usize,
  scroll_offset: usize,
  prompt_line_width: usize,
  prompt_cursor_col: usize,
  row_map: Vec<Option<usize>>,
  hovered: Option<usize>, // index of the currently hovered candidate, if any
  title: String,
  _mouse_guard: Option<TermGuard>,
}

#[derive(Debug)]
pub(crate) struct FuzzyCompleter {
  completer: SimpleCompleter,
  pub selector: FuzzySelector,
}

impl FuzzySelector {
  const SELECTOR_GRAY: &str = "\x1b[90m▌\x1b[0m";
  const PROMPT_ARROW: &str = "\x1b[1;36m>\x1b[0m";

  pub fn new(title: impl Into<String>) -> Self {
    Self {
      max_height: 8,
      query: QueryEditor::default(),
      filtered: vec![],
      candidates: vec![],
      cursor: ClampedUsize::new(0, 0, true),
      number_candidates: false,
      old_layout: None,
      scroll_offset: 0,
      prompt_line_width: 0,
      row_map: vec![],
      prompt_cursor_col: 0,
      hovered: None,
      title: title.into(),
      _mouse_guard: Shed::term_mut(|t| t.mouse_support_guard(true)).ok(),
    }
  }

  pub fn number_candidates(self, enable: bool) -> Self {
    Self {
      number_candidates: enable,
      ..self
    }
  }
  fn selector_hl() -> String {
    match Shed::term(|t| t.color_mode()) {
      Some(ColorMode::Truecolor) => "\x1b[38;2;200;0;120m▌\x1b[1;39;48;5;237m",
      Some(ColorMode::Palette256) => "\x1b[38;5;162m▌\x1b[1;39;48;5;237m",
      Some(ColorMode::Palette16) => "\x1b[35m▌\x1b[1;39;100m",
      None => "▌\x1b[1m",
    }
    .to_string()
  }

  fn selector_hover_hl() -> String {
    match Shed::term(|t| t.color_mode()) {
      Some(ColorMode::Truecolor) => "\x1b[90m▌\x1b[1;39;48;5;237m",
      Some(ColorMode::Palette256) => "\x1b[90m▌\x1b[1;39;48;5;237m",
      Some(ColorMode::Palette16) => "\x1b[90m▌\x1b[1;39;100m",
      None => "▌\x1b[1m",
    }
    .to_string()
  }

  /// Calculate how many rows we need in order to draw this thing
  pub fn predicted_rows(&self) -> usize {
    if self.candidates.is_empty() && self.filtered.is_empty() {
      return 0;
    }
    const CHROME_ROWS: usize = 4;

    let mut cand_rows = 0usize;
    for c in self.filtered.iter().skip(self.scroll_offset) {
      let h = c.candidate.content().trim_end().lines().count().max(1);
      if cand_rows + h > self.max_height {
        cand_rows = self.max_height;
        break;
      }
      cand_rows += h;
    }

    CHROME_ROWS + cand_rows
  }

  pub fn candidates(&self) -> &[Candidate] {
    &self.candidates
  }

  pub fn filtered(&self) -> &[ScoredCandidate] {
    &self.filtered
  }

  pub fn activate(&mut self, candidates: Vec<Candidate>) {
    self.candidates = candidates;
    self.score_candidates();
  }

  pub fn set_query(&mut self, query: String) {
    self.query.linebuf = LineBuf::new().with_initial(&query, query.len());
    self.query.update_scroll_offset();
    self.score_candidates();
  }

  pub fn reset_query(&mut self) {
    self.query.clear();
    self.score_candidates();
  }

  pub fn selected_candidate(&self) -> Option<Candidate> {
    self
      .filtered
      .get(self.cursor.get())
      .map(|c| c.candidate.clone())
  }

  pub fn set_prompt_line_context(&mut self, line_width: usize, cursor_col: usize) {
    self.prompt_line_width = line_width;
    self.prompt_cursor_col = cursor_col;
  }

  fn candidate_height(&self, idx: usize) -> usize {
    self
      .filtered
      .get(idx)
      .map(|c| c.candidate.content().trim_end().lines().count().max(1))
      .unwrap_or(1)
  }

  fn get_window(&mut self) -> &[ScoredCandidate] {
    self.update_scroll_offset();

    let mut lines = 0;
    let mut end = self.scroll_offset;
    while end < self.filtered.len() {
      if lines >= self.max_height {
        break;
      }
      lines += self.candidate_height(end);
      end += 1;
    }

    &self.filtered[self.scroll_offset..end]
  }

  pub fn update_scroll_offset(&mut self) {
    let cursor = self.cursor.get();

    // Scroll up: cursor above window
    if cursor < self.scroll_offset {
      self.scroll_offset = cursor;
      return;
    }

    // Scroll down: work backwards from cursor to find the
    // earliest offset that fits within max_height lines.
    let mut lines = 0;
    let mut new_offset = cursor;
    loop {
      let h = self.candidate_height(new_offset);
      if lines + h > self.max_height && new_offset < cursor {
        new_offset += 1;
        break;
      }
      lines += h;
      if new_offset == 0 {
        break;
      }
      new_offset -= 1;
    }

    if new_offset > self.scroll_offset {
      self.scroll_offset = new_offset;
    }
  }

  pub fn score_candidates(&mut self) {
    let mut scored: Vec<_> = self
      .candidates
      .clone()
      .into_iter()
      .filter_map(|c| {
        let mut sc = ScoredCandidate::new(c);
        let score = sc.fuzzy_score(&self.query.linebuf.joined());
        if score > i32::MIN { Some(sc) } else { None }
      })
      .collect();
    scored.sort_by_key(|sc| sc.score.unwrap_or(i32::MIN));
    scored.reverse();
    self.cursor.set_max(scored.len());
    self.filtered = scored;
  }

  pub fn handle_click(&mut self, row: usize, _col: usize) -> ShResult<SelectorResponse> {
    let top_left = self.old_layout.as_ref().map(|l| l.top_left).unwrap_or(0);
    let relative_row = row.saturating_sub(top_left);
    if let Some(idx) = self.row_map.get(relative_row).copied().flatten() {
      if self.cursor.val == idx {
        Ok(SelectorResponse::Accept(
          self.filtered[idx].candidate.clone(),
        ))
      } else {
        self.cursor = ClampedUsize::new(idx, self.filtered.len(), true);
        Ok(SelectorResponse::Consumed)
      }
    } else {
      Ok(SelectorResponse::Consumed)
    }
  }

  pub fn handle_hover(&mut self, row: usize) -> ShResult<SelectorResponse> {
    let top_left = self.old_layout.as_ref().map(|l| l.top_left).unwrap_or(0);
    let relative_row = row.saturating_sub(top_left);
    let idx = self.row_map.get(relative_row).copied().flatten();

    if self.hovered != idx {
      self.hovered = idx;
    }

    Ok(SelectorResponse::Consumed)
  }

  pub fn handle_key(&mut self, key: K) -> ShResult<SelectorResponse> {
    match key {
      K(C::MousePos(row, _), _) => self.handle_hover(row),
      K(C::LeftClick(row, col), _) => self.handle_click(row, col),
      key!(Ctrl + 'd') | key!(Esc) => {
        self.filtered.clear();
        Ok(SelectorResponse::Dismiss)
      }
      key!(Enter) => {
        if let Some(selected) = self.filtered.get(self.cursor.get()) {
          Ok(SelectorResponse::Accept(selected.candidate.clone()))
        } else {
          Ok(SelectorResponse::Dismiss)
        }
      }
      key @ (key!(ScrollUp) | key!(Shift + Tab) | key!(Up)) => {
        match key {
          key!(ScrollUp) => self.cursor.sub(1), // no wrap
          key!(Up) | key!(Shift + Tab) => self.cursor.wrap_sub(1), // wrap
          _ => unreachable!(),
        }
        Ok(SelectorResponse::Consumed)
      }
      key @ (key!(ScrollDown) | key!(Tab) | key!(Down)) => {
        match key {
          key!(ScrollDown) => self.cursor.add(1),            // no wrap
          key!(Down) | key!(Tab) => self.cursor.wrap_add(1), // wrap
          _ => unreachable!(),
        }
        self.update_scroll_offset();
        Ok(SelectorResponse::Consumed)
      }
      key!(Ctrl + 'c') => {
        self.query.clear();
        self.score_candidates();
        Ok(SelectorResponse::Consumed)
      }
      _ => {
        self.query.handle_key(key)?;
        self.score_candidates();
        Ok(SelectorResponse::Consumed)
      }
    }
  }

  pub fn draw(&mut self) -> ShResult<usize> {
    self.row_map.clear();
    let (cols, top_left) = Shed::term_mut(|t| {
      (
        t.t_cols(),
        t.get_cursor_pos()
          .ok()
          .flatten()
          .unwrap_or((Rows(0), Cols(0)))
          .0
          .0
          + 1,
      )
    });

    let pad = |content: &str, fill: &str, right_border: &str| {
      util::pad_line(content, fill, right_border, cols);
    };

    let mut row_map = vec![];
    let cursor_pos = self.cursor.get();
    let offset = self.scroll_offset;
    let number_candidates = self.number_candidates;
    let max_height = self.max_height;
    let num_filtered = self.filtered.len();
    let num_candidates = self.candidates.len();
    let min_pad = num_candidates.to_string().len().saturating_add(1).max(6);
    let hovered = self.hovered;

    self.query.set_available_width(cols.saturating_sub(6));
    self.query.update_scroll_offset();
    let query = self.query.get_window();
    let title = self.title.clone();
    let visible = self.get_window();
    let mut rows: usize = 0;

    // ╭─ Title ──────────────────╮
    let title_content = format!(
      "\n{}{} \x1b[1m{}\x1b[0m ",
      util::TOP_LEFT,
      util::HOR_LINE,
      title
    );
    pad(&title_content, util::HOR_LINE, util::TOP_RIGHT);
    rows += 1;
    row_map.push(None);

    // │ > query                  │
    let prompt_content = format!("{} {} {}", util::VERT_LINE, Self::PROMPT_ARROW, query);
    pad(&prompt_content, " ", util::VERT_LINE);
    rows += 1;

    // ├──filtered/total──────────┤
    let sep_content = format!(
      "{}{}\x1b[33m{}\x1b[0m/\x1b[33m{}\x1b[0m",
      util::TREE_LEFT,
      util::HOR_LINE.repeat(2),
      num_filtered,
      num_candidates
    );
    pad(&sep_content, util::HOR_LINE, util::TREE_RIGHT);
    rows += 1;

    // Candidate lines
    let mut lines_drawn = 0;
    let col_lim = if number_candidates {
      cols.saturating_sub(3 + min_pad)
    } else {
      cols.saturating_sub(3)
    };

    const MAX_DESC_COL: usize = 32;
    let desc_col_width = visible
      .iter()
      .filter(|sc| sc.candidate.desc.is_some())
      .map(|sc| sc.candidate.display())
      .filter_map(|s| s.trim_end().lines().next().map(calc_str_width))
      .max()
      .unwrap_or(0)
      .min(MAX_DESC_COL);

    for (i, s_cand) in visible.iter().enumerate() {
      if lines_drawn >= max_height {
        break;
      }

      let selected = i + offset == cursor_pos;
      let hovered = hovered == Some(i + offset);
      let selector = if selected {
        &Self::selector_hl()
      } else if hovered {
        &Self::selector_hover_hl()
      } else {
        Self::SELECTOR_GRAY
      };
      let mut drew_number = false;

      let mut first = true;
      let display = s_cand.candidate.display();
      for line in display.trim_end().lines() {
        if lines_drawn >= max_height {
          break;
        }

        let mut line = line.trim_end().replace('\t', "    ");
        if first {
          first = false;
          if let Some(desc) = &s_cand.candidate.desc {
            let cand_width = calc_str_width(&line);
            let pad = desc_col_width.saturating_sub(cand_width);
            line = format!("{line}{}\x1b[90m  {desc}\x1b[0m", " ".repeat(pad));
          }
        }
        if calc_str_width(&line) >= col_lim {
          line.truncate(col_lim.saturating_sub(6));
          line.push_str("...");
        }

        let left = if number_candidates && !drew_number {
          let num = i + offset + 1;
          format!(
            "{} {}\x1b[33m{num:<min_pad$}\x1b[39m{line}\x1b[0m",
            util::VERT_LINE,
            selector
          )
        } else if number_candidates {
          format!(
            "{} {}{:>min_pad$}{line}\x1b[0m",
            util::VERT_LINE,
            selector,
            ""
          )
        } else {
          format!("{} {}{line}\x1b[0m", util::VERT_LINE, selector)
        };

        pad(&left, " ", util::VERT_LINE);
        rows += 1;
        row_map.push(Some(i + offset));
        drew_number = true;
        lines_drawn += 1;
      }
    }

    // ╰──────────────────────────╯
    write_term!(
      "{}{}{}",
      util::BOT_LEFT,
      util::HOR_LINE.repeat(cols.saturating_sub(2)),
      util::BOT_RIGHT
    )
    .unwrap();
    rows += 1;
    row_map.push(None);

    // Move cursor back up to the query input line
    let lines_below_prompt = rows.saturating_sub(2);
    let cursor_in_window = self
      .query
      .linebuf
      .cursor_to_flat()
      .saturating_sub(self.query.scroll_offset);
    let cursor_col = cursor_in_window + 4;
    write_term!("\x1b[{lines_below_prompt}A\r\x1b[{cursor_col}C").unwrap();

    let new_layout = FuzzyLayout {
      top_left,
      rows,
      cols,
      cursor_col,
      preceding_line_width: self.prompt_line_width,
      preceding_cursor_col: self.prompt_cursor_col,
    };
    self.old_layout = Some(new_layout);
    self.row_map = row_map;

    Ok(rows)
  }

  pub fn clear(&mut self) -> ShResult<()> {
    if let Some(layout) = self.old_layout.take() {
      let new_cols = Shed::term(|t| t.t_cols());
      let total_cells = layout.rows * layout.cols;
      let physical_rows = if new_cols > 0 {
        total_cells.div_ceil(new_cols)
      } else {
        layout.rows
      };
      let cursor_offset = layout.cols + layout.cursor_col;
      let cursor_phys_row = cursor_offset.checked_div(new_cols).unwrap_or(1);
      let lines_below = physical_rows.saturating_sub(cursor_phys_row + 1);

      let gap_extra = if new_cols > 0 && layout.preceding_line_width > new_cols {
        let wrap_rows = (layout.preceding_line_width).div_ceil(new_cols);
        let cursor_wrap_row = layout.preceding_cursor_col / new_cols;
        wrap_rows.saturating_sub(cursor_wrap_row + 1)
      } else {
        0
      };

      if lines_below > 0 {
        write_term!("\x1b[{lines_below}B").unwrap();
      }
      for _ in 0..physical_rows {
        write_term!("\x1b[2K\x1b[A").unwrap();
      }
      write_term!("\x1b[2K").unwrap();
      for _ in 0..gap_extra {
        write_term!("\x1b[2K\x1b[A").unwrap();
      }
    }
    Ok(())
  }
}

impl Default for FuzzyCompleter {
  fn default() -> Self {
    Self {
      completer: SimpleCompleter::default(),
      selector: FuzzySelector::new("Complete"),
    }
  }
}

impl Completer for FuzzyCompleter {
  fn all_candidates(&self) -> Vec<Candidate> {
    self.selector.candidates.clone()
  }
  fn set_prompt_line_context(&mut self, line_width: usize, cursor_col: usize) {
    self
      .selector
      .set_prompt_line_context(line_width, cursor_col);
  }
  fn reset_stay_active(&mut self) {
    self.selector.reset_query();
  }
  fn get_completed_line(&self, _candidate: &str) -> String {
    log::debug!("Getting completed line for candidate: {}", _candidate);

    let selected = self.selector.selected_candidate().unwrap_or_default();
    let (start, end) = self.completer.token_span;
    // Wholesale replace `token_span` with the candidate. See
    // `SimpleCompleter::get_completed_line` for the rationale.
    let ret = format!(
      "{}{}{}",
      &self.completer.original_input[..start],
      selected.as_str(),
      &self.completer.original_input[end..],
    );
    log::debug!("Completed line: {}", ret);
    ret
  }
  fn complete(
    &mut self,
    line: String,
    cursor_pos: usize,
    direction: i32,
  ) -> ShResult<Option<String>> {
    self.completer.complete(line, cursor_pos, direction)?;
    let candidates: Vec<_> = self.completer.candidates.clone();
    if candidates.is_empty() {
      self.completer.reset();
      return Ok(None);
    } else if candidates.len() == 1 {
      self.selector.filtered = candidates.into_iter().map(ScoredCandidate::from).collect();
      let selected = self.selector.filtered[0].candidate.content().to_string();
      let completed = self.get_completed_line(&selected);
      return Ok(Some(completed));
    }
    self.selector.activate(candidates);
    Ok(None)
  }

  fn handle_key(&mut self, key: K) -> ShResult<CompResponse> {
    match self.selector.handle_key(key)? {
      SelectorResponse::Accept(s) => Ok(CompResponse::Accept(s)),
      SelectorResponse::Dismiss => Ok(CompResponse::Dismiss),
      SelectorResponse::Consumed => Ok(CompResponse::Consumed),
    }
  }
  fn clear(&mut self) -> ShResult<()> {
    self.selector.clear()
  }
  fn draw(&mut self) -> ShResult<usize> {
    self.selector.draw()
  }
  fn reset(&mut self) {
    self.completer.reset();
    self.selector.reset_query();
  }
  fn token_span(&self) -> (usize, usize) {
    self.completer.token_span()
  }
  fn is_active(&self) -> bool {
    !self.selector.candidates.is_empty()
  }
  fn selected_candidate(&self) -> Option<Candidate> {
    self.selector.selected_candidate()
  }
  fn original_input(&self) -> &str {
    &self.completer.original_input
  }
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

    Ok(result)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::{
    readline::{Prompt, ShedLine},
    state::{Shed, vars::VarFlags, vars::VarKind},
    tests::testutil::TestGuard,
  };
  fn test_vi(initial: &str) -> (ShedLine, TestGuard) {
    let g = TestGuard::new();
    let prompt = Prompt::default();
    let vi = ShedLine::new_no_hist(prompt).unwrap().with_initial(initial);
    (vi, g)
  }

  // ===================== ScoredCandidate::fuzzy_score =====================

  #[test]
  fn fuzzy_exact_match() {
    let mut c = ScoredCandidate::new("hello".into());
    let score = c.fuzzy_score("hello");
    assert!(score > 0);
  }

  #[test]
  fn fuzzy_prefix_match() {
    let mut c = ScoredCandidate::new("hello_world".into());
    let score = c.fuzzy_score("hello");
    assert!(score > 0);
  }

  #[test]
  fn fuzzy_no_match() {
    let mut c = ScoredCandidate::new("abc".into());
    let score = c.fuzzy_score("xyz");
    assert_eq!(score, i32::MIN);
  }

  #[test]
  fn fuzzy_empty_query() {
    let mut c = ScoredCandidate::new("anything".into());
    let score = c.fuzzy_score("");
    assert_eq!(score, 0);
  }

  #[test]
  fn fuzzy_boundary_bonus() {
    let mut a = ScoredCandidate::new("foo_bar".into());
    let mut b = ScoredCandidate::new("fxxxbxr".into());
    let score_a = a.fuzzy_score("fbr");
    let score_b = b.fuzzy_score("fbr");
    // word-boundary match should score higher
    assert!(score_a > score_b);
  }

  // ===================== CompResult::from_candidates =====================

  #[test]
  fn comp_result_no_match() {
    let result = CompResult::from_candidates(vec![]);
    assert!(matches!(result, CompResult::NoMatch));
  }

  #[test]
  fn comp_result_single() {
    let result = CompResult::from_candidates(vec!["foo".into()]);
    assert!(matches!(result, CompResult::Single { .. }));
  }

  #[test]
  fn comp_result_many() {
    let result = CompResult::from_candidates(vec!["foo".into(), "bar".into()]);
    assert!(matches!(result, CompResult::Many { .. }));
  }

  // ===================== complete_signals =====================

  #[test]
  fn complete_signals_int() {
    let results = complete_signals("INT");
    assert!(results.contains(&Candidate::from("INT")));
  }

  #[test]
  fn complete_signals_empty() {
    let results = complete_signals("");
    assert!(!results.is_empty());
  }

  #[test]
  fn complete_signals_no_match() {
    let results = complete_signals("ZZZZZZZ");
    assert!(results.is_empty());
  }

  // ===================== COMP_WORDBREAKS =====================

  #[test]
  fn wordbreak_equals_default() {
    let _g = TestGuard::new();
    let mut comp = SimpleCompleter::default();

    let line = "cmd --foo=bar".to_string();
    let cursor = line.len();
    let _ = comp.get_candidates(line.clone(), cursor);

    let eq_idx = line.find('=').unwrap();
    assert_eq!(
      comp.token_span.0,
      eq_idx + 1,
      "token_span.0 ({}) should be right after '=' ({})",
      comp.token_span.0,
      eq_idx
    );
  }

  #[test]
  fn wordbreak_colon_when_set() {
    let _g = TestGuard::new();
    Shed::vars_mut(|v| {
      v.set_var(
        "COMP_WORDBREAKS",
        VarKind::Str("=:".into()),
        VarFlags::empty(),
      )
    })
    .unwrap();

    let mut comp = SimpleCompleter::default();
    let line = "scp host:foo".to_string();
    let cursor = line.len();
    let _ = comp.get_candidates(line.clone(), cursor);

    let colon_idx = line.find(':').unwrap();
    assert_eq!(
      comp.token_span.0,
      colon_idx + 1,
      "token_span.0 ({}) should be right after ':' ({})",
      comp.token_span.0,
      colon_idx
    );
  }

  #[test]
  fn wordbreak_rightmost_wins() {
    let _g = TestGuard::new();
    Shed::vars_mut(|v| {
      v.set_var(
        "COMP_WORDBREAKS",
        VarKind::Str("=:".into()),
        VarFlags::empty(),
      )
    })
    .unwrap();

    let mut comp = SimpleCompleter::default();
    let line = "cmd --opt=host:val".to_string();
    let cursor = line.len();
    let _ = comp.get_candidates(line.clone(), cursor);

    let colon_idx = line.rfind(':').unwrap();
    assert_eq!(
      comp.token_span.0,
      colon_idx + 1,
      "should break at rightmost wordbreak char"
    );
  }

  // ===================== get_candidates: CompStrat dispatch =====================
  // The COMP_WORDBREAKS tests above exercise token_span calculation.
  // These tests exercise the strategy-routing match arms — Command,
  // Var, Tilde, Argument, Files (via redirect), Null, and the
  // empty-line fast path.

  /// Strip just the content list out of a CompResult for assertion.
  fn contents(result: &CompResult) -> Vec<&str> {
    match result {
      CompResult::NoMatch => vec![],
      CompResult::Single { result } => vec![result.content()],
      CompResult::Many { candidates } => candidates.iter().map(|c| c.content()).collect(),
    }
  }

  #[test]
  fn get_candidates_empty_line_routes_to_command_strategy() {
    // Empty line / cursor at 0 falls through to the
    // `Self::Command { prefix: "" }` default in CompStrat::resolve.
    let _g = TestGuard::new();
    let mut comp = SimpleCompleter::default();
    let result = comp.get_candidates("".into(), 0).unwrap();
    // Almost certainly Many — even a minimal PATH has > 1 binary. But
    // accept Single too in case some sandboxed env has exactly one.
    match result {
      CompResult::Many { .. } | CompResult::Single { .. } => {}
      CompResult::NoMatch => panic!("expected command candidates, got NoMatch"),
    }
  }

  #[test]
  fn get_candidates_var_prefix_completes_with_shell_var() {
    let _g = TestGuard::new();
    // Use a name unlikely to be defined by the environment.
    Shed::vars_mut(|v| {
      v.set_var(
        "UNIQUE_COMP_TEST_VAR_XYZZY",
        VarKind::Str("hello".into()),
        VarFlags::empty(),
      )
      .unwrap();
    });
    let mut comp = SimpleCompleter::default();
    let line = "echo $UNIQUE_COMP_TEST".to_string();
    let cursor = line.len();
    let result = comp.get_candidates(line, cursor).unwrap();
    let cs = contents(&result);
    assert!(
      cs.contains(&"UNIQUE_COMP_TEST_VAR_XYZZY"),
      "expected UNIQUE_COMP_TEST_VAR_XYZZY in candidates, got: {cs:?}"
    );
  }

  #[test]
  fn get_candidates_var_prefix_with_no_matches_returns_nomatch() {
    let _g = TestGuard::new();
    let mut comp = SimpleCompleter::default();
    // A prefix that absolutely won't match any shell or env var.
    let line = "echo $ZZZZZZZ_NOT_A_REAL_VAR_PREFIX_QQQ".to_string();
    let cursor = line.len();
    let result = comp.get_candidates(line, cursor).unwrap();
    assert!(matches!(result, CompResult::NoMatch));
  }

  #[test]
  fn get_candidates_path_arg_lists_directory_entries() {
    let _g = TestGuard::new();
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::write(dir.path().join("apple.txt"), "").unwrap();
    std::fs::write(dir.path().join("banana.txt"), "").unwrap();
    std::fs::write(dir.path().join("cherry.txt"), "").unwrap();

    let mut comp = SimpleCompleter::default();
    let line = format!("ls {}/", dir.path().display());
    let cursor = line.len();
    let result = comp.get_candidates(line, cursor).unwrap();
    let cs = contents(&result);
    assert!(cs.iter().any(|c| c.contains("apple.txt")), "got: {cs:?}");
    assert!(cs.iter().any(|c| c.contains("banana.txt")), "got: {cs:?}");
    assert!(cs.iter().any(|c| c.contains("cherry.txt")), "got: {cs:?}");
  }

  #[test]
  fn get_candidates_path_arg_with_prefix_filters() {
    let _g = TestGuard::new();
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::write(dir.path().join("apple.txt"), "").unwrap();
    std::fs::write(dir.path().join("apricot.txt"), "").unwrap();
    std::fs::write(dir.path().join("banana.txt"), "").unwrap();

    let mut comp = SimpleCompleter::default();
    // Prefix "ap" should match apple + apricot but not banana.
    let line = format!("ls {}/ap", dir.path().display());
    let cursor = line.len();
    let result = comp.get_candidates(line, cursor).unwrap();
    let cs = contents(&result);
    assert!(cs.iter().any(|c| c.contains("apple")), "got: {cs:?}");
    assert!(cs.iter().any(|c| c.contains("apricot")), "got: {cs:?}");
    assert!(!cs.iter().any(|c| c.contains("banana")), "got: {cs:?}");
  }

  #[test]
  fn get_candidates_redirect_uses_files_strategy() {
    let _g = TestGuard::new();
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::write(dir.path().join("output.log"), "").unwrap();

    let mut comp = SimpleCompleter::default();
    let line = format!("echo hi > {}/", dir.path().display());
    let cursor = line.len();
    let result = comp.get_candidates(line, cursor).unwrap();
    let cs = contents(&result);
    assert!(cs.iter().any(|c| c.contains("output.log")), "got: {cs:?}");
  }

  #[test]
  fn get_candidates_dirs_only_does_not_filter_argument_path() {
    // dirs_only is consulted only in the Files (redirect) strategy and
    // the Argument-NoSpec-NoMatch tail — NOT the default Argument path
    // that calls `complete_path`. This test pins that: even with
    // dirs_only=true, files surface through plain argument completion.
    let _g = TestGuard::new();
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::write(dir.path().join("file.txt"), "").unwrap();
    std::fs::create_dir(dir.path().join("subdir")).unwrap();

    let mut comp = SimpleCompleter {
      dirs_only: true,
      ..Default::default()
    };

    let line = format!("ls {}/", dir.path().display());
    let cursor = line.len();
    let result = comp.get_candidates(line, cursor).unwrap();
    let cs = contents(&result);
    assert!(cs.iter().any(|c| c.contains("file.txt")), "got: {cs:?}");
    assert!(cs.iter().any(|c| c.contains("subdir")), "got: {cs:?}");
  }

  #[test]
  fn get_candidates_token_span_set_for_var_prefix() {
    // get_candidates assigns token_span based on where the leaf starts.
    // Verify it lands on the $ position (well, just after) for a var.
    let _g = TestGuard::new();
    let mut comp = SimpleCompleter::default();
    let line = "echo $PA".to_string();
    let dollar_idx = line.find('$').unwrap();
    let _ = comp.get_candidates(line.clone(), line.len()).unwrap();
    // The span should cover the $ token; start should be at or just
    // after the $ depending on whether the leaf starts at $ or 'P'.
    assert!(
      comp.token_span.0 >= dollar_idx && comp.token_span.0 <= dollar_idx + 1,
      "expected token_span near $, got {:?}",
      comp.token_span
    );
  }

  #[test]
  fn get_candidates_dedups_and_sorts_many_results() {
    // The post-processing step sorts by length-then-alpha and dedups.
    // Easiest path: create files in a tempdir with predictable sort
    // order.
    let _g = TestGuard::new();
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::write(dir.path().join("a.txt"), "").unwrap();
    std::fs::write(dir.path().join("bbb.txt"), "").unwrap();
    std::fs::write(dir.path().join("c.txt"), "").unwrap();

    let mut comp = SimpleCompleter::default();
    let line = format!("ls {}/", dir.path().display());
    let cursor = line.len();
    let result = comp.get_candidates(line, cursor).unwrap();
    if let CompResult::Many { candidates } = result {
      // Sort key is (len, content), so the two shorter names come
      // before the longer one regardless of alphabetical order.
      let a_idx = candidates
        .iter()
        .position(|c| c.content().contains("a.txt"));
      let bbb_idx = candidates
        .iter()
        .position(|c| c.content().contains("bbb.txt"));
      if let (Some(a), Some(bbb)) = (a_idx, bbb_idx) {
        assert!(
          a < bbb,
          "shorter candidate should sort first; got order a@{a}, bbb@{bbb}"
        );
      }
    } else {
      panic!("expected Many with 3 files");
    }
  }

  // ===================== SimpleCompleter cycling =====================

  #[test]
  fn cycle_wraps_forward() {
    let _g = TestGuard::new();
    let mut comp = SimpleCompleter {
      candidates: vec!["aaa".into(), "bbb".into(), "ccc".into()],
      selected_idx: 2,
      original_input: "".into(),
      token_span: (0, 0),
      active: true,
      dirs_only: false,
      add_space: false,
      cursor_pos: 0,
    };
    comp.cycle_completion(1);
    assert_eq!(comp.selected_idx, 0);
  }

  #[test]
  fn cycle_wraps_backward() {
    let _g = TestGuard::new();
    let mut comp = SimpleCompleter {
      candidates: vec!["aaa".into(), "bbb".into(), "ccc".into()],
      selected_idx: 0,
      original_input: "".into(),
      token_span: (0, 0),
      active: true,
      dirs_only: false,
      add_space: false,
      cursor_pos: 0,
    };
    comp.cycle_completion(-1);
    assert_eq!(comp.selected_idx, 2);
  }

  // ===================== Completion escaping =====================

  #[test]
  fn escape_str_special_chars() {
    use crate::expand::escape_str;
    let escaped = escape_str("hello world", false);
    assert_eq!(escaped, "hello\\ world");
  }

  #[test]
  fn escape_str_multiple_specials() {
    use crate::expand::escape_str;
    let escaped = escape_str("a&b|c", false);
    assert_eq!(escaped, "a\\&b\\|c");
  }

  #[test]
  fn escape_str_no_specials() {
    use crate::expand::escape_str;
    let escaped = escape_str("hello", false);
    assert_eq!(escaped, "hello");
  }

  #[test]
  fn escape_str_all_shell_metacharacters() {
    use crate::expand::escape_str;
    for ch in [
      '\'', '"', '\\', '|', '&', ';', '(', ')', '<', '>', '$', '*', '!', '`', '{', '?', '[', '#',
      ' ', '\t', '\n',
    ] {
      let input = format!("a{ch}b");
      let escaped = escape_str(&input, false);
      let expected = format!("a\\{ch}b");
      assert_eq!(escaped, expected, "failed to escape {:?}", ch);
    }
  }

  #[test]
  fn escape_str_kitchen_sink() {
    use crate::expand::escape_str;
    let input = "f$le (with) 'spaces' & {braces} | pipes; #hash ~tilde `backtick` !bang";
    let escaped = escape_str(input, false);
    assert_eq!(
      escaped,
      "f\\$le\\ \\(with\\)\\ \\'spaces\\'\\ \\&\\ \\{braces}\\ \\|\\ pipes\\;\\ \\#hash\\ ~tilde\\ \\`backtick\\`\\ \\!bang"
    );
  }

  // `get_completed_line` now wholesale-replaces `token_span` with the
  // candidate. Escaping happens upstream in `complete_path` (the candidate
  // arrives splice-ready), so these tests verify the splice mechanics with
  // already-escaped candidates.

  #[test]
  fn completed_line_only_escapes_new_text() {
    let _g = TestGuard::new();
    // Candidate arrives pre-escaped from upstream: user-typed "hel" stays
    // verbatim, the matched suffix "lo world" was escaped to "lo\ world".
    let comp = SimpleCompleter {
      candidates: vec!["hello\\ world".into()],
      selected_idx: 0,
      original_input: "echo hel".into(),
      token_span: (5, 8),
      active: true,
      dirs_only: false,
      add_space: false,
      cursor_pos: 0,
    };
    let result = comp.get_completed_line();
    assert_eq!(result, "echo hello\\ world");
  }

  #[test]
  fn completed_line_no_new_text() {
    let _g = TestGuard::new();
    let comp = SimpleCompleter {
      candidates: vec!["hello".into()],
      selected_idx: 0,
      original_input: "echo hello".into(),
      token_span: (5, 10),
      active: true,
      dirs_only: false,
      add_space: false,
      cursor_pos: 0,
    };
    let result = comp.get_completed_line();
    assert_eq!(result, "echo hello");
  }

  #[test]
  fn completed_line_appends_suffix_with_escape() {
    let _g = TestGuard::new();
    // Wholesale replacement of `token_span` with the (pre-escaped) candidate.
    let comp = SimpleCompleter {
      candidates: vec!["hello\\ world".into()],
      selected_idx: 0,
      original_input: "echo hel".into(),
      token_span: (5, 8),
      active: true,
      dirs_only: false,
      add_space: false,
      cursor_pos: 0,
    };
    let result = comp.get_completed_line();
    assert_eq!(result, "echo hello\\ world");
  }

  #[test]
  fn completed_line_suffix_only_escapes_new_part() {
    let _g = TestGuard::new();
    // Candidate arrives with the user's "hello" preserved verbatim and the
    // appended " world&done" already escaped to "\ world\&done".
    let comp = SimpleCompleter {
      candidates: vec!["hello\\ world\\&done".into()],
      selected_idx: 0,
      original_input: "echo hello".into(),
      token_span: (5, 10),
      active: true,
      dirs_only: false,
      add_space: false,
      cursor_pos: 0,
    };
    let result = comp.get_completed_line();
    assert_eq!(result, "echo hello\\ world\\&done");
  }

  #[test]
  fn tab_escapes_special_in_filename() {
    let tmp = std::env::temp_dir().join("shed_test_tab_esc");
    let _ = std::fs::create_dir_all(&tmp);
    std::fs::write(tmp.join("hello world.txt"), "").unwrap();

    let (mut vi, _g) = test_vi("");
    std::env::set_current_dir(&tmp).unwrap();

    Shed::term_mut(|t| t.feed_bytes(b"echo hello\t"));
    let keys = Shed::term_mut(|t| t.drain_keys()).unwrap();
    let _ = vi.process_input(keys);

    let line = vi.editor.joined();
    assert!(
      line.contains("hello\\ world.txt"),
      "expected escaped space in completion: {line:?}"
    );

    std::fs::remove_dir_all(&tmp).ok();
  }

  #[test]
  fn tab_does_not_escape_user_text() {
    let tmp = std::env::temp_dir().join("shed_test_tab_noesc");
    let _ = std::fs::create_dir_all(&tmp);
    std::fs::write(tmp.join("my file.txt"), "").unwrap();

    let (mut vi, _g) = test_vi("");
    std::env::set_current_dir(&tmp).unwrap();

    // User types "echo my\ " with the space already escaped
    Shed::term_mut(|t| t.feed_bytes(b"echo my\\ \t"));
    let keys = Shed::term_mut(|t| t.drain_keys()).unwrap();
    let _ = vi.process_input(keys);

    let line = vi.editor.joined();
    // The user's "my\ " should be preserved, not double-escaped to "my\\\ "
    assert!(
      !line.contains("my\\\\ "),
      "user text should not be double-escaped: {line:?}"
    );
    assert!(
      line.contains("my\\ file.txt"),
      "expected completion with preserved user escape: {line:?}"
    );

    std::fs::remove_dir_all(&tmp).ok();
  }

  // ===================== CompStrat::resolve =====================

  /// Run the dispatcher against a literal source string and cursor position.
  /// Returns (strategy, replacement-span as a (start, end) tuple).
  fn dispatch(input: &str, cursor: usize) -> (CompStrat, (usize, usize)) {
    let tks = get_context_tokens(input);
    let (strat, span, _cursor_pos) = CompStrat::resolve(&tks, cursor);
    (strat, (span.range().start, span.range().end))
  }

  /// Helper: extract the prefix from a Var/Tilde/Command/Argument/Files/Dirs strat.
  fn prefix_of(strat: &CompStrat) -> &str {
    match strat {
      CompStrat::Var { prefix } | CompStrat::Tilde { prefix } | CompStrat::Command { prefix } => {
        prefix
      }
      CompStrat::Argument { path } | CompStrat::Files { path } => path,
      CompStrat::Separator | CompStrat::Null => "",
    }
  }

  #[test]
  fn dispatch_bare_var_sub() {
    let input = "echo $FL";
    let (strat, span) = dispatch(input, input.len());
    assert!(matches!(strat, CompStrat::Var { .. }), "got {strat:?}");
    assert_eq!(prefix_of(&strat), "FL");
    assert_eq!(&input[span.0..span.1], "FL");
  }

  #[test]
  fn dispatch_braced_var_sub_unclosed() {
    let input = "echo ${FL";
    let (strat, span) = dispatch(input, input.len());
    assert!(matches!(strat, CompStrat::Var { .. }), "got {strat:?}");
    assert_eq!(prefix_of(&strat), "FL");
    assert_eq!(&input[span.0..span.1], "FL");
  }

  #[test]
  fn dispatch_braced_var_sub_closed() {
    let input = "echo ${FL}";
    let cursor = input.find("FL").unwrap() + 2; // end of FL, just before `}`
    let (strat, span) = dispatch(input, cursor);
    assert!(matches!(strat, CompStrat::Var { .. }), "got {strat:?}");
    assert_eq!(prefix_of(&strat), "FL");
    assert_eq!(&input[span.0..span.1], "FL");
  }

  #[test]
  fn dispatch_braced_var_with_substitution_op() {
    let input = "echo ${FL/bar";
    let cursor = input.find("FL").unwrap() + 2; // end of FL
    let (strat, span) = dispatch(input, cursor);
    assert!(matches!(strat, CompStrat::Var { .. }), "got {strat:?}");
    assert_eq!(&input[span.0..span.1], "FL");
  }

  #[test]
  fn dispatch_var_sub_inside_path() {
    let input = "echo /foo/$FL/bar";
    let cursor = input.find("$FL").unwrap() + 3; // end of $FL
    let (strat, span) = dispatch(input, cursor);
    assert!(matches!(strat, CompStrat::Var { .. }), "got {strat:?}");
    assert_eq!(prefix_of(&strat), "FL");
    assert_eq!(&input[span.0..span.1], "FL");
  }

  #[test]
  fn dispatch_var_sub_inside_double_quoted_string() {
    let input = "echo \"foo $FL";
    let (strat, span) = dispatch(input, input.len());
    assert!(matches!(strat, CompStrat::Var { .. }), "got {strat:?}");
    assert_eq!(prefix_of(&strat), "FL");
    assert_eq!(&input[span.0..span.1], "FL");
  }

  #[test]
  fn dispatch_braced_var_inside_double_quoted_string() {
    let input = "echo \"foo ${FL}";
    let cursor = input.find("FL").unwrap() + 2; // end of FL
    let (strat, span) = dispatch(input, cursor);
    assert!(matches!(strat, CompStrat::Var { .. }), "got {strat:?}");
    assert_eq!(&input[span.0..span.1], "FL");
  }

  #[test]
  fn dispatch_empty_input_is_command() {
    let (strat, _) = dispatch("", 0);
    assert!(matches!(strat, CompStrat::Command { .. }), "got {strat:?}");
    assert_eq!(prefix_of(&strat), "");
  }

  #[test]
  fn dispatch_after_separator_is_command() {
    let input = "ls foo | ";
    let (strat, _) = dispatch(input, input.len());
    assert!(matches!(strat, CompStrat::Command { .. }), "got {strat:?}");
    assert_eq!(prefix_of(&strat), "");
  }

  #[test]
  fn dispatch_in_gap_after_command_uses_zero_width_span() {
    let input = "echo ";
    let (strat, span) = dispatch(input, input.len());
    assert!(matches!(strat, CompStrat::Argument { .. }), "got {strat:?}");
    assert_eq!(
      span,
      (input.len(), input.len()),
      "expected zero-width span at cursor, got {span:?}"
    );
  }

  #[test]
  fn dispatch_partial_command_name() {
    let input = "ls";
    let (strat, span) = dispatch(input, input.len());
    assert!(matches!(strat, CompStrat::Command { .. }), "got {strat:?}");
    assert_eq!(prefix_of(&strat), "ls");
    assert_eq!(&input[span.0..span.1], "ls");
  }

  #[test]
  fn dispatch_preserves_braces_under_string_recursion() {
    let input = "echo \"foo ${FL}/bar\"";
    let cursor = input.find("FL").unwrap() + 2;
    let (strat, span) = dispatch(input, cursor);
    assert!(matches!(strat, CompStrat::Var { .. }), "got {strat:?}");
    assert_eq!(&input[span.0..span.1], "FL");
  }

  // ===================== Integration tests (pty) =====================

  #[test]
  fn tab_completes_filename() {
    let tmp = std::env::temp_dir().join("shed_test_tab_fn");
    let _ = std::fs::create_dir_all(&tmp);
    std::fs::write(tmp.join("unique_shed_test_file.txt"), "").unwrap();

    let (mut vi, _g) = test_vi("");
    std::env::set_current_dir(&tmp).unwrap();

    // Type "echo unique_shed_test" then press Tab
    Shed::term_mut(|t| t.feed_bytes(b"echo unique_shed_test\t"));
    let keys = Shed::term_mut(|t| t.drain_keys()).unwrap();
    let _ = vi.process_input(keys);

    let line = vi.editor.joined();
    assert!(
      line.contains("unique_shed_test_file.txt"),
      "expected completion in line: {line:?}"
    );

    std::fs::remove_dir_all(&tmp).ok();
  }

  #[test]
  fn tab_completes_directory_with_slash() {
    let tmp = std::env::temp_dir().join("shed_test_tab_dir");
    let _ = std::fs::create_dir_all(tmp.join("mysubdir"));

    let (mut vi, _g) = test_vi("");
    std::env::set_current_dir(&tmp).unwrap();

    Shed::term_mut(|t| t.feed_bytes(b"cd mysub\t"));
    let keys = Shed::term_mut(|t| t.drain_keys()).unwrap();
    let _ = vi.process_input(keys);

    let line = vi.editor.joined();
    assert!(
      line.contains("mysubdir/"),
      "expected dir completion with trailing slash: {line:?}"
    );

    std::fs::remove_dir_all(&tmp).ok();
  }

  #[test]
  fn tab_after_equals() {
    let tmp = std::env::temp_dir().join("shed_test_tab_eq");
    let _ = std::fs::create_dir_all(&tmp);
    std::fs::write(tmp.join("eqfile.txt"), "").unwrap();

    let (mut vi, _g) = test_vi("");
    std::env::set_current_dir(&tmp).unwrap();

    Shed::term_mut(|t| t.feed_bytes(b"cmd --opt=eqf\t"));
    let keys = Shed::term_mut(|t| t.drain_keys()).unwrap();
    let _ = vi.process_input(keys);

    let line = vi.editor.joined();
    assert!(
      line.contains("--opt=eqfile.txt"),
      "expected completion after '=': {line:?}"
    );

    std::fs::remove_dir_all(&tmp).ok();
  }

  // ===================== get_branch / Null walk-up =====================

  #[test]
  fn dispatch_walks_up_from_escape_leaf() {
    // Cursor inside an Escape token (`\ `) should not produce Null —
    // the dispatcher should walk up to the parent Argument.
    let input = "echo my\\ ";
    let (strat, _span) = dispatch(input, input.len());
    assert!(
      !matches!(strat, CompStrat::Null),
      "Escape leaf should walk up to parent, got Null"
    );
  }

  #[test]
  fn dispatch_branch_chain_deep_nesting() {
    // Cursor inside the deeply nested `~/file` path through subshell→arg→
    // varsub→paramindex→cmdsub→arg→varsub→paramindex→cmdsub→argfile.
    // Just verify the branch chain resolves without panic and reaches a
    // non-Null strat.
    let input = "(echo foo ${bar[$(echo ${foo[$(cat ~/fil)]}) + 1]})";
    let cursor = input.find("~/fil").unwrap() + 3;
    let (strat, _span) = dispatch(input, cursor);
    assert!(
      !matches!(strat, CompStrat::Null),
      "deeply nested cursor should resolve, got {strat:?}"
    );
  }

  #[test]
  fn dispatch_argument_carries_full_path() {
    // CompStrat::Argument carries `path` (full token), not `prefix`. With
    // cursor in the middle, the strat must contain everything (so postfix
    // is preserved when completing).
    let input = "cd /tmp/foo/bar/baz";
    let cursor = input.find("foo").unwrap() + 2; // after 'fo', mid-token
    let (strat, _span) = dispatch(input, cursor);
    let p = prefix_of(&strat);
    assert!(
      p.contains("/bar/baz"),
      "Argument strat should contain full token incl. postfix; got {p:?}"
    );
  }

  // ===================== comp function arg quoting =====================
  //
  // exec_comp_func builds the function-call input as
  //   `{fn_name} {as_var_val_display(cmd)} {as_var_val_display(cword)} {as_var_val_display(pword)}`
  // The comp function's $1/$2/$3 must receive the original strings even when
  // they contain spaces, quotes, $, ;, etc. These tests exercise the same
  // formatting path that exec_comp_func uses.

  use crate::expand::as_var_val_display;
  use crate::tests::testutil::test_input;

  fn run_comp_func_with_args(cmd: &str, cword: &str, pword: &str) -> (String, String, String) {
    test_input("_capture() { CAP1=\"$1\"; CAP2=\"$2\"; CAP3=\"$3\"; }").unwrap();
    let input = format!(
      "_capture {} {} {}",
      as_var_val_display(cmd),
      as_var_val_display(cword),
      as_var_val_display(pword),
    );
    test_input(input).unwrap();
    (var!("CAP1"), var!("CAP2"), var!("CAP3"))
  }

  #[test]
  fn comp_args_plain_strings() {
    let _g = TestGuard::new();
    let (a, b, c) = run_comp_func_with_args("git", "checkout", "master");
    assert_eq!(a, "git");
    assert_eq!(b, "checkout");
    assert_eq!(c, "master");
  }

  #[test]
  fn comp_args_with_spaces() {
    let _g = TestGuard::new();
    let (a, b, c) = run_comp_func_with_args("my cmd", "foo bar", "baz qux");
    assert_eq!(a, "my cmd");
    assert_eq!(b, "foo bar");
    assert_eq!(c, "baz qux");
  }

  #[test]
  fn comp_args_with_dollar_sign() {
    let _g = TestGuard::new();
    let (a, b, _) = run_comp_func_with_args("$VAR", "$cmd", "");
    assert_eq!(a, "$VAR");
    assert_eq!(b, "$cmd");
  }

  #[test]
  fn comp_args_with_semicolon_and_pipe() {
    let _g = TestGuard::new();
    let (a, b, _) = run_comp_func_with_args("a;b", "x|y", "");
    assert_eq!(a, "a;b");
    assert_eq!(b, "x|y");
  }

  #[test]
  fn comp_args_with_single_quote() {
    let _g = TestGuard::new();
    let (a, _b, _c) = run_comp_func_with_args("it's", "", "");
    assert_eq!(a, "it's");
  }

  // ===================== FuzzySelector::handle_key =====================

  mod fuzzy_selector_handle_key {
    use super::*;
    use crate::keys::ModKeys as M;

    fn sel_with(items: &[&str]) -> FuzzySelector {
      let mut sel = FuzzySelector::new("test");
      let cands: Vec<Candidate> = items
        .iter()
        .map(|s| Candidate::from(s.to_string()))
        .collect();
      sel.activate(cands);
      sel
    }

    // ─── Enter ────────────────────────────────────────────────────────

    #[test]
    fn enter_with_empty_filtered_dismisses() {
      let _g = TestGuard::new();
      let mut sel = FuzzySelector::new("test");
      let resp = sel.handle_key(key!(Enter)).unwrap();
      assert!(matches!(resp, SelectorResponse::Dismiss));
    }

    #[test]
    fn enter_accepts_candidate_at_cursor_zero() {
      let _g = TestGuard::new();
      let mut sel = sel_with(&["alpha", "beta", "gamma"]);
      let expected = sel.filtered()[0].candidate.clone();
      let resp = sel.handle_key(key!(Enter)).unwrap();
      match resp {
        SelectorResponse::Accept(c) => assert_eq!(c, expected),
        _ => panic!("expected Accept"),
      }
    }

    #[test]
    fn enter_after_navigation_accepts_correct_candidate() {
      let _g = TestGuard::new();
      let mut sel = sel_with(&["alpha", "beta", "gamma"]);
      sel.handle_key(key!(Down)).unwrap();
      sel.handle_key(key!(Down)).unwrap();
      let expected = sel.filtered()[2].candidate.clone();
      let resp = sel.handle_key(key!(Enter)).unwrap();
      match resp {
        SelectorResponse::Accept(c) => assert_eq!(c, expected),
        _ => panic!("expected Accept"),
      }
    }

    // ─── Dismiss keys ─────────────────────────────────────────────────

    #[test]
    fn esc_dismisses_and_clears_filtered() {
      let _g = TestGuard::new();
      let mut sel = sel_with(&["a", "b", "c"]);
      assert_eq!(sel.filtered().len(), 3);
      let resp = sel.handle_key(key!(Esc)).unwrap();
      assert!(matches!(resp, SelectorResponse::Dismiss));
      assert!(sel.filtered().is_empty());
    }

    #[test]
    fn ctrl_d_dismisses_and_clears_filtered() {
      let _g = TestGuard::new();
      let mut sel = sel_with(&["a", "b", "c"]);
      let resp = sel.handle_key(key!(Ctrl + 'd')).unwrap();
      assert!(matches!(resp, SelectorResponse::Dismiss));
      assert!(sel.filtered().is_empty());
    }

    // ─── Cursor movement: forward ─────────────────────────────────────

    #[test]
    fn tab_advances_cursor() {
      let _g = TestGuard::new();
      let mut sel = sel_with(&["a", "b", "c"]);
      let target = sel.filtered()[1].candidate.clone();
      sel.handle_key(key!(Tab)).unwrap();
      assert_eq!(sel.selected_candidate().unwrap(), target);
    }

    #[test]
    fn down_advances_cursor() {
      let _g = TestGuard::new();
      let mut sel = sel_with(&["a", "b", "c"]);
      let target = sel.filtered()[1].candidate.clone();
      sel.handle_key(key!(Down)).unwrap();
      assert_eq!(sel.selected_candidate().unwrap(), target);
    }

    #[test]
    fn tab_wraps_at_end() {
      let _g = TestGuard::new();
      let mut sel = sel_with(&["a", "b", "c"]);
      let first = sel.filtered()[0].candidate.clone();
      for _ in 0..3 {
        sel.handle_key(key!(Tab)).unwrap();
      }
      assert_eq!(sel.selected_candidate().unwrap(), first);
    }

    #[test]
    fn down_wraps_at_end() {
      let _g = TestGuard::new();
      let mut sel = sel_with(&["a", "b"]);
      let first = sel.filtered()[0].candidate.clone();
      sel.handle_key(key!(Down)).unwrap();
      sel.handle_key(key!(Down)).unwrap();
      assert_eq!(sel.selected_candidate().unwrap(), first);
    }

    #[test]
    fn scroll_down_does_not_wrap() {
      let _g = TestGuard::new();
      let mut sel = sel_with(&["a", "b"]);
      let last = sel.filtered()[1].candidate.clone();
      // ScrollDown 3x: 0→1→1→1 (saturates at max-1).
      sel.handle_key(K(C::ScrollDown, M::NONE)).unwrap();
      sel.handle_key(K(C::ScrollDown, M::NONE)).unwrap();
      sel.handle_key(K(C::ScrollDown, M::NONE)).unwrap();
      assert_eq!(sel.selected_candidate().unwrap(), last);
    }

    // ─── Cursor movement: backward ────────────────────────────────────

    #[test]
    fn shift_tab_retreats_cursor() {
      let _g = TestGuard::new();
      let mut sel = sel_with(&["a", "b", "c"]);
      let first = sel.filtered()[0].candidate.clone();
      sel.handle_key(key!(Down)).unwrap();
      sel.handle_key(key!(Shift + Tab)).unwrap();
      assert_eq!(sel.selected_candidate().unwrap(), first);
    }

    #[test]
    fn shift_tab_wraps_at_top() {
      let _g = TestGuard::new();
      let mut sel = sel_with(&["a", "b", "c"]);
      let last = sel.filtered()[2].candidate.clone();
      sel.handle_key(key!(Shift + Tab)).unwrap();
      assert_eq!(sel.selected_candidate().unwrap(), last);
    }

    #[test]
    fn up_wraps_at_top() {
      let _g = TestGuard::new();
      let mut sel = sel_with(&["a", "b"]);
      let last = sel.filtered()[1].candidate.clone();
      sel.handle_key(key!(Up)).unwrap();
      assert_eq!(sel.selected_candidate().unwrap(), last);
    }

    #[test]
    fn scroll_up_does_not_wrap() {
      let _g = TestGuard::new();
      let mut sel = sel_with(&["a", "b"]);
      let first = sel.filtered()[0].candidate.clone();
      // ScrollUp at cursor=0 should stay at 0 (saturating).
      sel.handle_key(K(C::ScrollUp, M::NONE)).unwrap();
      sel.handle_key(K(C::ScrollUp, M::NONE)).unwrap();
      assert_eq!(sel.selected_candidate().unwrap(), first);
    }

    // ─── Response types ───────────────────────────────────────────────

    #[test]
    fn all_movement_keys_return_consumed() {
      let _g = TestGuard::new();
      let mut sel = sel_with(&["a", "b", "c"]);
      for key in [
        key!(Down),
        key!(Up),
        key!(Tab),
        key!(Shift + Tab),
        K(C::ScrollDown, M::NONE),
        K(C::ScrollUp, M::NONE),
      ] {
        let resp = sel.handle_key(key).unwrap();
        assert!(matches!(resp, SelectorResponse::Consumed));
      }
    }

    // ─── Query editing ────────────────────────────────────────────────

    #[test]
    fn typing_char_filters_candidates() {
      let _g = TestGuard::new();
      let mut sel = sel_with(&["alpha", "beta", "gamma"]);
      assert_eq!(sel.filtered().len(), 3);
      let resp = sel.handle_key(K(C::Char('g'), M::NONE)).unwrap();
      assert!(matches!(resp, SelectorResponse::Consumed));
      // 'g' only fuzzy-matches "gamma".
      assert_eq!(sel.filtered().len(), 1);
      assert_eq!(sel.filtered()[0].candidate.content(), "gamma");
    }

    #[test]
    fn typing_no_match_empties_filtered() {
      let _g = TestGuard::new();
      let mut sel = sel_with(&["alpha", "beta"]);
      sel.handle_key(K(C::Char('z'), M::NONE)).unwrap();
      assert!(sel.filtered().is_empty());
    }

    #[test]
    fn typing_then_enter_accepts_filtered_match() {
      let _g = TestGuard::new();
      let mut sel = sel_with(&["alpha", "beta", "gamma"]);
      sel.handle_key(K(C::Char('g'), M::NONE)).unwrap();
      let resp = sel.handle_key(key!(Enter)).unwrap();
      match resp {
        SelectorResponse::Accept(c) => assert_eq!(c.content(), "gamma"),
        _ => panic!("expected Accept"),
      }
    }

    #[test]
    fn typing_then_no_match_then_enter_dismisses() {
      let _g = TestGuard::new();
      let mut sel = sel_with(&["alpha", "beta"]);
      sel.handle_key(K(C::Char('z'), M::NONE)).unwrap();
      let resp = sel.handle_key(key!(Enter)).unwrap();
      assert!(matches!(resp, SelectorResponse::Dismiss));
    }

    #[test]
    fn ctrl_c_clears_query_restores_full_list() {
      let _g = TestGuard::new();
      let mut sel = sel_with(&["alpha", "beta", "gamma"]);
      sel.handle_key(K(C::Char('g'), M::NONE)).unwrap();
      assert_eq!(sel.filtered().len(), 1);
      let resp = sel.handle_key(key!(Ctrl + 'c')).unwrap();
      assert!(matches!(resp, SelectorResponse::Consumed));
      assert_eq!(sel.filtered().len(), 3);
    }

    // ─── Empty filtered: movement is a no-op ─────────────────────────

    #[test]
    fn navigation_on_empty_selector_does_not_panic() {
      let _g = TestGuard::new();
      let mut sel = FuzzySelector::new("test");
      for key in [
        key!(Down),
        key!(Up),
        key!(Tab),
        key!(Shift + Tab),
        K(C::ScrollDown, M::NONE),
        K(C::ScrollUp, M::NONE),
      ] {
        let resp = sel.handle_key(key).unwrap();
        assert!(matches!(resp, SelectorResponse::Consumed));
      }
      assert!(sel.selected_candidate().is_none());
    }

    // ─── Mouse handling ──────────────────────────────────────────────

    #[test]
    fn mouse_pos_returns_consumed() {
      let _g = TestGuard::new();
      let mut sel = sel_with(&["a", "b", "c"]);
      let resp = sel.handle_key(K(C::MousePos(0, 0), M::NONE)).unwrap();
      assert!(matches!(resp, SelectorResponse::Consumed));
    }

    #[test]
    fn left_click_out_of_range_returns_consumed_no_change() {
      let _g = TestGuard::new();
      let mut sel = sel_with(&["a", "b", "c"]);
      let before = sel.selected_candidate().unwrap();
      // row_map is empty without draw(); click row is out-of-range → no-op.
      let resp = sel.handle_key(K(C::LeftClick(99, 0), M::NONE)).unwrap();
      assert!(matches!(resp, SelectorResponse::Consumed));
      assert_eq!(sel.selected_candidate().unwrap(), before);
    }
  }

  // ===================== complete_jobs =====================

  mod complete_jobs_tests {
    use super::*;
    use crate::state::jobs::{ChildProc, JobBldr, JobID};
    use nix::sys::wait::WaitStatus;
    use nix::unistd::Pid;

    fn drain_jobs() {
      Shed::jobs_mut(|j| {
        let ids: Vec<JobID> = j
          .jobs()
          .iter()
          .flatten()
          .filter_map(|job| job.tabid().map(JobID::TableID))
          .collect();
        for id in ids {
          j.remove_job(id);
        }
      });
    }

    fn insert_named_job(pid: i32, cmd: &str) {
      let pid = Pid::from_raw(pid);
      let mut child = ChildProc::new(pid, Some(cmd), Some(pid), false).unwrap();
      child.set_stat(WaitStatus::StillAlive);
      let mut bldr = JobBldr::new();
      bldr.push_child(child);
      bldr.set_pgid(pid);
      let job = bldr.build();
      Shed::jobs_mut(|j| j.insert_job(job, true)).unwrap();
    }

    #[test]
    fn complete_jobs_empty_table_returns_empty() {
      let _g = TestGuard::new();
      drain_jobs();
      assert!(complete_jobs("").is_empty());
      assert!(complete_jobs("%").is_empty());
    }

    #[test]
    fn complete_jobs_no_percent_returns_pgid_candidates() {
      let _g = TestGuard::new();
      drain_jobs();
      insert_named_job(70001, "uniq_test_cmd_a");
      let out = complete_jobs("");
      assert!(!out.is_empty(), "got: {out:?}");
      // Bare-form candidates are pgid strings; "70001" should appear.
      assert!(out.iter().any(|c| c.content() == "70001"), "got: {out:?}");
    }

    #[test]
    fn complete_jobs_with_percent_prefix_returns_named_candidates() {
      let _g = TestGuard::new();
      drain_jobs();
      insert_named_job(70002, "uniq_named_job_xyz");
      let out = complete_jobs("%");
      // %-prefix means we want jobs *by name*. The candidate's
      // content should be prefixed with `%`.
      assert!(
        out.iter().any(|c| c.content().starts_with('%')),
        "got: {out:?}"
      );
    }
  }

  // ===================== complete_users =====================

  mod complete_users_tests {
    use super::*;

    #[test]
    fn complete_users_finds_root_on_unix() {
      // /etc/passwd reliably contains `root` on any Linux/CI machine.
      // If /etc/passwd is missing entirely, the function returns
      // empty — skip the assertion in that case.
      let _g = TestGuard::new();
      if !std::path::Path::new("/etc/passwd").exists() {
        return;
      }
      let out = complete_users("ro");
      assert!(
        out.iter().any(|c| c.content() == "root"),
        "expected 'root' in candidates, got: {out:?}"
      );
    }

    #[test]
    fn complete_users_filters_by_prefix() {
      let _g = TestGuard::new();
      if !std::path::Path::new("/etc/passwd").exists() {
        return;
      }
      // A nonsense prefix should match nobody.
      let out = complete_users("zzz_no_such_user_prefix_qqq");
      assert!(out.is_empty(), "got: {out:?}");
    }
  }

  // ===================== complete_builtins =====================

  mod complete_builtins_tests {
    use super::*;

    #[test]
    fn complete_builtins_known_prefix_matches() {
      let _g = TestGuard::new();
      let out = complete_builtins("ec"); // echo
      assert!(
        out.iter().any(|c| c.content() == "echo"),
        "expected 'echo' in candidates, got: {out:?}"
      );
    }

    #[test]
    fn complete_builtins_empty_prefix_returns_all() {
      let _g = TestGuard::new();
      let all = complete_builtins("");
      assert_eq!(all.len(), BUILTIN_NAMES.len());
    }

    #[test]
    fn complete_builtins_unknown_prefix_returns_empty() {
      let _g = TestGuard::new();
      assert!(complete_builtins("zzz_no_such_builtin_qqq").is_empty());
    }
  }

  // ===================== complete_commands './' prefix =====================

  mod complete_commands_dotslash_tests {
    use super::*;

    #[test]
    fn dotslash_prefix_routes_through_file_branch() {
      // The `./` prefix takes a separate branch in complete_commands
      // that filters cached_utils to UtilKind::File and re-prepends
      // "./" to the resulting candidates. We can't deterministically
      // populate the file cache from a unit test, so we just verify
      // the function returns successfully on a `./` prefix and that
      // any results that come back start with `./`.
      let _g = TestGuard::new();
      let out = complete_commands("./", 2);
      for c in &out {
        assert!(c.content.starts_with("./"), "got: {c:?}");
      }
    }
  }

  // ===================== complete_path ignore_case branch =====================

  mod complete_path_ignore_case_tests {
    use super::*;

    #[test]
    fn ignore_case_flag_makes_match_case_insensitive() {
      // Build a tempdir with a Mixed-Case filename, then complete with
      // a lowercase prefix. Without ignore_case → no match; with it →
      // match. We just toggle the flag and confirm the on-case path
      // actually returns something.
      let _g = TestGuard::new();
      let dir = tempfile::TempDir::new().unwrap();
      let file_path = dir.path().join("MixedCaseFile.txt");
      std::fs::write(&file_path, "x").unwrap();

      let prev_cwd = std::env::current_dir().unwrap();
      std::env::set_current_dir(dir.path()).unwrap();

      // Off: case-sensitive — "mixed" shouldn't match "MixedCaseFile".
      crate::shopt_mut!(prompt.completion_ignore_case = false);
      let strict = complete_path("mixed", 5);

      // On: case-insensitive — "mixed" should match "MixedCaseFile".
      crate::shopt_mut!(prompt.completion_ignore_case = true);
      let relaxed = complete_path("mixed", 5);

      std::env::set_current_dir(prev_cwd).unwrap();
      crate::shopt_mut!(prompt.completion_ignore_case = false);

      assert!(
        strict
          .iter()
          .all(|c| !c.content.ends_with("MixedCaseFile.txt")),
        "case-sensitive shouldn't match, got: {strict:?}"
      );
      assert!(
        relaxed
          .iter()
          .any(|c| c.content.ends_with("MixedCaseFile.txt")),
        "ignore-case should match, got: {relaxed:?}"
      );
    }
  }

  // ===================== BashCompSpec builders =====================

  mod bash_comp_spec_tests {
    use super::*;

    #[test]
    fn every_builder_method_sets_its_field() {
      // Cover all the with_*/enable-style methods on BashCompSpec in
      // one shot. The methods are pure setters, so we just confirm
      // each one flips the corresponding field.
      let spec = BashCompSpec::new()
        .with_func("complete_foo".into())
        .with_wordlist(vec!["a".into(), "b".into()])
        .with_source("complete -F complete_foo cmd".into())
        .files(true)
        .dirs(true)
        .commands(true)
        .builtins(true)
        .users(true)
        .vars(true)
        .signals(true)
        .jobs(true)
        .aliases(true);

      assert_eq!(spec.function, Some("complete_foo".to_string()));
      assert_eq!(spec.wordlist, Some(vec!["a".into(), "b".into()]));
      assert_eq!(spec.source, "complete -F complete_foo cmd");
      assert!(spec.files);
      assert!(spec.dirs);
      assert!(spec.commands);
      assert!(spec.builtins);
      assert!(spec.users);
      assert!(spec.vars);
      assert!(spec.signals);
      assert!(spec.jobs);
      assert!(spec.aliases);
    }

    #[test]
    fn builder_methods_can_disable_via_false() {
      // The enable parameter on each setter is a real boolean, not a
      // marker — passing false should leave (or clear) the field.
      let spec = BashCompSpec::new()
        .files(true)
        .files(false)
        .dirs(true)
        .dirs(false);
      assert!(!spec.files);
      assert!(!spec.dirs);
    }
  }
}
