use super::{SHED, Shed, try_var};

use std::{
  collections::HashMap,
  fs::OpenOptions,
  io::{Read, Write},
  path::{Path, PathBuf},
  rc::Rc,
  sync::Arc,
};

use nix::unistd::{User, getuid};
use rusqlite::Connection;
use scopeguard::defer;
use unicode_segmentation::UnicodeSegmentation;

use super::{
  ShResult, autocmd,
  eval::{
    execute::exec_nonint,
    lex::{LexFlags, LexStream},
  },
  match_loop,
  meta::{MetaTab, UtilKind, Utility},
  sherr,
  shopt::ShOpts,
  vars::{ArrIndex, Var, VarFlags, VarKind},
};

/// Parse `arr[idx]` into (name, raw_index_expr). Pure parsing, no expansion.
pub fn parse_arr_bracket(var_name: &str) -> Option<(String, String)> {
  let mut chars = var_name.chars();
  let mut name = String::new();
  let mut idx_raw = String::new();
  let mut bracket_depth = 0;

  match_loop!(chars.next() => ch, {
    '\\' => {
      chars.next();
    }
    '[' => {
      bracket_depth += 1;
      if bracket_depth > 1 {
        idx_raw.push(ch);
      }
    }
    ']' => {
      if bracket_depth > 0 {
        bracket_depth -= 1;
        if bracket_depth == 0 {
          if idx_raw.is_empty() {
            return None;
          }
          break;
        }
      }
      idx_raw.push(ch);
    }
    _ if bracket_depth > 0 => idx_raw.push(ch),
    _ => name.push(ch),
  });

  if name.is_empty() || idx_raw.is_empty() {
    None
  } else {
    Some((name, idx_raw))
  }
}

/// Expand the raw index expression and parse it into an ArrIndex.
pub fn expand_arr_index(idx_raw: &str, allow_side_effects: bool) -> ShResult<ArrIndex> {
  let expanded = LexStream::new(idx_raw.into(), LexFlags::empty())
    .map(|tk| tk.and_then(|tk| tk.expand()).map(|tk| tk.get_words()))
    .try_fold(vec![], |mut acc, wrds| {
      match wrds {
        Ok(wrds) => acc.extend(wrds),
        Err(e) => return Err(e),
      }
      Ok(acc)
    })?
    .into_iter()
    .next()
    .ok_or_else(|| sherr!(ParseErr, "Empty array index"))?;

  ArrIndex::parse(&expanded, allow_side_effects)
    .map_err(|_| sherr!(ParseErr, "Invalid array index: {}", expanded,))
}

/*
 * the functions below are some of the most important in the entire codebase
 * it's very important to understand these if you want to get anything done around here.
 *
 * Each one accesses a different part of the shared state (the "Shed" struct),
 * and they take a closure that operates on that part of the state.
 *
 *
 * With these, we can access shell state anywhere without threading a state object through every function.
 * However, we must be mindful of what the callstack looks like when we call them, to avoid re-entrancy issues.
 */

/// Query the SQLite database.
///
/// Takes a function that returns ShResult<T>, and returns ShResult<Option<T>>.
/// The option is necessary because `Shed.db_conn` can be None. This happens
/// in non-interactive cases, or cases where the database cannot be opened.
///
/// The returns look basically like this:
/// * Ok(None) means "there's no database connection"
/// * Err(e) is your function's ShErr
/// * Ok(Some(T)) means the connection exists and your function succeeded.
pub fn query_db<T, F: FnOnce(Arc<Connection>) -> ShResult<T>>(f: F) -> ShResult<Option<T>> {
  SHED.with(|shed| {
    let Some(Some(conn)) = shed.db_conn.get() else {
      return Ok(None);
    };

    f(Arc::clone(conn)).map(Some)
  })
}

pub fn with_vars<F, H, V, T>(vars: H, f: F) -> T
where
  F: FnOnce() -> T,
  H: Into<HashMap<String, V>>,
  V: Into<Var>,
{
  let snapshot = Shed::vars(|v| v.clone());
  let vars = vars.into();
  for (name, val) in vars {
    let val = val.into();
    let kind = val.kind().clone();
    let flags = val.flags();
    Shed::vars_mut(|v| v.set_var(&name, kind, flags).unwrap());
  }
  let _guard = scopeguard::guard(snapshot, |snap| {
    Shed::vars_mut(|v| *v = snap);
  });
  f()
}

pub fn change_dir<P: AsRef<Path>>(dir: P) -> ShResult<()> {
  let dir = dir.as_ref();
  let dir_raw = &dir.display().to_string();
  defer!(super::autocmd!(PostChangeDir));
  let current_dir = std::env::current_dir()?.display().to_string();
  with_vars(
    [
      ("NEW_DIR".into(), dir_raw.as_str()),
      ("OLD_DIR".into(), current_dir.as_str()),
    ],
    || autocmd!(PreChangeDir),
  );

  std::env::set_current_dir(dir)?;

  let new_dir_resolved = std::env::current_dir()?.display().to_string();
  Shed::vars_mut(|v| {
    v.set_var(
      "OLDPWD",
      VarKind::Str(current_dir.clone()),
      VarFlags::EXPORT,
    )
  })?;
  Shed::vars_mut(|v| v.set_var("PWD", VarKind::Str(new_dir_resolved), VarFlags::EXPORT))?;

  Ok(())
}

pub fn get_comp_wordbreaks() -> String {
  try_var!("COMP_WORDBREAKS").unwrap_or_else(|| String::from("\"'><;|=&(:"))
}

/// Get the first char of IFS
///
/// Used mainly for joining strings
pub fn get_separator() -> String {
  let separators = get_separators();
  separators
    .graphemes(true)
    .next()
    .unwrap_or_default()
    .to_string()
}

/// Get the entire IFS variable
///
/// Used mainly for splitting strings
pub fn get_separators() -> String {
  try_var!("IFS").unwrap_or(String::from(" \t\n"))
}

pub fn get_time_fmt() -> String {
  try_var!("TIMEFMT").unwrap_or_else(|| String::from("\nreal\t%*E\nuser\t%*U\nsys\t%*S"))
}

pub fn lookup_cmd(cmd: &str) -> Option<PathBuf> {
  if Shed::shopts(|o| o.set.hashall) {
    which_util(cmd)
      .filter(|u| matches!(u.kind(), UtilKind::Command(_) | UtilKind::File(_)))
      .map(|u| {
        let (UtilKind::Command(path) | UtilKind::File(path)) = u.kind() else {
          unreachable!()
        };
        path.clone()
      })
  } else {
    MetaTab::get_exec_files_in_cwd()
      .into_iter()
      .chain(MetaTab::get_cmds_in_path())
      .find(|u| u.name() == cmd)
      .and_then(|u| match u.kind() {
        UtilKind::Command(path) | UtilKind::File(path) => Some(path.clone()),
        _ => None,
      })
  }
}

pub fn which_util(name: &str) -> Option<Rc<Utility>> {
  // Check in shell resolution order: alias > function > builtin > cached command > PATH
  if Shed::logic(|l| l.get_alias(name).is_some()) {
    return Some(Rc::new(Utility::alias(name.to_string())));
  }
  if Shed::logic(|l| l.get_func(name).is_some()) {
    return Some(Rc::new(Utility::function(name.to_string())));
  }
  if crate::builtin::lookup_builtin(name).is_some() {
    return Some(Rc::new(Utility::builtin(name.to_string())));
  }
  // For external commands, check cache first, then scan PATH
  Shed::meta(|m| m.get_cached_cmd(name)).or_else(|| {
    MetaTab::get_cmds_in_path()
      .into_iter()
      .chain(MetaTab::get_exec_files_in_cwd())
      .find(|u| u.name() == name)
      .inspect(|u| Shed::meta_mut(|m| m.cache_util(Rc::clone(u))))
  })
}

pub fn try_hash() {
  if Shed::shopts(|o| o.set.hashall) {
    Shed::meta_mut(|m| m.try_rehash_utils());
  } else {
    Shed::meta_mut(|m| m.clear_cache());
  }
}

pub fn rc_file_path() -> Option<PathBuf> {
  try_var!("SHED_RC")
    .map(PathBuf::from)
    .or_else(|| get_home().map(|home| home.join(".shedrc")))
}

pub fn generate_default_rc() -> ShResult<Option<PathBuf>> {
  let rc_path =
    rc_file_path().ok_or_else(|| sherr!(InternalErr, "could not determine rc file path",))?;
  if rc_path.exists() {
    return Ok(None);
  }
  log::info!("Generating default rc file at {}", rc_path.display());
  let mut rc_file = OpenOptions::new()
    .write(true)
    .create(true)
    .truncate(true)
    .open(&rc_path)?;

  let mut lines: Vec<String> = vec![
    "# --- Shed Runtime Commands ---".into(),
    "# This file was automatically generated by shed.".into(),
    "# These are sane defaults for many shed-specific options and features.".into(),
    "# Edit this file to customize, or use it as a reference.".into(),
    "# Refer to the 'help' builtin for information on specific shed features.".into(),
    String::new(),
  ];
  lines.extend(ShOpts::generate_default_rc());
  lines.push(String::new());

  let static_lines = [
    "# -- Tab Completion --",
    "# The 'complete' builtin tells shed how to complete arguments for a command.",
    "complete -d cd     # Only complete directory names",
    "complete -d pushd  # Only complete directory names",
    "complete -d popd   # Only complete directory names",
    "complete -j fg     # Only complete job names",
    "complete -j bg     # Only complete job names",
    "complete -f source # Only complete file names",
    "complete -a alias  # Only complete alias names",
    "",
    "# -- Autocmds --",
    "# Register commands to run on shell lifecycle events.",
    "# Type 'help autocmd' on the prompt for more details.",
    "autocmd 'on-exit' 'echo exit 1>&2' # Print 'exit' when the shell exits",
    "",
    "# -- Keybinds --",
    "# Register commands to run on key presses while on the prompt.",
    "# Type 'help keymap' on the prompt for more advanced usage.",
    "keymap -ie '<C-L>' '<CMD>clear<CR>' # Ctrl+L clears the screen (insert + emacs mode)",
  ];

  for line in &lines {
    writeln!(rc_file, "{}", line)?;
  }
  for line in static_lines {
    writeln!(rc_file, "{}", line)?;
  }

  Ok(Some(rc_path))
}

pub fn source_runtime_file(name: &str, env_var_name: Option<&str>) -> ShResult<()> {
  let etc_path = PathBuf::from(format!("/etc/shed/{name}"));
  if etc_path.is_file()
    && let Err(e) = source_file(etc_path)
  {
    e.print_error();
  }

  let path = if let Some(name) = env_var_name
    && let Some(path) = try_var!(name)
  {
    PathBuf::from(&path)
  } else if let Some(home) = get_home() {
    home.join(format!(".{name}"))
  } else {
    return Err(sherr!(InternalErr, "could not determine home path",));
  };
  if !path.is_file() {
    return Ok(());
  }
  source_file(path)
}

pub fn source_rc() -> ShResult<()> {
  source_runtime_file("shedrc", Some("SHED_RC"))
}

pub fn source_login() -> ShResult<()> {
  source_runtime_file("shed_profile", Some("SHED_PROFILE"))
}

pub fn source_env() -> ShResult<()> {
  source_runtime_file("shedenv", Some("SHED_ENV"))
}

pub fn source_file(path: PathBuf) -> ShResult<()> {
  let source_name = path.to_string_lossy().to_string();
  let mut file = OpenOptions::new().read(true).open(path)?;

  let mut buf = String::new();
  file.read_to_string(&mut buf)?;
  exec_nonint(buf, Some(source_name.into()))?;
  Ok(())
}

pub fn set_ver_info() -> ShResult<()> {
  let version = env!("CARGO_PKG_VERSION");
  let mut semver = version.split('.');
  let major = semver.next().unwrap_or("0");
  let minor = semver.next().unwrap_or("0");
  let patch = semver.next().unwrap_or("0");
  let arch = std::env::consts::ARCH;
  let os = std::env::consts::OS;
  let ver_info = vec![
    ("major".into(), major.into()),
    ("minor".into(), minor.into()),
    ("patch".into(), patch.into()),
    ("arch".into(), arch.into()),
    ("os".into(), os.into()),
  ];

  Shed::vars_mut(|v| {
    v.set_var(
      "SHED_VERSION",
      VarKind::Str(version.into()),
      VarFlags::EXPORT,
    )?;
    v.set_var(
      "SHED_VER_INFO",
      VarKind::AssocArr(ver_info),
      VarFlags::empty(),
    )
  })?;

  Ok(())
}

pub fn set_sh_lvl() -> ShResult<()> {
  // Increment SHLVL, or set to 1 if not present or invalid.
  // This var represents how many nested shell instances we're in
  if let Some(var) = try_var!("SHLVL")
    && let Ok(lvl) = var.parse::<u32>()
  {
    Shed::vars_mut(|v| {
      v.set_var(
        "SHLVL",
        VarKind::Str((lvl + 1).to_string()),
        VarFlags::EXPORT,
      )
    })?;
  } else {
    Shed::vars_mut(|v| v.set_var("SHLVL", VarKind::Str("1".into()), VarFlags::EXPORT))?;
  }

  Ok(())
}

/// Get a clone of the shared database connection, if available.
pub fn get_db_conn() -> Option<Arc<Connection>> {
  SHED.with(|shed| shed.db_conn.get().cloned().flatten())
}

/// Initialize the shared database connection with an in-memory sqlite
/// database. Used by TestGuard. Safe to call multiple times — the OnceLock
/// only takes effect on the first call, so all tests in a thread share the
/// same in-memory db (per-test cleanup is handled separately by TestGuard).
#[cfg(test)]
pub fn init_test_db_conn() {
  SHED.with(|shed| {
    let _ = shed
      .db_conn
      .set(Connection::open_in_memory().ok().map(std::sync::Arc::new));
  });
}

/// Initialize the shared database connection on the `Shed` struct.
pub fn init_db_conn() {
  SHED.with(|shed| match open_db_conn().ok() {
    Some(conn) => {
      let Ok(_) = conn.execute_batch("PRAGMA journal_mode=WAL") else {
        return;
      };
      let Ok(_) = conn.execute_batch("PRAGMA case_sensitive_like = 1") else {
        return;
      };
      let _ = shed.db_conn.set(Some(conn.into()));
    }
    None => {
      let _ = shed.db_conn.set(None);
    }
  })
}

pub fn open_db_conn() -> ShResult<Connection> {
  let db_path = if let Some(var) = try_var!("SHED_HISTDB") {
    var
  } else {
    let home = try_var!("HOME").unwrap_or_else(|| ".".to_string());
    dirs::data_dir()
      .map(|p| p.to_string_lossy().to_string())
      .unwrap_or_else(|| format!("{home}/.local/share/shed/shed_hist.db"))
  };

  let db_path = PathBuf::from(db_path);
  if let Some(parent) = db_path.parent() {
    std::fs::create_dir_all(parent)?;
  }

  Ok(Connection::open(&db_path)?)
}

pub fn get_home() -> Option<PathBuf> {
  try_var!("HOME")
    .map(PathBuf::from)
    .or_else(|| User::from_uid(getuid()).ok().flatten().map(|u| u.dir))
}

pub fn get_home_str() -> Option<String> {
  get_home().map(|h| h.to_string_lossy().to_string())
}

pub fn get_exec_wrappers() -> Vec<String> {
  let mut wrappers = vec![
    "sudo".into(),
    "doas".into(),
    "pkexec".into(),
    "run0".into(),
    "please".into(),
    "gosu".into(),
    "strace".into(),
    "ltrace".into(),
    "ktrace".into(),
    "valgrind".into(),
    "heaptrack".into(),
    "nohup".into(),
    "nice".into(),
    "ionice".into(),
    "chrt".into(),
    "setsid".into(),
    "setpriv".into(),
    "prlimit".into(),
    "unshare".into(),
    "bwrap".into(),
    "firejail".into(),
    "systemd-run".into(),
    "proot".into(),
    "watch".into(),
    "chronic".into(),
    "parallel".into(),
    "stdbuf".into(),
    "hyperfine".into(),
    "command".into(),
    "builtin".into(),
    "env".into(),
    "exec".into(),
  ];

  // lets users define their own exec wrappers for the highlighter if they want
  // for instance, my personal config has a wrapper function called 'invoke'
  let user_wrappers = Shed::vars(|v| v.get_arr_elems("SHED_EXEC_WRAPPERS"));
  wrappers.extend(user_wrappers);

  wrappers
}

#[cfg(test)]
mod generate_default_rc_tests {
  use super::*;
  use crate::state::vars::{VarFlags, VarKind};
  use crate::tests::testutil::TestGuard;

  fn set_rc_path(p: &std::path::Path) {
    Shed::vars_mut(|v| {
      v.set_var(
        "SHED_RC",
        VarKind::Str(p.to_string_lossy().to_string()),
        VarFlags::empty(),
      )
      .unwrap();
    });
  }

  // ─── creates file when missing ──────────────────────────────────

  #[test]
  fn creates_file_when_not_present() {
    let _g = TestGuard::new();
    let dir = tempfile::TempDir::new().unwrap();
    let rc = dir.path().join("test.shedrc");
    set_rc_path(&rc);
    assert!(!rc.exists());
    let result = generate_default_rc().unwrap();
    assert_eq!(result, Some(rc.clone()));
    assert!(rc.exists());
  }

  // ─── doesn't overwrite an existing file ─────────────────────────

  #[test]
  fn does_not_overwrite_existing_file() {
    let _g = TestGuard::new();
    let dir = tempfile::TempDir::new().unwrap();
    let rc = dir.path().join("existing.shedrc");
    std::fs::write(&rc, "USER_CONTENT_MARKER").unwrap();
    set_rc_path(&rc);
    let result = generate_default_rc().unwrap();
    assert_eq!(result, None);
    // File still has user content.
    let content = std::fs::read_to_string(&rc).unwrap();
    assert_eq!(content, "USER_CONTENT_MARKER");
  }

  // ─── file content contains expected sections ────────────────────

  #[test]
  fn generated_file_contains_default_shopt_lines() {
    let _g = TestGuard::new();
    let dir = tempfile::TempDir::new().unwrap();
    let rc = dir.path().join("rc_with_shopts.shedrc");
    set_rc_path(&rc);
    generate_default_rc().unwrap();
    let content = std::fs::read_to_string(&rc).unwrap();
    // Header marker.
    assert!(
      content.contains("Shed Runtime Commands"),
      "got: {content:?}"
    );
    // ShOpts::generate_default_rc should produce `shopt set ...` lines
    // for known group names. We check a few representative ones.
    assert!(content.contains("core."), "missing core shopt lines");
    assert!(content.contains("prompt."), "missing prompt shopt lines");
    assert!(content.contains("line."), "missing line shopt lines");
  }

  #[test]
  fn generated_file_contains_static_helper_section() {
    let _g = TestGuard::new();
    let dir = tempfile::TempDir::new().unwrap();
    let rc = dir.path().join("rc_with_static.shedrc");
    set_rc_path(&rc);
    generate_default_rc().unwrap();
    let content = std::fs::read_to_string(&rc).unwrap();
    assert!(content.contains("complete -d cd"), "got: {content:?}");
    assert!(content.contains("autocmd"), "got: {content:?}");
    assert!(content.contains("keymap"), "got: {content:?}");
  }

  // The "no rc path resolvable" error path is essentially unreachable
  // in practice: `get_home` falls back to passwd-uid lookup, so even
  // with HOME unset rc_file_path returns Some. Not tested here.
}

#[cfg(test)]
mod source_runtime_file_tests {
  use super::*;
  use crate::state::vars::{VarFlags, VarKind};
  use crate::tests::testutil::TestGuard;
  use crate::var;

  fn set_var(name: &str, val: &str) {
    Shed::vars_mut(|v| {
      v.set_var(name, VarKind::Str(val.into()), VarFlags::empty())
        .unwrap();
    });
  }

  #[test]
  fn env_var_pointed_file_gets_sourced() {
    let _g = TestGuard::new();
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("source.sh");
    std::fs::write(&path, "MARKER_VAR=set_by_source\n").unwrap();
    set_var("TEST_RC_VAR", &path.to_string_lossy());
    source_runtime_file("testrc", Some("TEST_RC_VAR")).unwrap();
    assert_eq!(var!("MARKER_VAR"), "set_by_source");
  }

  #[test]
  fn missing_target_file_is_no_op() {
    let _g = TestGuard::new();
    set_var("TEST_RC_NONEXISTENT", "/path/that/should/never/exist/zzz");
    let res = source_runtime_file("nonexistent", Some("TEST_RC_NONEXISTENT"));
    assert!(res.is_ok());
  }

  #[test]
  fn env_var_unset_and_no_home_file_no_op() {
    let _g = TestGuard::new();
    // Point HOME to a tempdir with no matching file.
    let dir = tempfile::TempDir::new().unwrap();
    set_var("HOME", &dir.path().to_string_lossy());
    Shed::vars_mut(|v| v.unset_var("TEST_NOTHING").ok());
    let res = source_runtime_file("nothing", Some("TEST_NOTHING"));
    assert!(res.is_ok());
  }

  #[test]
  fn falls_back_to_home_dot_file_when_env_unset() {
    let _g = TestGuard::new();
    let dir = tempfile::TempDir::new().unwrap();
    let dotfile = dir.path().join(".my_test_rc");
    std::fs::write(&dotfile, "HOME_FALLBACK_MARKER=via_home\n").unwrap();
    set_var("HOME", &dir.path().to_string_lossy());
    Shed::vars_mut(|v| v.unset_var("ENV_NAME_NOT_SET").ok());
    source_runtime_file("my_test_rc", Some("ENV_NAME_NOT_SET")).unwrap();
    assert_eq!(var!("HOME_FALLBACK_MARKER"), "via_home");
  }
}

#[cfg(test)]
mod source_wrapper_tests {
  //! Thin one-liner wrappers that delegate to `source_runtime_file`
  //! with hardcoded (name, env_var) pairs. The tests verify that each
  //! wrapper uses the right env-var name — if any pair gets swapped,
  //! the assertion fails.

  use super::*;
  use crate::state::vars::{VarFlags, VarKind};
  use crate::tests::testutil::TestGuard;
  use crate::var;

  fn set_var(name: &str, val: &str) {
    Shed::vars_mut(|v| {
      v.set_var(name, VarKind::Str(val.into()), VarFlags::empty())
        .unwrap();
    });
  }

  #[test]
  fn source_rc_uses_shed_rc_env_var() {
    let _g = TestGuard::new();
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("rc.sh");
    std::fs::write(&path, "SOURCE_RC_MARKER=fired\n").unwrap();
    set_var("SHED_RC", &path.to_string_lossy());
    source_rc().unwrap();
    assert_eq!(var!("SOURCE_RC_MARKER"), "fired");
  }

  #[test]
  fn source_login_uses_shed_profile_env_var() {
    let _g = TestGuard::new();
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("profile.sh");
    std::fs::write(&path, "SOURCE_LOGIN_MARKER=fired\n").unwrap();
    set_var("SHED_PROFILE", &path.to_string_lossy());
    source_login().unwrap();
    assert_eq!(var!("SOURCE_LOGIN_MARKER"), "fired");
  }

  #[test]
  fn source_env_uses_shed_env_env_var() {
    let _g = TestGuard::new();
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("env.sh");
    std::fs::write(&path, "SOURCE_ENV_MARKER=fired\n").unwrap();
    set_var("SHED_ENV", &path.to_string_lossy());
    source_env().unwrap();
    assert_eq!(var!("SOURCE_ENV_MARKER"), "fired");
  }
}

#[cfg(test)]
mod lookup_cmd_tests {
  use super::*;
  use crate::tests::testutil::{TestGuard, has_cmd};

  #[test]
  fn lookup_returns_path_for_known_binary_with_hashall() {
    if !has_cmd("ls") {
      return;
    }
    let _g = TestGuard::new();
    crate::shopt_mut!(set.hashall = true);
    crate::state::util::try_hash();
    let path = lookup_cmd("ls");
    assert!(path.is_some(), "expected Some(path) for 'ls'");
    // Whatever the path is, it should end with "ls".
    let path = path.unwrap();
    assert_eq!(path.file_name().unwrap().to_string_lossy(), "ls");
  }

  #[test]
  fn lookup_returns_path_for_known_binary_without_hashall() {
    if !has_cmd("ls") {
      return;
    }
    let _g = TestGuard::new();
    crate::shopt_mut!(set.hashall = false);
    let path = lookup_cmd("ls");
    assert!(path.is_some(), "expected Some(path) for 'ls'");
  }

  #[test]
  fn lookup_returns_none_for_unknown_command() {
    let _g = TestGuard::new();
    assert!(lookup_cmd("definitely_not_a_real_binary_zzzqqq").is_none());
  }

  #[test]
  fn lookup_returns_none_for_builtin_name() {
    // `cd` resolves via which_util as UtilKind::Builtin, which the
    // filter in lookup_cmd rejects (only Command|File pass through).
    let _g = TestGuard::new();
    crate::shopt_mut!(set.hashall = true);
    crate::state::util::try_hash();
    assert!(lookup_cmd("cd").is_none());
  }
}

#[cfg(test)]
mod set_ver_info_tests {
  //! `set_ver_info` populates two shell vars from compile-time
  //! constants: `SHED_VERSION` (the Cargo.toml version string) and
  //! `SHED_VER_INFO` (an AssocArr with major/minor/patch/arch/os keys).
  //! The tests pin the structure rather than specific values, so they
  //! don't churn each version bump.

  use super::*;
  use crate::tests::testutil::TestGuard;
  use crate::var;

  #[test]
  fn sets_shed_version_to_cargo_pkg_version() {
    let _g = TestGuard::new();
    set_ver_info().unwrap();
    assert_eq!(var!("SHED_VERSION"), env!("CARGO_PKG_VERSION"));
  }

  #[test]
  fn ver_info_is_assoc_array_with_five_keys() {
    let _g = TestGuard::new();
    set_ver_info().unwrap();
    let kind = Shed::vars(|v| v.try_get_var_kind("SHED_VER_INFO"));
    match kind {
      Some(VarKind::AssocArr(items)) => {
        let keys: std::collections::HashSet<String> =
          items.iter().map(|(k, _)| k.clone()).collect();
        assert_eq!(keys.len(), 5, "got: {keys:?}");
        for expected in ["major", "minor", "patch", "arch", "os"] {
          assert!(
            keys.contains(expected),
            "missing key {expected}, got: {keys:?}"
          );
        }
      }
      other => panic!("expected AssocArr, got {other:?}"),
    }
  }

  #[test]
  fn ver_info_arch_and_os_match_compile_time_consts() {
    let _g = TestGuard::new();
    set_ver_info().unwrap();
    let items = match Shed::vars(|v| v.try_get_var_kind("SHED_VER_INFO")) {
      Some(VarKind::AssocArr(items)) => items,
      other => panic!("expected AssocArr, got {other:?}"),
    };
    let get = |k: &str| {
      items
        .iter()
        .find(|(key, _)| key == k)
        .map(|(_, v)| v.clone())
        .unwrap_or_default()
    };
    assert_eq!(get("arch"), std::env::consts::ARCH);
    assert_eq!(get("os"), std::env::consts::OS);
  }

  #[test]
  fn ver_info_semver_components_match_cargo_pkg_version() {
    let _g = TestGuard::new();
    set_ver_info().unwrap();
    let items = match Shed::vars(|v| v.try_get_var_kind("SHED_VER_INFO")) {
      Some(VarKind::AssocArr(items)) => items,
      other => panic!("expected AssocArr, got {other:?}"),
    };
    let get = |k: &str| {
      items
        .iter()
        .find(|(key, _)| key == k)
        .map(|(_, v)| v.clone())
        .unwrap_or_default()
    };
    let expected: Vec<&str> = env!("CARGO_PKG_VERSION").split('.').collect();
    assert_eq!(get("major"), expected[0]);
    assert_eq!(get("minor"), expected[1]);
    assert_eq!(get("patch"), expected[2]);
  }
}
