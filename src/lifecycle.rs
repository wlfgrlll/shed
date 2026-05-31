use std::{io::Write, path::PathBuf, process::ExitCode, sync::atomic::Ordering};

use clap::Parser;

use super::{
  ShResult, Shed, autocmd,
  eval::execute::{Dispatcher, exec_nonint},
  outln,
  procio::{
    self, MIN_INTERNAL_FD, RedirType, do_something_that_opens_fds_that_we_cant_access_hack,
  },
  signal::QUIT_CODE,
  state::{
    self,
    jobs::JobTab,
    logic::TrapTarget,
    meta::MetaTab,
    terminal::Terminal,
    util::{self, generate_default_rc, source_env},
    vars::{VarFlags, VarKind},
  },
  status_msg, try_var,
  util::flog,
};

#[expect(clippy::struct_excessive_bools)]
#[derive(Parser, Debug)]
#[command(
  author = "Kyler Clay",
  about = "An experimental POSIX shell",
  long_about = "shed is an experimental POSIX shell focused on interative user experience, extensibility, and powerful line editing."
)]
pub(super) struct ShedArgs {
  /// Evaluate the given string as a command and exit
  #[arg(short, long, conflicts_with_all = ["interactive", "stdin"])]
  pub(super) command: Option<String>,

  /// Script path and arguments
  #[arg(trailing_var_arg = true)]
  pub(super) script_args: Vec<String>,

  /// Print version info
  #[arg(long)]
  pub(super) version: bool,

  /// Start the shell in interactive mode
  #[arg(short, long)]
  pub(super) interactive: bool,

  /// Read input from stdin
  #[arg(short)]
  pub(super) stdin: bool,

  /// Start the shell as a login shell (sources .`shed_profile`)
  #[arg(long, short)]
  pub(super) login_shell: bool,

  /// Print the welcome message after arriving at the prompt
  #[arg(long, short)]
  pub(super) welcome: bool,

  /// Skip sourcing runtime command files
  #[arg(long)]
  pub(super) no_rc: bool,

  /// Provide the path to the runtime commands file
  #[arg(long)]
  pub(super) rc_path: Option<String>,

  /// List of POSIX 'set' options to enable
  #[arg(short = 'o', value_name = "OPTION", value_parser = Self::SET_OPTS)]
  pub(super) set: Vec<String>,

  /// Input is read as a keymap for the line editor to execute
  /// instead of raw shell commands. Used to script the line editor
  #[arg(long)]
  pub(super) edit_script: bool,
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

pub(super) fn setup() -> Option<ShedArgs> {
  yansi::enable();
  setup_panic_handler();
  flog::init().ok();
  util::set_ver_info().ok();
  util::set_sh_lvl().ok();

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

  if !args.no_rc {
    if let Some(ref path) = args.rc_path {
      Shed::vars_mut(|v| v.set_var("SHED_RC", VarKind::Str(path.clone()), VarFlags::EXPORT)).ok();
    }
    if let Err(e) = source_env() {
      e.print_error();
    }
  }

  for set_opt in &args.set {
    if set_opt == "emacs" {
      Shed::shopts_mut(|o| o.query("set.vi=false")).ok();
      continue;
    }
    Shed::shopts_mut(|o| o.query(&format!("set.{set_opt}=true"))).ok();
  }

  do_something_that_opens_fds_that_we_cant_access_hack(MIN_INTERNAL_FD, state::util::init_db_conn);

  Some(args)
}

pub(super) fn first_run_setup() -> ShResult<()> {
  let rc_path = generate_default_rc()?;

  if let Some(rc_path) = rc_path {
    status_msg!("Generated default rc file at '{}'", rc_path.display());
  }

  Ok(())
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
    Shed::jobs_mut(JobTab::hang_up);

    // log panic
    let data_dir = dirs::data_dir().unwrap_or_else(|| {
      let home = try_var!("HOME").unwrap();
      PathBuf::from(format!("{home}/.local/share"))
    });
    let log_dir = data_dir.join("shed").join("log");
    std::fs::create_dir_all(&log_dir).unwrap();
    let log_file_path = log_dir.join("panic.log");
    let mut log_file = procio::get_redir_file(RedirType::Output, log_file_path).unwrap();

    let panic_info_raw = info.to_string();
    log_file.write_all(panic_info_raw.as_bytes()).unwrap();
    log_file.write_all(b"\n\n").unwrap();

    let backtrace = std::backtrace::Backtrace::force_capture();
    log_file
      .write_all(format!("\nBacktrace:\n{backtrace:#?}").as_bytes())
      .unwrap();

    // call the default panic hook
    default_panic_hook(info);
  }));
}

#[expect(clippy::cast_sign_loss)]
pub(super) fn tear_down() -> ExitCode {
  if let Some(trap) = Shed::logic(|l| l.get_trap(TrapTarget::Exit))
    && let Err(e) = exec_nonint(trap, Some("trap".into()))
  {
    e.print_error();
  }

  let mut deferred = Shed::vars_mut(|v| v.cur_scope_mut().take_deferred_cmds());

  while let Some(cmd) = deferred.pop() {
    let mut dispatcher = Dispatcher::new(vec![cmd], "defer".into());
    if let Err(e) = dispatcher.begin_dispatch() {
      e.print_error();
    }
  }

  autocmd!(OnExit);

  if Shed::meta(MetaTab::interactive_shell) {
    crate::write_term!("\n").ok();
  }
  Shed::jobs_mut(JobTab::hang_up);
  Shed::term_mut(Terminal::reset_for_exit);

  ExitCode::from(QUIT_CODE.load(Ordering::SeqCst) as u8)
}
