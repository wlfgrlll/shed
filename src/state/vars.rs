use super::scopes::ScopeStack;

use std::{
  collections::{HashMap, VecDeque},
  fmt::{self, Display},
  path::PathBuf,
  str::FromStr,
};

use bitflags::bitflags;
use nix::unistd::{Pid, User, gethostname, getppid, isatty};

use super::{
  ShErr, ShResult,
  eval::lex::{LexFlags, LexStream, Tk},
  expand::{as_var_val_display, expand_arithmetic, expand_raw, markers},
  procio::stdin_fileno,
  readline::Candidate,
  sherr,
  util::get_separator,
};

/// a `std::sync::Once` that makes sure we only call VarTab::init_env() once.
static ENV_INIT: std::sync::Once = std::sync::Once::new();

/// Display key/value pairs as '{key}={value}\n'
///
/// The 'value' is escaped in such a way that the whole line can be reused as a shell assignment
pub(crate) fn display_as_vars(
  vars: impl Iterator<Item = (impl ToString, impl ToString)>,
) -> String {
  let mut vars = vars
    .map(|(k, v)| display_as_var(k, v))
    .collect::<Vec<String>>();
  vars.sort();
  vars.join("\n")
}

pub(crate) fn display_as_var(name: impl ToString, value: impl ToString) -> String {
  format!(
    "{}={}",
    name.to_string(),
    as_var_val_display(&value.to_string())
  )
}

pub(crate) fn display_env_vars() -> String {
  display_as_vars(std::env::vars())
}

fn display_vars_internal(vars: &ScopeStack, filter: Option<VarFlags>) -> String {
  let vars = vars.flatten_vars().into_iter();

  if let Some(flags) = filter {
    display_as_vars(vars.filter(|(_, v)| v.flags().contains(flags)))
  } else {
    display_as_vars(vars)
  }
}

pub(crate) fn display_readonly(vars: &ScopeStack) -> String {
  display_vars_internal(vars, Some(VarFlags::READONLY))
}

pub(crate) fn display_local(vars: &ScopeStack) -> String {
  display_vars_internal(vars, None)
}

#[derive(Hash, Eq, PartialEq, Debug, Clone, Copy)]
pub(crate) enum ShellParam {
  // Global
  Status,
  ShPid,
  LastJob,
  ShellName,

  // Local
  Pos(usize),
  AllArgs,
  AllArgsStr,
  ArgCount,
}

impl ShellParam {
  pub fn is_global(&self) -> bool {
    matches!(
      self,
      Self::Status | Self::ShPid | Self::LastJob | Self::ShellName
    )
  }

  pub fn from_char(c: &char) -> Option<Self> {
    match c {
      '?' => Some(Self::Status),
      '$' => Some(Self::ShPid),
      '!' => Some(Self::LastJob),
      '0' => Some(Self::ShellName),
      '@' => Some(Self::AllArgs),
      '*' => Some(Self::AllArgsStr),
      '#' => Some(Self::ArgCount),
      _ => None,
    }
  }
}

impl Display for ShellParam {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::Status => write!(f, "?"),
      Self::ShPid => write!(f, "$"),
      Self::LastJob => write!(f, "!"),
      Self::ShellName => write!(f, "0"),
      Self::Pos(n) => write!(f, "{}", n),
      Self::AllArgs => write!(f, "@"),
      Self::AllArgsStr => write!(f, "*"),
      Self::ArgCount => write!(f, "#"),
    }
  }
}

impl FromStr for ShellParam {
  type Err = ShErr;
  fn from_str(s: &str) -> Result<Self, Self::Err> {
    match s {
      "?" => Ok(Self::Status),
      "$" => Ok(Self::ShPid),
      "!" => Ok(Self::LastJob),
      "0" => Ok(Self::ShellName),
      "@" => Ok(Self::AllArgs),
      "*" => Ok(Self::AllArgsStr),
      "#" => Ok(Self::ArgCount),
      n if n.parse::<usize>().is_ok() => {
        let idx = n.parse::<usize>().unwrap();
        Ok(Self::Pos(idx))
      }
      _ => Err(sherr!(InternalErr, "Invalid shell parameter: {}", s,)),
    }
  }
}

bitflags! {
  #[derive(Clone, Default, Copy, Debug, Eq, PartialEq, PartialOrd, Ord)]
  pub struct VarFlags: u32 {
    const EXPORT = 1 << 0;
    const LOCAL = 1 << 1;
    const READONLY = 1 << 2;
  }
}

#[derive(Clone, Debug)]
pub(crate) enum ArrIndex {
  Literal(usize),
  FromBack(usize),
  ArgCount,
  AllJoined,
  AllSplit,
  Key(String),

  /// Unresolved index, parsed depending on whether we are targeting an
  /// indexed array or an associative array
  Raw(String),
}

impl ArrIndex {
  /// Parse an array index expression.
  ///
  /// the allow_side_effects parameter controls whether or not mutating parameter
  /// expansions and command substitutions will be evaluated.
  pub fn parse(s: &str, allow_side_effects: bool) -> ShResult<Self> {
    let s = crate::expand::expand_raw_inner(&mut s.chars().peekable(), allow_side_effects)?;
    match s.as_str() {
      "@" => Ok(Self::AllSplit),
      "*" => Ok(Self::AllJoined),
      "#" => Ok(Self::ArgCount),
      _ if s.starts_with('-')
        && !s[1..].is_empty()
        && s[1..].chars().all(|c| c.is_ascii_digit()) =>
      {
        let idx = s[1..].parse::<usize>().unwrap();
        Ok(Self::FromBack(idx))
      }
      _ if !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()) => {
        let idx = s.parse::<usize>().unwrap();
        Ok(Self::Literal(idx))
      }
      // Anything else — variable references, arithmetic expressions,
      // string keys — gets deferred to `resolve_for`. Whether it's
      // arithmetic-evaluated or used as a literal key depends on
      // whether the target is indexed or associative.
      _ => Ok(Self::Raw(s)),
    }
  }
}

impl ArrIndex {
  pub fn resolve_for(self, kind: &VarKind) -> ShResult<Self> {
    match self {
      Self::Raw(s) => match kind {
        VarKind::Arr(_) | VarKind::Str(_) | VarKind::Int(_) => {
          let evaluated = expand_arithmetic(&s)?;
          let n: usize = evaluated
            .parse()
            .map_err(|_| sherr!(ParseErr, "Invalid array index '{s}': not a number"))?;
          Ok(Self::Literal(n))
        }
        VarKind::AssocArr(_) => Ok(Self::Key(s)),
      },
      Self::Literal(n) if matches!(kind, VarKind::AssocArr(_)) => Ok(Self::Key(n.to_string())),
      _ => Ok(self),
    }
  }
}

/// Find the first `:` that isn't nested inside `${}`, `$()`, or `(())`
fn top_level_colon(s: &str) -> Option<usize> {
  let mut brace_depth = 0;
  let mut paren_depth = 0;
  for (i, ch) in s.char_indices() {
    match ch {
      '{' => brace_depth += 1,
      '}' => brace_depth -= 1,
      '(' => paren_depth += 1,
      ')' => paren_depth -= 1,
      ':' if brace_depth == 0 && paren_depth == 0 => return Some(i),
      _ => {}
    }
  }
  None
}

/// A parsed variable name, optionally with an array index and slice.
/// Index expansion happens at construction time, so it's safe
/// to use inside `Shed::vars`/`write_vars` closures without
/// causing re-entrant borrows.
#[derive(Clone, Debug)]
pub(crate) struct VarName {
  name: String,
  index: Option<ArrIndex>,
  slice_start: Option<usize>,
  slice_len: Option<usize>,
}

impl VarName {
  pub fn parse(raw: &str, allow_side_effects: bool) -> ShResult<Self> {
    let Some(bracket_start) = raw.find('[') else {
      return Ok(Self {
        name: raw.to_string(),
        index: None,
        slice_start: None,
        slice_len: None,
      });
    };

    // Find the matching ']' by tracking depth, since the index
    // content may contain nested brackets (e.g. ${arr[${i[0]}]})
    let mut depth = 0;
    let mut bracket_end = None;
    for (i, ch) in raw[bracket_start..].char_indices() {
      match ch {
        '[' => depth += 1,
        ']' => {
          depth -= 1;
          if depth == 0 {
            bracket_end = Some(bracket_start + i);
            break;
          }
        }
        _ => {}
      }
    }

    let Some(bracket_end) = bracket_end else {
      return Ok(Self {
        name: raw.to_string(),
        index: None,
        slice_start: None,
        slice_len: None,
      });
    };

    let name = raw[..bracket_start].to_string();
    let idx_str = &raw[bracket_start + 1..bracket_end];
    let index = ArrIndex::parse(idx_str, allow_side_effects)?;

    // Array slicing only applies to [@] and [*] indexes
    let (slice_start, slice_len) = if matches!(index, ArrIndex::AllSplit | ArrIndex::AllJoined) {
      let after_bracket = &raw[bracket_end + 1..];
      if let Some(rest) = after_bracket.strip_prefix(':') {
        // Split on ':' at the top level only (not inside ${} or $())
        if let Some(split_pos) = top_level_colon(rest) {
          let s = &rest[..split_pos];
          let l = &rest[split_pos + 1..];
          let s_exp = expand_raw(&mut s.chars().peekable()).unwrap_or_else(|_| s.to_string());
          let l_exp = expand_raw(&mut l.chars().peekable()).unwrap_or_else(|_| l.to_string());
          (s_exp.parse::<usize>().ok(), l_exp.parse::<usize>().ok())
        } else {
          let expanded =
            expand_raw(&mut rest.chars().peekable()).unwrap_or_else(|_| rest.to_string());
          (expanded.parse::<usize>().ok(), None)
        }
      } else {
        (None, None)
      }
    } else {
      (None, None)
    };

    Ok(Self {
      name,
      index: Some(index),
      slice_start,
      slice_len,
    })
  }

  pub fn name(&self) -> &str {
    &self.name
  }
  pub fn index(&self) -> Option<&ArrIndex> {
    self.index.as_ref()
  }
  /// Replace the parsed index. Used to substitute a pre-resolved index
  /// (e.g. one whose `expand_arithmetic` was performed outside a borrow
  /// to avoid forking under a held RefCell guard).
  pub fn set_index(&mut self, idx: ArrIndex) {
    self.index = Some(idx);
  }
  pub fn slice_start(&self) -> Option<usize> {
    self.slice_start
  }
  pub fn slice_len(&self) -> Option<usize> {
    self.slice_len
  }
}

#[derive(Clone, Debug)]
pub(crate) enum VarKind {
  Str(String),
  Int(i32),
  Arr(VecDeque<String>),
  AssocArr(Vec<(String, String)>),
}

impl Default for VarKind {
  fn default() -> Self {
    Self::Str(String::new())
  }
}

impl VarKind {
  pub fn arr_from_tk(tk: Tk) -> ShResult<Self> {
    let raw = tk.as_str();
    Self::arr_from_raw(raw)
  }

  pub fn arr_from_raw(raw: &str) -> ShResult<Self> {
    if !raw.starts_with('(') || !raw.ends_with(')') {
      return Err(sherr!(ParseErr, "Invalid array syntax: {}", raw,));
    }
    let raw = raw[1..raw.len() - 1].to_string();

    let tokens: VecDeque<String> = LexStream::new(raw.into(), LexFlags::empty())
      .map(|tk| tk.and_then(|tk| tk.expand()).map(|tk| tk.get_words()))
      .try_fold(String::new(), |mut acc, wrds| {
        match wrds {
          Ok(wrds) => {
            for wrd in wrds {
              if !acc.is_empty() {
                acc.push(markers::ARG_SEP);
              }
              acc.push_str(&wrd);
            }
          }
          Err(e) => return Err(e),
        }
        Ok(acc)
      })?
      .split(markers::ARG_SEP)
      .filter(|s| !s.is_empty())
      .map(|s| s.to_string())
      .collect();

    Ok(Self::Arr(tokens))
  }

  pub fn parse(raw: &str) -> Self {
    Self::arr_from_raw(raw).unwrap_or_else(|_| Self::Str(raw.to_string()))
  }

  pub fn arr_from_vec(vec: Vec<String>) -> Self {
    Self::Arr(VecDeque::from(vec))
  }

  pub fn assoc_arr_from_raw(raw: &str) -> ShResult<Self> {
    if !raw.starts_with('(') || !raw.ends_with(')') {
      return Err(sherr!(
        ParseErr,
        "Invalid associative array syntax: {}",
        raw,
      ));
    }
    let raw = raw[1..raw.len() - 1].to_string();

    let tokens: Vec<String> = LexStream::new(raw.into(), LexFlags::empty())
      .map(|tk| tk.and_then(|tk| tk.expand()).map(|tk| tk.get_words()))
      .try_fold(String::new(), |mut acc, wrds| {
        match wrds {
          Ok(wrds) => {
            for wrd in wrds {
              if !acc.is_empty() {
                acc.push(markers::ARG_SEP);
              }
              acc.push_str(&wrd);
            }
          }
          Err(e) => return Err(e),
        }
        Ok(acc)
      })?
      .split(markers::ARG_SEP)
      .filter(|s| !s.is_empty())
      .map(|s| s.to_string())
      .collect();

    let mut pairs = Vec::new();
    for token in tokens {
      if token.starts_with('[') && token.contains("]=") {
        let key_end = token.find("]=").unwrap();
        let key = token[1..key_end].to_string();
        let val = token[key_end + 2..].to_string();
        pairs.push((key, val));
      } else {
        return Err(sherr!(
          ParseErr,
          "Invalid associative array element: expected [key]=value, got {}",
          token,
        ));
      }
    }

    Ok(Self::AssocArr(pairs))
  }
}

impl Display for VarKind {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      VarKind::Str(s) => write!(f, "{s}"),
      VarKind::Int(i) => write!(f, "{i}"),
      VarKind::Arr(items) => {
        let mut item_iter = items.iter().peekable();
        while let Some(item) = item_iter.next() {
          write!(f, "{item}")?;
          if item_iter.peek().is_some() {
            write!(f, " ")?;
          }
        }
        Ok(())
      }
      VarKind::AssocArr(items) => {
        let mut item_iter = items.iter().peekable();
        while let Some(item) = item_iter.next() {
          let (k, v) = item;
          write!(f, "{k}={v}")?;
          if item_iter.peek().is_some() {
            write!(f, " ")?;
          }
        }
        Ok(())
      }
    }
  }
}

#[derive(Clone, Debug)]
pub(crate) struct Var {
  flags: VarFlags,
  kind: VarKind,
}

impl Default for Var {
  fn default() -> Self {
    Self {
      flags: VarFlags::default(),
      kind: VarKind::Str(String::new()),
    }
  }
}

impl Var {
  pub fn env_var(val: &str) -> Self {
    Self {
      flags: VarFlags::EXPORT,
      kind: VarKind::Str(val.to_string()),
    }
  }
  pub fn new(kind: VarKind, flags: VarFlags) -> Self {
    Self { flags, kind }
  }
  pub fn kind(&self) -> &VarKind {
    &self.kind
  }
  pub fn kind_mut(&mut self) -> &mut VarKind {
    &mut self.kind
  }
  pub fn mark_for_export(&mut self) {
    self.flags.set(VarFlags::EXPORT, true);
  }
  pub fn flags(&self) -> VarFlags {
    self.flags
  }
}

impl Display for Var {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    self.kind.fmt(f)
  }
}

impl From<Vec<String>> for Var {
  fn from(value: Vec<String>) -> Self {
    Self::new(VarKind::Arr(value.into()), VarFlags::empty())
  }
}

impl From<Vec<Candidate>> for Var {
  fn from(value: Vec<Candidate>) -> Self {
    let as_strs = value
      .into_iter()
      .map(|c| c.content().to_string())
      .collect::<Vec<_>>();
    Self::new(VarKind::Arr(as_strs.into()), VarFlags::empty())
  }
}

impl From<&[String]> for Var {
  fn from(value: &[String]) -> Self {
    let mut new = VecDeque::new();
    new.extend(value.iter().cloned());
    Self::new(VarKind::Arr(new), VarFlags::empty())
  }
}

macro_rules! impl_var_from {
    ($($t:ty),*) => {
			$(impl From<$t> for Var {
				fn from(value: $t) -> Self {
					Self::new(VarKind::Str(value.to_string()), VarFlags::empty())
				}
			})*
    };
}

impl_var_from!(
  i8, i16, i32, i64, isize, u8, u16, u32, u64, usize, String, &str, bool
);

#[derive(Default, Clone, Debug)]
pub(crate) struct VarTab {
  vars: HashMap<String, Var>,
  params: HashMap<ShellParam, String>,
  sh_argv: VecDeque<String>, /* Using a VecDeque makes the implementation of `shift` straightforward */

  deferred_cmds: Vec<String>,
}

impl VarTab {
  pub fn bare() -> Self {
    Self {
      vars: HashMap::new(),
      params: HashMap::new(),
      sh_argv: VecDeque::new(),
      deferred_cmds: Vec::new(),
    }
  }
  pub fn new() -> Self {
    let vars = Self::init_sh_vars();
    let params = Self::init_params();
    let mut var_tab = Self {
      vars,
      params,
      sh_argv: VecDeque::new(),
      deferred_cmds: Vec::new(),
    };
    var_tab.init_sh_argv();
    var_tab
  }
  fn init_params() -> HashMap<ShellParam, String> {
    let mut params = HashMap::new();
    params.insert(ShellParam::ArgCount, "0".into()); // Number of positional parameters
    params.insert(ShellParam::ShPid, Pid::this().to_string()); // PID of the shell
    params.insert(ShellParam::LastJob, "".into()); // PID of the last background job (if any)
    params
  }
  fn init_sh_vars() -> HashMap<String, Var> {
    let mut vars = HashMap::new();
    vars.insert("COMP_WORDBREAKS".into(), " \t\n\"'@><=;|&(:".into());
    vars.insert("OPTIND".into(), "1".into());
    let env_vars = Self::init_env();
    vars.extend(env_vars);
    vars
  }
  fn init_env() -> Vec<(String, Var)> {
    // The closure below runs exactly one time.
    // if we spawn any new threads, this won't happen again.
    ENV_INIT.call_once(|| {
      let pathbuf_to_string =
        |pb: Result<PathBuf, std::io::Error>| pb.unwrap_or_default().to_string_lossy().to_string();
      // First, inherit any env vars from the parent process
      let term = {
        if isatty(stdin_fileno()).unwrap_or_default() {
          if let Ok(term) = std::env::var("TERM") {
            term
          } else {
            "linux".to_string()
          }
        } else {
          "xterm-256color".to_string()
        }
      };
      let home_fallback;
      let username_fallback;
      let uid;
      if let Some(user) = User::from_uid(nix::unistd::Uid::current()).ok().flatten() {
        home_fallback = user.dir;
        username_fallback = user.name;
        uid = user.uid;
      } else {
        home_fallback = PathBuf::new();
        username_fallback = "unknown".into();
        uid = 0.into();
      }
      let home_fallback = pathbuf_to_string(Ok(home_fallback));
      let hostname = gethostname()
        .map(|hname| hname.to_string_lossy().to_string())
        .unwrap_or_default();

      let resolve = |var: &str, fallback: &str| -> String {
        let val = std::env::var(var).unwrap_or_else(|_| fallback.to_string());

        unsafe { std::env::set_var(var, &val) };
        val
      };

      let resolved_home = resolve("HOME", &home_fallback);
      let resolved_user = resolve("USER", &username_fallback);

      let mut data_dir = std::env::var("XDG_DATA_HOME")
        .ok()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("{resolved_home}/.local/share")));
      data_dir.push("shed");

      let shed_db = data_dir.join("shed_hist.db");
      let shed_rc = std::env::var("XDG_CONFIG_HOME")
        .ok()
        .map(|c| PathBuf::from(c).join("shed").join("shedrc"))
        .unwrap_or_else(|| PathBuf::from(format!("{resolved_home}/.config/shed/shedrc")));

      resolve("TMPDIR", "/tmp");
      resolve("TERM", &term);
      resolve("LANG", "en_US.UTF-8");
      resolve("LOGNAME", &resolved_user);
      resolve("SHELL", &pathbuf_to_string(std::env::current_exe()));
      resolve("SHED_HISTDB", &shed_db.display().to_string());
      resolve("SHED_RC", &shed_rc.display().to_string());

      let set_var = |var: &str, val: &str| {
        unsafe { std::env::set_var(var, val) };
      };

      // PWD always set from getcwd()
      let pwd = pathbuf_to_string(std::env::current_dir());
      set_var("PWD", &pwd);

      // OLDPWD inherited if set in parent env
      if let Ok(old) = std::env::var("OLDPWD") {
        set_var("OLDPWD", &old);
      }

      set_var("IFS", " \t\n");
      set_var("UID", &uid.to_string());
      set_var("PPID", &getppid().to_string());
      set_var("HOST", &hostname.clone());
    });

    let mut vars = vec![];
    for (key, val) in std::env::vars() {
      if !vars.iter().any(|(k, _)| k == &key) {
        vars.push((key, Var::env_var(&val)));
      }
    }

    vars
  }
  pub fn init_sh_argv(&mut self) {
    for arg in std::env::args() {
      self.bpush_arg(arg);
    }
  }
  pub fn defer_cmd(&mut self, cmd: String) {
    self.deferred_cmds.push(cmd);
  }
  pub fn take_deferred_cmds(&mut self) -> Vec<String> {
    std::mem::take(&mut self.deferred_cmds)
  }
  pub fn display_deferred_cmds(&self) -> String {
    self
      .deferred_cmds
      .iter()
      .map(|s| as_var_val_display(s))
      .collect::<Vec<_>>()
      .join("\n")
  }
  pub fn sh_argv(&self) -> &VecDeque<String> {
    &self.sh_argv
  }
  pub fn sh_argv_mut(&mut self) -> &mut VecDeque<String> {
    &mut self.sh_argv
  }
  pub fn clear_args(&mut self) {
    let first = self.sh_argv.pop_front();
    self.sh_argv.clear();

    // preserve the first arg, which is conventionally the name of the shell, script, or function
    if let Some(arg) = first {
      self.bpush_arg(arg);
    }
  }
  fn update_arg_params(&mut self) {
    self.set_param(
      ShellParam::AllArgs,
      &self.sh_argv.clone().into_iter().collect::<Vec<_>>()[1..]
        .join(&markers::ARG_SEP.to_string()),
    );
    self.set_param(ShellParam::ArgCount, &(self.sh_argv.len() - 1).to_string());
  }
  /// Push an arg to the back of the arg deque
  pub fn bpush_arg(&mut self, arg: String) {
    self.sh_argv.push_back(arg);
    self.update_arg_params();
  }
  /// Pop an arg from the front of the arg deque
  pub fn fpop_arg(&mut self) -> Option<String> {
    let arg = self.sh_argv.pop_front();
    self.update_arg_params();
    arg
  }
  pub fn vars(&self) -> &HashMap<String, Var> {
    &self.vars
  }
  pub fn vars_mut(&mut self) -> &mut HashMap<String, Var> {
    &mut self.vars
  }
  pub fn export_var(&mut self, var_name: &str) {
    if let Some(var) = self.vars.get_mut(var_name) {
      var.mark_for_export();
      unsafe { std::env::set_var(var_name, var.to_string()) };
    }
  }
  pub fn get_var(&self, var: &str) -> String {
    if let Ok(param) = var.parse::<ShellParam>() {
      let param = self.get_param(param);
      if !param.is_empty() {
        return param;
      }
    }
    if let Some(var) = self.vars.get(var).map(|s| s.to_string()) {
      var
    } else {
      std::env::var(var).unwrap_or_default()
    }
  }
  pub fn try_get_var_meta(&self, var: &str) -> Option<Var> {
    self.vars.get(var).cloned()
  }
  #[cfg(test)]
  pub fn get_var_flags(&self, var_name: &str) -> Option<VarFlags> {
    self.vars.get(var_name).map(|var| var.flags)
  }
  pub fn unset_var(&mut self, var_name: &str) -> ShResult<()> {
    if let Some(var) = self.vars.get(var_name)
      && var.flags.contains(VarFlags::READONLY)
    {
      return Err(sherr!(
        ExecFail,
        "cannot unset readonly variable '{}'",
        var_name,
      ));
    }
    self.vars.remove(var_name);
    unsafe { std::env::remove_var(var_name) };
    Ok(())
  }
  pub fn set_index(&mut self, var_name: &str, idx: ArrIndex, val: String) -> ShResult<()> {
    if self.var_exists(var_name)
      && let Some(var) = self.vars_mut().get_mut(var_name)
    {
      let idx = idx.resolve_for(var.kind())?;
      match var.kind_mut() {
        VarKind::Arr(items) => {
          let idx = match idx {
            ArrIndex::Literal(n) => n,
            ArrIndex::FromBack(n) => {
              if items.len() >= n {
                items.len() - n
              } else {
                return Err(sherr!(
                  ExecFail,
                  "Index {} out of bounds for array '{}'",
                  n,
                  var_name,
                ));
              }
            }
            _ => {
              return Err(sherr!(
                ExecFail,
                "Cannot index all elements of array '{}'",
                var_name,
              ));
            }
          };

          if idx >= items.len() {
            items.resize(idx + 1, String::new());
          }
          items[idx] = val;
          return Ok(());
        }
        VarKind::AssocArr(items) => {
          // resolve_for guarantees `idx` is `Key(_)` here (or one of the
          // wildcards below, which don't make sense for assignment).
          let ArrIndex::Key(key) = idx else {
            return Err(sherr!(
              ExecFail,
              "Cannot assign to all elements of associative array '{}'",
              var_name,
            ));
          };
          for (k, v) in items.iter_mut() {
            if k == &key {
              *v = val;
              return Ok(());
            }
          }
          items.push((key, val));
          return Ok(());
        }
        _ => {
          return Err(sherr!(ExecFail, "Variable '{}' is not an array", var_name,));
        }
      }
    }
    Ok(())
  }
  pub fn set_var(&mut self, var_name: &str, val: VarKind, flags: VarFlags) -> ShResult<()> {
    if let Some(var) = self.vars.get_mut(var_name) {
      if var.flags.contains(VarFlags::READONLY) && !flags.contains(VarFlags::READONLY) {
        return Err(sherr!(ExecFail, "Variable '{}' is readonly", var_name,));
      }
      var.kind = val;
      var.flags |= flags;
      if var.flags.contains(VarFlags::EXPORT) || flags.contains(VarFlags::EXPORT) {
        if flags.contains(VarFlags::EXPORT) && !var.flags.contains(VarFlags::EXPORT) {
          var.mark_for_export();
        }
        unsafe { std::env::set_var(var_name, var.kind.to_string()) };
      }
    } else {
      let mut var = Var::new(val, flags);
      if flags.contains(VarFlags::EXPORT) {
        var.mark_for_export();
        unsafe { std::env::set_var(var_name, var.to_string()) };
      }
      self.vars.insert(var_name.to_string(), var);
    }
    Ok(())
  }
  pub fn var_exists(&self, var_name: &str) -> bool {
    if let Ok(param) = var_name.parse::<ShellParam>() {
      return self.params.contains_key(&param);
    }
    self.vars.contains_key(var_name)
  }
  pub fn set_param(&mut self, param: ShellParam, val: &str) {
    self.params.insert(param, val.to_string());
  }
  pub fn get_param(&self, param: ShellParam) -> String {
    match param {
      ShellParam::Pos(n) => self
        .sh_argv()
        .get(n)
        .map(|s| s.to_string())
        .unwrap_or_default(),
      ShellParam::Status => self
        .params
        .get(&ShellParam::Status)
        .map(|s| s.to_string())
        .unwrap_or("0".into()),
      ShellParam::AllArgsStr => {
        let ifs = get_separator();
        self
          .params
          .get(&ShellParam::AllArgs)
          .map(|s| s.replace(markers::ARG_SEP, &ifs).to_string())
          .unwrap_or_default()
      }

      _ => self
        .params
        .get(&param)
        .map(|s| s.to_string())
        .unwrap_or_default(),
    }
  }
}

#[cfg(test)]
mod top_level_colon_tests {
  use super::top_level_colon;

  #[test]
  fn simple_colon_at_position() {
    assert_eq!(top_level_colon("foo:bar"), Some(3));
  }

  #[test]
  fn no_colon_returns_none() {
    assert_eq!(top_level_colon("foobar"), None);
    assert_eq!(top_level_colon(""), None);
  }

  #[test]
  fn colon_at_start() {
    assert_eq!(top_level_colon(":foo"), Some(0));
  }

  #[test]
  fn colon_at_end() {
    assert_eq!(top_level_colon("foo:"), Some(3));
  }

  #[test]
  fn colon_inside_braces_skipped() {
    // `${foo:bar}` — the `:` is nested inside braces.
    assert_eq!(top_level_colon("${foo:bar}"), None);
  }

  #[test]
  fn colon_inside_parens_skipped() {
    // `$(cmd:arg)` — the `:` is nested inside parens.
    assert_eq!(top_level_colon("$(cmd:arg)"), None);
  }

  #[test]
  fn outer_colon_found_when_inner_nested() {
    // Outer colon at index 1, inner colon nested inside `${}`.
    assert_eq!(top_level_colon("x:${foo:bar}"), Some(1));
  }

  #[test]
  fn first_colon_wins_when_multiple_top_level() {
    assert_eq!(top_level_colon("a:b:c"), Some(1));
  }

  #[test]
  fn nested_braces_keep_inner_colon_hidden() {
    assert_eq!(top_level_colon("{{a:b}c}"), None);
  }

  #[test]
  fn colon_after_closing_brace() {
    // `${x}:y` — after `}` we're back to depth 0, the `:` is top-level.
    assert_eq!(top_level_colon("${x}:y"), Some(4));
  }

  #[test]
  fn outer_then_inner_colons() {
    // First colon at index 1 (top-level), second at 7 (inside parens).
    assert_eq!(top_level_colon("a:$(cmd:arg):b"), Some(1));
  }
}

#[cfg(test)]
mod shell_param_fmt_tests {
  use super::*;

  #[test]
  fn status_formats_as_question_mark() {
    assert_eq!(ShellParam::Status.to_string(), "?");
  }

  #[test]
  fn shpid_formats_as_dollar() {
    assert_eq!(ShellParam::ShPid.to_string(), "$");
  }

  #[test]
  fn last_job_formats_as_bang() {
    assert_eq!(ShellParam::LastJob.to_string(), "!");
  }

  #[test]
  fn shell_name_formats_as_zero() {
    assert_eq!(ShellParam::ShellName.to_string(), "0");
  }

  #[test]
  fn pos_formats_as_number() {
    assert_eq!(ShellParam::Pos(1).to_string(), "1");
    assert_eq!(ShellParam::Pos(99).to_string(), "99");
  }

  #[test]
  fn all_args_formats_as_at() {
    assert_eq!(ShellParam::AllArgs.to_string(), "@");
  }

  #[test]
  fn all_args_str_formats_as_star() {
    assert_eq!(ShellParam::AllArgsStr.to_string(), "*");
  }

  #[test]
  fn arg_count_formats_as_hash() {
    assert_eq!(ShellParam::ArgCount.to_string(), "#");
  }

  // ─── round-trip via FromStr ───────────────────────────────────────

  #[test]
  fn every_variant_round_trips_through_from_str() {
    use std::str::FromStr;
    let cases = [
      ShellParam::Status,
      ShellParam::ShPid,
      ShellParam::LastJob,
      ShellParam::ShellName,
      ShellParam::Pos(7),
      ShellParam::AllArgs,
      ShellParam::AllArgsStr,
      ShellParam::ArgCount,
    ];
    for v in cases {
      let s = v.to_string();
      let parsed = ShellParam::from_str(&s).unwrap();
      assert_eq!(parsed, v, "round-trip mismatch for {v:?} via {s:?}");
    }
  }
}

#[cfg(test)]
mod set_index_tests {
  use super::*;
  use crate::tests::testutil::TestGuard;

  fn make_tab_with_arr(name: &str, items: Vec<&str>) -> VarTab {
    let mut tab = VarTab::new();
    tab
      .set_var(
        name,
        VarKind::Arr(items.into_iter().map(String::from).collect()),
        VarFlags::empty(),
      )
      .unwrap();
    tab
  }

  fn arr_items(tab: &VarTab, name: &str) -> Vec<String> {
    match tab.vars.get(name).map(|v| v.kind()) {
      Some(VarKind::Arr(items)) => items.iter().cloned().collect(),
      other => panic!("expected Arr for {name}, got {other:?}"),
    }
  }

  fn assoc_items(tab: &VarTab, name: &str) -> Vec<(String, String)> {
    match tab.vars.get(name).map(|v| v.kind()) {
      Some(VarKind::AssocArr(items)) => items.clone(),
      other => panic!("expected AssocArr for {name}, got {other:?}"),
    }
  }

  #[test]
  fn literal_index_replaces_existing_slot() {
    let _g = TestGuard::new();
    let mut tab = make_tab_with_arr("arr", vec!["a", "b", "c"]);
    tab
      .set_index("arr", ArrIndex::Literal(1), "B!".into())
      .unwrap();
    assert_eq!(arr_items(&tab, "arr"), vec!["a", "B!", "c"]);
  }

  #[test]
  fn literal_index_past_end_resizes_with_empty_strings() {
    let _g = TestGuard::new();
    let mut tab = make_tab_with_arr("arr", vec!["a"]);
    tab
      .set_index("arr", ArrIndex::Literal(3), "z".into())
      .unwrap();
    assert_eq!(arr_items(&tab, "arr"), vec!["a", "", "", "z"]);
  }

  #[test]
  fn from_back_index_targets_correct_slot() {
    let _g = TestGuard::new();
    let mut tab = make_tab_with_arr("arr", vec!["a", "b", "c"]);
    // FromBack(1) → items.len() - 1 = index 2.
    tab
      .set_index("arr", ArrIndex::FromBack(1), "C!".into())
      .unwrap();
    assert_eq!(arr_items(&tab, "arr"), vec!["a", "b", "C!"]);
  }

  #[test]
  fn from_back_index_out_of_bounds_errors() {
    let _g = TestGuard::new();
    let mut tab = make_tab_with_arr("arr", vec!["a", "b"]);
    let res = tab.set_index("arr", ArrIndex::FromBack(5), "x".into());
    assert!(res.is_err());
  }

  #[test]
  fn assoc_array_new_key_appended() {
    let _g = TestGuard::new();
    let mut tab = VarTab::new();
    tab
      .set_var("h", VarKind::AssocArr(vec![]), VarFlags::empty())
      .unwrap();
    tab
      .set_index("h", ArrIndex::Key("k".into()), "v".into())
      .unwrap();
    assert_eq!(
      assoc_items(&tab, "h"),
      vec![("k".to_string(), "v".to_string())]
    );
  }

  #[test]
  fn assoc_array_existing_key_overwritten_in_place() {
    let _g = TestGuard::new();
    let mut tab = VarTab::new();
    tab
      .set_var(
        "h",
        VarKind::AssocArr(vec![("k1".into(), "old".into()), ("k2".into(), "y".into())]),
        VarFlags::empty(),
      )
      .unwrap();
    tab
      .set_index("h", ArrIndex::Key("k1".into()), "new".into())
      .unwrap();
    assert_eq!(
      assoc_items(&tab, "h"),
      vec![
        ("k1".to_string(), "new".to_string()),
        ("k2".to_string(), "y".to_string())
      ]
    );
  }

  #[test]
  fn set_index_on_scalar_errors() {
    let _g = TestGuard::new();
    let mut tab = VarTab::new();
    tab
      .set_var("scalar", VarKind::Str("plain".into()), VarFlags::empty())
      .unwrap();
    // After resolve_for on a Str, the literal index applies but the
    // catchall arm returns "Variable '...' is not an array".
    let res = tab.set_index("scalar", ArrIndex::Literal(0), "x".into());
    assert!(res.is_err());
  }

  #[test]
  fn missing_var_is_silent_ok() {
    // var_exists guard at the top is false → function returns Ok
    // without creating anything.
    let _g = TestGuard::new();
    let mut tab = VarTab::new();
    tab
      .set_index("never_existed", ArrIndex::Literal(0), "x".into())
      .unwrap();
    assert!(!tab.vars.contains_key("never_existed"));
  }

  #[test]
  fn wildcard_index_into_indexed_array_errors() {
    let _g = TestGuard::new();
    let mut tab = make_tab_with_arr("arr", vec!["a", "b"]);
    let res = tab.set_index("arr", ArrIndex::AllSplit, "x".into());
    assert!(res.is_err());
  }

  #[test]
  fn wildcard_index_into_assoc_array_errors() {
    let _g = TestGuard::new();
    let mut tab = VarTab::new();
    tab
      .set_var(
        "h",
        VarKind::AssocArr(vec![("k".into(), "v".into())]),
        VarFlags::empty(),
      )
      .unwrap();
    // Wildcard (AllSplit / AllJoined) on assoc array → inner not-a-Key arm errors.
    let res = tab.set_index("h", ArrIndex::AllSplit, "x".into());
    assert!(res.is_err());
  }

  #[test]
  fn literal_index_on_assoc_array_becomes_string_key() {
    // resolve_for converts Literal(n) → Key(n.to_string()) for assoc
    // arrays. Pin this stringification behavior.
    let _g = TestGuard::new();
    let mut tab = VarTab::new();
    tab
      .set_var("h", VarKind::AssocArr(vec![]), VarFlags::empty())
      .unwrap();
    tab
      .set_index("h", ArrIndex::Literal(7), "v".into())
      .unwrap();
    assert_eq!(
      assoc_items(&tab, "h"),
      vec![("7".to_string(), "v".to_string())]
    );
  }
}
