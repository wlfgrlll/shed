use std::{
  cell::RefCell,
  os::fd::BorrowedFd,
  sync::{
    Arc, OnceLock,
    atomic::{AtomicBool, AtomicI32},
  },
};
use rusqlite::Connection;

use super::{
  keys,
  signal,
  parse,
  shopt,
  match_loop,
  autocmd,
  sherr,
  expand,
  procio,
  util::{ShResult,ShErr}
};

pub(super) mod scopes;
pub(super) mod logic;
pub(super) mod vars;
pub(super) mod util;
pub(super) mod meta;
pub(super) mod jobs;
pub(super) mod terminal;
pub(super) use util::{get_status,set_status};

pub(super) static INTERACTIVE: AtomicBool = AtomicBool::new(false);
pub(super) static STATUS_CODE: AtomicI32 = AtomicI32::new(0);

thread_local! {
  static SHED: Shed = Shed::new();
}

#[derive(Debug)]
pub struct Shed {
  // constructed in state/util.rs
  pub jobs:       RefCell<jobs::JobTab>,
  pub var_scopes: RefCell<scopes::ScopeStack>,
  pub meta:       RefCell<meta::MetaTab>,
  pub logic:      RefCell<logic::LogTab>,
  pub terminal:   RefCell<terminal::Terminal>,
  pub shopts:     RefCell<shopt::ShOpts>,
  pub db_conn:    OnceLock<Option<Arc<Connection>>>,

  #[cfg(test)]
  saved:          RefCell<Option<Box<Self>>>,
}

impl Shed {
  pub fn new() -> Self {
    Self {
      jobs:       RefCell::new(jobs::JobTab::new()),
      var_scopes: RefCell::new(scopes::ScopeStack::new()),
      meta:       RefCell::new(meta::MetaTab::new()),
      logic:      RefCell::new(logic::LogTab::new()),
      terminal:   RefCell::new(terminal::Terminal::new()),
      shopts:     RefCell::new(shopt::ShOpts::default()),
      db_conn:    OnceLock::new(),

      #[cfg(test)]
      saved:      RefCell::new(None),
    }
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
      saved: RefCell::new(None),
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
