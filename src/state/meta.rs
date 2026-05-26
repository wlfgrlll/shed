use std::{
  collections::{HashMap, HashSet, VecDeque},
  fmt::Write,
  os::{
    fd::{AsFd, OwnedFd},
    unix::{fs::PermissionsExt, net::UnixStream},
  },
  path::{Path, PathBuf},
  rc::Rc,
  str::FromStr,
  sync::Arc,
  time::{Duration, Instant},
};

use super::{
  ShResult, Shed,
  builtin::BUILTIN_NAMES,
  crate_util as util,
  expand::{expand_keymap, glob_to_regex},
  jobs::Job,
  keys::KeyEvent,
  logic::AutoCmdKind,
  match_loop,
  readline::{Candidate, CompSpec, LineData},
  sherr, socket,
  util::query_db,
  var, writefd,
};
use itertools::izip;
use nix::{
  errno::Errno,
  libc::time_t,
  poll::PollTimeout,
  sys::{
    resource::{Usage, UsageWho, getrusage},
    time::TimeVal,
    wait::WaitStatus as WtStat,
  },
  unistd::{read, write},
};
use regex::Regex;

#[derive(Debug, Clone)]
pub(crate) struct CmdTimer {
  command: String,
  wall_start: Instant,
  self_usage_start: Option<Usage>,
  child_usage_start: Option<Usage>,
  wall_end: Option<Duration>,
  self_usage_end: Option<Usage>,
  child_usage_end: Option<Usage>,
  report_time: bool,
}

impl CmdTimer {
  pub fn new(command: String, report_time: bool) -> ShResult<Self> {
    let (self_usage_start, child_usage_start) = if report_time {
      (
        Some(getrusage(UsageWho::RUSAGE_SELF)?),
        Some(getrusage(UsageWho::RUSAGE_CHILDREN)?),
      )
    } else {
      (None, None)
    };
    Ok(Self {
      command,
      wall_start: Instant::now(),
      self_usage_start,
      child_usage_start,
      wall_end: None,
      self_usage_end: None,
      child_usage_end: None,
      report_time,
    })
  }

  pub fn stop(&mut self) -> ShResult<()> {
    self.wall_end = Some(self.wall_start.elapsed());
    if self.report_time {
      self.self_usage_end = Some(getrusage(UsageWho::RUSAGE_SELF)?);
      self.child_usage_end = Some(getrusage(UsageWho::RUSAGE_CHILDREN)?);
    }
    Ok(())
  }

  pub fn still_running(&self) -> bool {
    self.wall_end.is_none() && self.self_usage_end.is_none() && self.child_usage_end.is_none()
  }

  pub fn should_report(&self) -> bool {
    self.report_time
  }

  pub fn cpu_pct(&self) -> ShResult<f64> {
    if self.still_running() {
      return Err(sherr!(
        InternalErr,
        "attempt to get CPU percentage from a CmdTimer that is still running"
      ));
    }
    let total_user_secs = self.total_user_secs()?;
    let total_sys_secs = self.total_sys_secs()?;
    let total_wall_secs = self.wall_end.unwrap().as_secs_f64();

    if total_wall_secs > 0.0 {
      Ok(((total_user_secs + total_sys_secs) / total_wall_secs) * 100.0)
    } else {
      Ok(0.0)
    }
  }

  pub fn max_rss(&self) -> ShResult<i64> {
    if self.still_running() {
      return Err(sherr!(
        InternalErr,
        "attempt to get max RSS from a CmdTimer that is still running"
      ));
    }
    let self_r_maxrss = self.self_usage_end.unwrap().max_rss();
    let child_r_maxrss = self.child_usage_end.unwrap().max_rss();
    Ok(self_r_maxrss.max(child_r_maxrss))
  }

  pub fn command(&self) -> &str {
    &self.command
  }

  pub fn total_wall_ms(&self) -> ShResult<i64> {
    if self.still_running() {
      return Err(sherr!(
        InternalErr,
        "attempt to get wall time from a CmdTimer that is still running"
      ));
    }
    Ok(self.wall_end.unwrap().as_millis() as i64)
  }

  pub fn total_user_ms(&self) -> ShResult<i64> {
    if self.still_running() {
      return Err(sherr!(
        InternalErr,
        "attempt to get user time from a CmdTimer that is still running"
      ));
    }
    let self_user_delta =
      self.self_usage_end.unwrap().user_time() - self.self_usage_start.unwrap().user_time();
    let child_user_delta =
      self.child_usage_end.unwrap().user_time() - self.child_usage_start.unwrap().user_time();
    Ok(Self::tv_to_ms(self_user_delta) + Self::tv_to_ms(child_user_delta))
  }

  pub fn total_sys_ms(&self) -> ShResult<i64> {
    if self.still_running() {
      return Err(sherr!(
        InternalErr,
        "attempt to get system time from a CmdTimer that is still running"
      ));
    }
    let self_sys_delta =
      self.self_usage_end.unwrap().system_time() - self.self_usage_start.unwrap().system_time();
    let child_sys_delta =
      self.child_usage_end.unwrap().system_time() - self.child_usage_start.unwrap().system_time();
    Ok(Self::tv_to_ms(self_sys_delta) + Self::tv_to_ms(child_sys_delta))
  }

  pub fn total_user_secs(&self) -> ShResult<f64> {
    let ms = self.total_user_ms()?;
    let seconds = ms as f64 / 1000.0;

    Ok(seconds)
  }

  pub fn total_sys_secs(&self) -> ShResult<f64> {
    let ms = self.total_sys_ms()?;
    let seconds = ms as f64 / 1000.0;

    Ok(seconds)
  }

  pub fn tv_to_ms(tv: TimeVal) -> i64 {
    let sec_millis = (tv.tv_sec() * 1000) as time_t;
    let usec_millis = (tv.tv_usec() / 1000) as time_t;
    sec_millis + usec_millis
  }

  fn format_ms(total: i64) -> String {
    let millis = total % 1000;
    let total_secs = total / 1000;
    let secs = total_secs % 60;
    let total_mins = total_secs / 60;
    let mins = total_mins % 60;
    let hours = total_mins / 60;

    let mut result = String::new();
    if hours > 0 {
      write!(result, "{hours}h").unwrap();
    }
    write!(result, "{mins}m").unwrap();
    write!(result, "{secs}.{millis:03}").unwrap();
    result
  }

  pub fn total_wall_formatted(&self) -> ShResult<String> {
    if self.still_running() {
      return Err(sherr!(
        InternalErr,
        "attempt to get wall time from a CmdTimer that is still running"
      ));
    }
    let total_ms = self.total_wall_ms()?;
    Ok(Self::format_ms(total_ms))
  }
  pub fn total_user_formatted(&self) -> ShResult<String> {
    if self.still_running() {
      return Err(sherr!(
        InternalErr,
        "attempt to get user time from a CmdTimer that is still running"
      ));
    }
    let total_ms = self.total_user_ms()?;
    Ok(Self::format_ms(total_ms))
  }
  pub fn total_sys_formatted(&self) -> ShResult<String> {
    if self.still_running() {
      return Err(sherr!(
        InternalErr,
        "attempt to get system time from a CmdTimer that is still running"
      ));
    }
    let total_ms = self.total_sys_ms()?;
    Ok(Self::format_ms(total_ms))
  }

  pub fn format_report(&self, fmt_str: &str) -> ShResult<String> {
    if self.still_running() {
      return Err(sherr!(
        InternalErr,
        "attempt to format a CmdTimer that is still running"
      ));
    }

    let mut output = String::new();
    let mut chars = fmt_str.chars().peekable();

    match_loop!(chars.next() => ch, {
      '\\' => {
        if let Some(esc) = chars.next() {
          output.push(esc);
        }
      }
      '%' => {
        let Some(param) = chars.next() else { break; };
        match param {
          'm' => {
            let Some(param2) = chars.next() else { break; };
            let millis = match param2 {
              'E' => self.wall_end.unwrap().as_millis() as i64,
              'U' => (self.total_user_secs()? * 1000.0) as i64,
              'S' => (self.total_sys_secs()? * 1000.0) as i64,
              _ => {
                output.push('%');
                output.push('m');
                output.push(param2);
                continue;
              }
            };

            write!(output, "{millis}").unwrap();
          }
          'u' => {
            let Some(param2) = chars.next() else { break; };
            let micros = match param2 {
              'E' => self.wall_end.unwrap().as_micros() as i64,
              'U' => (self.total_user_secs()? * 1_000_000.0).floor() as i64,
              'S' => (self.total_sys_secs()? * 1_000_000.0).floor() as i64,
              _ => {
                output.push('%');
                output.push('u');
                output.push(param2);
                continue;
              }
            };

            write!(output, "{micros}").unwrap();
          }
          '*' => {
            let Some(param2) = chars.next() else { break; };
            let millis = match param2 {
              'E' => self.wall_end.unwrap().as_millis() as i64,
              'U' => (self.total_user_secs()? * 1000.0) as i64,
              'S' => (self.total_sys_secs()? * 1000.0) as i64,
              _ => {
                output.push('%');
                output.push('*');
                output.push(param2);
                continue;
              }
            };
            output.push_str(&Self::format_ms(millis));
          }
          'E' => {
            // real seconds
            let secs = self.wall_end.unwrap().as_secs();
            write!(output, "{secs}").unwrap();
          }
          'U' => {
            // CPU user mode seconds
            let total = self.total_user_secs()?;

            write!(output, "{total}").unwrap();
          }
          'S' => {
            // CPU kernel mode seconds
            let total = self.total_sys_secs()?;

            write!(output, "{total}").unwrap();
          }
          'P' => {
            // CPU percentage ((user + sys) / real * 100)
            let total_user_secs = self.total_user_secs()?;
            let total_sys_secs = self.total_sys_secs()?;
            let total_wall_secs = self.wall_end.unwrap().as_secs_f64();

            if total_wall_secs > 0.0 {
              let percentage = ((total_user_secs + total_sys_secs) / total_wall_secs) * 100.0;

              write!(output, "{percentage:.2}%").unwrap();
            } else {
              write!(output, "0.00%").unwrap();
            }
          }
          'M' => {
            // max resident set size
            let self_r_maxrss = self.self_usage_end.unwrap().max_rss();
            let child_r_maxrss = self.child_usage_end.unwrap().max_rss();
            let maxrss = self_r_maxrss.max(child_r_maxrss);

            write!(output, "{maxrss}").unwrap();
          }
          'J' => {
            // command name
            output.push_str(&self.command);
          }
          _ => {
            output.push('%');
            output.push(param);
            break
          }
        };
      }
      _ => output.push(ch),
    });

    Ok(output)
  }
}

#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub(crate) enum UtilKind {
  Alias,
  Function,
  Builtin,
  Command(PathBuf),
  File(PathBuf),
}

#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub(crate) struct Utility {
  name: String,
  kind: UtilKind,
}

impl Utility {
  pub fn alias(name: String) -> Self {
    Self {
      name,
      kind: UtilKind::Alias,
    }
  }
  pub fn function(name: String) -> Self {
    Self {
      name,
      kind: UtilKind::Function,
    }
  }
  pub fn builtin(name: String) -> Self {
    Self {
      name,
      kind: UtilKind::Builtin,
    }
  }
  pub fn command(name: String, path: PathBuf) -> Self {
    Self {
      name,
      kind: UtilKind::Command(path),
    }
  }
  pub fn file(name: String, path: PathBuf) -> Self {
    Self {
      name,
      kind: UtilKind::File(path),
    }
  }
  pub fn name(&self) -> &str {
    &self.name
  }
  pub fn kind(&self) -> &UtilKind {
    &self.kind
  }
}

/// Automatically manages loop depth in the meta table.
///
/// When dropped, decrements the loop depth in the meta table.
pub(crate) struct LoopGuard;
impl Drop for LoopGuard {
  fn drop(&mut self) {
    Shed::meta_mut(|m| m.leave_loop())
  }
}

/// Automatically manages function depth in the meta table.
///
/// When dropped, decrements the function depth in the meta table.
pub(crate) struct FuncGuard;
impl Drop for FuncGuard {
  fn drop(&mut self) {
    Shed::meta_mut(|m| m.leave_func())
  }
}

/// Miscellaneous global data storage
#[derive(Debug)]
pub(crate) struct MetaTab {
  // Time when the shell was started, used for calculating shell uptime
  shell_time: Instant,
  // whether or not we initially started as an interactive shell
  // not to be confused with interactive context guarding with Terminal and TermGuard
  interactive_shell: bool,

  // command running duration
  runtime_start: Option<Instant>,
  runtime_stop: Option<Instant>,

  socket: Option<Arc<socket::ShedSocket>>,
  subscribers: Vec<Arc<UnixStream>>,
  last_job: Option<Job>,

  // pushd/popd stack
  dir_stack: VecDeque<PathBuf>,
  // getopts char offset for opts like -abc
  getopts_offset: usize,

  old_path: Option<String>,
  old_pwd: Option<String>,
  // regex cache - patterns we have seen before
  regexes: HashMap<String, Regex>,
  // utility cache - commands, functions, aliases, etc
  util_cache: HashSet<Rc<Utility>>,
  // programmable completion specs
  comp_specs: HashMap<String, Box<dyn CompSpec>>,

  // stack of currently open procsubs
  procsub_stack: Vec<Vec<OwnedFd>>,

  // pending keys from widget function
  pending_widget_keys: Vec<KeyEvent>,

  func_depth: usize,
  loop_depth: usize,

  // completion candidates given by compadd
  comp_add_candidates: Vec<Candidate>,

  // whether or not the last command had a function definition
  last_was_func_def: bool,

  main_loop_timeout: Option<PollTimeout>,

  ignore_hist: bool,
}

impl Clone for MetaTab {
  fn clone(&self) -> Self {
    Self {
      shell_time: self.shell_time,
      interactive_shell: self.interactive_shell,
      runtime_start: self.runtime_start,
      runtime_stop: self.runtime_stop,
      socket: self.socket.clone(),
      subscribers: self.subscribers.clone(),
      last_job: self.last_job.clone(),
      dir_stack: self.dir_stack.clone(),
      getopts_offset: self.getopts_offset,
      old_path: self.old_path.clone(),
      old_pwd: self.old_pwd.clone(),
      loop_depth: self.loop_depth,
      func_depth: self.func_depth,
      comp_add_candidates: self.comp_add_candidates.clone(),
      regexes: self.regexes.clone(),
      util_cache: self.util_cache.clone(),
      comp_specs: self.comp_specs.clone(),
      pending_widget_keys: self.pending_widget_keys.clone(),
      last_was_func_def: self.last_was_func_def,
      main_loop_timeout: self.main_loop_timeout,
      ignore_hist: self.ignore_hist,

      procsub_stack: vec![], // does not implement clone
    }
  }
}

impl Default for MetaTab {
  fn default() -> Self {
    Self {
      shell_time: Instant::now(),
      interactive_shell: false,
      runtime_start: None,
      runtime_stop: None,
      socket: None,
      subscribers: vec![],
      last_job: None,
      dir_stack: VecDeque::new(),
      getopts_offset: 0,
      old_path: None,
      old_pwd: None,
      loop_depth: 0,
      func_depth: 0,
      procsub_stack: vec![],
      comp_add_candidates: vec![],
      regexes: HashMap::new(),
      util_cache: HashSet::new(),
      comp_specs: HashMap::new(),
      pending_widget_keys: vec![],
      last_was_func_def: false,
      main_loop_timeout: None,
      ignore_hist: false,
    }
  }
}

pub(crate) struct ProcSubGuard;
impl Drop for ProcSubGuard {
  fn drop(&mut self) {
    Shed::meta_mut(|m| m.pop_procsub_frame())
  }
}

impl MetaTab {
  pub fn new() -> Self {
    Self::default()
  }

  /// Set a poll timeout for the main loop to use
  ///
  /// This is used mainly for managing status message lifetimes.
  /// If a status message is showing below the prompt, the timeout
  /// will trigger a redraw and clear it.
  pub fn set_poll_timeout(&mut self, timeout: Option<PollTimeout>) {
    self.main_loop_timeout = timeout;
  }
  pub fn take_poll_timeout(&mut self) -> Option<PollTimeout> {
    self.main_loop_timeout.take()
  }

  pub fn push_procsub_frame(&mut self) -> ProcSubGuard {
    self.procsub_stack.push(vec![]);
    ProcSubGuard
  }
  pub fn set_no_hist_save(&mut self) {
    self.ignore_hist = true;
  }

  pub fn no_hist_save(&mut self) -> bool {
    std::mem::take(&mut self.ignore_hist)
  }

  pub fn pop_procsub_frame(&mut self) {
    self.procsub_stack.pop();
  }

  pub fn save_procsub_fd(&mut self, fd: OwnedFd) {
    if self.procsub_stack.is_empty() {
      self.procsub_stack.push(vec![]);
    }
    if let Some(frame) = self.procsub_stack.last_mut() {
      frame.push(fd);
    }
  }

  pub fn shell_time(&self) -> Instant {
    self.shell_time
  }
  pub fn ensure_meta_table(&self) -> ShResult<()> {
    query_db(|conn| {
      conn.execute(
        "CREATE TABLE IF NOT EXISTS meta (
					key TEXT PRIMARY KEY,
					value TEXT NOT NULL
				)",
        [],
      )?;
      Ok(())
    })?;
    Ok(())
  }
  pub fn disable_welcome_message(&self) -> ShResult<()> {
    query_db(|conn| {
      conn.execute(
        "INSERT INTO meta (key, value) VALUES ('show_welcome', '0')
				ON CONFLICT(key) DO UPDATE SET value='0' WHERE key='welcome_message'",
        [],
      )?;
      Ok(())
    })?;
    Ok(())
  }
  pub fn enter_loop(&mut self) -> LoopGuard {
    self.loop_depth += 1;

    LoopGuard
  }
  pub fn leave_loop(&mut self) {
    if self.loop_depth > 0 {
      self.loop_depth -= 1;
    }
  }
  pub fn enter_func(&mut self) -> FuncGuard {
    self.func_depth += 1;

    FuncGuard
  }
  pub fn leave_func(&mut self) {
    if self.func_depth > 0 {
      self.func_depth -= 1;
    }
  }
  pub fn in_loop(&self) -> bool {
    self.loop_depth > 0
  }
  pub fn in_func(&self) -> bool {
    self.func_depth > 0
  }
  pub fn func_depth(&self) -> usize {
    self.func_depth
  }
  pub fn welcome_message(&self, force: bool) -> Option<String> {
    let res = query_db(|conn| {
      let result = conn.query_row(
        "SELECT value FROM meta WHERE key='show_welcome'",
        [],
        |row| row.get::<_, String>(0),
      );
      match result {
        Ok(val) => Ok(Some(val)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
      }
    })
    .ok()
    .flatten()
    .flatten();

    if res.is_some_and(|r| r == "0") && !force {
      return None;
    }

    let content_lines = [
      "",
      "\x1b[1mWelcome to shed!\x1b[0m",
      "",
      "Type \x1b[33mhelp\x1b[0m to get started.",
      "",
    ];

    let mut longest = -1;
    content_lines.iter().for_each(|l| {
      if longest < (l.len() as i32) {
        longest = l.len() as i32;
      }
    });
    let longest = longest as usize;

    let version = env!("CARGO_PKG_VERSION");

    let mut buf = String::new();

    // ╭─ shed v0.xx.x ───────────╮
    let title = format!(
      "{}{} \x1b[1;35mshed\x1b[0m v{} ",
      util::TOP_LEFT,
      util::HOR_LINE,
      version
    );
    util::pad_line_into(&mut buf, &title, util::HOR_LINE, util::TOP_RIGHT, longest);
    buf.push('\n');

    for line in &content_lines {
      let row = format!("{} {}", util::VERT_LINE, line);
      util::pad_line_into(&mut buf, &row, " ", util::VERT_LINE, longest);
      buf.push('\n');
    }

    // ╰──────────────────────────╯
    write!(
      buf,
      "{}{}{}",
      util::BOT_LEFT,
      util::HOR_LINE.repeat(longest.saturating_sub(2)),
      util::BOT_RIGHT
    )
    .unwrap();

    Some(buf)
  }
  pub fn set_pending_widget_keys(&mut self, keys: &str) {
    let exp = expand_keymap(keys);
    self.pending_widget_keys = exp;
  }
  pub fn get_regex(&mut self, pat: String) -> Result<Regex, String> {
    if let Some(regex) = self.regexes.get(&pat) {
      Ok(regex.clone())
    } else {
      let regex = match Regex::new(&pat) {
        Ok(re) => re,
        Err(e) => return Err(e.to_string()),
      };
      self.regexes.insert(pat, regex.clone());
      Ok(regex)
    }
  }
  pub fn get_glob_regex(&mut self, pat: String, anchored: bool) -> Regex {
    if let Some(regex) = self.regexes.get(&pat) {
      regex.clone()
    } else {
      let regex = glob_to_regex(&pat, anchored);
      self.regexes.insert(pat, regex.clone());
      regex
    }
  }
  pub fn take_pending_widget_keys(&mut self) -> Option<Vec<KeyEvent>> {
    if self.pending_widget_keys.is_empty() {
      None
    } else {
      Some(std::mem::take(&mut self.pending_widget_keys))
    }
  }
  pub fn set_last_job(&mut self, job: Option<Job>) {
    self.last_job = job;
  }
  pub fn last_job(&self) -> Option<&Job> {
    self.last_job.as_ref()
  }
  pub fn getopts_char_offset(&self) -> usize {
    self.getopts_offset
  }
  pub fn inc_getopts_char_offset(&mut self) -> usize {
    let offset = self.getopts_offset;
    self.getopts_offset += 1;
    offset
  }
  pub fn reset_getopts_char_offset(&mut self) {
    self.getopts_offset = 0;
  }
  pub fn cached_utils(&self) -> impl Iterator<Item = Rc<Utility>> {
    self.util_cache.iter().cloned()
  }
  pub fn comp_specs(&self) -> &HashMap<String, Box<dyn CompSpec>> {
    &self.comp_specs
  }
  pub fn get_comp_spec(&self, cmd: &str) -> Option<Box<dyn CompSpec>> {
    self.comp_specs.get(cmd).cloned()
  }
  pub fn set_comp_spec(&mut self, cmd: String, spec: Box<dyn CompSpec>) {
    self.comp_specs.insert(cmd, spec);
  }
  pub fn remove_comp_spec(&mut self, cmd: &str) -> bool {
    self.comp_specs.remove(cmd).is_some()
  }
  pub fn cache_contains(&self, cmd: &str) -> bool {
    self.util_cache.iter().any(|util| util.name() == cmd)
  }
  pub fn get_cached_cmd(&self, cmd: &str) -> Option<Rc<Utility>> {
    // used when the hashall option is set
    // and we use cached command paths for the execve system call
    self
      .util_cache
      .iter()
      .find(|util| util.name() == cmd && matches!(util.kind(), UtilKind::Command(_)))
      .cloned()
  }
  pub fn set_last_was_func_def(&mut self, was_func_def: bool) {
    self.last_was_func_def = was_func_def;
  }
  pub fn take_last_was_func_def(&mut self) -> bool {
    std::mem::take(&mut self.last_was_func_def)
  }
  pub fn get_cmds_in_path() -> Vec<Rc<Utility>> {
    let path = var!("PATH");
    let paths = path.split(":").map(PathBuf::from);
    let mut seen = HashSet::new();
    let mut cmds = vec![];
    for path in paths {
      if let Ok(entries) = path.read_dir() {
        for entry in entries.flatten() {
          // file_type() is free (uses dirent.d_type when available). For
          // regular files, entry.metadata() avoids the full-path resolution
          // that std::fs::metadata(entry.path()) would do. For symlinks we
          // must use the path-based stat so the target is followed (PATH
          // entries on NixOS / Homebrew are routinely symlink farms).
          let ft = entry.file_type().ok();
          let is_symlink = ft.is_some_and(|t| t.is_symlink());
          let meta = if is_symlink {
            std::fs::metadata(entry.path())
          } else {
            entry.metadata()
          };
          let Ok(meta) = meta else {
            continue;
          };
          let is_exec = meta.permissions().mode() & 0o111 != 0;

          if meta.is_file()
            && is_exec
            && let Some(name) = entry.file_name().to_str()
            && seen.insert(name.to_string())
          {
            let util = Utility::command(name.to_string(), entry.path());
            cmds.push(util.into());
          }
        }
      }
    }
    cmds
  }
  pub fn get_exec_files_in_cwd() -> Vec<Rc<Utility>> {
    let cwd = var!("PWD");
    let mut files = vec![];
    if let Ok(entries) = Path::new(&cwd).read_dir() {
      for entry in entries.flatten() {
        let ft = entry.file_type().ok();
        let is_symlink = ft.is_some_and(|t| t.is_symlink());
        let meta = if is_symlink {
          std::fs::metadata(entry.path())
        } else {
          entry.metadata()
        };
        let Ok(meta) = meta else {
          continue;
        };
        let is_exec = meta.permissions().mode() & 0o111 != 0;

        if meta.is_file()
          && is_exec
          && let Some(name) = entry.file_name().to_str()
        {
          let util = Utility::file(name.to_string(), entry.path());
          files.push(util.into());
        }
      }
    }
    files
  }
  pub fn create_socket(&mut self) -> ShResult<()> {
    let sock = socket::ShedSocket::new()?;
    self.socket = Some(sock.into());
    Ok(())
  }
  pub fn get_socket(&self) -> Option<Arc<socket::ShedSocket>> {
    self.socket.as_ref().cloned()
  }
  pub fn read_socket(&mut self) -> ShResult<Vec<(UnixStream, socket::SocketRequest)>> {
    let mut requests = vec![];
    let Some(listener) = self.get_socket() else {
      return Ok(requests);
    };

    while let Ok((conn, _)) = listener.listener().accept()
      && let Some(req) = self.read_request(&conn)
    {
      requests.push((conn, req));
    }

    Ok(requests)
  }
  pub fn read_request(&self, conn: &UnixStream) -> Option<socket::SocketRequest> {
    conn.set_nonblocking(false).ok();
    let mut bytes = vec![];
    loop {
      let mut buffer = [0u8; 1024];
      match read(conn, &mut buffer) {
        Ok(0) => break,
        Ok(n) => {
          if let Some(pos) = buffer[..n].iter().position(|&b| b == b'\n') {
            bytes.extend_from_slice(&buffer[..pos]);
            break;
          }
          bytes.extend_from_slice(&buffer[..n]);
        }
        Err(Errno::EINTR) => continue,
        Err(e) => {
          writefd!(conn, "error>> failed to parse request: {e}\n").ok();
          break;
        }
      }
    }
    let input = String::from_utf8_lossy(&bytes).to_string();
    let request = match socket::SocketRequest::from_str(&input) {
      Ok(req) => req,
      Err(e) => {
        write(
          conn,
          format!("error>> failed to parse request: {e}\n").as_bytes(),
        )
        .ok();
        return None;
      }
    };

    Some(request)
  }
  pub fn push_subscriber(&mut self, subscriber: UnixStream) {
    self.subscribers.push(Arc::new(subscriber));
  }
  pub fn notify_autocmd(&self, kind: AutoCmdKind) -> ShResult<()> {
    for subscriber in &self.subscribers {
      write(subscriber, format!("autocmd_event>>{kind}\n").as_bytes()).ok();
    }

    Ok(())
  }
  pub fn num_subscribers(&self) -> usize {
    self.subscribers.len()
  }
  fn broadcast<F: FnMut(&Arc<UnixStream>) -> std::io::Result<()>>(&mut self, mut f: F) {
    let mut dead = vec![];
    for (i, subscriber) in self.subscribers.iter().enumerate() {
      if f(subscriber).is_err() {
        dead.push(i);
      }
    }

    for i in dead.into_iter().rev() {
      self.subscribers.remove(i);
    }
  }
  pub fn notify_job_complete(&mut self, job: &Job) -> ShResult<()> {
    let id = job.tabid().map(|i| (i + 1).to_string()).unwrap_or_default();
    let pids = job.get_pids();
    let stats = job.get_stats();
    let cmds = job.get_cmds();

    self.broadcast(|sub| {
      let mut buf = format!("job>>begin>>{id} {}\n", pids.len());
      for (pid, stat, cmd) in izip!(&pids, &stats, &cmds) {
        let stat_str = match stat {
          WtStat::Exited(_, 0) => "done".to_string(),
          WtStat::Exited(_, n) => format!("failed:{n}"),
          WtStat::Signaled(_, sig, _) => format!("signaled:{sig:?}"),
          other => format!("{other:?}"),
        };
        buf.push_str(&format!("job>>child>>{pid} {stat_str} {cmd}\n"));
      }
      writefd!(sub, "{buf}")?;
      Ok(())
    });
    Ok(())
  }
  pub fn notify_line_edit(&mut self, data: LineData) -> ShResult<()> {
    let LineData {
      buffer,
      cursor,
      anchor,
      hint,
      mode,
    } = data;

    self.broadcast(|sub| {
      let mut buf = String::new();
      buf.push_str(&format!("line>>buffer>>{buffer}\n"));
      buf.push_str(&format!("line>>cursor>>{cursor}\n"));
      if let Some(anchor) = anchor {
        buf.push_str(&format!("line>>anchor>>{anchor}\n"));
      }
      if let Some(hint) = &hint {
        buf.push_str(&format!("line>>hint>>{hint}\n"));
      }
      buf.push_str(&format!("line>>mode>>{mode}\n"));

      write(sub, buf.as_bytes())?;
      Ok(())
    });

    Ok(())
  }
  pub fn notify_key_event(&mut self, event: KeyEvent) -> ShResult<()> {
    let seq = event.as_vim_seq()?;

    self.broadcast(|sub| {
      let buf = format!("line>>key_event>>{seq}\n");
      write(sub, buf.as_bytes())?;
      Ok(())
    });

    Ok(())
  }
  pub fn cache_util(&mut self, util: Rc<Utility>) {
    self.util_cache.insert(util);
  }
  pub fn clear_cached_files(&mut self) {
    self
      .util_cache
      .retain(|util| !matches!(util.kind(), UtilKind::File(_)));
  }
  pub fn clear_cached_cmds(&mut self) {
    self
      .util_cache
      .retain(|util| !matches!(util.kind(), UtilKind::Command(_)));
  }
  pub fn clear_cached_aliases(&mut self) {
    self
      .util_cache
      .retain(|util| !matches!(util.kind(), UtilKind::Alias));
  }
  pub fn clear_cached_functions(&mut self) {
    self
      .util_cache
      .retain(|util| !matches!(util.kind(), UtilKind::Function));
  }
  pub fn clear_cached_builtins(&mut self) {
    self
      .util_cache
      .retain(|util| !matches!(util.kind(), UtilKind::Builtin));
  }
  pub fn clear_cache(&mut self) {
    self.util_cache.clear();
  }
  pub fn rehash_path(&mut self) {
    let path = var!("PATH");
    self.clear_cached_cmds();
    self.old_path = Some(path.clone());
    let cmds_in_path = Self::get_cmds_in_path();
    for cmd in cmds_in_path {
      self.cache_util(cmd);
    }
  }
  pub fn rehash_cwd(&mut self) {
    let cwd = var!("PWD");
    self.clear_cached_files();
    self.old_pwd = Some(cwd.clone());
    let exec_files_in_cwd = Self::get_exec_files_in_cwd();
    for file in exec_files_in_cwd {
      self.cache_util(file);
    }
  }
  pub fn rehash_internals(&mut self) {
    Shed::logic_mut(|l| {
      if !l.dirty() {
        return;
      }
      self.clear_cached_aliases();
      self.clear_cached_functions();
      self.clear_cached_builtins();
      let funcs = l.funcs();
      let aliases = l.aliases();
      for func in funcs.keys() {
        let util = Utility::function(func.to_string());
        self.cache_util(util.into());
      }
      for alias in aliases.keys() {
        let util = Utility::alias(alias.to_string());
        self.cache_util(util.into());
      }
      l.set_dirty(false);
    });

    for cmd in BUILTIN_NAMES {
      let util = Utility::builtin(cmd.to_string());
      self.cache_util(util.into());
    }
  }
  pub fn rehash(&mut self) {
    self.rehash_path();
    self.rehash_cwd();
    self.rehash_internals();
  }
  pub fn try_rehash_utils(&mut self) {
    let path = var!("PATH");
    let cwd = var!("PWD");
    if self.old_path.as_ref().is_none_or(|old| *old != path) {
      self.rehash_path();
    }
    if self.old_pwd.as_ref().is_none_or(|old| *old != cwd) {
      self.rehash_cwd();
    }
    self.rehash_internals();
  }
  pub fn start_timer(&mut self) {
    self.runtime_start = Some(Instant::now());
  }
  pub fn stop_timer(&mut self) -> Option<Duration> {
    self.runtime_stop = Some(Instant::now());
    self.get_time()
  }
  pub fn get_time(&self) -> Option<Duration> {
    if let (Some(start), Some(stop)) = (self.runtime_start, self.runtime_stop) {
      Some(stop.duration_since(start))
    } else {
      None
    }
  }
  pub fn comp_add(&mut self, candidate: Candidate) {
    self.comp_add_candidates.push(candidate);
  }
  pub fn take_comp_candidates(&mut self) -> Vec<Candidate> {
    std::mem::take(&mut self.comp_add_candidates)
  }
  pub fn set_interactive_shell(&mut self, interactive: bool) {
    self.interactive_shell = interactive;
  }
  pub fn interactive_shell(&self) -> bool {
    self.interactive_shell
  }
  pub fn push_dir(&mut self, path: PathBuf) {
    self.dir_stack.push_front(path);
  }
  pub fn pop_dir(&mut self) -> Option<PathBuf> {
    self.dir_stack.pop_front()
  }
  pub fn dirs(&self) -> &VecDeque<PathBuf> {
    &self.dir_stack
  }
  pub fn dirs_mut(&mut self) -> &mut VecDeque<PathBuf> {
    &mut self.dir_stack
  }
}

#[cfg(test)]
mod read_request_tests {
  use crate::socket::SocketRequest;
  use crate::state::Shed;
  use crate::tests::testutil::TestGuard;
  use std::io::Write;
  use std::os::unix::net::UnixStream;

  /// Set up a UnixStream pair; the test writes a request to `writer`
  /// then calls read_request on `reader`.
  fn pair() -> (UnixStream, UnixStream) {
    UnixStream::pair().unwrap()
  }

  #[test]
  fn read_request_subscribe() {
    let _g = TestGuard::new();
    let (mut writer, reader) = pair();
    writer.write_all(b"subscribe\n").unwrap();
    let req = Shed::meta(|m| m.read_request(&reader)).unwrap();
    assert!(matches!(req, SocketRequest::Subscribe));
  }

  #[test]
  fn read_request_redraw() {
    let _g = TestGuard::new();
    let (mut writer, reader) = pair();
    writer.write_all(b"redraw\n").unwrap();
    let req = Shed::meta(|m| m.read_request(&reader)).unwrap();
    assert!(matches!(req, SocketRequest::RefreshPrompt));
  }

  #[test]
  fn read_request_msg_system() {
    let _g = TestGuard::new();
    let (mut writer, reader) = pair();
    writer.write_all(b"msg::system::hello world\n").unwrap();
    let req = Shed::meta(|m| m.read_request(&reader)).unwrap();
    match req {
      SocketRequest::PostSystemMessage(msg) => assert_eq!(msg, "hello world"),
      other => panic!("expected PostSystemMessage, got {other:?}"),
    }
  }

  #[test]
  fn read_request_msg_status() {
    let _g = TestGuard::new();
    let (mut writer, reader) = pair();
    writer.write_all(b"msg::status::saved\n").unwrap();
    let req = Shed::meta(|m| m.read_request(&reader)).unwrap();
    match req {
      SocketRequest::PostStatusMessage(msg) => assert_eq!(msg, "saved"),
      other => panic!("expected PostStatusMessage, got {other:?}"),
    }
  }

  #[test]
  fn read_request_invalid_returns_none_after_write_error() {
    let _g = TestGuard::new();
    let (mut writer, reader) = pair();
    writer.write_all(b"nonsense_request\n").unwrap();
    let req = Shed::meta(|m| m.read_request(&reader));
    assert!(req.is_none());
  }

  #[test]
  fn read_request_handles_buffered_split_across_reads() {
    let _g = TestGuard::new();
    let (mut writer, reader) = pair();
    // Write in two chunks to exercise the loop's accumulator branch.
    writer.write_all(b"subscr").unwrap();
    writer.write_all(b"ibe\n").unwrap();
    let req = Shed::meta(|m| m.read_request(&reader)).unwrap();
    assert!(matches!(req, SocketRequest::Subscribe));
  }

  #[test]
  fn read_request_eof_without_newline_parses_buffered() {
    let _g = TestGuard::new();
    let (writer, reader) = pair();
    {
      let mut w = writer;
      w.write_all(b"subscribe").unwrap();
      // Drop writer → EOF on reader side; the loop's Ok(0) branch
      // fires and we parse what we have.
    }
    let req = Shed::meta(|m| m.read_request(&reader)).unwrap();
    assert!(matches!(req, SocketRequest::Subscribe));
  }
}

#[cfg(test)]
mod cmd_timer_tests {
  //! Coverage targets the cold parts of CmdTimer: the still_running
  //! Err returns on every reporting method, the `hours > 0` branch in
  //! format_ms, and the format_report %-spec branches.

  use super::*;
  use crate::tests::testutil::TestGuard;

  fn running_timer() -> CmdTimer {
    CmdTimer::new("test_cmd".into(), true).unwrap()
  }

  fn stopped_timer() -> CmdTimer {
    let mut t = CmdTimer::new("test_cmd".into(), true).unwrap();
    t.stop().unwrap();
    t
  }

  // ===================== still_running guards =====================

  #[test]
  fn cpu_pct_errors_when_still_running() {
    let _g = TestGuard::new();
    assert!(running_timer().cpu_pct().is_err());
  }

  #[test]
  fn max_rss_errors_when_still_running() {
    let _g = TestGuard::new();
    assert!(running_timer().max_rss().is_err());
  }

  #[test]
  fn total_wall_ms_errors_when_still_running() {
    let _g = TestGuard::new();
    assert!(running_timer().total_wall_ms().is_err());
  }

  #[test]
  fn total_user_ms_errors_when_still_running() {
    let _g = TestGuard::new();
    assert!(running_timer().total_user_ms().is_err());
  }

  #[test]
  fn total_sys_ms_errors_when_still_running() {
    let _g = TestGuard::new();
    assert!(running_timer().total_sys_ms().is_err());
  }

  #[test]
  fn total_wall_formatted_errors_when_still_running() {
    let _g = TestGuard::new();
    assert!(running_timer().total_wall_formatted().is_err());
  }

  #[test]
  fn total_user_formatted_errors_when_still_running() {
    let _g = TestGuard::new();
    assert!(running_timer().total_user_formatted().is_err());
  }

  #[test]
  fn total_sys_formatted_errors_when_still_running() {
    let _g = TestGuard::new();
    assert!(running_timer().total_sys_formatted().is_err());
  }

  #[test]
  fn format_report_errors_when_still_running() {
    let _g = TestGuard::new();
    assert!(running_timer().format_report("%E").is_err());
  }

  // ===================== format_ms =====================

  #[test]
  fn format_ms_zero() {
    assert_eq!(CmdTimer::format_ms(0), "0m0.000");
  }

  #[test]
  fn format_ms_sub_second_pads_millis() {
    assert_eq!(CmdTimer::format_ms(7), "0m0.007");
    assert_eq!(CmdTimer::format_ms(123), "0m0.123");
  }

  #[test]
  fn format_ms_seconds_only() {
    assert_eq!(CmdTimer::format_ms(45_000), "0m45.000");
  }

  #[test]
  fn format_ms_with_minutes_and_seconds() {
    // 5 min 30.250s
    assert_eq!(CmdTimer::format_ms(5 * 60_000 + 30_250), "5m30.250");
  }

  #[test]
  fn format_ms_includes_hours_when_over_one_hour() {
    // Exercises the `if hours > 0 { write!(result, "{hours}h") }` branch
    // that was uncovered. 2h 15m 7.500s.
    let total = 2 * 3_600_000 + 15 * 60_000 + 7_500;
    assert_eq!(CmdTimer::format_ms(total), "2h15m7.500");
  }

  // ===================== format_report happy paths =====================

  #[test]
  fn format_report_literal_text_passes_through() {
    let _g = TestGuard::new();
    let t = stopped_timer();
    assert_eq!(t.format_report("hello world").unwrap(), "hello world");
  }

  #[test]
  fn format_report_backslash_escapes_next_char() {
    // `\X` consumes the backslash and pushes X verbatim — no special
    // interpretation (so \n is the literal char 'n').
    let _g = TestGuard::new();
    let t = stopped_timer();
    assert_eq!(t.format_report("\\n").unwrap(), "n");
    assert_eq!(t.format_report("a\\\\b").unwrap(), "a\\b");
  }

  #[test]
  fn format_report_j_emits_command_name() {
    let _g = TestGuard::new();
    let t = stopped_timer();
    assert_eq!(t.format_report("%J").unwrap(), "test_cmd");
  }

  #[test]
  fn format_report_e_emits_wall_seconds() {
    let _g = TestGuard::new();
    let t = stopped_timer();
    let out = t.format_report("%E").unwrap();
    assert!(out.chars().all(|c| c.is_ascii_digit()), "got: {out:?}");
  }

  #[test]
  fn format_report_u_and_s_emit_seconds() {
    let _g = TestGuard::new();
    let t = stopped_timer();
    assert!(!t.format_report("%U").unwrap().is_empty());
    assert!(!t.format_report("%S").unwrap().is_empty());
  }

  #[test]
  fn format_report_p_emits_percentage_with_trailing_pct() {
    let _g = TestGuard::new();
    let t = stopped_timer();
    let out = t.format_report("%P").unwrap();
    assert!(out.ends_with('%'), "got: {out:?}");
  }

  #[test]
  fn format_report_m_emits_maxrss() {
    let _g = TestGuard::new();
    let t = stopped_timer();
    let out = t.format_report("%M").unwrap();
    // Just digits (or possibly a sign on weird platforms).
    assert!(
      out.chars().all(|c| c.is_ascii_digit() || c == '-'),
      "got: {out:?}"
    );
  }

  #[test]
  fn format_report_ms_and_us_subspecs_emit_integer_strings() {
    // %mE / %mU / %mS — wall/user/sys in milliseconds.
    // %uE / %uU / %uS — wall/user/sys in microseconds.
    let _g = TestGuard::new();
    let t = stopped_timer();
    for spec in ["%mE", "%mU", "%mS", "%uE", "%uU", "%uS"] {
      let out = t.format_report(spec).unwrap();
      assert!(
        out.chars().all(|c| c.is_ascii_digit() || c == '-'),
        "{spec} → {out:?}"
      );
    }
  }

  #[test]
  fn format_report_star_routes_through_format_ms() {
    // %*E / %*U / %*S all run their ms value through CmdTimer::format_ms.
    // We pinned format_ms's shape above ("Xm" + "Y.ZZZ"), so the output
    // here must contain at least an 'm' and a '.'.
    let _g = TestGuard::new();
    let t = stopped_timer();
    for spec in ["%*E", "%*U", "%*S"] {
      let out = t.format_report(spec).unwrap();
      assert!(out.contains('m') && out.contains('.'), "{spec} → {out:?}");
    }
  }

  // ===================== format_report fallthrough / edge =====================

  #[test]
  fn format_report_unknown_m_subspec_passes_through_literally() {
    let _g = TestGuard::new();
    let t = stopped_timer();
    assert_eq!(t.format_report("%mZ").unwrap(), "%mZ");
  }

  #[test]
  fn format_report_unknown_u_subspec_passes_through_literally() {
    let _g = TestGuard::new();
    let t = stopped_timer();
    assert_eq!(t.format_report("%uZ").unwrap(), "%uZ");
  }

  #[test]
  fn format_report_unknown_star_subspec_passes_through_literally() {
    let _g = TestGuard::new();
    let t = stopped_timer();
    assert_eq!(t.format_report("%*Z").unwrap(), "%*Z");
  }

  #[test]
  fn format_report_unknown_top_level_spec_breaks_loop() {
    // The catchall `_` arm in the %-dispatch pushes %{param} and breaks,
    // so anything after the unknown spec is silently dropped.
    let _g = TestGuard::new();
    let t = stopped_timer();
    let out = t.format_report("%Q extra").unwrap();
    assert!(out.contains("%Q"), "got: {out:?}");
    assert!(!out.contains("extra"), "got: {out:?}");
  }

  #[test]
  fn format_report_trailing_percent_terminates_cleanly() {
    let _g = TestGuard::new();
    let t = stopped_timer();
    assert_eq!(t.format_report("hello%").unwrap(), "hello");
  }

  #[test]
  fn format_report_trailing_backslash_terminates_cleanly() {
    let _g = TestGuard::new();
    let t = stopped_timer();
    assert_eq!(t.format_report("hello\\").unwrap(), "hello");
  }

  #[test]
  fn format_report_trailing_m_with_no_subspec_breaks() {
    // `%m` with nothing after — the inner `let Some(param2) = chars.next() else { break; };`
    // fires on the missing subspec.
    let _g = TestGuard::new();
    let t = stopped_timer();
    let out = t.format_report("ms=%m").unwrap();
    assert_eq!(out, "ms=");
  }
}
