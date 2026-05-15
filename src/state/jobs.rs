use std::fmt::{self, Write};

use ariadne::Fmt;
use bitflags::bitflags;
use itertools::izip;
use nix::{
  errno::Errno,
  sys::{
    signal::{Signal, kill, killpg},
    wait::{WaitPidFlag as WtFlag, WaitStatus as WtStat, waitpid},
  },
  unistd::{Pid, getpgrp, getpid, setpgid, write},
};
use scopeguard::defer;
use yansi::Color;

use super::{
  Shed,
  logic::AutoCmdKind,
  meta::CmdTimer,
  util::{get_time_fmt, with_vars},
  vars::ShellParam,
};
use crate::{
  procio::{stderr_fileno, stdout_fileno},
  sherr,
  signal::{disable_reaping, enable_reaping},
  util::ShResult,
};

pub const SIG_EXIT_OFFSET: i32 = 128;

bitflags! {
  #[derive(Debug, Copy, Clone)]
  pub struct JobCmdFlags: u8 {
    const LONG     = 0b0000_0001; // 0x01
    const PIDS     = 0b0000_0010; // 0x02
    const NEW_ONLY = 0b0000_0100; // 0x04
    const RUNNING  = 0b0000_1000; // 0x08
    const STOPPED  = 0b0001_0000; // 0x10
    const INIT     = 0b0010_0000; // 0x20
  }
}

#[derive(Debug)]
pub struct DisplayWaitStatus(pub WtStat);

impl fmt::Display for DisplayWaitStatus {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match &self.0 {
      WtStat::Exited(_, code) => match code {
        0 => write!(f, "done"),
        _ => write!(f, "failed: {}", code),
      },
      WtStat::Signaled(_, signal, _) => {
        write!(f, "signaled: {:?}", signal)
      }
      WtStat::Stopped(_, signal) => {
        write!(f, "stopped: {:?}", signal)
      }
      WtStat::PtraceEvent(_, signal, _) => {
        write!(f, "ptrace event: {:?}", signal)
      }
      WtStat::PtraceSyscall(_) => {
        write!(f, "ptrace syscall")
      }
      WtStat::Continued(_) => {
        write!(f, "continued")
      }
      WtStat::StillAlive => {
        write!(f, "running")
      }
    }
  }
}

pub fn code_from_status(stat: &WtStat) -> Option<i32> {
  match stat {
    WtStat::Exited(_, exit_code) => Some(*exit_code),
    WtStat::Stopped(_, sig) => Some(SIG_EXIT_OFFSET + *sig as i32),
    WtStat::Signaled(_, sig, _) => Some(SIG_EXIT_OFFSET + *sig as i32),
    _ => None,
  }
}

#[derive(Clone, Debug)]
pub enum JobID {
  Pgid(Pid),
  Pid(Pid),
  TableID(usize),
  Command(String),
}

#[derive(Debug, Clone)]
pub struct ChildProc {
  pgid: Pid,
  pid: Pid,
  command: Option<String>,
  stat: WtStat,
  timer: CmdTimer,
}

impl ChildProc {
  pub fn new(
    pid: Pid,
    command: Option<&str>,
    pgid: Option<Pid>,
    report_time: bool,
  ) -> ShResult<Self> {
    let command = command.map(|str| str.to_string());
    let timer = CmdTimer::new(command.clone().unwrap_or_default(), report_time)?;
    let stat = if kill(pid, None).is_ok() {
      WtStat::StillAlive
    } else {
      WtStat::Exited(pid, 0)
    };
    let mut child = Self {
      pgid: pid,
      pid,
      command,
      stat,
      timer,
    };
    if let Some(pgid) = pgid {
      child.set_pgid(pgid).ok();
    }
    Ok(child)
  }
  pub fn pid(&self) -> Pid {
    self.pid
  }
  pub fn pgid(&self) -> Pid {
    self.pgid
  }
  pub fn cmd(&self) -> Option<&str> {
    self.command.as_deref()
  }
  pub fn stat(&self) -> WtStat {
    self.stat
  }
  pub fn wait(&mut self, flags: Option<WtFlag>) -> Result<WtStat, Errno> {
    let result = waitpid(self.pid, flags);
    if let Ok(stat) = result {
      self.stat = stat
    }
    result
  }
  pub fn kill<T: Into<Option<Signal>>>(&self, sig: T) -> ShResult<()> {
    Ok(kill(self.pid, sig)?)
  }
  pub fn set_pgid(&mut self, pgid: Pid) -> ShResult<()> {
    setpgid(self.pid, pgid)?;
    self.pgid = pgid;
    Ok(())
  }
  pub fn set_stat(&mut self, stat: WtStat) {
    self.stat = stat
  }
  pub fn is_alive(&self) -> bool {
    self.stat == WtStat::StillAlive
  }
  pub fn is_stopped(&self) -> bool {
    matches!(self.stat, WtStat::Stopped(..))
  }
  pub fn exited(&self) -> bool {
    matches!(self.stat, WtStat::Exited(..))
  }
}

#[derive(Debug)]
pub struct JobBldr {
  table_id: Option<usize>,
  pgid: Option<Pid>,
  children: Vec<ChildProc>,
  send_hup: bool,
}

impl Default for JobBldr {
  fn default() -> Self {
    Self::new()
  }
}

impl JobBldr {
  pub fn new() -> Self {
    Self {
      table_id: None,
      pgid: None,
      children: vec![],
      send_hup: true,
    }
  }
  pub fn with_id(self, id: usize) -> Self {
    Self {
      table_id: Some(id),
      ..self
    }
  }
  pub fn with_pgid(self, pgid: Pid) -> Self {
    Self {
      pgid: Some(pgid),
      ..self
    }
  }
  pub fn with_children(self, children: Vec<ChildProc>) -> Self {
    Self { children, ..self }
  }
  pub fn push_child(&mut self, child: ChildProc) {
    self.children.push(child);
  }
  pub fn set_pgid(&mut self, pgid: Pid) {
    self.pgid = Some(pgid);
  }
  pub fn pgid(&self) -> Option<Pid> {
    self.pgid
  }
  pub fn no_hup(mut self) -> Self {
    self.send_hup = false;
    self
  }
  pub fn build(self) -> Job {
    Job {
      table_id: self.table_id,
      pgid: self.pgid.unwrap_or(Pid::from_raw(0)),
      children: self.children,
      notify: false,
      send_hup: self.send_hup,
    }
  }
}

/// A wrapper around Vec<JobBldr> with some job-specific methods
#[derive(Default, Debug)]
pub struct JobStack(Vec<JobBldr>);

impl JobStack {
  pub fn new() -> Self {
    Self::default()
  }
  pub fn new_job(&mut self) {
    self.0.push(JobBldr::new())
  }
  pub fn curr_job_mut(&mut self) -> Option<&mut JobBldr> {
    self.0.last_mut()
  }
  pub fn finalize_job(&mut self) -> Option<Job> {
    self.0.pop().map(|bldr| bldr.build())
  }
}

#[derive(Debug, Clone)]
pub struct Job {
  table_id: Option<usize>,
  pgid: Pid,
  children: Vec<ChildProc>,
  notify: bool,
  send_hup: bool,
}

impl Job {
  pub fn set_tabid(&mut self, id: usize) {
    self.table_id = Some(id)
  }
  pub fn no_hup(&mut self) {
    self.send_hup = false;
  }
  pub fn send_hup(&self) -> bool {
    self.send_hup
  }
  pub fn set_notify(&mut self, notify: bool) {
    self.notify = notify;
  }
  pub fn notify(&self) -> bool {
    self.notify
  }
  pub fn running(&self) -> bool {
    !self.children.iter().all(|chld| chld.exited())
  }
  pub fn tabid(&self) -> Option<usize> {
    self.table_id
  }
  pub fn pgid(&self) -> Pid {
    self.pgid
  }
  pub fn get_cmds(&self) -> Vec<&str> {
    self
      .children
      .iter()
      .map(|c| c.cmd().unwrap_or_default())
      .collect()
  }
  pub fn get_cmd_line(&self) -> String {
    self.get_cmds().join(" | ")
  }
  pub fn set_stats(&mut self, stat: WtStat) {
    for child in self.children.iter_mut() {
      child.set_stat(stat);
    }
  }
  pub fn get_stats(&self) -> Vec<WtStat> {
    self.children.iter().map(|chld| chld.stat()).collect()
  }
  pub fn pipe_status(stats: &[WtStat]) -> Option<Vec<i32>> {
    if stats.iter().any(|stat| {
      matches!(
        stat,
        WtStat::StillAlive | WtStat::Continued(_) | WtStat::PtraceSyscall(_)
      )
    }) || stats.len() <= 1
    {
      return None;
    }
    Some(
      stats
        .iter()
        .map(|stat| match stat {
          WtStat::Exited(_, code) => *code,
          WtStat::Signaled(_, signal, _) => SIG_EXIT_OFFSET + *signal as i32,
          WtStat::Stopped(_, signal) => SIG_EXIT_OFFSET + *signal as i32,
          WtStat::PtraceEvent(_, signal, _) => SIG_EXIT_OFFSET + *signal as i32,
          WtStat::PtraceSyscall(_) | WtStat::Continued(_) | WtStat::StillAlive => unreachable!(),
        })
        .collect(),
    )
  }
  pub fn get_pids(&self) -> Vec<Pid> {
    self
      .children
      .iter()
      .map(|chld| chld.pid())
      .collect::<Vec<Pid>>()
  }
  pub fn children(&self) -> &[ChildProc] {
    &self.children
  }
  pub fn children_mut(&mut self) -> &mut Vec<ChildProc> {
    &mut self.children
  }
  pub fn is_done(&self) -> bool {
    self.children.iter().all(|chld| {
      chld.exited() || chld.stat() == WtStat::Signaled(chld.pid(), Signal::SIGHUP, true)
    })
  }
  pub fn killpg(&mut self, sig: Signal) -> ShResult<()> {
    let stat = match sig {
      Signal::SIGTSTP => WtStat::Stopped(self.pgid, Signal::SIGTSTP),
      Signal::SIGCONT => WtStat::Continued(self.pgid),
      sig => WtStat::Signaled(self.pgid, sig, false),
    };
    self.set_stats(stat);
    Ok(killpg(self.pgid, sig)?)
  }
  pub fn wait_pgrp(&mut self) -> ShResult<(Vec<WtStat>, Vec<CmdTimer>)> {
    let mut stats = vec![];
    let mut timers = vec![];
    for child in self.children.iter_mut() {
      if child.pid == Pid::this() {
        let code = Shed::get_status();
        stats.push(WtStat::Exited(child.pid, code));
        child.timer.stop()?;
        timers.push(child.timer.clone());
        continue;
      }
      loop {
        let result = child.wait(Some(WtFlag::WSTOPPED));
        child.timer.stop()?;
        match result {
          Ok(stat) => {
            stats.push(stat);
            timers.push(child.timer.clone());
            break;
          }
          Err(Errno::ECHILD) => break,
          Err(Errno::EINTR) => continue,
          Err(e) => return Err(e.into()),
        }
      }
    }
    Ok((stats, timers))
  }
  pub fn update_by_id(&mut self, id: JobID, stat: WtStat) -> ShResult<()> {
    match id {
      JobID::Pid(pid) => {
        let query_result = self.children.iter_mut().find(|chld| chld.pid == pid);
        if let Some(child) = query_result {
          child.set_stat(stat);
        }
      }
      JobID::Command(cmd) => {
        let query_result = self
          .children
          .iter_mut()
          .find(|chld| chld.cmd().is_some_and(|chld_cmd| chld_cmd.contains(&cmd)));
        if let Some(child) = query_result {
          child.set_stat(stat);
        }
      }
      JobID::TableID(tid) => {
        if self.table_id.is_some_and(|tblid| tblid == tid) {
          for child in self.children.iter_mut() {
            child.set_stat(stat);
          }
        }
      }
      JobID::Pgid(pgid) => {
        if pgid == self.pgid {
          for child in self.children.iter_mut() {
            child.set_stat(stat);
          }
        }
      }
    }
    Ok(())
  }
  pub fn name(&self) -> Option<&str> {
    self.children().first().and_then(|child| child.cmd())
  }
  pub fn display(&self, job_order: &[usize], flags: JobCmdFlags) -> String {
    let long = flags.contains(JobCmdFlags::LONG);
    let init = flags.contains(JobCmdFlags::INIT);
    let pids = flags.contains(JobCmdFlags::PIDS);

    let current = job_order.last();
    let prev = (job_order.len() > 2)
      .then(|| job_order.get(job_order.len() - 2))
      .flatten();

    let id = self.table_id.unwrap();
    let symbol = if current == self.table_id.as_ref() {
      "+"
    } else if prev == self.table_id.as_ref() {
      "-"
    } else {
      " "
    };

    let job_pids = self.get_pids();
    let job_stats = self.get_stats();
    let job_cmds = self.get_cmds();
    let zipped = izip!(0.., job_pids.iter(), job_stats.iter(), job_cmds.iter(),);

    let id_box = format!("[{}]{}", id + 1, symbol);
    let id_width = id_box.len();
    let last_cmd = self.get_cmds().len().saturating_sub(1);

    let mut output = format!("{id_box}\t");

    for (i, pid, job_stat, cmd) in zipped {
      let fmt_stat = DisplayWaitStatus(*job_stat).to_string();
      let pipe = if i != last_cmd { " |" } else { "" };

      let stat_line = if pids || init {
        format!("{pid} {fmt_stat}  {cmd}{pipe}")
      } else {
        format!("{fmt_stat}  {cmd}{pipe}")
      };

      let stat_line = match job_stat {
        WtStat::Stopped(..) | WtStat::Signaled(..) => stat_line.fg(Color::Magenta),
        WtStat::Exited(_, 0) => stat_line.fg(Color::Green),
        WtStat::Exited(..) => stat_line.fg(Color::Red),
        _ => stat_line.fg(Color::Cyan),
      }
      .to_string();

      if i == 0 {
        if long {
          writeln!(output, "{pid} {stat_line}").ok();
        } else {
          writeln!(output, "{stat_line}").ok();
        }
      } else if long {
        writeln!(output, "{:>id_width$}\t{pid} {stat_line}", "").ok();
      } else {
        writeln!(output, "{:>id_width$}\t{stat_line}", "").ok();
      }
    }
    output
  }
}

/// Calls attach_tty() on the shell's process group to retake control of the
/// terminal
pub fn take_term() -> ShResult<()> {
  Shed::term_mut(|t| t.attach(getpgrp()))?;
  killpg(getpgrp(), Signal::SIGWINCH)?;
  Ok(())
}

pub fn wait_bg(id: JobID) -> ShResult<()> {
  disable_reaping();
  defer! {
    enable_reaping();
  };
  match id {
    JobID::Pid(pid) => {
      let stat = loop {
        match waitpid(pid, None) {
          Ok(stat) => break stat,
          Err(Errno::EINTR) => continue,
          Err(Errno::ECHILD) => return Ok(()),
          Err(e) => return Err(e.into()),
        }
      };
      Shed::jobs_mut(|j| j.update_by_id(id, stat))?;
      Shed::set_status(code_from_status(&stat).unwrap_or(0));
    }
    _ => {
      let Some(mut job) = Shed::jobs_mut(|j| j.remove_job(id.clone())) else {
        return Err(sherr!(ExecFail, "wait: No such job with id {:?}", id,));
      };
      let (statuses, timers) = job.wait_pgrp()?;

      for timer in &timers {
        if timer.should_report() {
          report_timer(timer)?;
        }
      }

      let mut was_stopped = false;
      let mut code = 0;
      for status in &statuses {
        code = code_from_status(status).unwrap_or(0);
        match status {
          WtStat::Stopped(_, _) => {
            was_stopped = true;
          }
          WtStat::Signaled(_, sig, _) => {
            if *sig == Signal::SIGTSTP {
              was_stopped = true;
            }
          }
          _ => {}
        }
      }

      Shed::set_pipe_status(&statuses)?;

      if was_stopped {
        Shed::jobs_mut(|j| j.insert_job(job, false))?;
      }
      Shed::set_status(code);
    }
  }
  Ok(())
}

fn report_timer(timer: &CmdTimer) -> ShResult<()> {
  let has_autocmds = Shed::logic(|l| !l.get_autocmds(AutoCmdKind::OnTimeReport).is_empty());

  if !has_autocmds {
    let fmt_str = get_time_fmt();
    let report = timer.format_report(&fmt_str)?;
    write(stderr_fileno(), format!("{report}\n").as_bytes()).ok();
  } else {
    let vars = [
      ("TIME_REAL_MS".into(), timer.total_wall_ms()?.to_string()),
      ("TIME_USER_MS".into(), timer.total_user_ms()?.to_string()),
      ("TIME_SYS_MS".into(), timer.total_sys_ms()?.to_string()),
      (
        "TIME_REAL_FMT".into(),
        timer.total_wall_formatted()?.to_string(),
      ),
      (
        "TIME_USER_FMT".into(),
        timer.total_user_formatted()?.to_string(),
      ),
      (
        "TIME_SYS_FMT".into(),
        timer.total_sys_formatted()?.to_string(),
      ),
      ("TIME_CPU_PCT".into(), timer.cpu_pct()?.to_string()),
      ("TIME_RSS".into(), timer.max_rss()?.to_string()),
      ("TIME_CMD".into(), timer.command().to_string()),
    ];
    with_vars(vars, || crate::autocmd!(OnTimeReport));
  }
  Ok(())
}

pub fn wait_fg(job: Job, interactive: bool) -> ShResult<()> {
  if job.children().is_empty() {
    return Ok(());
  }
  let mut code = 0;
  let mut was_stopped = false;
  if interactive {
    Shed::term_mut(|t| t.attach(job.pgid()))?;
  }
  disable_reaping();
  defer! {
    enable_reaping();
  }
  let (statuses, timers) = Shed::jobs_mut(|j| j.new_fg(job))?;

  for timer in &timers {
    if timer.should_report() {
      report_timer(timer)?;
    }
  }

  for status in &statuses {
    code = code_from_status(status).unwrap_or(0);
    match status {
      WtStat::Stopped(_, _) => {
        was_stopped = true;
        Shed::jobs_mut(|j| j.fg_to_bg(*status))?;
      }
      WtStat::Signaled(_, sig, _) => {
        if *sig == Signal::SIGINT {
          kill(getpid(), Signal::SIGINT)?;
        } else if *sig == Signal::SIGTSTP {
          was_stopped = true;
          Shed::jobs_mut(|j| j.fg_to_bg(*status))?;
        }
      }
      _ => {}
    }
  }
  Shed::set_pipe_status(&statuses)?;

  if !was_stopped {
    let job = Shed::jobs_mut(|j| j.take_fg());
    if interactive {
      Shed::meta_mut(|m| m.set_last_job(job));
    }
  }
  if interactive {
    take_term()?;
  }
  Shed::set_status(code);
  Ok(())
}

pub fn dispatch_job(mut job: Job, is_bg: bool, interactive: bool) -> ShResult<()> {
  if interactive {
    job.set_notify(true);
  }
  if is_bg {
    Shed::jobs_mut(|j| j.insert_job(job, !interactive))?;
  } else {
    wait_fg(job, interactive)?;
  }
  Ok(())
}

#[derive(Clone, Default, Debug)]
pub struct JobTab {
  fg: Option<Job>,
  order: Vec<usize>,
  new_updates: Vec<usize>,
  jobs: Vec<Option<Job>>,
}

impl JobTab {
  pub fn new() -> Self {
    Self::default()
  }
  pub fn take_fg(&mut self) -> Option<Job> {
    self.fg.take()
  }
  fn next_open_pos(&self) -> usize {
    if let Some(position) = self.jobs.iter().position(|slot| slot.is_none()) {
      position
    } else {
      self.jobs.len()
    }
  }
  pub fn jobs(&self) -> &Vec<Option<Job>> {
    &self.jobs
  }
  pub fn jobs_mut(&mut self) -> &mut Vec<Option<Job>> {
    &mut self.jobs
  }
  pub fn curr_job(&self) -> Option<usize> {
    self
      .order
      .iter()
      .rev()
      .find(|&&id| self.jobs.get(id).is_some_and(|slot| slot.is_some()))
      .copied()
  }
  fn prune_jobs(&mut self) {
    while let Some(job) = self.jobs.last() {
      if job.is_none() || job.as_ref().unwrap().is_done() {
        self.jobs.pop();
      } else {
        break;
      }
    }
  }
  pub fn insert_job(&mut self, mut job: Job, silent: bool) -> ShResult<usize> {
    self.prune_jobs();
    let tab_pos = if let Some(id) = job.tabid() {
      id
    } else {
      self.next_open_pos()
    };
    job.set_tabid(tab_pos);
    let last_pid = job.children().last().map(|c| c.pid());
    self.order.push(tab_pos);
    if !silent {
      let msg = job.display(&self.order, JobCmdFlags::INIT);
      Shed::meta_mut(|m| m.post_system_message(msg));
    }
    if tab_pos == self.jobs.len() {
      self.jobs.push(Some(job))
    } else {
      self.jobs[tab_pos] = Some(job);
    }

    if let Some(pid) = last_pid {
      Shed::vars_mut(|v| v.set_param(ShellParam::LastJob, &pid.to_string()))
    }

    Ok(tab_pos)
  }
  pub fn order(&self) -> &[usize] {
    &self.order
  }
  pub fn query(&self, identifier: JobID) -> Option<&Job> {
    match identifier {
      JobID::Pgid(pgid) => self
        .jobs
        .iter()
        .find_map(|job| job.as_ref().filter(|j| j.pgid() == pgid)),
      JobID::Pid(pid) => self.jobs.iter().find_map(|job| {
        job
          .as_ref()
          .filter(|j| j.children().iter().any(|child| child.pid() == pid))
      }),
      JobID::TableID(id) => self.jobs.get(id).and_then(|job| job.as_ref()),
      JobID::Command(cmd) => self.jobs.iter().find_map(|job| {
        job.as_ref().filter(|j| {
          j.children()
            .iter()
            .any(|child| child.cmd().as_ref().is_some_and(|c| c.contains(&cmd)))
        })
      }),
    }
  }
  pub fn update_by_id(&mut self, id: JobID, stat: WtStat) -> ShResult<()> {
    let Some(job) = self.query_mut(id.clone()) else {
      return Ok(());
    };
    match id {
      JobID::Pid(pid) => {
        let Some(child) = job.children_mut().iter_mut().find(|c| c.pid() == pid) else {
          return Ok(());
        };
        child.set_stat(stat);
      }
      JobID::Pgid(_) | JobID::TableID(_) | JobID::Command(_) => {
        job.set_stats(stat);
      }
    }
    Ok(())
  }
  pub fn query_mut(&mut self, identifier: JobID) -> Option<&mut Job> {
    match identifier {
      JobID::Pgid(pgid) => self
        .jobs
        .iter_mut()
        .find_map(|job| job.as_mut().filter(|j| j.pgid() == pgid)),
      JobID::Pid(pid) => self.jobs.iter_mut().find_map(|job| {
        job
          .as_mut()
          .filter(|j| j.children().iter().any(|child| child.pid() == pid))
      }),
      JobID::TableID(id) => self.jobs.get_mut(id).and_then(|job| job.as_mut()),
      JobID::Command(cmd) => self.jobs.iter_mut().find_map(|job| {
        job.as_mut().filter(|j| {
          j.children()
            .iter()
            .any(|child| child.cmd().as_ref().is_some_and(|c| c.contains(&cmd)))
        })
      }),
    }
  }
  pub fn get_fg(&self) -> Option<&Job> {
    self.fg.as_ref()
  }
  pub fn get_fg_mut(&mut self) -> Option<&mut Job> {
    self.fg.as_mut()
  }
  pub fn new_fg(&mut self, job: Job) -> ShResult<(Vec<WtStat>, Vec<CmdTimer>)> {
    self.fg = Some(job);
    let (statuses, timers) = self.fg.as_mut().unwrap().wait_pgrp()?;
    Ok((statuses, timers))
  }
  pub fn fg_to_bg(&mut self, stat: WtStat) -> ShResult<()> {
    if self.fg.is_none() {
      return Ok(());
    }
    take_term()?;
    let fg = std::mem::take(&mut self.fg);
    if let Some(mut job) = fg {
      job.set_stats(stat);
      self.insert_job(job, false)?;
    }
    Ok(())
  }
  pub fn wait_all_bg(&mut self) -> ShResult<()> {
    disable_reaping();
    defer! {
      enable_reaping();
    }
    let mut code = 0;
    for job in self.jobs.iter_mut() {
      let Some(job) = job else { continue };
      let (statuses, _) = job.wait_pgrp()?;
      code = statuses.last().and_then(code_from_status).unwrap_or(0);
    }
    Shed::set_status(code);
    Ok(())
  }
  pub fn remove_job(&mut self, id: JobID) -> Option<Job> {
    let tabid = self.query(id).map(|job| job.tabid().unwrap());
    if let Some(tabid) = tabid {
      self.jobs.get_mut(tabid).and_then(Option::take)
    } else {
      None
    }
  }
  pub fn print_jobs(&mut self, flags: JobCmdFlags) -> ShResult<()> {
    let jobs = if flags.contains(JobCmdFlags::NEW_ONLY) {
      &self
        .jobs
        .iter()
        .filter(|job| {
          job
            .as_ref()
            .is_some_and(|job| self.new_updates.contains(&job.tabid().unwrap()))
        })
        .map(|job| job.as_ref())
        .collect::<Vec<Option<&Job>>>()
    } else {
      &self
        .jobs
        .iter()
        .map(|job| job.as_ref())
        .collect::<Vec<Option<&Job>>>()
    };
    let mut jobs_to_remove = vec![];
    for job in jobs.iter().flatten() {
      let id = job.tabid().unwrap();
      if flags.contains(JobCmdFlags::RUNNING)
        && !matches!(
          job.get_stats().get(id).unwrap(),
          WtStat::StillAlive | WtStat::Continued(_)
        )
      {
        continue;
      }
      if flags.contains(JobCmdFlags::STOPPED)
        && !matches!(job.get_stats().get(id).unwrap(), WtStat::Stopped(_, _))
      {
        continue;
      }
      write(
        stdout_fileno(),
        format!("{}\n", job.display(&self.order, flags)).as_bytes(),
      )?;
      if job
        .get_stats()
        .iter()
        .all(|stat| matches!(stat, WtStat::Exited(_, _) | WtStat::Signaled(_, _, _)))
      {
        jobs_to_remove.push(JobID::TableID(id));
      }
    }
    for id in jobs_to_remove {
      self.remove_job(id);
    }
    Ok(())
  }

  pub fn hang_up(&mut self) {
    for job in self.jobs_mut().iter_mut().flatten() {
      if job.send_hup() {
        job.killpg(Signal::SIGHUP).ok();
      }
    }
  }

  pub fn disown(&mut self, id: JobID, nohup: bool) -> ShResult<()> {
    if let Some(job) = self.query_mut(id.clone()) {
      if nohup {
        job.no_hup();
      } else {
        self.remove_job(id);
      }
    }
    Ok(())
  }

  pub fn disown_all(&mut self, nohup: bool) -> ShResult<()> {
    let mut ids_to_remove = vec![];
    for job in self.jobs_mut().iter_mut().flatten() {
      if nohup {
        job.no_hup();
      } else {
        ids_to_remove.push(JobID::TableID(job.tabid().unwrap()));
      }
    }
    for id in ids_to_remove {
      self.remove_job(id);
    }
    Ok(())
  }
}
