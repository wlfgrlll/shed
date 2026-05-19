use std::{
  fmt::Debug,
  io::Write,
  os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd},
  sync::LazyLock,
  time::{Duration, Instant},
};

mod util;
pub(crate) use util::{
  ColorMode, CursorStyle, calc_str_width, get_win_size, truncate_with_ellipsis, width,
};
use util::{enable_cooked_mode, enable_raw_mode};

mod guard;
use guard::Snapshot;
pub(crate) use guard::{FlushGuard, SyncOutputGuard, TermGuard};

mod parse;
use parse::TermEvent;
pub(crate) use parse::{Cols, Rows};

use bitflags::bitflags;

use nix::{
  errno::Errno,
  fcntl::{FcntlArg, OFlag, fcntl, open},
  poll::{PollFd, PollFlags, PollTimeout, poll},
  sys::{
    signal::{SigSet, SigmaskHow, Signal, kill, killpg, pthread_sigmask},
    stat::Mode,
    termios::{self, Termios, tcgetattr, tcsetattr},
  },
  unistd::{Pid, getpgrp, isatty, tcsetpgrp, write},
};

use super::{
  Pos, ShErr, ShErrKind, ShResult, Shed,
  keys::{self, KeyEvent},
  match_loop, procio, sherr, shopt_macro as shopt, try_var, write_term,
};

static TTY_FILENO: LazyLock<Option<OwnedFd>> = LazyLock::new(|| {
  let fd = open("/dev/tty", OFlag::O_RDWR, Mode::empty()).ok()?;
  // Move the tty fd above the user-accessible range so that
  // `exec 3>&-` and friends don't collide with shell internals.
  procio::move_high(fd).ok()
});

bitflags! {
  #[derive(Debug, Clone, Copy, PartialEq, Eq)]
  pub(crate) struct TermCap: u32 {
    const TRUECOLOR = 1<<0;
    const KITTY_KBD_PROTO = 1<<1;
    const SGR_MOUSE = 1<<2;
    const SCROLL_UPDOWN = 1<<3;
    const ALT_SCREEN = 1<<4;
    const BRACKET_PASTE = 1<<5;
    const FOCUS_REPORT = 1<<6;
    const SYNC_OUTPUT = 1<<7;
    const STRIKETHROUGH = 1<<8;
    const UNDERLINE_STYLES = 1<<9;
  }
}

/// An abstraction over the terminal that manages terminal attributes, and I/O.
#[derive(Debug)]
pub(crate) struct Terminal {
  tty: Option<RawFd>,
  reader: parse::PollReader,
  input_buf: String,

  bracketed_paste: bool,
  kitty_kbd_proto: bool,
  raw_mode: bool,
  alt_buffer: bool,
  cursor_style: CursorStyle,
  cursor_visible: bool,
  mouse_enabled: bool,
  interactive: bool,

  termios_stack: Vec<Termios>,
  term_caps: TermCap,
  xt_version: Option<parse::XtVersion>,

  t_cols: usize,
  t_rows: usize,

  scroll_region: Option<(u16, u16)>,

  last_bell: Option<Instant>,

  /// When set, terminal-capability and cursor-position probes short-circuit
  /// instead of sending escape sequences and waiting for replies. Used by
  /// tests where the PTY peer doesn't synthesize responses.
  test_mode: bool,
}

impl Clone for Terminal {
  fn clone(&self) -> Self {
    Self {
      tty: self.tty,
      reader: self.reader.clone(),
      input_buf: self.input_buf.clone(),
      bracketed_paste: self.bracketed_paste,
      kitty_kbd_proto: self.kitty_kbd_proto,
      raw_mode: self.raw_mode,
      alt_buffer: self.alt_buffer,
      cursor_style: self.cursor_style,
      cursor_visible: self.cursor_visible,
      mouse_enabled: self.mouse_enabled,
      interactive: self.interactive,
      termios_stack: self.termios_stack.clone(),
      term_caps: self.term_caps,
      xt_version: self.xt_version.clone(),
      t_cols: self.t_cols,
      t_rows: self.t_rows,
      scroll_region: self.scroll_region,
      last_bell: self.last_bell,
      test_mode: self.test_mode,
    }
  }
}

impl Terminal {
  pub const CAP_BURST: &str = concat!(
    "\x1b[?u",
    "\x1bP+q5375\x1b\\",
    "\x1bP+q524742\x1b\\",
    "\x1b[>q",
    "\x1b[c",
  );
  pub const MODIFY_OTHER_KEYS: &str = "\x1b[>4;1m";
  pub const APPLICATION_KEYPAD: &str = "\x1b=";
  pub const SYNC_START: &str = "\x1b[?2026h";
  pub const SYNC_END: &str = "\x1b[?2026l";
  pub const BRACKET_PASTE_ON: &str = "\x1b[?2004h";
  pub const BRACKET_PASTE_OFF: &str = "\x1b[?2004l";
  pub const KITTY_PROTO_ON: &str = "\x1b[>17u";
  pub const KITTY_PROTO_OFF: &str = "\x1b[<u";
  pub const ALT_BUFFER_ENTER: &str = "\x1b[?1049h";
  pub const ALT_BUFFER_EXIT: &str = "\x1b[?1049l";
  pub const CURSOR_HIDE: &str = "\x1b[?25l";
  pub const CURSOR_SHOW: &str = "\x1b[?25h";
  pub const CURSOR_QUERY: &str = "\x1b[6n";
  pub const MOUSE_ON: &str = "\x1b[?1000h\x1b[?1003h\x1b[?1006h";
  pub const MOUSE_OFF: &str = "\x1b[?1003l\x1b[?1000l\x1b[?1006l";
  pub const SCROLL_REGION_RESET: &str = "\x1b[r";
  pub const CURSOR_SAVE: &str = "\x1b7";
  pub const CURSOR_RESTORE: &str = "\x1b8";
  pub const ROW_CLEAR: &str = "\x1b[2K";
  pub const OSC_PROMPT_START: &str = "\x1b]133;A\x07";
  pub const OSC_PROMPT_END: &str = "\x1b]133;B\x07";
  pub const OSC_EXEC_START: &str = "\x1b]133;C\x07";
  pub fn osc_exec_end(code: i32) -> String {
    format!("\x1b]133;D;{code}\x07")
  }

  pub fn emit_osc_prompt_start(&mut self) -> ShResult<()> {
    write!(self, "{}", Self::OSC_PROMPT_START)?;

    Ok(())
  }

  pub fn emit_osc_prompt_end(&mut self) -> ShResult<()> {
    write!(self, "{}", Self::OSC_PROMPT_END)?;
    Ok(())
  }

  pub fn emit_osc_exec_start(&mut self) -> ShResult<()> {
    write!(self, "{}", Self::OSC_EXEC_START)?;
    Ok(())
  }

  pub fn emit_osc_exec_end(&mut self, code: i32) -> ShResult<()> {
    write!(self, "{}", Self::osc_exec_end(code))?;
    Ok(())
  }

  pub fn color_mode(&self) -> Option<ColorMode> {
    if !try_var!("NO_COLOR")?.is_empty() {
      return None;
    }

    if let Some(val) = try_var!("SHED_COLOR_MODE") {
      match val.as_str() {
        "truecolor" | "24bit" => return Some(ColorMode::Truecolor),
        "256" | "256color" => return Some(ColorMode::Palette256),
        "16" | "8" => return Some(ColorMode::Palette16),
        "none" | "off" => return None,
        _ => {}
      }
    }

    if self.term_caps.contains(TermCap::TRUECOLOR) {
      return Some(ColorMode::Truecolor);
    }

    if let Some(term) = try_var!("TERM") {
      if term == "dumb" {
        return None;
      }

      if term.contains("256color") {
        return Some(ColorMode::Palette256);
      }
    }

    Some(ColorMode::Palette16)
  }

  fn toggle_attr(
    buf: &mut String,
    switch: &mut bool,
    on_ctl: &str,
    off_ctl: &str,
    on: bool,
  ) -> ShResult<()> {
    let control = if on && !*switch {
      on_ctl
    } else if !on && *switch {
      off_ctl
    } else {
      return Ok(());
    };

    buf.push_str(control);

    *switch = on;
    Ok(())
  }

  pub fn new() -> Self {
    let tty: Option<RawFd> = TTY_FILENO
      .as_ref()
      .filter(|fd| isatty(fd.as_fd()).unwrap_or(false))
      .map(|fd| fd.as_raw_fd());
    let (cols, rows) = tty.map(get_win_size).unwrap_or((80, 24));

    Self {
      tty,
      reader: parse::PollReader::new(),
      input_buf: String::new(),
      bracketed_paste: false,
      kitty_kbd_proto: false,
      alt_buffer: false,
      cursor_style: CursorStyle::Default,
      interactive: false,
      cursor_visible: true,
      mouse_enabled: false,
      raw_mode: false,
      termios_stack: vec![],
      term_caps: TermCap::empty(),
      xt_version: None,
      t_cols: cols as usize,
      t_rows: rows as usize,
      scroll_region: None,
      last_bell: None,
      test_mode: false,
    }
  }

  /// Access the underlying tty file descriptor.
  pub fn tty(&self) -> Option<BorrowedFd<'static>> {
    let raw = self.tty?;
    let borrowed = unsafe { BorrowedFd::borrow_raw(raw) };
    let isatty = isatty(borrowed).unwrap_or(false);
    let get_fd = fcntl(borrowed, FcntlArg::F_GETFD).is_ok();
    (isatty && get_fd).then_some(borrowed)
  }

  /// Helper for mapping the tty fd to a raw fd
  ///
  /// Not part of the public interface for a reason.
  fn tty_raw(&self) -> Option<RawFd> {
    self.tty().map(|tty| tty.as_raw_fd())
  }

  pub fn isatty(&self) -> bool {
    self.tty.is_some_and(|raw| {
      let borrowed = unsafe { BorrowedFd::borrow_raw(raw) };
      isatty(borrowed).unwrap_or(false)
    })
  }

  pub fn interactive(&self) -> bool {
    self.interactive
  }

  pub fn interactive_guard(&mut self, on: bool) -> TermGuard {
    let old = self.interactive;
    self.interactive = on;

    let guard = TermGuard::new().with_interactive(old);
    guard.activate()
  }

  pub fn mouse_support_guard(&mut self, on: bool) -> ShResult<TermGuard> {
    let guard = TermGuard::new().with_mouse_support(self.mouse_enabled);
    self.toggle_mouse_support(on)?;
    Ok(guard.activate())
  }

  pub fn setup_terminal(&mut self) -> ShResult<TermGuard> {
    let guard = self.save_state();
    self.edit_termios(enable_raw_mode)?;

    self.query_term_caps()?;
    if self.term_caps.contains(TermCap::KITTY_KBD_PROTO)
      && self
        .xt_version
        .as_ref()
        .is_none_or(|v| !v.has_broken_kitty_kbd())
    {
      self.toggle_kitty_proto(true)?;
    } else if self
      .xt_version
      .as_ref()
      .is_some_and(|v| v.needs_wezterm_workaround())
    {
      self.write_direct(Self::APPLICATION_KEYPAD)?;
    } else {
      self.write_direct(Self::MODIFY_OTHER_KEYS)?;
      self.write_direct(Self::APPLICATION_KEYPAD)?;
    }

    Ok(guard.activate())
  }

  pub fn query_term_caps(&mut self) -> ShResult<()> {
    if self.test_mode {
      log::debug!("query_term_caps: test_mode set, skipping probe");
      return Ok(());
    }
    let Some(tty) = self.tty() else {
      log::debug!("query_term_caps: no tty, skipping probe");
      return Ok(());
    };
    let mut caps = TermCap::empty();
    log::debug!(
      "query_term_caps: sending capability burst ({} bytes)",
      Self::CAP_BURST.len()
    );
    self.write_direct(Self::CAP_BURST)?;

    let deadline = Instant::now() + Duration::from_secs(2);
    'outer: while Instant::now() < deadline {
      let remaining = deadline.saturating_duration_since(Instant::now());
      let Ok(timeout) = PollTimeout::try_from(remaining) else {
        log::trace!("query_term_caps: timeout conversion failed, exiting loop");
        break;
      };
      if self.poll(timeout)? == 0 {
        log::debug!("query_term_caps: poll timeout reached, exiting loop");
        break;
      }

      self.reader.read(tty)?;

      match_loop!(self.reader.read_event()? => event, {
        TermEvent::KittyKbdFlags => {
          log::debug!("query_term_caps: kitty keyboard protocol supported");
          self.term_caps.insert(TermCap::KITTY_KBD_PROTO);
        }
        TermEvent::Capabilities { name, .. } => match name.as_str() {
          "RGB" => {
            log::debug!("query_term_caps: TRUECOLOR supported");
            caps.insert(TermCap::TRUECOLOR);
          }
          "Su" => {
            log::debug!("query_term_caps: SYNC_OUTPUT supported");
            caps.insert(TermCap::SYNC_OUTPUT);
          }
          _ => {
            log::trace!("query_term_caps: unrecognized cap name {name:?}");
          }
        }
        TermEvent::XtVersion(ver) => {
          log::debug!("query_term_caps: recording xt_version={ver:?}");
          self.xt_version = Some(ver);
        }
        TermEvent::PrimaryDevAttr => {
          log::debug!("query_term_caps: found DA1");
          break 'outer
        }
        other => {
          log::trace!("query_term_caps: ignoring event {other:?}");
        }
      });
    }

    if let Some(val) = try_var!("COLORTERM")
      && matches!(val.as_str(), "truecolor" | "24bit")
    {
      log::debug!("query_term_caps: COLORTERM environment variable indicates truecolor support");
      caps.insert(TermCap::TRUECOLOR);
    }

    self.term_caps |= caps;

    log::debug!(
      "query_term_caps: complete (term_caps={:?}, local_caps={:?}, xt_version={:?})",
      self.term_caps,
      caps,
      self.xt_version
    );
    Ok(())
  }

  pub fn term_caps(&self) -> TermCap {
    self.term_caps
  }

  fn save_state(&self) -> Snapshot {
    let guard = TermGuard::new()
      .with_raw_mode(self.raw_mode)
      .with_bracketed_paste(self.bracketed_paste)
      .with_kitty_proto(self.kitty_kbd_proto)
      .with_alt_buffer(self.alt_buffer)
      .with_cursor_style(self.cursor_style)
      .with_mouse_support(self.mouse_enabled)
      .with_cursor_visible(self.cursor_visible)
      .with_termios_depth(self.termios_stack.len())
      .with_scroll_region(self.scroll_region);

    Snapshot::new(guard)
  }

  pub fn yield_terminal(&mut self) -> TermGuard {
    let guard = TermGuard::new().with_scroll_region(self.scroll_region);
    self.reset_scroll_region().ok();
    self.flush().ok(); // ensure the reset reaches the terminal before exec
    guard.activate()
  }

  pub fn scroll_up(&mut self, lines: usize) -> ShResult<()> {
    if lines == 0 {
      return Ok(());
    }
    self.write_direct(&format!("\x1b[{lines}S"))?;
    Ok(())
  }

  pub fn load_state(&mut self, guard: &TermGuard) -> ShResult<()> {
    let Some(_tty) = self.tty() else {
      return Ok(());
    };

    if let Some(depth) = guard.termios_depth() {
      while self.termios_stack.len() > depth {
        self.pop_termios()?;
      }
    }

    let mut wrote_seq = false;
    if let Some(bracketed_paste) = guard.bracketed_paste() {
      self.toggle_bracketed_paste(bracketed_paste)?;
      wrote_seq = true;
    }
    if let Some(kitty_proto) = guard.kitty_proto() {
      self.toggle_kitty_proto(kitty_proto)?;
      wrote_seq = true;
    }
    if let Some(alt_buffer) = guard.alt_buffer() {
      self.toggle_alt_buffer(alt_buffer)?;
      wrote_seq = true;
    }
    if let Some(cursor_visible) = guard.cursor_visible() {
      self.toggle_cursor_visibility(cursor_visible)?;
      wrote_seq = true;
    }
    if let Some(cursor_style) = guard.cursor_style() {
      self.set_cursor_style(cursor_style)?;
      wrote_seq = true;
    }
    if let Some(mouse_mode) = guard.mouse_support() {
      self.toggle_mouse_support(mouse_mode)?;
      wrote_seq = true;
    }
    if let Some(interactive) = guard.interactive() {
      self.interactive = interactive;
    }
    if let Some(scroll_region) = guard.scroll_region() {
      match scroll_region {
        Some((top, bottom)) => self.set_scroll_region(top, bottom)?,
        None => self.reset_scroll_region()?,
      }
      wrote_seq = true;
    }

    if wrote_seq {
      self.flush()?; // flush restore sequences immediately
    }
    Ok(())
  }

  pub fn update_t_dims(&mut self) {
    let Some(tty) = self.tty() else { return };
    let (cols, rows) = get_win_size(tty.as_raw_fd());
    self.t_cols = cols as usize;
    self.t_rows = rows as usize;

    // If a scroll region is active, recompute its bottom relative to the
    // new terminal size. Assumes the owner intends to reserve 2 rows at
    // the bottom (status line + gap above it).
    if let Some((top, _)) = self.scroll_region {
      let new_bottom = (rows.saturating_sub(2)).max(top);
      self.set_scroll_region(top, new_bottom).ok();
    }
  }

  pub fn poll(&mut self, timeout: PollTimeout) -> ShResult<i32> {
    let Some(tty) = self.tty() else { return Ok(0) };
    let poll_fd = PollFd::new(tty, PollFlags::POLLIN);
    Ok(poll(&mut [poll_fd], timeout)?)
  }

  pub fn get_cursor_pos(&mut self) -> ShResult<Option<(Rows, Cols)>> {
    if self.test_mode {
      return Ok(None);
    }
    let Some(tty) = self.tty() else {
      return Ok(None);
    };

    // ask the terminal where our cursor is
    self.write_direct(Self::CURSOR_QUERY)?;

    if self.poll(PollTimeout::from(50u8))? == 0 {
      // timeout - assume we didn't get a response
      return Ok(None);
    }

    self.reader.read(tty)?;

    while let Some(event) = self.reader.read_event()? {
      let TermEvent::CursorPos(row, col) = event else {
        continue;
      };
      return Ok(Some((row, col)));
    }
    Ok(None)
  }

  /// Called before the prompt is drawn. If we are not on column 1, push a vid-inverted '%' and then a '\n\r'.
  ///
  /// Aping zsh with this but it's a nice feature.
  pub fn fix_cursor_column(&mut self) -> ShResult<()> {
    let Some((_, c)) = self.get_cursor_pos()? else {
      return Ok(());
    };

    if c.0 != 1 {
      self.input_buf.push_str("\x1b[7m%\x1b[0m\n\r");
    }
    Ok(())
  }

  pub fn calc_cursor_movement(&mut self, old: Pos, new: Pos) -> ShResult<()> {
    let err = |_| {
      ShErr::simple(
        ShErrKind::InternalErr,
        "Failed to write to cursor movement buffer",
      )
    };

    match new.row.cmp(&old.row) {
      std::cmp::Ordering::Greater => {
        let shift = new.row - old.row;
        match shift {
          1 => self.input_buf.push_str("\x1b[B"),
          _ => write!(self, "\x1b[{shift}B").map_err(err)?,
        }
      }
      std::cmp::Ordering::Less => {
        let shift = old.row - new.row;
        match shift {
          1 => self.input_buf.push_str("\x1b[A"),
          _ => write!(self, "\x1b[{shift}A").map_err(err)?,
        }
      }
      std::cmp::Ordering::Equal => { /* Do nothing */ }
    }

    match new.col.cmp(&old.col) {
      std::cmp::Ordering::Greater => {
        let shift = new.col - old.col;
        match shift {
          1 => self.input_buf.push_str("\x1b[C"),
          _ => write!(self, "\x1b[{shift}C").map_err(err)?,
        }
      }
      std::cmp::Ordering::Less => {
        let shift = old.col - new.col;
        match shift {
          1 => self.input_buf.push_str("\x1b[D"),
          _ => write!(self, "\x1b[{shift}D").map_err(err)?,
        }
      }
      std::cmp::Ordering::Equal => { /* Do nothing */ }
    }

    Ok(())
  }

  pub fn t_cols(&self) -> usize {
    self.t_cols
  }

  pub fn t_rows(&self) -> usize {
    self.t_rows
  }

  pub fn buf_ends_with_newline(&self) -> bool {
    self.input_buf.ends_with('\n')
  }

  pub fn verbatim_single(&mut self, on: bool) {
    self.reader.verbatim_single = on;
  }

  pub fn send_bell(&mut self) -> ShResult<()> {
    if shopt!(core.bell_enabled) {
      // we use a cooldown because I don't like having my ears assaulted by 1 million bells
      // whenever i finish clearing the line using backspace.
      let now = Instant::now();

      // surprisingly, a fixed cooldown like '100' is actually more annoying than 1 million bells.
      // I've found this range of 50-150 to be the best balance
      let cooldown = rand::random_range(50..150);
      let should_send = match self.last_bell {
        None => true,
        Some(time) => now.duration_since(time).as_millis() > cooldown,
      };
      if should_send {
        self.write_direct("\x07")?;
        self.last_bell = Some(now);
      }
    }
    Ok(())
  }

  pub fn controller(&self) -> Option<Pid> {
    let tty = self.tty()?;
    nix::unistd::tcgetpgrp(tty).ok()
  }

  pub fn attach(&mut self, pgid: Pid) -> ShResult<()> {
    let Some(tty) = self.tty() else {
      return Ok(());
    };
    // If we aren't attached to a terminal, the pgid already controls it, or the
    // process group does not exist Then return ok
    let term_controller = self.controller().unwrap_or(Pid::this());
    let isatty = self.isatty();
    if !isatty || pgid == term_controller || killpg(pgid, None).is_err() {
      return Ok(());
    }

    if pgid == getpgrp() && term_controller != getpgrp() {
      kill(term_controller, Signal::SIGTTOU).ok();
    }

    let mut new_mask = SigSet::empty();
    let mut mask_bkup = SigSet::empty();

    new_mask.add(Signal::SIGTSTP);
    new_mask.add(Signal::SIGTTIN);
    new_mask.add(Signal::SIGTTOU);

    pthread_sigmask(SigmaskHow::SIG_BLOCK, Some(&new_mask), Some(&mut mask_bkup))?;

    let result = tcsetpgrp(tty, pgid);

    pthread_sigmask(
      SigmaskHow::SIG_SETMASK,
      Some(&mask_bkup),
      Some(&mut new_mask),
    )?;

    if let Err(e) = result {
      log::error!("Failed to set terminal process group: {e}");
      tcsetpgrp(tty, getpgrp())?;
    }

    Ok(())
  }

  pub fn read(&mut self) -> ShResult<usize> {
    let Some(tty) = self.tty() else { return Ok(0) };
    self.reader.read(tty)
  }

  pub fn drain_keys(&mut self) -> ShResult<Vec<KeyEvent>> {
    let mut keys = vec![];
    while let Some(key) = self.reader.readkey()? {
      keys.push(key);
    }
    Ok(keys)
  }

  pub fn cooked_mode_guard(&mut self) -> ShResult<TermGuard> {
    let guard = self.save_state();
    self.toggle_bracketed_paste(false)?;
    self.edit_termios(enable_cooked_mode)?;
    Ok(guard.activate())
  }

  pub fn cooked_no_echo_guard(&mut self) -> ShResult<TermGuard> {
    let guard = self.save_state();
    self.toggle_bracketed_paste(false)?;
    self.edit_termios(|t| {
      enable_cooked_mode(t);
      t.local_flags.remove(termios::LocalFlags::ECHO);
    })?;
    Ok(guard.activate())
  }

  pub fn prepare_for_pager(&mut self) -> ShResult<TermGuard> {
    let guard = self.save_state();
    self.edit_termios(enable_raw_mode)?;
    self.toggle_bracketed_paste(false)?;
    self.toggle_alt_buffer(true)?;
    self.toggle_mouse_support(true)?;
    self.set_cursor_style(CursorStyle::Default)?;
    self.toggle_cursor_visibility(false)?;
    self.flush()?;
    Ok(guard.activate())
  }

  pub fn prepare_for_exec(&mut self) -> ShResult<TermGuard> {
    let guard = self.save_state();
    self.toggle_bracketed_paste(false)?;
    self.toggle_alt_buffer(false)?;
    self.edit_termios(enable_cooked_mode)?;
    self.set_cursor_style(CursorStyle::Default)?;
    self.toggle_kitty_proto(false)?;
    self.flush()?; // flush escape sequences before switching to cooked mode
    Ok(guard.activate())
  }

  pub fn raw_mode_guard(&mut self) -> ShResult<TermGuard> {
    let guard = self.save_state();
    self.edit_termios(enable_raw_mode)?;
    Ok(guard.activate())
  }

  fn push_termios(&mut self) -> ShResult<()> {
    let Some(tty) = self.tty() else { return Ok(()) };
    let current =
      tcgetattr(tty).map_err(|e| sherr!(InternalErr, "Failed to get terminal attributes: {e}"))?;

    self.termios_stack.push(current);
    Ok(())
  }

  fn pop_termios(&mut self) -> ShResult<()> {
    let Some(tty) = self.tty_raw() else {
      return Ok(());
    };
    if let Some(termios) = self.termios_stack.pop() {
      tcsetattr(
        unsafe { BorrowedFd::borrow_raw(tty) },
        termios::SetArg::TCSANOW,
        &termios,
      )
      .map_err(|e| sherr!(InternalErr, "Failed to restore terminal attributes: {e}"))?;
    }
    Ok(())
  }

  /// Defensively re-apply raw mode to the tty.
  ///
  /// Some child programs (notably pagers like less invoked by bat) run their
  /// own termios cleanup on exit. When they die after the shell has reaped
  /// their parent, their cleanup races with our pop_termios and can leave
  /// the tty in cooked mode. We follow zsh's mitigation here: just re-apply
  /// raw mode at the start of every readline iteration. Cheap (one ioctl)
  /// and resilient to any late tcsetattr from orphaned descendants.
  pub fn enforce_raw_mode(&mut self) -> ShResult<()> {
    let Some(tty) = self.tty_raw() else {
      return Ok(());
    };
    let tty = unsafe { BorrowedFd::borrow_raw(tty) };
    let mut t =
      tcgetattr(tty).map_err(|e| sherr!(InternalErr, "Failed to get terminal attributes: {e}"))?;
    enable_raw_mode(&mut t);
    tcsetattr(tty, termios::SetArg::TCSANOW, &t)
      .map_err(|e| sherr!(InternalErr, "Failed to set terminal attributes: {e}"))?;
    Ok(())
  }

  pub fn edit_termios<F: FnOnce(&mut Termios)>(&mut self, f: F) -> ShResult<()> {
    let Some(tty) = self.tty_raw() else {
      return Ok(());
    };
    let tty = unsafe { BorrowedFd::borrow_raw(tty) };
    self.push_termios()?;

    let mut raw =
      tcgetattr(tty).map_err(|e| sherr!(InternalErr, "Failed to get terminal attributes: {e}"))?;

    f(&mut raw);
    tcsetattr(tty, termios::SetArg::TCSANOW, &raw)
      .map_err(|e| sherr!(InternalErr, "Failed to set terminal attributes: {e}"))?;

    Ok(())
  }

  pub fn write_direct(&mut self, buf: &str) -> ShResult<()> {
    let Some(tty) = self.tty() else {
      return Ok(());
    };
    let mut buf = buf.as_bytes();
    while !buf.is_empty() {
      match write(tty, buf) {
        Ok(n) => buf = &buf[n..],
        Err(Errno::EINTR) => continue,
        Err(_) => return Err(std::io::Error::last_os_error().into()),
      }
    }
    Ok(())
  }

  pub fn set_cursor_style(&mut self, style: CursorStyle) -> ShResult<()> {
    if self.cursor_style == style {
      return Ok(());
    }

    let style_raw = style.to_string();
    self.write_all(style_raw.as_bytes())?;
    self.cursor_style = style;
    Ok(())
  }

  pub fn toggle_cursor_visibility(&mut self, visible: bool) -> ShResult<()> {
    Self::toggle_attr(
      &mut self.input_buf,
      &mut self.cursor_visible,
      Self::CURSOR_SHOW,
      Self::CURSOR_HIDE,
      visible,
    )
  }

  pub fn toggle_alt_buffer(&mut self, on: bool) -> ShResult<()> {
    Self::toggle_attr(
      &mut self.input_buf,
      &mut self.alt_buffer,
      Self::ALT_BUFFER_ENTER,
      Self::ALT_BUFFER_EXIT,
      on,
    )?;
    // Most xterm-class terminals save/restore the scroll region across
    // alt-screen transitions. Re-assert ours on exit defensively in case
    // the terminal didn't. Bracket with cursor save/restore so DECSTBM
    // doesn't home the cursor as a side effect.
    if !on && let Some((top, bottom)) = self.scroll_region {
      self.with_saved_cursor(|this| {
        write!(this, "\x1b[{top};{bottom}r").ok();
      });
    }
    Ok(())
  }

  pub fn toggle_bracketed_paste(&mut self, on: bool) -> ShResult<()> {
    Self::toggle_attr(
      &mut self.input_buf,
      &mut self.bracketed_paste,
      Self::BRACKET_PASTE_ON,
      Self::BRACKET_PASTE_OFF,
      on,
    )
  }

  pub fn toggle_mouse_support(&mut self, on: bool) -> ShResult<()> {
    Self::toggle_attr(
      &mut self.input_buf,
      &mut self.mouse_enabled,
      Self::MOUSE_ON,
      Self::MOUSE_OFF,
      on,
    )
  }

  pub fn toggle_kitty_proto(&mut self, on: bool) -> ShResult<()> {
    Self::toggle_attr(
      &mut self.input_buf,
      &mut self.kitty_kbd_proto,
      Self::KITTY_PROTO_ON,
      Self::KITTY_PROTO_OFF,
      on,
    )
  }

  /// Set the terminal scroll region (DECSTBM). `top` and `bottom` are
  /// 1-indexed inclusive row numbers.
  pub fn set_scroll_region(&mut self, top: u16, bottom: u16) -> ShResult<()> {
    self.with_saved_cursor(|this| {
      write!(this, "\x1b[{top};{bottom}r").ok();
    });
    self.scroll_region = Some((top, bottom));
    Ok(())
  }

  /// Perform an operation and restore the cursor's original position afterwards.
  pub fn with_saved_cursor<T>(&mut self, f: impl Fn(&mut Self) -> T) -> T {
    self.save_cursor();
    let res = f(self);
    self.restore_cursor();
    res
  }

  pub fn reset_scroll_region(&mut self) -> ShResult<()> {
    if let Some((_, bottom)) = self.scroll_region {
      let max_row = self.t_rows as u16;
      self.with_saved_cursor(|this| {
        for row in (bottom + 1)..=max_row {
          this.move_cursor_abs(row, 1);
          this.input_buf.push_str(Self::ROW_CLEAR);
        }
        this.input_buf.push_str(Self::SCROLL_REGION_RESET);
      });
      self.scroll_region = None;
    }
    Ok(())
  }

  pub fn scroll_region(&self) -> Option<(u16, u16)> {
    self.scroll_region
  }

  /// Buffer an `\x1b7` cursor-save. Pairs with `restore_cursor`.
  pub fn save_cursor(&mut self) {
    self.input_buf.push_str(Self::CURSOR_SAVE);
  }

  /// Buffer an `\x1b8` cursor-restore. Restores both position and SGR
  /// state from the matching `save_cursor`.
  pub fn restore_cursor(&mut self) {
    self.input_buf.push_str(Self::CURSOR_RESTORE);
  }

  /// Buffer a CUP (cursor position) sequence to move to absolute (row, col).
  /// Both are 1-indexed.
  pub fn move_cursor_abs(&mut self, row: u16, col: u16) {
    write!(self, "\x1b[{row};{col}H").ok();
  }

  /// Render the status line at the bottom row of the terminal.
  pub fn draw_status_line(&mut self, content: &str) {
    let bottom_row = self.t_rows as u16;
    self.with_saved_cursor(|this| {
      this.move_cursor_abs(bottom_row, 1);
      this.input_buf.push_str(Self::ROW_CLEAR);
      this.input_buf.push_str(content);
    });
  }

  /// Render an ephemeral status message on the row directly above the status line (`t_rows - 1`).
  pub fn draw_status_message(&mut self, content: &str) {
    let row = (self.t_rows as u16).saturating_sub(1);
    self.with_saved_cursor(|this| {
      this.move_cursor_abs(row, 1);
      this.input_buf.push_str(Self::ROW_CLEAR);
      this.input_buf.push_str(content);
    });
  }

  /// Detach this Terminal from the TTY. After calling, `tty()` returns
  /// None and `flush()` silently discards buffered output. Used in forked
  /// children whose stdout is redirected (e.g., command substitutions) to
  /// prevent any terminal-control escape sequences they might emit from
  /// reaching the parent's TTY through the shared fd.
  pub fn detach_tty(&mut self) {
    self.input_buf.clear();
    self.tty = None;
  }

  pub fn clear_under_cursor(&mut self) -> ShResult<()> {
    self.input_buf.push_str("\x1b[0J");
    Ok(())
  }

  #[cfg(test)]
  pub fn set_fd_for_testing(&mut self, fd: Option<RawFd>) {
    self.tty = fd;
    self.test_mode = fd.is_some();
  }
  #[cfg(test)]
  pub fn feed_bytes(&mut self, bytes: &[u8]) {
    self.reader.feed_bytes(bytes);
  }

  pub fn reset_for_exit(&mut self) {
    let Some(_tty) = self.tty() else { return };

    self.reset_scroll_region().ok();
    self.toggle_bracketed_paste(false).ok();
    self.toggle_kitty_proto(false).ok();
    self.toggle_cursor_visibility(true).ok();
    self.toggle_alt_buffer(false).ok();
    if self.cursor_style != CursorStyle::Default {
      self.set_cursor_style(CursorStyle::Default).ok();
    }
    self.flush().ok();
    while !self.termios_stack.is_empty() {
      self.pop_termios().ok();
    }
  }
}

impl Default for Terminal {
  fn default() -> Self {
    Self::new()
  }
}

impl std::io::Write for Terminal {
  fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
    match std::str::from_utf8(buf) {
      Ok(s) => self.input_buf.push_str(s),
      Err(_) => self.input_buf.push_str(&String::from_utf8_lossy(buf)),
    }
    Ok(buf.len())
  }
  fn flush(&mut self) -> std::io::Result<()> {
    let Some(tty) = self.tty() else {
      self.input_buf.clear();
      return Ok(());
    };
    let mut buf = self.input_buf.as_bytes();
    while !buf.is_empty() {
      match write(tty, buf) {
        Ok(n) => buf = &buf[n..],
        Err(Errno::EINTR) => continue,
        Err(_) => {
          self.input_buf.clear();
          return Err(std::io::Error::last_os_error());
        }
      }
    }
    self.input_buf.clear();
    Ok(())
  }
}
