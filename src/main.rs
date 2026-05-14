#![allow(
  clippy::derivable_impls,
  clippy::tabs_in_doc_comments,
  clippy::while_let_on_iterator,
  clippy::result_large_err
)]
use std::collections::VecDeque;
use std::fs::OpenOptions;
use std::io::Write;
use std::os::fd::{AsRawFd, BorrowedFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::Ordering;
use std::time::Instant;

use nix::errno::Errno;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::sys::signal::{Signal, kill};
use nix::sys::stat::{FchmodatFlags, fchmodat};
use nix::unistd::{Pid, isatty, read, write};
use scopeguard::defer;
use smallvec::SmallVec;

use crate::builtin::{source_builtin_completions, source_builtin_scripts};
use crate::expand::expand_keymap;
use crate::keys::KeyEvent;
use crate::parse::execute::{exec_dash_c, exec_int, exec_nonint};
use crate::procio::{
  MIN_INTERNAL_FD, RedirType, do_something_that_opens_fds_that_we_cant_access_hack, stdin_fileno,
};
use crate::readline::{Hint, Lines, Pos};
use crate::readline::{LineData, Prompt, ReadlineEvent, ShedLine};
use crate::signal::{
  GOT_SIGUSR1, GOT_SIGWINCH, JOB_DONE, QUIT_CODE, check_signals, sig_setup, signals_pending,
};
use crate::state::Shed;
use crate::state::logic::TrapTarget;
use crate::state::{
  meta::LineHeader, meta::QueryHeader, meta::ShedSocket, meta::SocketRequest, meta::StatusHeader,
  terminal::TermGuard, util::generate_default_rc, util::rc_file_path, util::read_logic,
  util::read_meta, util::read_shopts, util::source_env, util::source_login, util::source_rc,
  util::with_term, util::write_meta, util::write_shopts, vars::VarKind,
};
use crate::util::{ShErrKind, ShResult};
use clap::Parser;

pub mod builtin;
pub mod expand;
pub mod getopt;
pub mod jobs;
pub mod parse;
pub mod procio;
pub mod readline;
pub mod shopt;
pub mod signal;
pub mod state;

pub(crate) mod keys;
pub(crate) mod util;
use keys::KeyMapMatch;

#[cfg(test)]
pub mod tests;

#[derive(Parser, Debug)]
#[command(
  author = "Kyler Clay",
  about = "An experimental POSIX shell",
  long_about = "shed is an experimental POSIX shell focused on interative user experience, extensibility, and powerful line editing."
)]
struct ShedArgs {
  /// Evaluate the given string as a command and exit
  #[arg(short, long, conflicts_with_all = ["interactive", "stdin"])]
  command: Option<String>,

  /// Script path and arguments
  #[arg(trailing_var_arg = true)]
  script_args: Vec<String>,

  /// Print version info
  #[arg(long)]
  version: bool,

  /// Start the shell in interactive mode
  #[arg(short, long)]
  interactive: bool,

  /// Read input from stdin
  #[arg(short)]
  stdin: bool,

  /// Start the shell as a login shell (sources .shed_profile)
  #[arg(long, short)]
  login_shell: bool,

  /// Print the welcome message after arriving at the prompt
  #[arg(long, short)]
  welcome: bool,

  /// Skip sourcing runtime command files
  #[arg(long)]
  no_rc: bool,

  /// List of POSIX 'set' options to enable
  #[arg(short = 'o', value_name = "OPTION", value_parser = Self::SET_OPTS)]
  set: Vec<String>,

  /// Input is read as a keymap for the line editor to execute
  /// instead of raw shell commands. Used to script the line editor
  #[arg(long)]
  edit_script: bool,
}

impl ShedArgs {
  const SET_OPTS: [&str; 15] = [
    "errexit",
    "allexport",
    "ignoreeof",
    "monitor",
    "noclobber",
    "noglob",
    "noexec",
    "nolog",
    "notify",
    "nounset",
    "verbose",
    "vi",
    "emacs",
    "xtrace",
    "hashall",
  ];
}

/// We need to make sure that even if we panic, our child processes get sighup
///
/// This basically just wraps the default panic handler with our job control stuff
fn setup_panic_handler() {
  // take the default hook
  let default_panic_hook = std::panic::take_hook();

  // set our hook
  std::panic::set_hook(Box::new(move |info| {
    // hang up jobs
    Shed::jobs_mut(|j| j.hang_up());

    // log panic
    let data_dir = dirs::data_dir().unwrap_or_else(|| {
      let home = std::env::var("HOME").unwrap();
      PathBuf::from(format!("{home}/.local/share"))
    });
    let log_dir = data_dir.join("shed").join("log");
    std::fs::create_dir_all(&log_dir).unwrap();
    let log_file_path = log_dir.join("panic.log");
    let mut log_file = procio::get_redir_file(RedirType::Output, log_file_path).unwrap();

    let panic_info_raw = info.to_string();
    log_file.write_all(panic_info_raw.as_bytes()).unwrap();

    let backtrace = std::backtrace::Backtrace::force_capture();
    log_file
      .write_all(format!("\nBacktrace:\n{:?}", backtrace).as_bytes())
      .unwrap();

    // call the default panic hook
    default_panic_hook(info);
  }));
}

fn setup() -> Option<ShedArgs> {
  yansi::enable();
  let pid = Pid::this();
  if let Ok(log_file) = OpenOptions::new()
    .create(true)
    .truncate(true)
    .open(format!("/tmp/shed{pid}.log"))
  {
    env_logger::Builder::from_default_env()
      .target(env_logger::Target::Pipe(Box::new(log_file)))
      .init();
  } else {
    env_logger::init();
  }
  let _guard = scopeguard::guard(pid, |pid| {
    let _ = std::fs::remove_file(format!("/tmp/shed{pid}.log"));
  });

  setup_panic_handler();
  state::util::set_ver_info().ok();
  state::util::set_sh_lvl().ok();

  let mut args = ShedArgs::parse();
  if std::env::args().next().is_some_and(|a| a.starts_with('-')) {
    // first arg is '-shed'
    // meaning we are in a login shell
    args.login_shell = true;
  }
  if args.version {
    outln!(
      "shed {} ({} {})",
      env!("CARGO_PKG_VERSION"),
      std::env::consts::ARCH,
      std::env::consts::OS
    );
    return None;
  }

  if !args.no_rc
    && let Err(e) = source_env()
  {
    e.print_error();
  }

  for set_opt in &args.set {
    if set_opt == "emacs" {
      write_shopts(|o| o.query("set.vi=false")).ok();
      continue;
    }
    write_shopts(|o| o.query(&format!("set.{set_opt}=true"))).ok();
  }

  do_something_that_opens_fds_that_we_cant_access_hack(MIN_INTERNAL_FD, state::util::init_db_conn);

  Some(args)
}

fn main() -> ExitCode {
  let Some(args) = setup() else {
    return ExitCode::SUCCESS;
  };

  if let Err(e) = dispatch_input(args) {
    e.print_error();
    if QUIT_CODE.load(Ordering::SeqCst) == 0 {
      QUIT_CODE.store(1, Ordering::SeqCst);
    }
  };

  tear_down()
}

fn tear_down() -> ExitCode {
  if let Some(trap) = read_logic(|l| l.get_trap(TrapTarget::Exit))
    && let Err(e) = exec_nonint(trap, Some("trap".into()))
  {
    e.print_error();
  }

  let mut deferred = Shed::vars_mut(|v| v.cur_scope_mut().take_deferred_cmds());

  while let Some(cmd) = deferred.pop() {
    if let Err(e) = exec_nonint(cmd, Some("defer".into())) {
      e.print_error();
    }
  }

  autocmd!(OnExit);

  Shed::jobs_mut(|j| j.hang_up());

  let code = QUIT_CODE.load(Ordering::SeqCst) as u8;
  if code == 0 && isatty(stdin_fileno()).unwrap_or_default() {
    errln!("\nexit");
  }

  with_term(|t| t.reset_for_exit());

  ExitCode::from(QUIT_CODE.load(Ordering::SeqCst) as u8)
}

fn dispatch_input(mut args: ShedArgs) -> ShResult<()> {
  match args.edit_script {
    true => {
      // in this arm, we interpret the input we are given as a sequence of keys
      // for the line editor to consume and execute
      let input = if let Some(ref cmd) = args.command {
        cmd.clone()
      } else if args.stdin || !isatty(stdin_fileno()).unwrap_or(false) {
        read_input()?
      } else if !args.script_args.is_empty() {
        let path = args.script_args.remove(0);
        std::fs::read_to_string(path)?
      } else {
        // no input provided, just run interactively
        status_msg!("warning: --script was passed but no input was given");
        return shed_interactive(args, None);
      };

      let keys = expand_keymap(&input);
      shed_interactive(args, Some(keys))
    }
    false => {
      if let Some(cmd) = args.command {
        exec_dash_c(cmd, args.script_args)
      } else if args.stdin || !isatty(stdin_fileno()).unwrap_or(false) {
        read_commands(args.script_args)
      } else if !args.script_args.is_empty() {
        let path = args.script_args.remove(0);
        run_script(path, args.script_args)
      } else {
        shed_interactive(args, None)
      }
    }
  }
}

fn read_commands(args: Vec<String>) -> ShResult<()> {
  let commands = read_input()?;

  Shed::vars_mut(|v| {
    let scope = v.cur_scope_mut();
    let zero = scope.sh_argv().front().cloned().unwrap_or_default();
    scope.sh_argv_mut().clear();
    scope.bpush_arg(zero);
    for arg in args {
      scope.bpush_arg(arg);
    }
  });

  exec_nonint(commands, None)
}

fn read_input() -> ShResult<String> {
  let mut input = vec![];
  let mut read_buf = [0u8; 4096];
  loop {
    match read(stdin_fileno(), &mut read_buf) {
      Ok(0) => break,
      Ok(n) => input.extend_from_slice(&read_buf[..n]),
      Err(Errno::EINTR) => continue,
      Err(e) => {
        QUIT_CODE.store(1, Ordering::SeqCst);
        return Err(sherr!(CleanExit(1), "error reading from stdin: {e}",));
      }
    }
  }

  Ok(String::from_utf8_lossy(&input).to_string())
}

fn run_script<P: AsRef<Path>>(path: P, args: Vec<String>) -> ShResult<()> {
  let path = path.as_ref();
  let path_raw = path.to_string_lossy().to_string();
  if !path.is_file() {
    errln!("shed: Failed to open input file: {}", path.display());
    QUIT_CODE.store(1, Ordering::SeqCst);
    return Err(sherr!(CleanExit(1), "input file not found",));
  }
  let Ok(input) = std::fs::read_to_string(path) else {
    errln!("shed: Failed to read input file: {}", path.display());
    QUIT_CODE.store(1, Ordering::SeqCst);
    return Err(sherr!(CleanExit(1), "failed to read input file",));
  };

  let path_str = path.to_string_lossy().to_string();
  Shed::vars_mut(|v| {
    v.set_param(state::vars::ShellParam::ShellName, &path_str); // $0
    let scope = v.cur_scope_mut();
    scope.sh_argv_mut().clear();
    scope.bpush_arg(path_str.clone());
    for arg in args {
      scope.bpush_arg(arg);
    }
  });

  exec_nonint(input, Some(path_raw.into()))
}

fn first_run_setup() -> ShResult<()> {
  let rc_path = generate_default_rc()?;

  if let Some(rc_path) = rc_path {
    status_msg!("Generated default rc file at '{}'", rc_path.display());
  }

  Ok(())
}

fn handle_signals_interactive(readline: &mut ShedLine) -> ShResult<bool> {
  // Handle any pending signals
  while signals_pending() {
    if let Err(e) = check_signals() {
      match e.kind() {
        ShErrKind::Interrupt => {
          // We got Ctrl+C - clear current input and redraw
          readline.reset_active_widget(false)?;
        }
        ShErrKind::CleanExit(code) => {
          QUIT_CODE.store(*code, Ordering::SeqCst);
          return Ok(false);
        }
        _ => e.print_error(),
      }
    }
  }

  if GOT_SIGWINCH.swap(false, Ordering::SeqCst) {
    log::info!("Window size change detected, updating readline dimensions");
    // Restore cursor to saved row before clearing, since the terminal
    // may have moved it during resize/rewrap
    with_term(|t| t.update_t_dims());
    readline.mark_dirty();
  }

  if JOB_DONE.swap(false, Ordering::SeqCst) {
    // update the prompt so any job count escape sequences update dynamically
    readline.prompt_mut().refresh();
  }

  if GOT_SIGUSR1.swap(false, Ordering::SeqCst) {
    log::info!("SIGUSR1 received: refreshing readline state");
    readline.mark_dirty();
    readline.prompt_mut().refresh();
  }

  readline.print_line(false)?;

  Ok(true)
}

fn get_poll_timeout(readline: &mut ShedLine) -> (PollTimeout, Option<String>) {
  let mut exec_if_timeout = None;

  let timeout = if !readline.pending_keymap().is_empty() {
    // wait for more keymap keys
    PollTimeout::from(1000u16)
  } else if let Some(timeout) = write_meta(|m| m.take_poll_timeout()) {
    // something gave us an explicit poll timeout to use.
    // usually this means there is a status message showing.
    // after the timeout, it will trigger a redraw that clears
    // the status message.
    timeout
  } else {
    let screensaver_cmd = read_shopts(|o| o.prompt.screensaver_cmd.clone())
      .trim()
      .to_string();
    let screensaver_idle_time = read_shopts(|o| o.prompt.screensaver_idle_time);
    if screensaver_idle_time == 0 || screensaver_cmd.is_empty() {
      // no screensaver stuff, set no timeout
      PollTimeout::NONE
    } else {
      exec_if_timeout = Some(screensaver_cmd);
      PollTimeout::try_from((screensaver_idle_time * 1000) as i32).unwrap_or(PollTimeout::NONE)
    }
  };

  (timeout, exec_if_timeout)
}

fn interactive_setup(args: ShedArgs) -> ShResult<TermGuard> {
  let raw_mode = with_term(|t| t.setup_terminal())?;

  sig_setup(args.login_shell);
  crate::state::INTERACTIVE.store(true, Ordering::SeqCst);

  write_meta(|m| {
    m.ensure_meta_table()?;
    m.create_socket()
  })?;

  if let Some(msg) = read_meta(|m| m.welcome_message(args.welcome)) {
    outln!("\n{msg}\n\n");
  }

  if args.login_shell && !args.no_rc {
    source_login().ok();
  }

  if rc_file_path().is_none_or(|f| !f.is_file()) {
    // we didn't find any runtime files at all
    // let's run a first time setup
    if let Err(e) = first_run_setup() {
      e.print_error();
    }
  }

  if !args.no_rc
    && let Err(e) = source_rc()
  {
    e.print_error();
  }

  source_builtin_scripts();
  source_builtin_completions();

  if let Ok(welcome) = std::env::var("SHELL_WELCOME") {
    // support for systemd's run0 message
    errln!("\n{welcome}\n\n");
  }

  if read_shopts(|o| o.statline.enable) {
    // statline enabled, reserve scroll region rows
    // also move the cursor down there too
    with_term(|t| -> ShResult<()> {
      let bottom = (t.t_rows() as u16).saturating_sub(2).max(1);
      t.set_scroll_region(1, bottom)?;
      t.move_cursor_abs(bottom, 1);
      Ok(())
    })?;
  }

  Ok(raw_mode)
}

fn shed_interactive(args: ShedArgs, script_keys: Option<Vec<KeyEvent>>) -> ShResult<()> {
  let _raw_mode = interactive_setup(args)?;
  state::util::try_hash();

  let mut readline = match ShedLine::new(Prompt::new()) {
    Ok(rl) => rl,
    Err(e) => {
      // try to fall back to no hist
      match ShedLine::new_no_hist(Prompt::new()) {
        Ok(rl) => {
          errln!("Failed to load history: {e}");
          rl
        }
        Err(e) => {
          // that failed too. we probably arent in a context where readline can work at all.
          errln!("Failed to initialize readline: {e}");
          QUIT_CODE.store(1, Ordering::SeqCst);
          return Err(sherr!(CleanExit(1), "readline initialization failed",));
        }
      }
    }
  };

  if let Some(keys) = script_keys {
    return run_script_keys(&mut readline, keys);
  }

  let mut vi_mode = read_shopts(|o| o.set.vi);
  let mut socket_mode = ShedSocket::mode();

  let mut poll_fds: SmallVec<[PollFd; 2]> = SmallVec::new();
  let Some(tty_fd) = with_term(|t| t.tty().map(|fd| fd.as_raw_fd())) else {
    errln!("Failed to access terminal file descriptor");
    QUIT_CODE.store(1, Ordering::SeqCst);
    return Err(sherr!(CleanExit(1), "terminal access failed",));
  };

  let tty_poll = PollFd::new(unsafe { BorrowedFd::borrow_raw(tty_fd) }, PollFlags::POLLIN);

  let socket_fd = write_meta(|m| m.get_socket().map(|s| s.as_raw_fd()));
  let socket_poll =
    socket_fd.map(|fd| PollFd::new(unsafe { BorrowedFd::borrow_raw(fd) }, PollFlags::POLLIN));

  // Main poll loop
  loop {
    state::util::try_hash();
    let _flush_guard = state::terminal::FlushGuard; // flushes terminal on drop

    poll_fds.clear();
    poll_fds.push(tty_poll.clone());
    if let Some(fd) = &socket_poll {
      poll_fds.push(fd.clone());
    }

    if read_shopts(|o| o.set.vi) != vi_mode {
      // the editing mode option changed.
      // we have to make sure the edit mode reflects the option now
      readline.fix_editing_mode();

      vi_mode = !vi_mode; // and toggle this
    } else if read_meta(|m| m.num_subscribers()) == 0 && readline.in_insert_mode() {
      // we are in remote mode with no consumers for our broadcasted input.
      // That effectively soft locks the shell, so let's fix that
      readline.fix_editing_mode();
    }

    if !handle_signals_interactive(&mut readline)? {
      return Ok(());
    }

    let (timeout, exec_if_timeout) = get_poll_timeout(&mut readline);
    with_term(|t| t.flush())?;

    match poll(&mut poll_fds, timeout) {
      Ok(0) => {
        // We timed out. Check if there's a screensaver command
        if let Some(cmd) = exec_if_timeout
          && readline.editor().is_empty()
        {
          // don't exec screensaver if we have a pending command
          let prepared = ReadlineEvent::Line(cmd.clone());
          let _guard = scopeguard::guard(read_shopts(|o| o.core.auto_hist), |opt| {
            // restores old auto_hist value
            write_shopts(|o| o.core.auto_hist = opt);
          });
          write_shopts(|o| o.core.auto_hist = false); // don't save screensaver command to history

          autocmd!(OnScreensaverExec);
          let res = {
            defer!(autocmd!(OnScreensaverReturn));
            handle_readline_event(&mut readline, Ok(prepared))?
          };

          match res {
            true => return Ok(()),
            false => continue,
          }
        }
      }
      Err(Errno::EINTR) => {
        // Interrupted by signal, loop back to handle it
        continue;
      }
      Err(e) => {
        errln!("poll error: {e}");
        break;
      }
      Ok(_) => {}
    }

    // resolve pending keymap ambiguity
    if !readline.pending_keymap().is_empty()
      && poll_fds[0]
        .revents()
        .is_none_or(|r| !r.contains(PollFlags::POLLIN))
    {
      resolve_keymap(&mut readline)?;
      continue;
    }

    // Check if stdin has data
    if let Some(revents) = poll_fds[0].revents() {
      if revents.intersects(PollFlags::POLLHUP | PollFlags::POLLERR | PollFlags::POLLNVAL) {
        // we don't have a terminal anymore. let's get outta here
        QUIT_CODE.store(0, Ordering::SeqCst);
        return Ok(());
      }
      if revents.contains(PollFlags::POLLIN) {
        match with_term(|t| t.read()) {
          Ok(_) => { /* data read, will be processed below */ }
          Err(e) => match e.kind() {
            ShErrKind::LoopBreak(_) => break,
            ShErrKind::LoopContinue(_) => continue,
            _ => {
              e.print_error();
              break;
            }
          },
        }
      }
    }

    // check socket fd
    if poll_fds
      .get(1)
      .and_then(|fd| fd.revents())
      .is_some_and(|r| r.contains(PollFlags::POLLIN))
    {
      let requests = write_meta(|m| m.read_socket())?;
      for (conn, req) in requests {
        let res = handle_socket_request(conn, req, &mut readline).transpose();
        if let Some(event) = res
          && handle_readline_event(&mut readline, event)?
        {
          return Ok(());
        }
      }
    }

    // Process the input that we read above
    let keys = with_term(|t| t.drain_keys())?;
    let event = readline.process_input(keys);

    match handle_readline_event(&mut readline, event)? {
      true => return Ok(()),
      false => { /* continue looping */ }
    }

    // check the socket mode
    let curr_socket_mode = ShedSocket::mode();

    if curr_socket_mode != socket_mode {
      // the mode changed, call chmod
      let path = ShedSocket::path();
      fchmodat(
        nix::fcntl::AT_FDCWD,
        Path::new(&path),
        curr_socket_mode,
        FchmodatFlags::FollowSymlink,
      )
      .ok();
      socket_mode = curr_socket_mode;
    }
  }

  Ok(())
}

/// Run some provided key events. These come from the --edit-script flag,
/// and are meant to simulate a user typing those keys interactively.
/// Mainly useful for testing/profiling, but may have some other legitimate niche use-cases
fn run_script_keys(readline: &mut ShedLine, keys: Vec<KeyEvent>) -> ShResult<()> {
  let mut queue: VecDeque<KeyEvent> = keys.into();
  while let Some(key) = queue.pop_front() {
    let event = readline.process_input(vec![key]);
    if handle_readline_event(readline, event)? {
      return Ok(());
    }
  }
  if !readline.pending_keymap().is_empty() {
    resolve_keymap(readline)?;
  }
  Ok(())
}

/// Handle a ReadlineEvent. Returns a boolean, `true` means "exit the shell", `false` means "keep looping"
fn handle_readline_event(
  readline: &mut ShedLine,
  event: ShResult<ReadlineEvent>,
) -> ShResult<bool> {
  match event {
    Ok(ReadlineEvent::Line(input)) => {
      let token = read_shopts(|s| s.core.auto_hist)
        .then(|| readline.history_mut().push(input.clone()).ok().flatten())
        .flatten(); // token is used as a stable identifier for the command in the history

      let exec_start = Instant::now();
      autocmd!(PreCmd);

      let cmd_start = Instant::now();
      write_meta(|m| m.start_timer());

      with_term(|t| t.emit_osc_exec_start()).ok();

      let res = {
        // _guard restores terminal state on drop
        let _guard = with_term(|t| t.prepare_for_exec())?;
        exec_int(input.clone(), Some("<stdin>".into()))
      };

      with_term(|t| t.emit_osc_exec_end(state::util::get_status())).ok();

      if let Err(e) = res {
        match e.kind() {
          ShErrKind::Interrupt => {
            // We got Ctrl+C during command execution
            // Just fall through here
          }
          ShErrKind::CleanExit(code) => {
            QUIT_CODE.store(*code, Ordering::SeqCst);
            return Ok(true);
          }
          ShErrKind::ErrInterrupt => {
            // set -e exit path
            QUIT_CODE.store(0, Ordering::SeqCst);
            e.print_error();
            return Ok(true);
          }
          _ => e.print_error(),
        }
      }
      let command_run_time = cmd_start.elapsed();
      log::info!("Command executed in {:.2?}", command_run_time);
      let runtime = write_meta(|m| m.stop_timer());

      autocmd!(PostCmd);

      let was_func_def = write_meta(|m| m.take_last_was_func_def());
      let should_write = read_shopts(|o| o.core.auto_hist)
        && (!was_func_def || !read_shopts(|o| o.set.nolog))
        && !builtin::fixcmd::NO_HIST_SAVE.swap(false, Ordering::SeqCst)
        && !input.is_empty();

      if let Some(token) = token
        && !should_write
      {
        readline
          .history_mut()
          .delete("WHERE token = ?1", rusqlite::params![token.to_string()])?;
      }

      if read_shopts(|s| s.core.auto_hist)
        && should_write
        && let Some(token) = token
        && let Err(e) = readline
          .history_mut()
          .set_status(token, runtime, state::util::get_status())
      {
        e.print_error();
      }

      with_term(|t| t.fix_cursor_column())?;
      write_term!("\n\r")?;

      // Reset for next command with fresh prompt
      readline.reset(true)?;

      let real_end = exec_start.elapsed();
      log::info!("Total round trip time: {:.2?}", real_end);
      Ok(false)
    }
    Ok(ReadlineEvent::Eof) => {
      // Ctrl+D on empty line
      QUIT_CODE.store(0, Ordering::SeqCst);
      Ok(true)
    }
    Ok(ReadlineEvent::Pending) => {
      // No complete input yet, keep polling
      Ok(false)
    }
    Err(e) => match e.kind() {
      ShErrKind::CleanExit(code) => {
        QUIT_CODE.store(*code, Ordering::SeqCst);
        Ok(true)
      }
      _ => {
        e.print_error();
        Ok(false)
      }
    },
  }
}

fn resolve_keymap(readline: &mut ShedLine) -> ShResult<()> {
  let keymap_flags = readline.curr_keymap_flags();
  let matches = read_logic(|l| l.keymaps_filtered(keymap_flags, readline.pending_keymap()));
  // If there's an exact match, fire it; otherwise flush as normal keys
  let exact = matches
    .iter()
    .find(|km| km.compare(readline.pending_keymap()) == KeyMapMatch::IsExact);
  if let Some(km) = exact {
    let action = km.action_expanded();
    readline.pending_keymap_mut().clear();
    for key in action {
      let event = readline.handle_key(key).transpose();
      if let Some(event) = event {
        handle_readline_event(readline, event)?;
      }
    }
  } else {
    let buffered = std::mem::take(readline.pending_keymap_mut());
    for key in buffered {
      let event = readline.handle_key(key).transpose();
      if let Some(event) = event {
        handle_readline_event(readline, event)?;
      }
    }
  }
  readline.print_line(false)?;
  Ok(())
}

fn handle_socket_request(
  conn: UnixStream,
  request: SocketRequest,
  readline: &mut ShedLine,
) -> ShResult<Option<ReadlineEvent>> {
  match request {
    SocketRequest::PostSystemMessage(msg) => {
      system_msg!("{msg}");
      write(&conn, b"ok\n").ok();
    }
    SocketRequest::PostStatusMessage(msg) => {
      status_msg!("{msg}");
      write(&conn, b"ok\n").ok();
    }
    SocketRequest::Subscribe => {
      write_meta(|m| m.push_subscriber(conn));
    }
    SocketRequest::RefreshPrompt => {
      kill(Pid::this(), Signal::SIGUSR1)?;
      write(&conn, b"ok\n").ok();
    }
    SocketRequest::LineGet(line_header) => {
      let LineData {
        buffer,
        cursor,
        anchor,
        hint,
        mode,
      } = readline.get_line_data();
      match line_header {
        LineHeader::Buffer => {
          write(&conn, buffer.as_bytes()).ok();
          write(&conn, b"\n").ok();
        }
        LineHeader::Cursor => {
          write(&conn, cursor.to_string().as_bytes()).ok();
          write(&conn, b"\n").ok();
        }
        LineHeader::Anchor => {
          if let Some(anchor) = anchor {
            write(&conn, anchor.to_string().as_bytes()).ok();
          }
          write(&conn, b"\n").ok();
        }
        LineHeader::Hint => {
          if let Some(hint) = hint {
            write(&conn, hint.as_bytes()).ok();
          }
          write(&conn, b"\n").ok();
        }
        LineHeader::Mode => {
          write(&conn, mode.to_string().as_bytes()).ok();
          write(&conn, b"\n").ok();
        }
      }
    }
    SocketRequest::LineSet(line_header, value) => match line_header {
      LineHeader::Buffer => {
        let joined = readline.editor().joined();
        let pos = readline.editor().cursor_to_flat();

        readline.editor_mut().edit(|this| {
          this.set_buffer(value.clone());
        });

        readline.history_mut().update_pending_cmd((&joined, pos));

        let hint = readline.history().get_hint();

        readline.editor_mut().set_hint(hint);
        readline.editor_mut().move_cursor_to_end();
        readline.set_needs_redraw(true);
      }
      LineHeader::Cursor => readline.editor_mut().with_hint(|this| {
        if let Some((row, col)) = value.split_once(':')
          && let Ok(row) = row.parse::<usize>()
          && let Ok(col) = col.parse::<usize>()
        {
          this.set_cursor(Pos::new(row, col));
        } else if let Ok(pos) = value.parse::<usize>() {
          this.set_cursor_from_flat(pos);
        }
      }),
      LineHeader::Hint => {
        readline
          .editor_mut()
          .set_hint(Some(Hint::Override(Lines::to_lines(value))));
      }
      LineHeader::Mode => {
        if !readline.try_swap_mode_from_str(&value) {
          return Ok(None);
        }
      }
      LineHeader::Anchor => {
        if let Some((row, col)) = value.split_once(':')
          && let Ok(row) = row.parse::<usize>()
          && let Ok(col) = col.parse::<usize>()
        {
          readline.editor_mut().set_anchor(Pos::new(row, col));
        } else if let Ok(pos) = value.parse::<usize>() {
          readline.editor_mut().set_anchor_from_flat(pos);
        }
      }
    },
    SocketRequest::LineSendKeys(events) => {
      if let Some(event) = readline.replay_keys(events, true)? {
        return Ok(Some(event));
      }
    }
    SocketRequest::Query(query_header) => match query_header {
      QueryHeader::Cwd => {
        let cwd = std::env::current_dir()?.to_string_lossy().to_string();
        write(&conn, cwd.as_bytes()).ok();
        write(&conn, b"\n").ok();
      }
      QueryHeader::GetVar(var) => {
        let var = Shed::vars(|v| v.get_var(&var));
        write(&conn, var.as_bytes()).ok();
        write(&conn, b"\n").ok();
      }
      QueryHeader::SetVar(var, val, flags) => {
        Shed::vars_mut(|v| v.set_var(&var, VarKind::Str(val), flags)).ok();
        write(&conn, b"ok\n").ok();
      }
      QueryHeader::Status(headers) => {
        let mut responses = vec![];
        for header in headers {
          match header {
            StatusHeader::ExitCode => responses.push(state::util::get_status().to_string()),
            StatusHeader::CommandName => {
              if let Some(job) = read_meta(|m| m.last_job().cloned())
                && let Some(cmd) = job.name()
              {
                responses.push(cmd.to_string());
              } else {
                responses.push("".to_string());
              }
            }
            StatusHeader::Runtime => {
              let Some(dur) = write_meta(|m| m.get_time()) else {
                responses.push("".to_string());
                continue;
              };
              responses.push(format!("{}", dur.as_millis()));
            }
            StatusHeader::Pid => {
              let Some(job) = write_meta(|m| m.last_job().cloned()) else {
                responses.push("".to_string());
                continue;
              };
              responses.push(
                job
                  .get_pids()
                  .first()
                  .map(|p| p.to_string())
                  .unwrap_or_default(),
              );
            }
            StatusHeader::Pgid => {
              let Some(job) = write_meta(|m| m.last_job().cloned()) else {
                responses.push("".to_string());
                continue;
              };
              responses.push(job.pgid().to_string());
            }
          }
        }
        let output = responses.join(" ");
        write(&conn, output.as_bytes()).ok();
        write(&conn, b"\n").ok();
      }
    },
  }
  Ok(None)
}
