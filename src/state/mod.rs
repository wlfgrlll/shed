use chrono::{DateTime, Local};
use rusqlite::Connection;
use std::{
  cell::RefCell,
  collections::VecDeque,
  fmt::Display,
  sync::{
    Arc, OnceLock,
    atomic::{AtomicI32, Ordering},
  },
  time::SystemTime,
};

use super::{
  WtStat, autocmd, builtin, eval, expand, keys, match_loop, procio, readline, sherr,
  shopt as shopt_macro, signal,
  state::vars::{VarFlags, VarKind},
  system_msg, try_var, two_way_display, util as crate_util,
  util::{Pos, ShErr, ShErrKind, ShResult},
  var, write_term, writefd,
};

pub mod jobs;
pub(super) mod logic;
pub(super) mod meta;
pub(super) mod scopes;
pub mod shopt;
pub(super) mod terminal;
pub(super) mod util;
pub(super) mod vars;

thread_local! {
  static SHED: Shed = Shed::new();
}

#[derive(Clone, Debug)]
pub(super) struct Message {
  when: SystemTime,
  what: String,
}

impl Message {
  pub fn new(what: String) -> Self {
    Self {
      when: SystemTime::now(),
      what,
    }
  }
  pub fn with_timestamp(&self) -> String {
    let time: DateTime<Local> = (self.when).into();
    let formatted = time.format("[%H:%M:%S]").to_string();
    let msg = self.what.trim().replace('\n', "\n\t\t"); // aligns multiline messages

    format!("{formatted}\t{msg}")
  }
}

impl Display for Message {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    write!(f, "{}", self.what)
  }
}

/// The shell
///
/// Every bit of data that this program needs to track over
/// its lifecycle is stored here
#[derive(Debug)]
pub(super) struct Shed {
  // constructed in state/util.rs
  jobs: RefCell<jobs::JobTab>,
  var_scopes: RefCell<scopes::ScopeStack>,
  meta: RefCell<meta::MetaTab>,
  logic: RefCell<logic::LogTab>,
  terminal: RefCell<terminal::Terminal>,
  shopts: RefCell<shopt::ShOpts>,
  db_conn: OnceLock<Option<Arc<Connection>>>,
  status_code: AtomicI32,

  status_msg_queue: RefCell<VecDeque<Message>>,
  status_msg_hist: RefCell<VecDeque<Message>>,

  system_msg_queue: RefCell<VecDeque<Message>>,
  system_msg_hist: RefCell<VecDeque<Message>>,

  #[cfg(test)]
  saved: RefCell<Option<Box<Self>>>,
}

impl Shed {
  pub fn new() -> Self {
    Self {
      jobs: RefCell::new(jobs::JobTab::new()),
      var_scopes: RefCell::new(scopes::ScopeStack::new()),
      meta: RefCell::new(meta::MetaTab::new()),
      logic: RefCell::new(logic::LogTab::new()),
      terminal: RefCell::new(terminal::Terminal::new()),
      shopts: RefCell::new(shopt::ShOpts::default()),
      db_conn: OnceLock::new(),
      status_code: AtomicI32::new(0),

      status_msg_queue: RefCell::new(VecDeque::new()),
      status_msg_hist: RefCell::new(VecDeque::new()),

      system_msg_queue: RefCell::new(VecDeque::new()),
      system_msg_hist: RefCell::new(VecDeque::new()),

      #[cfg(test)]
      saved: RefCell::new(None),
    }
  }

  /*
   * State Accessor Functions
   *
   * The reason we use this "take a function, execute it on a borrow" pattern
   * is to make positively sure that the lifetimes of the borrows are handled safely.
   *
   * The idea is that this makes it much harder to have overlapping borrows of the same field.
   * Like, you wouldn't call Shed::vars() inside of Shed::vars(), for instance.
   *
   * The main footgun associated with using these is re-entrancy.
   * For instance, If you call Shed::vars_mut() in a place that can be accessed
   * by Shed::vars_mut(), (e.g. inside the VarTab methods), the shell will crash with a borrow error.
   * Let's not do that!
   *
   * This pattern results in the codebase being split into two parts:
   * 1. The part that can call these functions.
   * 2. The part that can be interacted with from inside these functions.
   *
   * The second part is pretty much entirely housed within this module.
   * These two parts must be as separated as possible. It's not possible to get complete isolation,
   * since codepaths like expansion can find ways to escape back into regular execution contexts.
   *
   * Overall, if we only use these to get and set data and not perform any actual calculations, we should be fine.
   */

  /// Read from the job table
  pub fn jobs<T, F: FnOnce(&jobs::JobTab) -> T>(f: F) -> T {
    SHED.with(|shed| f(&shed.jobs.borrow()))
  }
  pub fn jobs_mut<T, F: FnOnce(&mut jobs::JobTab) -> T>(f: F) -> T {
    SHED.with(|shed| f(&mut shed.jobs.borrow_mut()))
  }

  /// Read from the var scope stack
  pub fn vars<T, F: FnOnce(&scopes::ScopeStack) -> T>(f: F) -> T {
    SHED.with(|shed| f(&shed.var_scopes.borrow()))
  }
  pub fn vars_mut<T, F: FnOnce(&mut scopes::ScopeStack) -> T>(f: F) -> T {
    SHED.with(|shed| f(&mut shed.var_scopes.borrow_mut()))
  }

  /// Read from the metadata table
  pub fn meta<T, F: FnOnce(&meta::MetaTab) -> T>(f: F) -> T {
    SHED.with(|shed| f(&shed.meta.borrow()))
  }
  pub fn meta_mut<T, F: FnOnce(&mut meta::MetaTab) -> T>(f: F) -> T {
    SHED.with(|shed| f(&mut shed.meta.borrow_mut()))
  }

  /// Read from the logic table
  pub fn logic<T, F: FnOnce(&logic::LogTab) -> T>(f: F) -> T {
    SHED.with(|shed| f(&shed.logic.borrow()))
  }
  pub fn logic_mut<T, F: FnOnce(&mut logic::LogTab) -> T>(f: F) -> T {
    SHED.with(|shed| f(&mut shed.logic.borrow_mut()))
  }

  /// Read from the shell options
  pub fn shopts<T, F: FnOnce(&shopt::ShOpts) -> T>(f: F) -> T {
    SHED.with(|shed| f(&shed.shopts.borrow()))
  }
  pub fn shopts_mut<T, F: FnOnce(&mut shopt::ShOpts) -> T>(f: F) -> T {
    SHED.with(|shed| f(&mut shed.shopts.borrow_mut()))
  }

  #[track_caller]
  pub fn term<T, F: FnOnce(&terminal::Terminal) -> T>(f: F) -> T {
    let caller = std::panic::Location::caller();
    SHED.with(|shed| {
      let term = shed
        .terminal
        .try_borrow()
        .unwrap_or_else(|_| panic!("with_term: RefCell already borrowed (called from {caller})"));
      f(&term)
    })
  }
  #[track_caller]
  pub fn term_mut<T, F: FnOnce(&mut terminal::Terminal) -> T>(f: F) -> T {
    let caller = std::panic::Location::caller();
    SHED.with(|shed| {
      let mut term = shed
        .terminal
        .try_borrow_mut()
        .unwrap_or_else(|_| panic!("with_term: RefCell already borrowed (called from {caller})"));
      f(&mut term)
    })
  }

  pub fn system_msg_pending() -> bool {
    SHED.with(|shed| !shed.system_msg_queue.borrow().is_empty())
  }

  pub fn post_status_msg(msg: String) {
    SHED.with(|shed| {
      let msg = Message::new(msg);
      shed.status_msg_queue.borrow_mut().push_back(msg);
    });
  }
  pub fn pop_status_msg() -> Option<String> {
    SHED.with(|shed| {
      let mut queue = shed.status_msg_queue.borrow_mut();
      let mut hist = shed.status_msg_hist.borrow_mut();
      Self::pop_msg(&mut queue, &mut hist)
    })
  }
  pub fn post_system_msg(msg: String) {
    SHED.with(|shed| {
      let msg = Message::new(msg);
      shed.system_msg_queue.borrow_mut().push_back(msg);
    });
  }
  pub fn pop_system_msg() -> Option<String> {
    SHED.with(|shed| {
      let mut queue = shed.system_msg_queue.borrow_mut();
      let mut hist = shed.system_msg_hist.borrow_mut();
      Self::pop_msg(&mut queue, &mut hist)
    })
  }
  fn pop_msg(queue: &mut VecDeque<Message>, hist: &mut VecDeque<Message>) -> Option<String> {
    let msg = queue.pop_front()?;

    hist.push_back(msg.clone());
    if hist.len() > 1000 {
      hist.pop_front();
    }

    Some(msg.to_string())
  }

  pub fn status_msg_hist() -> Vec<Message> {
    SHED.with(|shed| {
      shed
        .status_msg_hist
        .borrow()
        .iter()
        .cloned()
        .collect::<Vec<Message>>()
    })
  }
  pub fn system_msg_hist() -> Vec<Message> {
    SHED.with(|shed| {
      shed
        .system_msg_hist
        .borrow()
        .iter()
        .cloned()
        .collect::<Vec<Message>>()
    })
  }

  pub fn get_status() -> i32 {
    SHED.with(|shed| shed.status_code.load(Ordering::Relaxed))
  }
  pub fn set_status(code: i32) {
    SHED.with(|shed| shed.status_code.store(code, Ordering::Relaxed));
  }
  pub fn set_status_from_bool(code: bool) {
    Self::set_status(if code { 0 } else { 1 })
  }
  pub fn set_pipe_status(stats: &[WtStat]) -> ShResult<()> {
    if let Some(pipe_status) = jobs::Job::pipe_status(stats) {
      let pipe_status = pipe_status
        .into_iter()
        .map(|s| s.to_string())
        .collect::<VecDeque<String>>();

      Self::vars_mut(|v| v.set_var("PIPESTATUS", VarKind::Arr(pipe_status), VarFlags::empty()))?;
    }
    Ok(())
  }

  #[cfg(test)]
  pub fn save_state() {
    SHED.with(|shed| shed.save())
  }

  #[cfg(test)]
  pub fn restore_state() {
    SHED.with(|shed| shed.restore())
  }

  #[cfg(test)]
  fn clone_db_conn(&self) -> OnceLock<Option<Arc<Connection>>> {
    let lock = OnceLock::new();
    if let Some(val) = self.db_conn.get() {
      let _ = lock.set(val.clone());
    }
    lock
  }
}

impl Default for Shed {
  fn default() -> Self {
    Self::new()
  }
}

#[cfg(test)]
impl Shed {
  pub fn save(&self) {
    let saved = Self {
      jobs: RefCell::new(self.jobs.borrow().clone()),
      var_scopes: RefCell::new(self.var_scopes.borrow().clone()),
      meta: RefCell::new(self.meta.borrow().clone()),
      logic: RefCell::new(self.logic.borrow().clone()),
      shopts: RefCell::new(self.shopts.borrow().clone()),
      db_conn: self.clone_db_conn(),
      terminal: RefCell::new(self.terminal.borrow().clone()),
      status_msg_queue: RefCell::new(self.status_msg_queue.borrow().clone()),
      status_msg_hist: RefCell::new(self.status_msg_hist.borrow().clone()),
      system_msg_queue: RefCell::new(self.system_msg_queue.borrow().clone()),
      system_msg_hist: RefCell::new(self.system_msg_hist.borrow().clone()),
      saved: RefCell::new(None),
      status_code: AtomicI32::new(self.status_code.load(Ordering::Relaxed)),
    };
    *self.saved.borrow_mut() = Some(Box::new(saved));
  }

  pub fn restore(&self) {
    if let Some(saved) = self.saved.take() {
      *self.jobs.borrow_mut() = saved.jobs.into_inner();
      *self.var_scopes.borrow_mut() = saved.var_scopes.into_inner();
      *self.meta.borrow_mut() = saved.meta.into_inner();
      *self.logic.borrow_mut() = saved.logic.into_inner();
      *self.shopts.borrow_mut() = saved.shopts.into_inner();
    }
  }
}
