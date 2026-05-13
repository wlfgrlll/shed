use std::collections::VecDeque;

use super::{Lines, Pos};

pub const MAX_KILL_RING: usize = 60;

#[derive(Default, Debug, Clone)]
pub struct KillRing {
  pub kills: VecDeque<Lines>,
  pub merging: bool,
  pub selected: Option<usize>,
  pub kill_cycle_span: Option<(Pos, Pos)>,
}

impl KillRing {
  pub fn new() -> Self {
    Self {
      kills: VecDeque::new(),
      merging: false,
      selected: None,
      kill_cycle_span: None,
    }
  }
  pub fn push_back(&mut self, kill: Lines) {
    if kill.is_empty() || (kill.len() == 1 && kill[0].is_empty()) {
      return;
    }
    self.kills.push_back(kill);
    if self.kills.len() > MAX_KILL_RING {
      self.kills.pop_front();
    }
  }
  pub fn push_front(&mut self, kill: Lines) {
    if kill.is_empty() || (kill.len() == 1 && kill[0].is_empty()) {
      return;
    }
    self.kills.push_front(kill);
    if self.kills.len() > MAX_KILL_RING {
      self.kills.pop_back();
    }
  }
  pub fn pop_back(&mut self) -> Option<Lines> {
    self.kills.pop_back()
  }
  pub fn pop_front(&mut self) -> Option<Lines> {
    self.kills.pop_front()
  }
  pub fn len(&self) -> usize {
    self.kills.len()
  }
  pub fn is_empty(&self) -> bool {
    self.kills.is_empty()
  }
  pub fn next_idx(&mut self) -> usize {
    let idx = match self.selected {
      Some(0) | None => self.kills.len(),
      Some(i) => i,
    }
    .saturating_sub(1);
    self.selected = Some(idx);
    idx
  }
  pub fn reset(&mut self) {
    self.selected = None;
    self.kill_cycle_span = None;
  }
}

impl Iterator for KillRing {
  type Item = Lines;
  fn next(&mut self) -> Option<Self::Item> {
    let next_idx = self.next_idx();
    self.kills.get(next_idx).cloned()
  }
}
