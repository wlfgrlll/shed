use super::{SHED, Shed};

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
  vars::{ArrIndex, VarFlags, VarKind},
};
use crate::state::vars::Var;

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
  std::env::var("COMP_WORDBREAKS").unwrap_or_else(|_| String::from("\"'><;|=&(:"))
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
  Shed::vars(|v| v.try_get_var("IFS")).unwrap_or(String::from(" \t\n"))
}

pub fn get_time_fmt() -> String {
  Shed::vars(|v| v.try_get_var("TIMEFMT"))
    .unwrap_or_else(|| String::from("\nreal\t%*E\nuser\t%*U\nsys\t%*S"))
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
  Shed::vars(|v| v.try_get_var("SHED_RC"))
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
    && let Some(path) = Shed::vars(|v| v.try_get_var(name))
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
  if let Ok(var) = std::env::var("SHLVL")
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
  let db_path = if let Ok(var) = std::env::var("SHED_HISTDB") {
    var
  } else {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
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
  Shed::vars(|v| v.try_get_var("HOME"))
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
