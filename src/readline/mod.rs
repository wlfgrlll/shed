use nix::poll::PollTimeout;
use scopeguard::defer;
use std::collections::VecDeque;
use std::time::Instant;
use std::{cmp::Ordering, sync::mpsc};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

mod complete;
mod context;
mod editcmd;
mod editmode;
mod highlight;
mod histimport;
mod history;
mod layout;
mod linebuf;
mod register;
pub(super) mod stash;

use complete::{
  CompResponse, Completer, FuzzyCompleter, FuzzySelector, GridCompleter, SelectorResponse,
  SimpleCompleter,
};
use editcmd::{Cmd, CmdFlags, EditCmd, Motion, Verb, invert_char_motion};
use editmode::{
  CmdReplay, EditMode, Emacs, RemoteMode, ViEx, ViInsert, ViNormal, ViReplace, ViSearch,
  ViSearchRev, ViVerbatim, ViVisual,
};
use layout::{Layout, clear_rows, move_cursor_to_end, redraw};
use linebuf::LineBuf;
use register::{RegisterContent, RegisterName};

use super::state::meta::MetaTab;
use super::state::terminal::Terminal;
use super::{
  autocmd, builtin, eval,
  expand::{self, expand_keymap, expand_prompt},
  flush_term, key, keys,
  keys::{KeyCode, KeyEvent, KeyMapFlags, KeyMapMatch, ModKeys},
  match_loop, motion, procio, sherr, shopt, socket,
  state::{
    self, Shed,
    shopt::CompleteStyle,
    terminal::{SyncOutputGuard, calc_str_width, truncate_with_ellipsis},
    util::with_vars,
    vars::{Var, VarFlags, VarKind},
  },
  status_msg, system_msg, try_var,
  util::{self, ShResult},
  var, verb, write_term,
};

pub(super) use complete::{
  BashCompSpec, Candidate, CompContext, CompFlags, CompMatch, CompOptFlags, CompOpts, CompSpec,
  ScoredCandidate,
};
pub(super) use editcmd::Direction;
pub(super) use editmode::ModeReport;
pub(super) use histimport::import_history;
pub(super) use history::{HistEntry, History};
pub(super) use linebuf::{Hint, Lines, Pos};

#[cfg(test)]
pub(super) use register::{restore_registers, save_registers};

#[cfg(test)]
pub mod tests;
pub(super) const DEFAULT_PS1: &str =
  "\\e[0m\\n\\e[1;0m\\u\\e[1;36m@\\e[1;31m\\h\\n\\e[1;36m\\W\\e[1;32m/\\n\\e[1;32m\\$\\e[0m ";

/// A simple line editor with optional history
///
/// Used for simpler text inputs like Ex mode and the help builtin's search bar
/// Do note that passing a table name to this struct will create a database table if it doesn't already exist.
#[derive(Default, Debug)]
pub(super) struct SimpleEditor {
  pub buf: LineBuf,
  pub mode: Emacs,
  pub history: Option<History>,
}

impl SimpleEditor {
  pub fn new(history_table: Option<&str>) -> Self {
    let history = history_table.map(|name| {
      state::util::get_db_conn()
        .and_then(|conn| History::new(conn, name).ok())
        .unwrap_or(History::empty(name))
    });
    Self {
      history,
      buf: LineBuf::default(),
      mode: Emacs::default(),
    }
  }
  fn should_grab_history(&mut self, cmd: &EditCmd) -> bool {
    cmd.verb().is_none()
      && (cmd
        .motion()
        .is_some_and(|m| matches!(m, Cmd(_, Motion::LineUp)))
        && self.buf.start_of_line() == 0)
      || (cmd
        .motion()
        .is_some_and(|m| matches!(m, Cmd(_, Motion::LineDown)))
        && self.buf.on_last_line())
  }
  fn scroll_history(&mut self, count: isize) {
    let Some(history) = self.history.as_mut() else {
      return;
    };
    let entry = history.scroll(count);
    if let Some(entry) = entry {
      let buf = std::mem::take(&mut self.buf);
      self.buf.set_buffer(entry.command());
      if history.pending.is_none() {
        history.pending = Some(buf);
      }
      self.buf.set_hint(None);
      self.buf.move_cursor_to_end();
    } else if let Some(pending) = history.pending.take() {
      self.buf = pending;
    }
  }
  pub fn handle_key(&mut self, key: KeyEvent) -> ShResult<()> {
    let Some(mut cmd) = self.mode.handle_key(key) else {
      return Ok(());
    };
    if self.should_grab_history(&cmd) {
      let count = match cmd.motion().unwrap() {
        Cmd(_, Motion::LineUp) => -1,
        Cmd(_, Motion::LineDown) => 1,
        _ => unreachable!(),
      };
      self.scroll_history(count);
      return Ok(());
    }
    if let Some(Cmd(_, Verb::DeleteOrEof)) = cmd.verb_mut() {
      // user pressed Ctrl+D in emacs mode
      // we've gotta resolve this into either Delete or EndOfFile here
      if self.buf.is_empty() {
        cmd.verb_mut().unwrap().1 = Verb::EndOfFile;
      } else {
        cmd.verb_mut().unwrap().1 = Verb::Delete;
      }
    }

    self.buf.exec_cmd(&cmd)
  }
}

/// Non-blocking readline result
#[derive(Debug)]
pub(super) enum ReadlineEvent {
  /// A complete line was entered
  Line(String),
  /// Ctrl+D on empty line - request to exit
  Eof,
  /// No complete input yet, need more bytes
  Pending,
}

pub(super) struct LineData {
  pub buffer: String,
  pub cursor: usize,
  pub anchor: Option<usize>,
  pub hint: Option<String>,
  pub mode: String,
}

pub(super) struct StatusLine {
  left: String,
  middle: String,
  right: String,
  dirty: bool,
}

impl StatusLine {
  pub fn new() -> Self {
    let (left_raw, middle_raw, right_raw) = Shed::shopts(|o| {
      let s = &o.statline;
      (
        s.left_string.clone(),
        s.middle_string.clone(),
        s.right_string.clone(),
      )
    });
    let saved_status = state::Shed::get_status();
    let left = expand_prompt(&left_raw).unwrap_or(left_raw.clone());
    let middle = expand_prompt(&middle_raw).unwrap_or(middle_raw.clone());
    let right = expand_prompt(&right_raw).unwrap_or(right_raw.clone());
    state::Shed::set_status(saved_status);

    Self {
      left,
      middle,
      right,
      dirty: false,
    }
  }
  pub fn parts(&mut self) -> (&str, &str, &str) {
    if self.dirty {
      self.refresh_now();
    }
    (&self.left, &self.middle, &self.right)
  }
  pub fn render(&mut self, term_width: usize) -> String {
    let (left, middle, right) = self.parts();

    let lw = calc_str_width(left);
    let mw = calc_str_width(middle);
    let rw = calc_str_width(right);

    let right_w = rw.min(term_width);
    let after_right = term_width.saturating_sub(right_w);

    let middle_w = mw.min(after_right);
    let after_middle = after_right.saturating_sub(middle_w);

    let left_w = lw.min(after_middle);
    let leftover = after_middle.saturating_sub(left_w);

    let middle_str = if middle_w < mw {
      truncate_with_ellipsis(middle, middle_w)
    } else {
      middle.to_string()
    };

    let left_str = if left_w < lw {
      truncate_with_ellipsis(left, left_w)
    } else {
      left.to_string()
    };

    let pad_lm = " ".repeat(leftover / 2);
    let pad_mr = " ".repeat(leftover - (leftover / 2));

    format!("{left_str}{pad_lm}{middle_str}{pad_mr}{right}")
  }
  pub fn refresh(&mut self) {
    self.dirty = true;
  }
  pub fn refresh_now(&mut self) {
    *self = Self::new();
  }
}

impl Default for StatusLine {
  fn default() -> Self {
    Self::new()
  }
}

pub(super) struct Prompt {
  ps1_expanded: String,
  psr_expanded: Option<String>,
  dirty: bool,
}

#[expect(clippy::similar_names)]
impl Prompt {
  pub fn new() -> Self {
    autocmd!(PrePrompt);

    let Some(ps1_raw) = try_var!("PS1") else {
      return Self::default();
    };
    // PS1 expansion may involve running commands (e.g., for \h or \W), which can modify shell state
    let saved_status = state::Shed::get_status();

    let Ok(ps1_expanded) = expand_prompt(&ps1_raw) else {
      return Self::default();
    };
    let psr_raw = try_var!("PSR");
    let psr_expanded = psr_raw
      .clone()
      .map(|r| expand_prompt(&r))
      .transpose()
      .ok()
      .flatten();

    // Restore shell state after prompt expansion, since it may have been modified by command substitutions in the prompt
    state::Shed::set_status(saved_status);

    autocmd!(PostPrompt);

    Self {
      ps1_expanded,
      psr_expanded,
      dirty: false,
    }
  }

  pub fn get_ps1(&mut self) -> &str {
    if self.dirty {
      self.refresh_now();
    }
    &self.ps1_expanded
  }
  fn refresh_now(&mut self) {
    let saved_status = state::Shed::get_status();
    *self = Self::new();
    state::Shed::set_status(saved_status);
    self.dirty = false;
  }

  pub fn refresh(&mut self) {
    self.dirty = true;
  }
}

impl Default for Prompt {
  fn default() -> Self {
    Self {
      ps1_expanded: expand_prompt(DEFAULT_PS1).unwrap_or_else(|_| DEFAULT_PS1.to_string()),
      psr_expanded: None,
      dirty: false,
    }
  }
}

enum LineCmd {
  Execute(EditCmd),
  SubmitLine(EditCmd),
  AppendHint,
  ScrollHist(isize),
  ScrollHistVirtual(EditCmd),
  EndOfFile,
  Quit,
  ClearScreen,
  ResetWidget,
  NormalSeq(Vec<usize>, String),
  TriggerCompletion,
  TriggerHistSearch,
}

impl LineCmd {
  pub fn switch_to_normal() -> Self {
    Self::Execute(EditCmd {
      register: RegisterName::default(),
      verb: Some(verb!(Verb::NormalMode)),
      motion: None,
      raw_seq: String::new(),
      flags: CmdFlags::empty(),
    })
  }
}

#[derive(Default, Debug)]
enum MacroRecord {
  #[default]
  Idle,
  Recording(RegisterName, Vec<KeyEvent>),
}

impl MacroRecord {
  pub fn is_recording(&self) -> bool {
    matches!(self, MacroRecord::Recording(_, _))
  }
  pub fn feed_key_event(&mut self, event: KeyEvent) {
    if let MacroRecord::Recording(_, keys) = self {
      keys.push(event);
    }
  }
  pub fn commit_recording(&mut self) -> Option<RegisterName> {
    if let MacroRecord::Recording(reg, keys) = std::mem::take(self) {
      reg.write_to_register(register::RegisterContent::Macro(keys));

      Some(reg)
    } else {
      None
    }
  }
  pub fn start_recording(&mut self, reg: RegisterName) {
    *self = MacroRecord::Recording(reg, vec![]);
  }
  pub fn status(&self) -> Option<String> {
    match self {
      MacroRecord::Recording(reg, _) => {
        let name = reg.display()?;
        Some(format!("recording {name}"))
      }
      MacroRecord::Idle => None,
    }
  }
}

struct CompHintRequest {
  req_gen: u64,
  buffer: String,
  cursor_pos: usize,
}

struct HintWorker {
  channel: Option<mpsc::Sender<CompHintRequest>>,
  req_gen: u64,
  last_sent: Option<(String, usize)>,
}

impl HintWorker {
  pub fn new() -> Self {
    let (channel, receiver) = mpsc::channel::<CompHintRequest>();
    std::thread::spawn(move || Self::main(&receiver));
    Self {
      channel: Some(channel),
      req_gen: 0,
      last_sent: None,
    }
  }
  pub fn dispatch_worker(&mut self, buffer: String, cursor_pos: usize) {
    if self
      .last_sent
      .as_ref()
      .is_some_and(|(b, c)| b == &buffer && *c == cursor_pos)
    {
      return;
    }
    self.last_sent = Some((buffer.clone(), cursor_pos));
    self.req_gen = self.req_gen.wrapping_add(1);
    let req = CompHintRequest {
      req_gen: self.req_gen,
      buffer,
      cursor_pos,
    };
    if let Some(channel) = &self.channel {
      channel.send(req).ok();
    }
  }
  fn main(receiver: &mpsc::Receiver<CompHintRequest>) {
    let mut completer = SimpleCompleter::default();
    let token = &*socket::PRIVATE_TOKEN;
    while let Ok(mut req) = receiver.recv() {
      while let Ok(newer) = receiver.try_recv() {
        // drain until newest
        req = newer;
      }
      let CompHintRequest {
        req_gen,
        buffer,
        cursor_pos,
      } = req;
      completer.reset();
      let source = complete::CompSource::Shell;
      let outcome = completer
        .complete(buffer, cursor_pos, 1, source)
        .ok()
        .flatten();

      // If we got an exact match, use it as the hint
      if let Some(CompMatch::Exact { line }) = outcome {
        let token_start = completer.token_span().0;
        let msg = format!("PRIVATE {token} set-comp-hint {req_gen} {token_start} {line}");
        socket::send_to_socket(&msg).ok();
      }
    }
  }
}

pub(super) struct ShedLine {
  prompt: Prompt,
  statline: Option<StatusLine>,
  completer: Option<Box<dyn Completer>>,

  mode: Box<dyn EditMode>,
  saved_mode: Option<Box<dyn EditMode>>,
  pending_keymap: Vec<KeyEvent>,
  repeat_action: Option<CmdReplay>,
  repeat_motion: Option<Cmd<Motion>>,
  repeat_macro: Option<RegisterName>,
  editor: LineBuf,
  macro_record: MacroRecord,

  old_layout: Option<Layout>,
  blank_rows_above: u16,
  overlay_displacement: u16,
  history: History,
  ex_history: History,

  needs_redraw: bool,
  ctrl_d_warning_counter: usize,
  status_msgs: VecDeque<(String, Instant)>,

  worker: HintWorker,
}

impl ShedLine {
  pub fn new(prompt: Prompt) -> ShResult<Self> {
    Self::new_private(prompt, true)
  }

  pub fn new_no_hist(prompt: Prompt) -> ShResult<Self> {
    Self::new_private(prompt, false)
  }

  fn new_private(prompt: Prompt, with_hist: bool) -> ShResult<Self> {
    let statline = shopt!(statline.enable).then(StatusLine::new);

    let history = if with_hist {
      if let Some(conn) = state::util::get_db_conn() {
        History::new(conn, "shed_history")?
      } else {
        History::empty("shed_history")
      }
    } else {
      History::empty("shed_history")
    };
    let ex_history = if let Some(conn) = state::util::get_db_conn() {
      History::new(conn, "ex_history")?
    } else {
      History::empty("ex_history")
    };
    let mode = if shopt!(set.vi) {
      Box::new(ViInsert::new()) as Box<dyn EditMode>
    } else {
      Box::new(Emacs::new()) as Box<dyn EditMode>
    };
    let mut new = Self {
      prompt,
      statline,
      completer: None,
      mode,
      saved_mode: None,
      pending_keymap: Vec::new(),
      old_layout: None,
      blank_rows_above: 0,
      overlay_displacement: 0,
      repeat_action: None,
      repeat_motion: None,
      repeat_macro: None,
      editor: LineBuf::new(),
      macro_record: MacroRecord::Idle,
      history,
      ex_history,
      needs_redraw: true,
      ctrl_d_warning_counter: 0,
      status_msgs: VecDeque::new(),
      worker: HintWorker::new(),
    };
    Shed::vars_mut(|v| {
      v.set_var(
        "SHED_EDIT_MODE",
        VarKind::Str(new.mode.report_mode().to_string()),
        VarFlags::empty(),
      )
    })?;
    new.prompt.refresh();
    if let Some(line) = new.statline.as_mut() {
      line.refresh();
    }
    write_term!("\n").ok();
    new.print_line(false)?;
    Ok(new)
  }

  pub fn get_line_data(&self) -> LineData {
    LineData {
      buffer: self.editor.joined().replace('\n', "\\n"),
      cursor: self.editor.cursor_to_flat(),
      anchor: self.editor.anchor_to_flat(),
      hint: self.editor.try_join_hint().map(|s| s.replace('\n', "\\n")),
      mode: self.mode.report_mode().to_string(),
    }
  }

  /// A mutable reference to the currently focused editor
  /// This includes the main `LineBuf`, and sub-editors for modes like Ex mode.
  fn focused_editor(&mut self) -> &mut LineBuf {
    self.mode.editor().unwrap_or(&mut self.editor)
  }

  /// A mutable reference to the currently focused history, if any.
  /// This includes the main history struct, and history for sub-editors like Ex mode.
  fn focused_history(&mut self) -> &mut History {
    self.mode.history().unwrap_or(&mut self.history)
  }

  fn history_fzf(&mut self) -> Option<&mut FuzzySelector> {
    self.focused_history().fuzzy_finder.as_mut()
  }

  /// Mark that the display needs to be redrawn (e.g., after SIGWINCH)
  pub fn mark_dirty(&mut self) {
    self.needs_redraw = true;
    self.prompt.refresh();
    if let Some(line) = self.statline.as_mut() {
      line.refresh();
    }
  }

  pub fn reset_active_widget(&mut self, full_redraw: bool) -> ShResult<()> {
    if let Some(comp) = self.completer.as_mut() {
      comp.reset_stay_active();
      self.needs_redraw = true;
      Ok(())
    } else if let Some(finder) = self.history_fzf() {
      finder.reset_query();
      self.needs_redraw = true;
      Ok(())
    } else {
      self.reset(full_redraw)
    }
  }

  /// Reset readline state for a new prompt
  pub fn reset(&mut self, full_redraw: bool) -> ShResult<()> {
    // Clear old display before resetting state - old_layout must survive
    // so print_line can call clear_rows with the full multi-line layout
    self.prompt.refresh();
    if let Some(line) = self.statline.as_mut() {
      line.refresh();
    }
    self.editor = LineBuf::default();
    let mut mode = if shopt!(set.vi) {
      Box::new(ViInsert::new()) as Box<dyn EditMode>
    } else {
      Box::new(Emacs::new()) as Box<dyn EditMode>
    };
    self.swap_mode(&mut mode);
    self.needs_redraw = true;
    if full_redraw {
      self.old_layout = None;
    }
    if self.statline.is_none() && shopt!(statline.enable) {
      Shed::term_mut(|t| -> ShResult<()> {
        let total_rows = t.t_rows() as u16;
        let new_bottom = total_rows.saturating_sub(2).max(1);
        let cursor_row = t
          .get_cursor_pos()
          .ok()
          .flatten()
          .map_or(new_bottom, |(r, _)| r.0 as u16);
        if cursor_row > new_bottom {
          let scroll_amount = (cursor_row - new_bottom) as usize;
          t.scroll_up(scroll_amount).ok();
          // scroll_up shifts content; the visual cursor row doesn't
          // change. Move it up so it tracks the prompt's new row.
          t.write_direct(&format!("\x1b[{scroll_amount}A")).ok();
        }
        t.set_scroll_region(1, new_bottom);
        Ok(())
      })?;
      self.old_layout = None;
      self.statline = Some(StatusLine::new());
    }
    self.focused_history().pending = None;
    self.focused_history().reset();

    self.print_line(false)
  }

  pub fn prompt_mut(&mut self) -> &mut Prompt {
    &mut self.prompt
  }

  pub fn curr_keymap_flags(&self) -> KeyMapFlags {
    let mut flags = KeyMapFlags::empty();
    match self.mode.report_mode() {
      ModeReport::Insert => flags |= KeyMapFlags::INSERT,
      ModeReport::Normal => flags |= KeyMapFlags::NORMAL,
      ModeReport::Ex => flags |= KeyMapFlags::EX,
      ModeReport::Visual => flags |= KeyMapFlags::VISUAL,
      ModeReport::Replace => flags |= KeyMapFlags::REPLACE,
      ModeReport::Verbatim => flags |= KeyMapFlags::VERBATIM,
      ModeReport::Emacs => flags |= KeyMapFlags::EMACS,
      ModeReport::Remote => flags |= KeyMapFlags::REMOTE,
      ModeReport::Search | ModeReport::RevSearch => {}
    }

    if self.mode.pending_seq().is_some_and(|seq| !seq.is_empty()) {
      flags |= KeyMapFlags::OP_PENDING;
    }

    flags
  }

  /// This method ensures that the editing mode (Vi or Emacs) matches the 'vi' option, and switches modes if necessary.
  pub fn fix_editing_mode(&mut self) {
    if shopt!(set.vi) && self.mode.report_mode() == ModeReport::Emacs {
      self.swap_mode(&mut (Box::new(ViInsert::new()) as Box<dyn EditMode>));
    } else if !shopt!(set.vi) && self.mode.report_mode() != ModeReport::Emacs {
      self.swap_mode(&mut (Box::new(Emacs::new()) as Box<dyn EditMode>));
    }
  }

  fn should_complete(&mut self) -> bool {
    !self.focused_editor().cursor_in_leading_ws()
  }

  fn should_submit(&mut self) -> bool {
    if self.mode.report_mode() == ModeReport::Normal {
      return true;
    }
    if self.editor.cursor_is_escaped()
      && matches!(
        self.mode.report_mode(),
        ModeReport::Emacs | ModeReport::Insert
      )
    {
      return false;
    }
    let (depth, failed) = self.editor.cursor_indent_level();
    depth == 0 && !failed
  }

  fn handle_hist_search_key(&mut self, key: KeyEvent) -> ShResult<()> {
    let finder = self.history_fzf().unwrap();
    match finder.handle_key(key)? {
      SelectorResponse::Accept(cmd) => {
        let entry_idx = cmd.id().unwrap(); // history entries having an id to unwrap is an invariant.
        self.scroll_history_to(entry_idx);
        if let Some(finder) = self.history_fzf() {
          finder.clear();
        }
        self.focused_history().stop_search();

        with_vars([("HIST_ENTRY".into(), cmd.content().to_string())], || {
          autocmd!(OnHistorySelect);
        });

        Shed::vars_mut(|v| {
          v.set_var(
            "SHED_EDIT_MODE",
            VarKind::Str(self.mode.report_mode().to_string()),
            VarFlags::empty(),
          )
        })
        .ok();
        self.prompt.refresh();
        if let Some(line) = self.statline.as_mut() {
          line.refresh();
        }
        self.needs_redraw = true;
      }
      SelectorResponse::Dismiss => {
        autocmd!(OnHistoryClose);

        self.editor.clear_hint();
        if let Some(finder) = self.history_fzf() {
          finder.clear();
        }
        self.focused_history().stop_search();
        Shed::vars_mut(|v| {
          v.set_var(
            "SHED_EDIT_MODE",
            VarKind::Str(self.mode.report_mode().to_string()),
            VarFlags::empty(),
          )
        })
        .ok();
        self.prompt.refresh();
        if let Some(line) = self.statline.as_mut() {
          line.refresh();
        }
        self.needs_redraw = true;
      }
      SelectorResponse::Consumed => {
        self.needs_redraw = true;
      }
    }
    Ok(())
  }

  fn handle_completion_key(&mut self, key: &KeyEvent) -> ShResult<bool> {
    let dismiss_completer = |this: &mut Self| -> ShResult<()> {
      autocmd!(OnCompletionCancel);

      this.update_editor_hint();
      if let Some(comp) = this.completer.as_mut() {
        comp.clear();
      }
      this.completer = None;
      Shed::vars_mut(|v| {
        v.set_var(
          "SHED_EDIT_MODE",
          VarKind::Str(this.mode.report_mode().to_string()),
          VarFlags::empty(),
        )
      })
      .ok();
      this.prompt.refresh();
      if let Some(line) = this.statline.as_mut() {
        line.refresh();
      }
      this.needs_redraw = true;
      Ok(())
    };

    let comp = self.completer.as_mut().unwrap();
    match comp.handle_key(key.clone())? {
      CompResponse::Accept(candidate) => {
        let comp = self.completer.as_ref().unwrap();
        let span_start = comp.token_span().0;
        let new_cursor = span_start + candidate.len();
        let line = comp.get_completed_line(&candidate);
        self.focused_editor().set_buffer(&line);
        self.focused_editor().set_cursor_from_flat(new_cursor);

        if !self.focused_history().at_pending() {
          self.focused_history().reset_to_pending();
        }
        self.update_editor_hint();
        // clear() needs old_layout to erase the selector, so clear before dropping
        if let Some(comp) = self.completer.as_mut() {
          comp.clear();
        }
        self.completer = None;
        self.needs_redraw = true;

        Shed::vars_mut(|v| {
          v.set_var(
            "SHED_EDIT_MODE",
            VarKind::Str(self.mode.report_mode().to_string()),
            VarFlags::empty(),
          )
        })
        .ok();
        self.prompt.refresh();
        if let Some(line) = self.statline.as_mut() {
          line.refresh();
        }

        with_vars(
          [("COMP_CANDIDATE".into(), candidate.content().to_string())],
          || autocmd!(OnCompletionSelect),
        );

        Ok(true)
      }
      CompResponse::Preview(candidate) => {
        // Splice the candidate into the buffer the same way Accept does,
        // but DON'T dismiss the completer. The user is still cycling.
        let comp = self.completer.as_ref().unwrap();
        let span_start = comp.token_span().0;
        let new_cursor = span_start + candidate.len();
        let line = comp.get_completed_line(&candidate);
        self.focused_editor().set_buffer(&line);
        self.focused_editor().set_cursor_from_flat(new_cursor);
        self.update_editor_hint();
        self.needs_redraw = true;
        Ok(true)
      }
      CompResponse::Consumed => {
        /* just redraw */
        self.needs_redraw = true;
        Ok(true)
      }
      CompResponse::Passthrough => Ok(false),
      CompResponse::Dismiss => {
        dismiss_completer(self)?;
        Ok(true)
      }
      CompResponse::DismissPassthrough => {
        dismiss_completer(self)?;
        Ok(false)
      }
    }
  }

  fn handle_keymap(&mut self, key: &KeyEvent) -> ShResult<Option<ReadlineEvent>> {
    let keymap_flags = self.curr_keymap_flags();
    self.pending_keymap.push(key.clone());

    let mut matches = Shed::logic(|l| l.keymaps_filtered(keymap_flags, &self.pending_keymap));
    let is_exact =
      matches.len() == 1 && matches[0].compare(&self.pending_keymap) == KeyMapMatch::IsExact;

    if matches.is_empty() {
      // No matches. Drain the buffered keys and execute them.
      for key in std::mem::take(&mut self.pending_keymap) {
        if let Some(event) = self.handle_key(&key)? {
          return Ok(Some(event));
        }
      }
      self.needs_redraw = true;
    } else if is_exact {
      // We have a single exact match. Execute it.
      let keymap = matches.remove(0);
      self.pending_keymap.clear();
      let action = keymap.action_expanded();
      for key in action {
        if let Some(event) = self.handle_key(&key)? {
          return Ok(Some(event));
        }
      }
      self.needs_redraw = true;
    }

    // There is ambiguity. Allow the timeout in the main loop to handle this.
    Ok(None)
  }

  /// Process any available input and return readline event
  /// This is non-blocking - returns Pending if no complete line yet
  pub fn process_input(&mut self, keys: Vec<KeyEvent>) -> ShResult<ReadlineEvent> {
    // Redraw if needed
    if self.needs_redraw {
      self.print_line(false)?;
      self.needs_redraw = false;
    }

    // Process all available keys
    for key in keys {
      if self.macro_record.is_recording() {
        if let KeyEvent(KeyCode::Char('q'), ModKeys::NONE) = key {
          self.repeat_macro = self.macro_record.commit_recording();
          continue;
        }
        self.macro_record.feed_key_event(key.clone());
      }
      if let Some(ev) = self.dispatch_key(key)? {
        return Ok(ev);
      }
    }
    if self.completer.is_none() && self.history_fzf().is_none() {
      Shed::vars_mut(|v| {
        v.set_var(
          "SHED_EDIT_MODE",
          VarKind::Str(self.mode.report_mode().to_string()),
          VarFlags::empty(),
        )
      })
      .ok();
    }

    // Redraw if we processed any input
    if self.needs_redraw {
      self.print_line(false)?;
      self.needs_redraw = false;
    }
    let line_data = self.get_line_data();
    Shed::meta_mut(|m| m.notify_line_edit(line_data));

    self.try_comp_hint();

    Ok(ReadlineEvent::Pending)
  }

  fn try_comp_hint(&mut self) {
    if !self.editor.cursor_at_max() {
      return;
    }

    let buf = self.editor.joined();
    let cursor_pos = self.editor.cursor_to_flat();
    if !buf.is_empty() {
      self.worker.dispatch_worker(buf, cursor_pos);
    }
  }

  pub fn worker_req_gen(&mut self) -> u64 {
    self.worker.req_gen
  }

  fn dispatch_key(&mut self, key: KeyEvent) -> ShResult<Option<ReadlineEvent>> {
    if self.history_fzf().is_some() {
      self.handle_hist_search_key(key)?;
      Ok(None)
    } else if self.completer.is_some() && self.handle_completion_key(&key)? {
      // self.handle_completion_key() returns true if we need to continue the loop
      Ok(None)
    } else if self.mode.pending_seq().is_some_and(|seq| !seq.is_empty())
      || self.mode.is_input_mode()
    {
      // Vi mode is waiting for more input (e.g. after 'f', 'd', etc.)
      // Bypass keymap matching and send directly to the mode handler
      let ev = self.handle_key(&key)?;
      self.update_editor_search();
      self.editor.set_cursor_clamp(self.mode.clamp_cursor());

      Ok(ev)
    } else {
      self.handle_keymap(&key)
    }
  }

  /// Replay a sequence of `KeyEvent`s as if they came from the input stream.
  pub fn replay_keys(
    &mut self,
    keys: Vec<KeyEvent>,
    with_keymaps: bool,
  ) -> ShResult<Option<ReadlineEvent>> {
    for key in keys {
      let ev = if with_keymaps {
        self.dispatch_key(key)?
      } else {
        self.handle_key(&key)?
      };
      if let Some(ev) = ev {
        return Ok(Some(ev));
      }
    }
    Ok(None)
  }

  fn accept_hint(&mut self) -> Option<ReadlineEvent> {
    self.editor.edit(|e| {
      e.accept_hint();
    });
    if !self.focused_history().at_pending() {
      self.focused_history().reset_to_pending();
    }
    self
      .history
      .update_pending_cmd((&self.editor.joined(), self.editor.cursor_to_flat()));
    self.needs_redraw = true;

    None
  }

  fn handle_tab(&mut self, key: &KeyEvent) -> Option<ReadlineEvent> {
    let KeyEvent(KeyCode::Tab, mod_keys) = key else {
      return None;
    };

    if self.mode.report_mode() != ModeReport::Ex
      && self
        .editor
        .edit(|e| e.attempt_inline_expansion(&self.history))
    {
      // If history expansion occurred, don't attempt completion yet
      self.update_editor_hint();
      return None;
    }

    let direction = match *mod_keys {
      ModKeys::SHIFT => -1,
      _ => 1,
    };
    let line = self.focused_editor().joined();
    let cursor_pos = self.focused_editor().cursor_byte_pos();

    let mut comp = self
      .completer
      .take()
      .unwrap_or_else(|| match shopt!(prompt.complete_style) {
        CompleteStyle::Grid => Box::new(GridCompleter::new()),
        CompleteStyle::Fuzzy => Box::new(FuzzyCompleter::default()),
      });
    let source = if self.mode.report_mode() == ModeReport::Ex {
      complete::CompSource::ExMode
    } else {
      complete::CompSource::Shell
    };
    match comp.complete(line, cursor_pos, direction, source) {
      Err(e) => {
        e.print_error();
        // Printing the error invalidates the layout
        self.old_layout = None;
      }
      Ok(Some(comp_match)) => {
        let line = comp_match.into_line();
        let cand = comp.selected_candidate().unwrap_or_default();
        with_vars(
          [("COMP_CANDIDATE".into(), cand.content().to_string())],
          || autocmd!(OnCompletionSelect),
        );

        let span_start = comp.token_span().0;

        let new_cursor = span_start
          + comp
            .selected_candidate()
            .map(|c| c.len())
            .unwrap_or_default();

        self.focused_editor().set_buffer(&line);
        self.focused_editor().set_cursor_from_flat(new_cursor);

        if !self.focused_history().at_pending() {
          self.focused_history().reset_to_pending();
        }
        self.update_editor_hint();
        Shed::vars_mut(|v| {
          v.set_var(
            "SHED_EDIT_MODE",
            VarKind::Str(self.mode.report_mode().to_string()),
            VarFlags::empty(),
          )
        })
        .ok();

        // Single candidate, don't store the completer
      }
      Ok(None) => {
        let candidates = comp.all_candidates();
        let num_candidates = candidates.len();
        with_vars(
          [
            ("NUM_MATCHES".into(), Into::<Var>::into(num_candidates)),
            ("MATCHES".into(), Into::<Var>::into(candidates)),
            ("SEARCH_STR".into(), Into::<Var>::into(comp.token())),
          ],
          || autocmd!(OnCompletionStart),
        );

        if comp.is_active() {
          self.completer = Some(comp);
          Shed::vars_mut(|v| {
            v.set_var(
              "SHED_EDIT_MODE",
              VarKind::Str("COMPLETE".to_string()),
              VarFlags::empty(),
            )
          })
          .ok();
          self.prompt.refresh();
          if let Some(line) = self.statline.as_mut() {
            line.refresh();
          }
          self.needs_redraw = true;
          self.editor.clear_hint();
        } else {
          Shed::term_mut(Terminal::send_bell).ok();
        }
      }
    }

    self.needs_redraw = true;
    None
  }

  fn start_hist_search(&mut self) {
    let initial = self.focused_editor().joined();
    if let Some(entry) = self.focused_history().start_search(&initial) {
      with_vars([("HIST_ENTRY".into(), entry.clone())], || {
        autocmd!(OnHistorySelect);
      });

      self.focused_editor().set_buffer(&entry);
      self.focused_editor().move_cursor_to_end();
      self
        .history
        .update_pending_cmd((&self.editor.joined(), self.editor.cursor_to_flat()));
      self.editor.clear_hint();
    } else {
      let finder = self.history_fzf().unwrap();
      let entries = finder.candidates().to_vec();
      let matches = finder
        .filtered()
        .iter()
        .map(|sc| sc.candidate.content().to_string())
        .collect::<Vec<_>>();

      let num_entries = entries.len();
      let num_matches = matches.len();
      with_vars(
        [
          ("ENTRIES".into(), Into::<Var>::into(entries)),
          ("NUM_ENTRIES".into(), Into::<Var>::into(num_entries)),
          ("MATCHES".into(), Into::<Var>::into(matches)),
          ("NUM_MATCHES".into(), Into::<Var>::into(num_matches)),
          ("SEARCH_STR".into(), Into::<Var>::into(initial)),
        ],
        || autocmd!(OnHistoryOpen),
      );

      if self.history_fzf().is_some() {
        Shed::vars_mut(|v| {
          v.set_var(
            "SHED_EDIT_MODE",
            VarKind::Str("SEARCH".to_string()),
            VarFlags::empty(),
          )
        })
        .ok();
        self.prompt.refresh();
        if let Some(line) = self.statline.as_mut() {
          line.refresh();
        }
        self.needs_redraw = true;
        self.editor.clear_hint();
      } else {
        Shed::term_mut(Terminal::send_bell).ok();
      }
    }
  }

  pub(crate) fn in_insert_mode(&self) -> bool {
    matches!(self.mode.report_mode(), ModeReport::Insert)
  }

  fn extract_line_nums(&self, cmd: &EditCmd) -> ShResult<Vec<usize>> {
    if let Some(Cmd(_, Verb::ExCmd(node))) = cmd.verb() {
      return self.editor.lines_for_ex_node(node);
    }
    Ok(vec![self.editor.row()])
  }

  fn submit(&mut self) -> ShResult<Option<ReadlineEvent>> {
    self.editor.clear_hint();
    self.editor.set_cursor_from_flat(self.editor.cursor_max());
    self.print_line(true)?;
    if let Some(layout) = &self.old_layout {
      move_cursor_to_end(layout);
    }
    if shopt!(line.trim_on_submit) {
      self.editor.trim();
    }
    write_term!("\r\n").ok();
    // Command output fills the region from below the prompt; tracked
    // blank rows above will scroll into scrollback as it does, and any
    // overlay displacement is moot once the prompt is gone.
    self.blank_rows_above = 0;
    self.overlay_displacement = 0;
    let buf = self.editor.take_buf();
    self.focused_history().reset();
    Ok(Some(ReadlineEvent::Line(buf)))
  }

  fn resolve_key(&mut self, key: &KeyEvent) -> ShResult<Option<LineCmd>> {
    if self.should_accept_hint(key) {
      return Ok(Some(LineCmd::AppendHint));
    } else if let KeyEvent(KeyCode::Tab, _) = key
      && self.should_complete()
    {
      return Ok(Some(LineCmd::TriggerCompletion));
    } else if let key!(Ctrl + 'r') = key
      && matches!(
        self.mode.report_mode(),
        ModeReport::Emacs | ModeReport::Insert | ModeReport::Ex
      )
    {
      return Ok(Some(LineCmd::TriggerHistSearch));
    }

    let Ok(cmd) = self.mode.handle_key_fallible(key.clone()) else {
      // it's an ex mode error
      return Ok(Some(LineCmd::switch_to_normal()));
    };

    let Some(cmd) = cmd else { return Ok(None) };

    self.resolve_cmd(cmd)
  }

  fn resolve_cmd(&mut self, mut cmd: EditCmd) -> ShResult<Option<LineCmd>> {
    if let Some(Cmd(_, Verb::Interrupt)) = cmd.verb() {
      return Ok(Some(LineCmd::ResetWidget));
    }

    if let Some(seq) = cmd.try_get_normal_seq() {
      let line_nums = self.extract_line_nums(&cmd)?;
      return Ok(Some(LineCmd::NormalSeq(line_nums, seq.to_string())));
    }

    if self.should_grab_history(&cmd) {
      let offset = cmd.history_scroll_offset().unwrap();

      if shopt!(prompt.hist_cat)
        && cmd
          .flags
          .intersects(CmdFlags::HAS_SHIFT | CmdFlags::HAS_CTRL)
      {
        return Ok(Some(LineCmd::ScrollHistVirtual(cmd)));
      }
      return Ok(Some(LineCmd::ScrollHist(offset)));
    }

    if cmd.is_submit_action() {
      return Ok(Some(LineCmd::SubmitLine(cmd)));
    }

    if let Some(Cmd(_, Verb::DeleteOrEof)) = cmd.verb_mut() {
      // user pressed Ctrl+D in emacs mode
      // we've gotta resolve this into either Delete or EndOfFile here
      if self.focused_editor().is_empty() {
        return Ok(Some(LineCmd::EndOfFile));
      }
      cmd.verb_mut().unwrap().1 = Verb::Delete;
      return Ok(Some(LineCmd::Execute(cmd)));
    } else if let Some(Cmd(_, Verb::ClearScreen)) = cmd.verb() {
      return Ok(Some(LineCmd::ClearScreen));
    }

    if cmd.verb_is(&Verb::EndOfFile) && self.focused_editor().is_empty() {
      return Ok(Some(LineCmd::EndOfFile));
    } else if cmd.is_quit() {
      return Ok(Some(LineCmd::Quit));
    } else if cmd.verb_is(&Verb::AcceptHint) {
      return Ok(Some(LineCmd::AppendHint));
    }

    Ok(Some(LineCmd::Execute(cmd)))
  }

  fn run_cmd(&mut self, cmd: EditCmd) -> ShResult<Option<ReadlineEvent>> {
    // check if it's an edit
    // we don't count Verb::Change since its possible for it to be called and not actually change anything
    // e.g. 'cc' on an empty line, 'C' at the end of a line, etc.
    // this is only used for ringing the bell
    let has_edit_verb = cmd
      .verb()
      .is_some_and(|v| v.1.is_edit() && v.1 != Verb::Change);

    let is_ctrl_d_motion = cmd.motion_is(&Motion::HalfScreenDown);

    let is_ex_cmd = cmd.flags.contains(CmdFlags::IS_EX_CMD);
    if is_ex_cmd {
      self.ex_history.push(&cmd.raw_seq).ok();
      self.ex_history.reset();
    }

    if cmd.verb_is(&Verb::RecordMacro) {
      log::debug!("starting macro recording with cmd: {cmd:?}");
      if cmd.register.name().is_none() {
        return Ok(None);
      }
      cmd.register.write_to_register(RegisterContent::Empty);

      self.macro_record.start_recording(cmd.register);
      return Ok(None);
    }

    if cmd.verb_is(&Verb::PlayMacro) {
      let target = if cmd.register.name().is_some() {
        cmd.register
      } else if let Some(reg) = self.repeat_macro {
        reg
      } else {
        return Ok(None);
      };

      let events = match target.read_from_register() {
        None => return Ok(None),
        Some(content) => match content {
          RegisterContent::Empty => return Ok(None),
          RegisterContent::Span(s) | RegisterContent::Line(s) | RegisterContent::Block(s) => {
            let joined = Lines::from(s).join();
            expand_keymap(&joined)
          }
          RegisterContent::Macro(keys) => keys,
        },
      };

      self.editor.start_undo_merge();
      if let Ok(Some(event)) = self.replay_keys(events, false) {
        self.editor.stop_undo_merge();
        return Ok(Some(event));
      }
      self.editor.stop_undo_merge();
      return Ok(None);
    }

    let before = self.editor.joined();
    let before_cursor = self.editor.cursor();

    self.exec_cmd(cmd, false)?;

    if let Some(keys) = Shed::meta_mut(MetaTab::take_pending_widget_keys) {
      self.replay_keys(keys, false)?;
    }
    let after = self.editor.joined();
    let after_cursor = self.editor.cursor();

    if before != after {
      self.history.mark_mask_stale();
    } else if before == after && has_edit_verb {
      Shed::term_mut(Terminal::send_bell).ok();
    } else if before_cursor == after_cursor && is_ctrl_d_motion {
      if self.ctrl_d_warning_counter == 3 || self.editor.is_empty() {
        // our silly user is spamming ctrl+d for some reason
        // maybe they want to exit the shell?
        status_msg!("Ctrl+D only quits in insert mode. try ':q' or entering insert mode with 'i'");
        self.ctrl_d_warning_counter = 0;
      } else {
        self.ctrl_d_warning_counter += 1;
      }
    }

    self.update_editor_hint();
    self.needs_redraw = true;
    Ok(None)
  }

  fn update_editor_search(&mut self) {
    if matches!(
      self.mode.report_mode(),
      ModeReport::RevSearch | ModeReport::Search
    ) {
      self.editor.update_pending_search(self.mode.pending_seq());
      self.needs_redraw = true;
    }
  }

  pub fn handle_key(&mut self, key: &KeyEvent) -> ShResult<Option<ReadlineEvent>> {
    let Some(linecmd) = self.resolve_key(key)? else {
      self.update_editor_search();
      self.needs_redraw = true;
      return Ok(None);
    };
    if !matches!(&linecmd, LineCmd::ScrollHistVirtual(_)) {
      self.focused_history().stop_virtual_scroll();
      self.editor.clear_concats();
    }

    match linecmd {
      LineCmd::Execute(cmd) => self.run_cmd(cmd),
      LineCmd::ScrollHist(off) => {
        self.scroll_history(off);
        self.needs_redraw = true;
        Ok(None)
      }
      LineCmd::ScrollHistVirtual(cmd) => {
        self.scroll_history_virtual(cmd);
        self.needs_redraw = true;
        Ok(None)
      }
      LineCmd::EndOfFile => {
        if self.focused_editor().joined().is_empty() {
          Ok(Some(ReadlineEvent::Eof))
        } else {
          self.reset_active_widget(false)?;
          Ok(None)
        }
      }
      LineCmd::Quit => Ok(Some(ReadlineEvent::Eof)),
      LineCmd::ClearScreen => {
        // if the status line is enabled, park the cursor at the bottom.
        if shopt!(statline.enable)
          && let Some((top, bottom)) = Shed::term_mut(|t| t.scroll_region()).dims()
        {
          let region_height = (bottom.saturating_sub(top) + 1) as usize;
          Shed::term_mut(|t| t.scroll_up(region_height)).ok();
          Shed::term_mut(|t| t.move_cursor_abs(bottom, 1));
          self.old_layout = None; // stale after manual cursor move
          self.needs_redraw = true;
          return Ok(None);
        }

        // Original behavior: scroll just enough to put the prompt at row 1.
        let cursor_row = Shed::term_mut(Terminal::get_cursor_pos)
          .ok()
          .flatten()
          .map_or(1, |(r, _)| r.0);

        let prompt_cursor_offset = self.old_layout.as_ref().map_or(0, |l| l.cursor.row);

        let prompt_top = cursor_row.saturating_sub(prompt_cursor_offset);
        let scroll_amount = prompt_top.saturating_sub(1);

        if scroll_amount > 0 {
          Shed::term_mut(|t| t.scroll_up(scroll_amount)).ok();
          // Move cursor up to track the prompt's new position
          flush_term!("\x1b[{scroll_amount}A")?;
        }
        self.needs_redraw = true;
        Ok(None)
      }
      LineCmd::ResetWidget => {
        self.reset_active_widget(false)?;
        Ok(None)
      }
      LineCmd::NormalSeq(line_nums, seq) => {
        let keys = expand_keymap(&seq);

        self.editor.start_undo_merge();
        for line in line_nums {
          self.editor.set_cursor(linebuf::Pos { row: line, col: 0 });
          self.swap_mode(&mut (Box::new(ViNormal::new()) as Box<dyn EditMode>));

          if let Err(e) = self.replay_keys(keys.clone(), false) {
            self.editor.stop_undo_merge();
            return Err(e);
          }
        }
        self.editor.stop_undo_merge();

        // just in case
        self.swap_mode(&mut (Box::new(ViNormal::new()) as Box<dyn EditMode>));

        Ok(None)
      }
      LineCmd::TriggerCompletion => Ok(self.handle_tab(key)),
      LineCmd::TriggerHistSearch => {
        self.start_hist_search();
        Ok(None)
      }
      LineCmd::SubmitLine(cmd) => {
        if self.editor.attempt_alias_expansion() {
          self.update_editor_hint();
        }
        if self.editor.attempt_history_expansion(&self.history) {
          // If history expansion occurred, don't submit yet
          self.update_editor_hint();

          Ok(None)
        } else if self.should_submit() || !shopt!(line.linebreak_on_incomplete) {
          self.submit()
        } else {
          self.run_cmd(cmd)
        }
      }
      LineCmd::AppendHint => Ok(self.accept_hint()),
    }
  }

  fn get_layout(&mut self, line: &str) -> Layout {
    let to_cursor = self.editor.window_slice_to_cursor();
    let cols = Shed::term(Terminal::t_cols);
    let prompt = layout::pad_prompt_for_gutter(
      self.prompt.get_ps1(),
      line,
      self.editor.scroll_offset(),
      cols,
    );
    Layout::from_parts(cols, &prompt, &to_cursor, line)
  }
  fn scroll_history_virtual(&mut self, cmd: EditCmd) {
    // This function is used for the Shift/Ctrl+Up/Down history concatenation.
    // Instead of replacing the buffer with a scrolled-to history entry
    // This function appends it to the end of the current buffer with '&&' or ';'
    // depending on if the user is holding shift or ctrl.

    let Cmd(count, motion) = &cmd.motion.unwrap();
    let sep = if cmd.flags.contains(CmdFlags::HAS_SHIFT) {
      " && "
    } else {
      "; "
    };
    match motion {
      Motion::LineUp => {
        self
          .editor
          .edit(|e| match self.history.virtual_scroll_direction() {
            Some(Direction::Forward) => {
              for _ in 0..*count {
                if !e.pop_right() {
                  e.clear_buffer();
                  self.history.stop_virtual_scroll();
                  break;
                }
                self.history.virt_scroll(-1);
              }
            }
            None | Some(Direction::Backward) => {
              for _ in 0..*count {
                let Some(entry) = self.history.virt_scroll(-1) else {
                  continue;
                };
                let command = entry.command().to_string();
                e.concat_left(sep, &command);
                e.move_cursor_to_end();
              }
            }
          });
      }
      Motion::LineDown => {
        self
          .editor
          .edit(|e| match self.history.virtual_scroll_direction() {
            Some(Direction::Backward) => {
              for _ in 0..*count {
                if !e.pop_left() {
                  e.clear_buffer();
                  self.history.stop_virtual_scroll();
                  break;
                }
                self.history.virt_scroll(1);
              }
            }
            None | Some(Direction::Forward) => {
              for _ in 0..*count {
                let Some(entry) = self.history.virt_scroll(1) else {
                  continue;
                };
                let command = entry.command().to_string();
                e.concat_right(sep, &command);
                e.move_cursor_to_end();
              }
            }
          });
      }
      _ => unreachable!(),
    }
  }
  fn scroll_history_to(&mut self, hist_idx: usize) {
    let hist = self.focused_history();
    hist.merge_search_entries();
    hist.constrain_entries(None);
    let entry = self.focused_history().scroll_to(hist_idx).cloned();
    if entry.is_some() {
      let total = self.focused_history().search_mask_count();
      status_msg!("jumped to hist entry: {}/{}", hist_idx + 1, total);
    }
    self.swap_history_editor(entry);
  }
  fn scroll_history(&mut self, count: isize) {
    if self.focused_history().pending.is_none() {
      if count >= 0 {
        // if count >= 0, we are scrolling down
        // but if we are here, it means we are already at the pending command,
        // so return and bell
        Shed::term_mut(Terminal::send_bell).ok();
        return;
      }
      // We are scrolling up from a pending command
      // Let's refresh the search mask to make sure
      // our history is up to date
      let joined = self.editor.joined();
      self.focused_history().update_search_mask(Some(&joined));
    }
    let entry = self.focused_history().scroll(count).cloned();
    self.swap_history_editor(entry);
  }
  fn swap_history_editor(&mut self, entry: Option<HistEntry>) {
    if let Some(entry) = entry {
      let editor = std::mem::take(self.focused_editor());
      self.focused_editor().set_buffer(entry.command());
      if self.focused_history().pending.is_none() {
        self.focused_history().pending = Some(editor);
      }
      self.focused_editor().clear_hint();
      self.focused_editor().move_cursor_to_end();
    } else if let Some(pending) = self.focused_history().pending.take() {
      *self.focused_editor() = pending;
    } else {
      // If we are here it should mean we are on our pending command
      // And the user tried to scroll history down
      // Since there is no "future" history, we should just bell and do nothing
      Shed::term_mut(Terminal::send_bell).ok();
      return;
    }
    let clamp = self.mode.clamp_cursor();
    self.focused_editor().set_cursor_clamp(clamp);
    self.focused_editor().fix_cursor();
  }
  fn should_accept_hint(&self, event: &KeyEvent) -> bool {
    if self.editor.cursor_at_max() && self.editor.has_hint() {
      match self.mode.report_mode() {
        ModeReport::Replace | ModeReport::Insert | ModeReport::Emacs => {
          matches!(event, KeyEvent(KeyCode::Right, ModKeys::NONE))
        }
        ModeReport::Visual | ModeReport::Normal => {
          matches!(event, KeyEvent(KeyCode::Right, ModKeys::NONE))
            || (self.mode.pending_seq().unwrap(/* always Some on normal mode */).is_empty()
              && matches!(event, KeyEvent(KeyCode::Char('l'), ModKeys::NONE)))
        }
        _ => false,
      }
    } else {
      false
    }
  }

  fn should_grab_history(&mut self, cmd: &EditCmd) -> bool {
    cmd.is_virtual_scroll()
      || cmd
        .verb()
        .is_some_and(|v| matches!(v, Cmd(_, Verb::HistoryUp | Verb::HistoryDown)))
      || cmd.verb().is_none()
        && (cmd
          .motion()
          .is_some_and(|m| matches!(m, Cmd(_, Motion::LineUp)))
          && self.editor.start_of_line() == 0)
      || (cmd
        .motion()
        .is_some_and(|m| matches!(m, Cmd(_, Motion::LineDown)))
        && self.editor.on_last_line())
        && !cmd.flags.contains(CmdFlags::IS_SUBMIT)
  }

  pub fn needs_redraw(&self) -> bool {
    self.needs_redraw
  }

  #[expect(clippy::too_many_lines)]
  pub fn print_line(&mut self, final_draw: bool) -> ShResult<()> {
    let _sync = SyncOutputGuard::begin();
    if self.statline.is_some() && !shopt!(statline.enable) {
      self.statline = None;
      Shed::term_mut(|t| -> ShResult<()> {
        let total_rows = t.t_rows() as u16;
        let new_bottom = total_rows.saturating_sub(1).max(1);
        t.with_saved_cursor(|t| t.write_direct(format!("\x1b[{total_rows};1H\x1b[2K").as_str()))?;
        t.set_scroll_region(1, new_bottom);
        Ok(())
      })?;
    }

    // if the cursor ended up in the status message area
    // we have to rescue it.
    if !final_draw
      && !shopt!(statline.enable)
      && let Some((_, bottom)) = Shed::term(Terminal::scroll_region).dims()
    {
      let cursor_row = Shed::term_mut(Terminal::get_cursor_pos)
        .ok()
        .flatten()
        .map_or(bottom, |(r, _)| r.0 as u16);
      if cursor_row > bottom {
        Shed::term_mut(|t| {
          t.move_cursor_abs(bottom, 1);
          t.scroll_up(1)
        })?;

        self.old_layout = None; // stale after manual cursor move
      }
    }

    let line = self.editor.display_window_joined();
    let mut new_layout = self.get_layout(&line);

    let pending_seq = self
      .macro_record
      .status()
      .or_else(|| self.mode.pending_seq());
    let mut prompt_string_right = self.prompt.psr_expanded.clone();
    let has_sub_editor = matches!(
      self.mode.report_mode(),
      ModeReport::Ex | ModeReport::RevSearch | ModeReport::Search
    );

    if prompt_string_right
      .as_ref()
      .is_some_and(|psr| psr.lines().count() > 1)
    {
      log::warn!("PSR has multiple lines, truncating to one line");
      prompt_string_right =
        prompt_string_right.map(|psr| psr.lines().next().unwrap_or_default().to_string());
    }

    let t_cols = Shed::term(Terminal::t_cols);
    let row0_used = self
      .prompt
      .get_ps1()
      .lines()
      .next()
      .map(|l| Layout::calc_pos(t_cols, l, Pos { col: 0, row: 0 }, 0, false))
      .map(|p| p.col)
      .unwrap_or_default();
    let one_line = new_layout.end.row == 0;

    if let Some(comp) = self.completer.as_mut() {
      comp.clear();
    }
    if let Some(finder) = self.history_fzf() {
      finder.clear();
    }

    let predicted_overlay_rows: u16 = self
      .completer
      .as_ref()
      .and_then(|c| c.predicted_rows())
      .unwrap_or(0)
      .saturating_add(
        self
          .focused_history()
          .fuzzy_finder
          .as_ref()
          .map_or(0, FuzzySelector::predicted_rows),
      )
      .try_into()
      .unwrap_or(u16::MAX);

    let mut system_msg = String::new();
    if Shed::system_msg_pending() {
      use std::fmt::Write as FmtWrite;
      while let Some(msg) = Shed::pop_system_msg() {
        writeln!(system_msg, "{msg}").ok();
      }
    }
    let system_msg_layout = Layout::from_parts(t_cols, "", &system_msg, &system_msg);

    if let Some(layout) = self.old_layout.as_ref() {
      clear_rows(layout)?;

      let prev_overlay_rows = std::mem::take(&mut self.overlay_displacement);

      if shopt!(statline.enable) {
        let old_h = layout.end.row as i32 + i32::from(prev_overlay_rows);
        let mut new_h = new_layout.end.row as i32
          + i32::from(predicted_overlay_rows)
          + system_msg_layout.end.row as i32;
        if has_sub_editor {
          new_h += 1;
        }
        let diff = new_h - old_h;
        match diff.cmp(&0) {
          Ordering::Less => {
            // the prompt shrank
            let delta = (-diff) as u16;
            write_term!("\x1b[{delta}L")?; // insert empty rows at cursor
            write_term!("\x1b[{delta}B")?; // move down
            self.blank_rows_above = self.blank_rows_above.saturating_add(delta);
          }
          Ordering::Greater => {
            let diff_u = diff as u16;
            let consume = self.blank_rows_above.min(diff_u);
            let scroll_needed = diff_u.saturating_sub(consume);
            if scroll_needed > 0 {
              // pushes existing content upward
              Shed::term_mut(|t| t.scroll_up(scroll_needed as usize)).ok();
            }
            // take needed space
            write_term!("\x1b[{diff_u}A")?;
            self.blank_rows_above = self.blank_rows_above.saturating_sub(consume);
          }
          Ordering::Equal => { /* nothing to do */ }
        }
      }
    }

    if !system_msg.is_empty() {
      Shed::term_mut(Terminal::clear_under_cursor);
      write_term!("{system_msg}")?;
    }

    redraw(
      self.prompt.get_ps1(),
      &line,
      &new_layout,
      self.editor.scroll_offset(),
      self.editor.lines().len(),
    );

    let seq_fits = pending_seq
      .as_ref()
      .is_some_and(|seq| row0_used + 1 < t_cols.saturating_sub(seq.width()));
    let psr_fits = prompt_string_right
      .as_ref()
      .is_some_and(|psr| new_layout.end.col + 1 < t_cols.saturating_sub(psr.width()));

    if !final_draw
      && let Some(seq) = pending_seq
      && !seq.is_empty()
      && !(prompt_string_right.is_some() && one_line)
      && seq_fits
      && !self.mode.is_input_mode()
    {
      // write our pending sequence
      let to_col = t_cols - calc_str_width(&seq);
      let up = new_layout.cursor.row; // rows to move up from cursor to top line of prompt

      let move_up = if up > 0 {
        format!("\x1b[{up}A")
      } else {
        String::new()
      };

      // Save cursor, move up to top row, move right to column, write sequence,
      // restore cursor
      write_term!("\x1b7{move_up}\x1b[{to_col}G{seq}\x1b8").unwrap();
    } else if !final_draw
      && let Some(psr) = prompt_string_right
      && psr_fits
    {
      // write PSR
      let to_col = t_cols - calc_str_width(&psr);
      let down = new_layout.end.row.saturating_sub(new_layout.cursor.row);
      let move_down = if down > 0 {
        format!("\x1b[{down}B")
      } else {
        String::new()
      };

      write_term!("\x1b7{move_down}\x1b[{to_col}G{psr}\x1b8").unwrap();

      // Record where the PSR ends so clear_rows can account for wrapping
      // if the terminal shrinks.
      let psr_start = Pos {
        row: new_layout.end.row,
        col: to_col,
      };
      new_layout.psr_end = Some(Layout::calc_pos(t_cols, &psr, psr_start, 0, false));
    }

    write_term!("{}", &self.mode.cursor_style()).unwrap();

    // Move to end of layout for overlay draws (completer, history search)
    let has_overlays = self.completer.is_some() || self.history_fzf().is_some();

    let down = new_layout.end.row.saturating_sub(new_layout.cursor.row);
    if has_overlays && down > 0 {
      write_term!("\x1b[{down}B")?;
      new_layout.cursor.row = new_layout.end.row;
    }

    // write sub-prompts for stuff like ex mode
    if let ModeReport::Ex | ModeReport::RevSearch | ModeReport::Search = self.mode.report_mode() {
      let mut pending_seq = self.mode.pending_seq().unwrap_or_default();
      let prefix_seq = match self.mode.report_mode() {
        ModeReport::Ex => ": ",
        ModeReport::RevSearch => "?",
        ModeReport::Search => "/",
        _ => unreachable!(),
      };
      let down = new_layout.end.row - new_layout.cursor.row;
      let move_down = if down > 0 {
        format!("\x1b[{down}B")
      } else {
        String::new()
      };
      if let ModeReport::Ex = self.mode.report_mode()
        && shopt!(highlight.enable)
      {
        let cursor_pos = self.focused_editor().cursor_to_flat();
        pending_seq = highlight::highlight_ex(&pending_seq, &highlight::Palette::new(), cursor_pos);
      }

      write_term!("{move_down}\x1b[1G\n{prefix_seq}{pending_seq}").unwrap();
      new_layout.end.row += 1;
      new_layout.cursor.row = new_layout.end.row;
      new_layout.cursor.col = {
        let cursor_offset = self.mode.pending_cursor().unwrap_or(pending_seq.len());
        let before_cursor = pending_seq
          .graphemes(true)
          .take(cursor_offset)
          .collect::<String>();

        prefix_seq.width() + before_cursor.width()
      };

      write_term!("\x1b[{}G", new_layout.cursor.col + 1).unwrap();
    }

    // Tell the completer the width of the prompt line above its \n so it can
    // account for wrapping when clearing after a resize.
    let preceding_width = if new_layout.psr_end.is_some() {
      t_cols
    } else {
      // Without PSR, use the content width on the cursor's row
      (new_layout.end.col + 1).max(new_layout.cursor.col + 1)
    };

    let mut overlay_rows: usize = 0;
    if let Some(comp) = self.completer.as_mut() {
      comp.set_prompt_line_context(preceding_width, new_layout.end.col);
      overlay_rows += comp.draw();
    }

    if let Some(finder) = self.history_fzf() {
      finder.set_prompt_line_context(preceding_width, new_layout.end.col);
      overlay_rows += finder.draw();
    }
    self.overlay_displacement = overlay_rows.try_into().unwrap_or(u16::MAX);

    if let Some(statline) = self.statline.as_mut()
      && !final_draw
    {
      let cols = Shed::term(Terminal::t_cols);
      let rendered = statline.render(cols);
      Shed::term_mut(|t| t.draw_status_line(&rendered));
    }

    while let Some(msg) = Shed::pop_status_msg() {
      let now = Instant::now();
      self.status_msgs.push_back((msg, now));
    }
    while self.status_msgs.len() > 1 {
      self.status_msgs.pop_front();
    }

    if !final_draw {
      let content = if let Some((msg, time)) = self.status_msgs.front() {
        let elapsed = time.elapsed().as_secs();
        if elapsed < 5 {
          // Schedule a wakeup so the row clears when the message expires
          // even if the user isn't typing.
          let diff = 5000.0 - time.elapsed().as_millis() as f64;
          let timeout = PollTimeout::try_from(diff.max(0.0) as i32).unwrap_or(PollTimeout::NONE);
          Shed::meta_mut(|m| m.set_poll_timeout(Some(timeout)));
          // Reserved row is single-line; if the message has multiple lines,
          // show only the first one.
          msg.lines().next().unwrap_or("").to_string()
        } else {
          self.status_msgs.pop_front();
          String::new()
        }
      } else {
        String::new()
      };

      if !content.is_empty() {
        Shed::term_mut(|t| t.draw_status_message(&content));
      }
    }

    self.old_layout = Some(new_layout);
    self.needs_redraw = false;
    Ok(())
  }

  pub fn try_swap_mode_from_str(&mut self, name: &str) -> bool {
    let Ok(mode) = name.parse::<ModeReport>() else {
      // invalid mode report, ignore
      return false;
    };
    let mut mode = mode.as_edit_mode();
    self.swap_mode(&mut mode);
    true
  }

  fn swap_mode(&mut self, mode: &mut Box<dyn EditMode>) {
    autocmd!(PreModeChange);
    defer!(autocmd!(PostModeChange));

    std::mem::swap(&mut self.mode, mode);
    self.editor.set_cursor_clamp(self.mode.clamp_cursor());
    Shed::vars_mut(|v| {
      v.set_var(
        "SHED_EDIT_MODE",
        VarKind::Str(self.mode.report_mode().to_string()),
        VarFlags::empty(),
      )
    })
    .ok();
    self.prompt.refresh();
    if let Some(line) = self.statline.as_mut() {
      line.refresh();
    }
  }

  #[expect(clippy::too_many_lines)]
  fn exec_mode_transition(&mut self, mut cmd: EditCmd, from_replay: bool) -> ShResult<()> {
    let mut is_insert_mode = false;
    let count = cmd.verb_count();

    let mut mode: Box<dyn EditMode> = if matches!(
      self.mode.report_mode(),
      ModeReport::Ex | ModeReport::Verbatim
    ) && cmd.flags.contains(CmdFlags::EXIT_CUR_MODE)
    {
      if self.mode.report_mode() == ModeReport::Ex
        && let Some(mode) = self.saved_mode.as_ref()
        && let ModeReport::Visual = mode.report_mode()
      {
        self.editor.stop_selecting();
        Box::new(ViNormal::new())
      } else if let Some(saved) = self.saved_mode.take() {
        saved
      } else {
        Box::new(ViNormal::new())
      }
    } else {
      match cmd.verb().unwrap().1 {
        Verb::Change | Verb::InsertModeLineBreak(_) | Verb::InsertMode => {
          is_insert_mode = true;
          Box::new(
            ViInsert::new()
              .with_count(count as u16)
              .record_cmd(cmd.clone()),
          )
        }

        Verb::ExMode => Box::new(ViEx::new(self.editor.is_selecting())),

        Verb::VerbatimMode => {
          Shed::term_mut(|t| t.verbatim_single(true));
          Box::new(ViVerbatim::new().with_count(count as u16))
        }

        Verb::NormalMode => Box::new(ViNormal::new()),

        Verb::ReplaceMode => Box::new(ViReplace::new()),

        Verb::VisualModeSelectLast => {
          if self.mode.report_mode() != ModeReport::Visual {
            self.editor.start_char_select();
          }
          let mut mode: Box<dyn EditMode> = Box::new(ViVisual::new());
          self.swap_mode(&mut mode);

          return self.fire_editor_command(&cmd);
        }
        Verb::VisualMode => {
          self.editor.start_char_select();
          Box::new(ViVisual::new())
        }
        Verb::VisualModeLine => {
          self.editor.start_line_select();
          Box::new(ViVisual::new())
        }

        Verb::SearchMode => Box::new(ViSearch::new(count)),
        Verb::RevSearchMode => Box::new(ViSearchRev::new(count)),

        _ => unreachable!(),
      }
    };

    // The mode we just created swaps places with our current mode
    // After this line, 'mode' contains our previous mode.
    self.swap_mode(&mut mode);

    // check if we left insert/replace mode
    if matches!(
      mode.report_mode(), // 'mode' now contains the mode we just left
      ModeReport::Insert | ModeReport::Replace
    ) {
      self.editor.stop_undo_merge();
    }

    // check if we entered ex/verbatim mode
    if matches!(
      self.mode.report_mode(),
      ModeReport::Ex | ModeReport::Verbatim
    ) {
      self.saved_mode = Some(mode);
      Shed::vars_mut(|v| {
        v.set_var(
          "SHED_EDIT_MODE",
          VarKind::Str(self.mode.report_mode().to_string()),
          VarFlags::empty(),
        )
      })?;
      self.prompt.refresh();
      if let Some(line) = self.statline.as_mut() {
        line.refresh();
      }
      return Ok(());
    }

    if mode.is_repeatable() && !from_replay {
      self.repeat_action = mode.as_replay();
    }

    if let Some(range) = self.editor.select_range()
      && cmd
        .verb()
        .is_some_and(|v| !matches!(v.1, Verb::VisualMode | Verb::VisualModeLine))
    {
      cmd.motion = Some(motion!(range));
    }

    // Set cursor clamp BEFORE executing the command so that motions
    // (like EndOfLine for 'A') can reach positions valid in the new mode
    self.editor.set_cursor_clamp(self.mode.clamp_cursor());
    self.fire_editor_command(&cmd)?;

    if mode.report_mode() == ModeReport::Visual && self.editor.select_range().is_some() {
      self.editor.stop_selecting();
    }

    if is_insert_mode {
      self.editor.mark_insert_mode_start_pos();
    } else {
      self.editor.clear_insert_mode_start_pos();
    }

    Shed::vars_mut(|v| {
      v.set_var(
        "SHED_EDIT_MODE",
        VarKind::Str(self.mode.report_mode().to_string()),
        VarFlags::empty(),
      )
    })?;
    self.prompt.refresh();
    if let Some(line) = self.statline.as_mut() {
      line.refresh();
    }

    Ok(())
  }

  fn handle_cmd_repeat(&mut self, cmd: EditCmd) -> ShResult<()> {
    let Some(replay) = self.repeat_action.clone() else {
      return Ok(());
    };
    let EditCmd { verb, .. } = cmd;
    let Cmd(count, _) = verb.unwrap();
    match replay {
      CmdReplay::ModeReplay { cmds, mut repeat } => {
        if count > 1 {
          repeat = count as u16;
        }

        let old_mode = self.mode.report_mode();

        for _ in 0..repeat {
          let cmds = cmds.clone();
          for (i, cmd) in cmds.iter().enumerate() {
            self.exec_cmd(cmd.clone(), true)?;
            // After the first command, start merging so all subsequent
            // edits fold into one undo entry (e.g. cw + inserted chars)
            if i == 0 {
              self.editor.start_undo_merge();
            }
          }
          // Stop merging at the end of the replay
          self.editor.stop_undo_merge();

          let old_mode_clone: Box<dyn EditMode> = match old_mode {
            ModeReport::Normal => Box::new(ViNormal::new()),
            ModeReport::Insert => Box::new(ViInsert::new()),
            ModeReport::Visual => Box::new(ViVisual::new()),
            ModeReport::Replace => Box::new(ViReplace::new()),
            ModeReport::Verbatim => Box::new(ViVerbatim::new()),
            ModeReport::Emacs => Box::new(Emacs::new()),
            ModeReport::Remote => Box::new(RemoteMode),
            ModeReport::Ex => Box::new(ViEx::new(self.editor.is_selecting())),
            ModeReport::Search => Box::new(ViSearch::new(1)),
            ModeReport::RevSearch => Box::new(ViSearchRev::new(1)),
          };
          self.mode = old_mode_clone;
        }
      }
      CmdReplay::Single(mut cmd) => {
        if count > 1 {
          // Override the counts with the one passed to the '.' command
          if cmd.verb.is_some() {
            if let Some(v_mut) = cmd.verb.as_mut() {
              v_mut.0 = count;
            }
            if let Some(m_mut) = cmd.motion.as_mut() {
              m_mut.0 = 1;
            }
          } else {
            return Ok(()); // it has to have a verb to be repeatable,
            // something weird happened
          }
        }
        self.fire_editor_command(&cmd)?;
      }
    }
    Ok(())
  }

  fn handle_motion_repeat(&mut self, cmd: EditCmd) -> ShResult<()> {
    match cmd.motion.as_ref().unwrap() {
      Cmd(count, Motion::RepeatMotion) => {
        let Some(motion) = self.repeat_motion.clone() else {
          return Ok(());
        };
        let repeat_cmd = EditCmd {
          register: RegisterName::default(),
          verb: cmd.verb,
          motion: Some(motion),
          raw_seq: format!("{count};"),
          flags: CmdFlags::empty(),
        };
        self.fire_editor_command(&repeat_cmd)
      }
      Cmd(count, Motion::RepeatMotionRev) => {
        let Some(motion) = self.repeat_motion.clone() else {
          return Ok(());
        };
        let mut new_motion = invert_char_motion(motion);
        new_motion.0 = *count;
        let repeat_cmd = EditCmd {
          register: RegisterName::default(),
          verb: cmd.verb,
          motion: Some(new_motion),
          raw_seq: format!("{count},"),
          flags: CmdFlags::empty(),
        };
        self.fire_editor_command(&repeat_cmd)
      }
      _ => unreachable!(),
    }
  }
  fn exec_cmd(&mut self, mut cmd: EditCmd, from_replay: bool) -> ShResult<()> {
    if cmd.verb().is_some()
      && let Some(range) = self.editor.select_range()
    {
      cmd.motion = Some(motion!(range));
    }

    if cmd.flags.contains(CmdFlags::IS_CANCEL) {
      self.editor.clear_pending_search();
    }

    if cmd.is_mode_transition() {
      self.exec_mode_transition(cmd, from_replay)
    } else if cmd.is_cmd_repeat() {
      self.handle_cmd_repeat(cmd)
    } else if cmd.is_motion_repeat() {
      self.handle_motion_repeat(cmd)
    } else {
      if self.mode.report_mode() == ModeReport::Visual && self.editor.select_range().is_none() {
        self.editor.stop_selecting();
        let mut mode: Box<dyn EditMode> = Box::new(ViNormal::new());
        self.swap_mode(&mut mode);
      }

      if cmd.is_repeatable() && !from_replay {
        let mut replay_cmd = cmd.clone();
        if self.mode.report_mode() == ModeReport::Visual {
          if let Some(shape_motion) = self.editor.select_mode() {
            replay_cmd.motion = Some(motion!(shape_motion));
          } else {
            log::warn!("You're in visual mode with no select range??");
          }
        }
        self.repeat_action = Some(CmdReplay::Single(Box::new(replay_cmd)));
      }

      if cmd.is_char_search() {
        self.repeat_motion.clone_from(&cmd.motion);
      }

      self.fire_editor_command(&cmd)?;

      self.update_editor_hint();

      if self.mode.report_mode() == ModeReport::Visual
        && cmd
          .verb()
          .is_some_and(|v| v.1.is_edit() || v.1 == Verb::Yank)
      {
        self.editor.stop_selecting();
        let mut mode: Box<dyn EditMode> = Box::new(ViNormal::new());
        self.swap_mode(&mut mode);
      }

      if self.mode.report_mode() != ModeReport::Visual && self.editor.select_range().is_some() {
        self.editor.stop_selecting();
      }

      if cmd.flags.contains(CmdFlags::EXIT_CUR_MODE) {
        let mut mode: Box<dyn EditMode> = if matches!(
          self.mode.report_mode(),
          ModeReport::Ex | ModeReport::Verbatim
        ) {
          if let Some(saved) = self.saved_mode.take() {
            saved
          } else {
            Box::new(ViNormal::new())
          }
        } else {
          Box::new(ViNormal::new())
        };
        self.swap_mode(&mut mode);
      }

      Ok(())
    }
  }

  fn update_editor_hint(&mut self) {
    self
      .history
      .update_pending_cmd((&self.editor.joined(), self.editor.cursor_to_flat()));
    let hint = self.history.get_hint();
    self.editor.set_hint(hint);
  }

  fn fire_editor_command(&mut self, cmd: &EditCmd) -> ShResult<()> {
    let is_shell_cmd = cmd.is_shell_cmd();
    let res = self.editor.exec_cmd(cmd);

    if is_shell_cmd {
      self.needs_redraw = true;
      self.prompt.refresh();
      if let Some(line) = self.statline.as_mut() {
        line.refresh();
      }
    }

    res
  }

  pub(super) fn editor(&self) -> &LineBuf {
    &self.editor
  }

  pub(super) fn editor_mut(&mut self) -> &mut LineBuf {
    &mut self.editor
  }

  pub(super) fn pending_keymap(&self) -> &[KeyEvent] {
    &self.pending_keymap
  }

  pub(super) fn history(&self) -> &History {
    &self.history
  }

  pub(super) fn history_mut(&mut self) -> &mut History {
    &mut self.history
  }

  pub(super) fn pending_keymap_mut(&mut self) -> &mut Vec<KeyEvent> {
    &mut self.pending_keymap
  }

  pub(super) fn set_needs_redraw(&mut self, needs_redraw: bool) {
    self.needs_redraw = needs_redraw;
  }
  #[cfg(test)]
  pub fn with_initial(mut self, initial: &str) -> Self {
    self.editor = LineBuf::new().with_initial(initial, 0);
    {
      let s = self.editor.joined();
      let c = self.editor.cursor_to_flat();
      self.focused_history().update_pending_cmd((&s, c));
    }
    self
  }
}
