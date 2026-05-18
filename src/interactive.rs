use std::{
  collections::VecDeque,
  io::Write,
  os::fd::{AsRawFd, BorrowedFd},
  path::Path,
  sync::atomic::Ordering,
  time::Instant,
};

use nix::{
  errno::Errno,
  poll::{PollFd, PollFlags, PollTimeout, poll},
  sys::stat::{FchmodatFlags, fchmodat},
};
use scopeguard::defer;
use smallvec::SmallVec;

use crate::lifecycle;

use super::{
  KeyEvent, KeyMapMatch, Prompt, ReadlineEvent, ShErrKind, ShResult, Shed, ShedLine, autocmd,
  builtin::{source_builtin_completions, source_builtin_scripts},
  errln,
  eval::execute::exec_int,
  lifecycle::first_run_setup,
  outln, sherr, shopt, shopt_mut,
  signal::{
    GOT_SIGUSR1, GOT_SIGWINCH, JOB_DONE, QUIT_CODE, check_signals, sig_setup, signals_pending,
  },
  socket::handle_socket_request,
  state::{
    self,
    meta::ShedSocket,
    terminal::TermGuard,
    util::{rc_file_path, source_login, source_rc},
  },
  util, write_term,
};

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
    Shed::term_mut(|t| t.update_t_dims());
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
  } else if let Some(timeout) = Shed::meta_mut(|m| m.take_poll_timeout()) {
    // something gave us an explicit poll timeout to use.
    // usually this means there is a status message showing.
    // after the timeout, it will trigger a redraw that clears
    // the status message.
    timeout
  } else {
    let screensaver_cmd = shopt!(prompt.screensaver_cmd.clone()).trim().to_string();
    let screensaver_idle_time = shopt!(prompt.screensaver_idle_time);
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

fn interactive_setup(args: lifecycle::ShedArgs) -> ShResult<TermGuard> {
  let raw_mode = Shed::term_mut(|t| t.setup_terminal())?;
  let _interactive_mode = Shed::term_mut(|t| t.interactive_guard(true));

  sig_setup(args.login_shell);

  Shed::meta_mut(|m| {
    m.ensure_meta_table()?;
    m.create_socket()
  })?;

  if let Some(msg) = Shed::meta(|m| m.welcome_message(args.welcome)) {
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

  if shopt!(statline.enable) {
    // statline enabled, reserve scroll region rows
    // also move the cursor down there too
    Shed::term_mut(|t| -> ShResult<()> {
      let bottom = (t.t_rows() as u16).saturating_sub(2).max(1);
      t.set_scroll_region(1, bottom)?;
      t.move_cursor_abs(bottom, 1);
      Ok(())
    })?;
  }

  Ok(raw_mode)
}

pub(super) fn shed_interactive(
  args: lifecycle::ShedArgs,
  script_keys: Option<Vec<KeyEvent>>,
) -> ShResult<()> {
  let _raw_mode = interactive_setup(args)?;
  state::util::try_hash();
  Shed::meta_mut(|m| m.set_interactive_shell(true));

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

  let mut vi_mode = shopt!(set.vi);
  let mut socket_mode = ShedSocket::mode();

  let mut poll_fds: SmallVec<[PollFd; 2]> = SmallVec::new();
  let Some(tty_fd) = Shed::term(|t| t.tty().map(|fd| fd.as_raw_fd())) else {
    errln!("Failed to access terminal file descriptor");
    QUIT_CODE.store(1, Ordering::SeqCst);
    return Err(sherr!(CleanExit(1), "terminal access failed",));
  };
  let tty_poll = PollFd::new(unsafe { BorrowedFd::borrow_raw(tty_fd) }, PollFlags::POLLIN);

  let socket_fd = Shed::meta_mut(|m| m.get_socket().map(|s| s.as_raw_fd()));
  let socket_poll =
    socket_fd.map(|fd| PollFd::new(unsafe { BorrowedFd::borrow_raw(fd) }, PollFlags::POLLIN));

  // Main poll loop
  loop {
    state::util::try_hash();
    util::flog::update_log_level();
    let _flush_guard = state::terminal::FlushGuard; // flushes terminal on drop

    poll_fds.clear();
    poll_fds.push(tty_poll.clone());
    if let Some(fd) = &socket_poll {
      poll_fds.push(fd.clone());
    }

    if shopt!(set.vi) != vi_mode {
      // the editing mode option changed.
      // we have to make sure the edit mode reflects the option now
      readline.fix_editing_mode();

      vi_mode = !vi_mode; // and toggle this
    } else if Shed::meta(|m| m.num_subscribers()) == 0 && readline.in_insert_mode() {
      // we are in remote mode with no consumers for our broadcasted input.
      // That effectively soft locks the shell, so let's fix that
      readline.fix_editing_mode();
    }

    if !handle_signals_interactive(&mut readline)? {
      return Ok(());
    }

    let (timeout, exec_if_timeout) = get_poll_timeout(&mut readline);
    Shed::term_mut(|t| t.flush())?;

    match poll(&mut poll_fds, timeout) {
      Ok(0) => {
        // We timed out. Check if there's a screensaver command
        if let Some(cmd) = exec_if_timeout
          && readline.editor().is_empty()
        {
          // don't exec screensaver if we have a pending command
          let prepared = ReadlineEvent::Line(cmd.clone());
          let _guard = scopeguard::guard(shopt!(core.auto_hist), |opt| {
            // restores old auto_hist value
            shopt_mut!(core.auto_hist = opt);
          });
          shopt_mut!(core.auto_hist = false); // don't save screensaver command to history

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
        match Shed::term_mut(|t| t.read()) {
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
      let requests = Shed::meta_mut(|m| m.read_socket())?;
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
    let keys = Shed::term_mut(|t| t.drain_keys())?;
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
      let token = shopt!(core.auto_hist)
        .then(|| readline.history_mut().push(input.clone()).ok().flatten())
        .flatten(); // token is used as a stable identifier for the command in the history

      autocmd!(PreCmd);

      let cmd_start = Instant::now();
      Shed::meta_mut(|m| m.start_timer());

      Shed::term_mut(|t| t.emit_osc_exec_start()).ok();

      let res = {
        // _guard restores terminal state on drop
        let _guard = Shed::term_mut(|t| t.prepare_for_exec())?;
        exec_int(input.clone(), Some("<stdin>".into()))
      };

      Shed::term_mut(|t| t.emit_osc_exec_end(Shed::get_status())).ok();

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
      let runtime = Shed::meta_mut(|m| m.stop_timer());

      autocmd!(PostCmd);

      let no_hist_save = Shed::meta_mut(|m| m.no_hist_save());

      let was_func_def = Shed::meta_mut(|m| m.take_last_was_func_def());
      let nolog = was_func_def && shopt!(set.nolog);

      let should_write = shopt!(core.auto_hist) && !nolog && !no_hist_save && !input.is_empty();

      if let Some(token) = token
        && !should_write
      {
        readline
          .history_mut()
          .delete("WHERE token = ?1", rusqlite::params![token.to_string()])?;
      }

      if shopt!(core.auto_hist)
        && should_write
        && let Some(token) = token
        && let Err(e) = readline
          .history_mut()
          .set_status(token, runtime, state::Shed::get_status())
      {
        e.print_error();
      }

      Shed::term_mut(|t| t.fix_cursor_column())?;
      write_term!("\n\r")?;

      // Reset for next command with fresh prompt
      readline.reset(true)?;
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
  let matches = Shed::logic(|l| l.keymaps_filtered(keymap_flags, readline.pending_keymap()));
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
