use ariadne::Span as ASpan;
use nix::unistd::Pid;
use scopeguard::defer;

use crate::state::meta::UtilKind;

use super::{
  errln,
  eval::{
    self, NdFlags, NdRule, Node,
    execute::{AssignBehavior, Dispatcher, exec_nonint, prepare_argv},
    lex::{KEYWORDS, Span, Tk},
  },
  expand::{self, as_var_val_display},
  key, keys, match_loop, out, outln, procio,
  procio::RedirSet,
  readline, sherr, shopt, signal, state,
  state::{Shed, jobs::ChildProc, shopt as shopt_internal},
  status_msg, system_msg, try_var,
  util::{self, ShErrKind, ShResult, var_ctx_guard, with_status},
  var, write_term,
};

mod alias;
mod arrops;
mod autocmd;
mod cd;
mod complete;
mod defer;
mod dirstack;
mod echo;
mod evaluate;
mod exec;
mod fixcmd;
mod flog;
mod flowctl;
mod getopt;
mod getopts;
mod hash;
mod help;
mod hist;
mod intro;
mod jobctl;
mod keymap;
mod msg;
mod pwd;
mod read;
mod resource;
mod seek;
mod set;
mod shift;
mod shopt;
mod source;
mod stash;
mod test; // [[ ]] thing
mod times;
mod trap;
mod varcmds;

use getopt::{Opt, OptSpec, get_opts_from_tokens, get_opts_from_tokens_strict};
pub(super) use test::double_bracket_test;

/// Embed a completion script directly in the binary.
///
/// The script has to define a completion function *and* call complete -F {func} {name}
macro_rules! embed {
  ($path:literal) => {
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/include/", $path))
  };
}

/// Register a script to embed in the binary and source on shell startup
///
/// These are mainly used for builtin functions, aliases and utility variables
macro_rules! register_scripts {
  ($($path:literal),* $(,)?) => {
    static SCRIPTS: &[(&str,&str)] = &[
      $(($path, embed!($path))),*
    ];

    pub fn source_builtin_scripts() {
      let mut code = 0;
      for (path, src) in SCRIPTS {
        if let Err(e) = $crate::eval::execute::exec_nonint(src.to_string(), Some(format!("{path}").into())) {
          code = 2;
          e.print_error();
        }
      }
      $crate::state::Shed::set_status(code);
    }
  };
}

macro_rules! register_completions {
  ($($name:literal => $script:expr),* $(,)?) => {
    static COMPLETIONS: &[(&str,&str)] = &[
      $(($name, $script)),*
    ];

    pub fn source_builtin_completions() {
      let mut code = 0;
      for (name, src) in COMPLETIONS {
        if let Err(e) = $crate::eval::execute::exec_nonint(src.to_string(), Some(format!("{name} comp").into())) {
          code = 2;
          e.print_error();
        }
      }
      $crate::state::Shed::set_status(code);
    }
  };
}

macro_rules! register_builtins {
  ($($name:literal => $ty:expr),* $(,)?) => {
    static BUILTIN_TABLE: &[(&str, &dyn Builtin)] = &[
      $(($name, &$ty)),*
    ];

    pub const BUILTIN_NAMES: &[&str] = &[
      $($name),*
    ];

    // credit goes to fish shell for this idea. very nice pattern
    // at compile time, checks to see if the name list is sorted alphabetically
    // if not, compiler error
    const _: () = {
      let mut i = 1;
      while i < BUILTIN_NAMES.len() {
        let prev = BUILTIN_NAMES[i - 1].as_bytes();
        let curr = BUILTIN_NAMES[i].as_bytes();
        let len = if prev.len() < curr.len() {
          prev.len()
        } else {
          curr.len()
        };
        let mut j = 0;
        while j < len {
          if prev[j] > curr[j] {
            panic!("Builtin names must be in alphabetical order");
          }
          if prev[j] < curr[j] {
            break;
          }
          j += 1;
        }

        if j == len && prev.len() >= curr.len() {
          panic!("Builtin names must be in alphabetical order");
        }

        i += 1;
      }
    };
  };
}

// these have to be in alphabetical order, because of the way lookup_builtin() works
// if the list is unsorted, that is a compile error thanks to the const evaluation above
// if you're using vim, you can visual select the block and filter it through ''<,'>:!LC_ALL=C sort'
// you can also yank this macro and execute it with @" -> /^register_builtins!$viB:!LC_ALL=C sort:wviBga=:w
// if you're not using vim, idk. you know the alphabet right?
register_builtins! {
  "."        => source::Source,
  ":"        => Colon,
  "alias"    => alias::Alias,
  "autocmd"  => autocmd::AutoCmdBuiltin,
  "bg"       => jobctl::Bg,
  "break"    => flowctl::Break,
  "builtin"  => BuiltinBuiltin,
  "cd"       => cd::Cd,
  "command"  => CommandBuiltin,
  "compadd"  => complete::Compadd,
  "compgen"  => complete::CompGen,
  "complete" => complete::Complete,
  "continue" => flowctl::Continue,
  "declare"  => varcmds::Declare,
  "defer"    => defer::Defer,
  "dirs"     => dirstack::Dirs,
  "disown"   => jobctl::Disown,
  "echo"     => echo::Echo,
  "eval"     => evaluate::Eval,
  "exec"     => exec::Exec,
  "exit"     => flowctl::Exit,
  "export"   => varcmds::Export,
  "false"    => False,
  "fc"       => fixcmd::FixCmd,
  "fg"       => jobctl::Fg,
  "flog"     => flog::Flog,
  "fpop"     => arrops::FrontPop,
  "fpush"    => arrops::FrontPush,
  "getopts"  => getopts::GetOpts,
  "hash"     => hash::Hash,
  "help"     => help::Help,
  "hist"     => hist::Hist,
  "jobs"     => jobctl::Jobs,
  "keymap"   => keymap::KeyMapBuiltin,
  "kill"     => jobctl::Kill,
  "local"    => varcmds::Local,
  "msg"      => msg::Msg,
  "pop"      => arrops::Pop,
  "popd"     => dirstack::PopDir,
  "push"     => arrops::Push,
  "pushd"    => dirstack::PushDir,
  "pwd"      => pwd::Pwd,
  "read"     => read::Read,
  "readkey"  => read::ReadKey,
  "readonly" => varcmds::Readonly,
  "return"   => flowctl::Return,
  "rotate"   => arrops::Rotate,
  "seek"     => seek::Seek,
  "set"      => set::Set,
  "shift"    => shift::Shift,
  "shopt"    => shopt::Shopt,
  "source"   => source::Source,
  "stash"    => stash::StashBuiltin,
  "times"    => times::Times,
  "trap"     => trap::Trap,
  "true"     => True,
  "type"     => intro::Type,
  "ulimit"   => resource::ULimit,
  "umask"    => resource::UMask,
  "unalias"  => alias::Unalias,
  "unset"    => varcmds::Unset,
  "wait"     => jobctl::Wait,
}

/// Autogenerate a completion spec for the given command using a compgen flag
///
/// Useful for simple builtins
macro_rules! compgen {
  ($name:literal, $flag:expr) => {
    concat!(
      "_",
      $name,
      "_comp() { compadd $(compgen ",
      $flag,
      r#" -- "$2"); }; complete -F _"#,
      $name,
      "_comp ",
      $name
    )
  };
}

register_completions! {
  "unalias"  => compgen!("unalias",  "-a"),
  "alias"    => compgen!("alias",    "-a"),
  "pushd"    => compgen!("pushd",    "-d"),
  "unset"    => compgen!("unset",    "-v"),
  "type"     => compgen!("type",     "-c"),
  "hash"     => compgen!("hash",     "-c"),
  "command"  => compgen!("command",  "-c"),
  "exec"     => compgen!("exec",     "-c"),
  "bg"       => compgen!("bg",       "-j"),
  "fg"       => compgen!("fg",       "-j"),
  "readonly" => compgen!("readonly", "-v"),
  "export"   => compgen!("export",   "-v"),
  "local"    => compgen!("local",    "-v"),
  "disown"   => compgen!("disown",   "-j"),
  "wait"     => compgen!("wait",     "-j"),
  "source"   => compgen!("source",   "-f"),
  "."        => compgen!(".",        "-f"),
  "read"     => compgen!("read",     "-v"),
  "readkey"  => compgen!("readkey",  "-v"),
  "pop"      => compgen!("pop",      "-v"),
  "fpop"     => compgen!("fpop",     "-v"),
  "push"     => compgen!("push",     "-v"),
  "fpush"    => compgen!("fpush",    "-v"),
  "rotate"   => compgen!("rotate",   "-v"),
  "builtin"  => compgen!("builtin",  "-b"),
  "kill"     => embed!("completions/kill_comp.sh"),
  "keymap"   => embed!("completions/keymap_comp.sh"),
  "declare"  => embed!("completions/declare_comp.sh"),
  "trap"     => embed!("completions/trap_comp.sh"),
  "ulimit"   => embed!("completions/ulimit_comp.sh"),
  "set"      => embed!("completions/set_comp.sh"),
  "autocmd"  => embed!("completions/autocmd_comp.sh"),
  "cd"       => embed!("completions/cd_comp.sh"),
  "shopt"    => embed!("completions/shopt_comp.sh"),
  "compadd"  => embed!("completions/compadd_comp.sh"),
  "help"     => embed!("completions/help_comp.sh"),
  "hist"     => embed!("completions/hist_comp.sh"),
}

register_scripts! {
  "version.sh",
}

/// Lookup a name in the builtin table via binary search
pub(super) fn lookup_builtin(name: &str) -> Option<&'static dyn Builtin> {
  BUILTIN_TABLE
    .binary_search_by_key(&name, |(n, _)| n)
    .ok()
    .map(|idx| BUILTIN_TABLE[idx].1 as &dyn Builtin)
}

type ArgVector = Vec<(String, Span)>;
pub(super) trait Builtin: Sync {
  /// The actual logic of the builtin. The only required member of Builtin.
  fn execute(&self, args: BuiltinArgs) -> ShResult<()>;

  /// The option specification for the builtin.
  fn opts(&self) -> Vec<OptSpec> {
    vec![]
  }
  /// Whether unrecognized flags should be treated as errors.
  fn strict_opts(&self) -> bool {
    false
  }
  /// The way that the builtin parses its options. Some of them are weird, like `set`
  fn get_argv_and_opts(&self, argv: Vec<Tk>) -> ShResult<(ArgVector, Vec<Opt>)> {
    let opts = self.opts();
    let (mut argv, opts) = if opts.is_empty() {
      (prepare_argv(argv)?, vec![])
    } else if self.strict_opts() {
      get_opts_from_tokens_strict(argv, &opts)?
    } else {
      get_opts_from_tokens(argv, &opts)?
    };
    if !argv.is_empty() {
      argv.remove(0);
    };
    Ok((argv, opts))
  }
  /// The main entry point for running a builtin. This is responsible for setting up the environment, handling redirections, and catching control flow errors.
  fn setup_builtin(&self, mut node: Node, dispatcher: &mut Dispatcher) -> ShResult<()> {
    let cmd_raw = node.get_command().unwrap().to_string();
    let report_time = node.flags.contains(NdFlags::REPORT_TIME);
    let context = node.context.clone();
    let NdRule::Command { assignments, argv } = &mut node.class else {
      unreachable!()
    };
    let env_vars =
      dispatcher.set_assignments(std::mem::take(assignments), AssignBehavior::Export)?;
    let _var_guard = var_ctx_guard(env_vars.into_iter().collect());
    let fork_builtins = node.flags.contains(NdFlags::FORK_BUILTINS);

    if argv.len() == 2 && argv[1].as_str() == "--help" {
      // we have been asked for help
      // is this a hack? only the nose knows.
      return exec_nonint(
        format!("help builtin-{cmd_raw}"),
        Some("<builtin-help>".into()),
      );
    }

    // Set up redirections here so we can attach the guard to propagated errors.
    let redirs: RedirSet = RedirSet::from(std::mem::take(&mut node.redirs));
    let guard = redirs.apply()?;

    // Register ChildProc in current job
    let job = dispatcher.job_stack.curr_job_mut().unwrap();
    let child_pgid = if let Some(pgid) = job.pgid() {
      pgid
    } else {
      let pid = Pid::this();
      job.set_pgid(pid);
      pid
    };
    let child = ChildProc::new(
      Pid::this(),
      Some(&cmd_raw),
      fork_builtins.then_some(child_pgid),
      report_time,
    )?;
    job.push_child(child);

    // Handle exec specially - persist redirections before dispatch
    if cmd_raw.as_str() == "exec"
      && let Some(guard) = guard
    {
      guard.persist();
    }

    let result = self.run_builtin(node, dispatcher);

    // Now we inspect the error that we got, if any
    match result {
      Ok(()) => Ok(()),
      Err(e) => {
        // if we aren't in the context these are looking for
        // then they will bubble all the way up to main
        // which cancels execution. Let's catch that here
        let should_propagate = match e.kind() {
          ShErrKind::CleanExit(_) => true, // this one always goes
          ShErrKind::LoopBreak(_) | ShErrKind::LoopContinue(_) => {
            state::Shed::meta(|m| m.in_loop())
          }
          ShErrKind::FuncReturn(_) => state::Shed::meta(|m| m.in_func()),
          _ => false,
        };

        if should_propagate {
          Err(e.with_context(context))
        } else {
          e.with_context(context).print_error();
          with_status(1)
        }
      }
    }
  }
  /// Parse arguments and options, pack BuiltinArgs, run self.execute()
  fn run_builtin(&self, node: Node, _dispatcher: &mut Dispatcher) -> ShResult<()> {
    let span = node.get_span().clone();
    let NdRule::Command {
      assignments: _,
      argv,
    } = node.class
    else {
      unreachable!()
    };

    let (argv, opts) = self.get_argv_and_opts(argv)?;
    let builtin_args = BuiltinArgs { argv, opts, span };

    self.execute(builtin_args)
  }
}

/// The arguments for a builtin.
///
/// Contains the argument vector (`argv`), the parsed options (`opts`), and the `span` of the entire command for error reporting.
pub struct BuiltinArgs {
  argv: Vec<(String, Span)>,
  opts: Vec<Opt>,
  span: Span,
}

impl BuiltinArgs {
  pub fn span(&self) -> Span {
    // cloning spans is cheap
    self.span.clone()
  }
}

// Join all of the word-split arguments into a single string
// Preserve the span too
pub fn join_raw_args(args: Vec<(String, Span)>) -> (String, Span) {
  join_raw_arg_iter(args.into_iter())
}

pub fn join_raw_arg_iter(args: impl Iterator<Item = (String, Span)>) -> (String, Span) {
  args.fold((String::new(), Span::default()), |mut acc, arg| {
    if acc.1 == Span::default() {
      acc.1 = arg.1.clone();
    } else {
      let new_end = arg.1.end();
      let start = acc.1.start();
      acc.1.set_range(start..new_end);
    }

    if acc.0.is_empty() {
      acc.0 = arg.0;
    } else {
      acc.0 = acc.0 + &format!(" {}", arg.0);
    }
    acc
  })
}

// The easy ones

struct Colon;
impl Builtin for Colon {
  fn execute(&self, _args: BuiltinArgs) -> ShResult<()> {
    with_status(0)
  }
}

struct True;
impl Builtin for True {
  fn execute(&self, _args: BuiltinArgs) -> ShResult<()> {
    with_status(0)
  }
}

struct False;
impl Builtin for False {
  fn execute(&self, _args: BuiltinArgs) -> ShResult<()> {
    with_status(1)
  }
}

struct BuiltinBuiltin;
impl Builtin for BuiltinBuiltin {
  // lol
  fn execute(&self, _args: BuiltinArgs) -> ShResult<()> {
    unreachable!("this one operates on the node directly")
  }
  fn setup_builtin(&self, mut node: Node, dispatcher: &mut Dispatcher) -> ShResult<()> {
    let span = node.get_span();
    let NdRule::Command {
      assignments: _,
      ref mut argv,
    } = node.class
    else {
      unreachable!()
    };
    *argv = argv
      .iter_mut()
      .skip(1)
      .map(|tk| tk.clone())
      .collect::<Vec<Tk>>();

    let cmd = argv.first().map(|tk| tk.as_str()).unwrap_or("");
    let Some(builtin) = lookup_builtin(cmd) else {
      sherr!(NotFound @ span, "builtin not found: {cmd}").print_error();
      return with_status(127);
    };

    builtin.setup_builtin(node, dispatcher)
  }
}

pub struct CommandBuiltin;
impl Builtin for CommandBuiltin {
  fn execute(&self, _args: BuiltinArgs) -> ShResult<()> {
    unreachable!("this one operates on the node directly")
  }
  fn run_builtin(&self, mut node: Node, dispatcher: &mut Dispatcher) -> ShResult<()> {
    let NdRule::Command {
      assignments: _,
      ref mut argv,
    } = node.class
    else {
      unreachable!()
    };
    if !argv.is_empty() {
      argv.remove(0);
    }

    let mut use_default_path = false;
    let mut print_path = false;
    let mut print_type = false;
    let mut seen_dd = false;

    let iter = std::mem::take(argv).into_iter();
    let mut rest = vec![];

    for tk in iter {
      if !rest.is_empty() || seen_dd {
        rest.push(tk);
        continue;
      }

      match tk.as_str() {
        "-p" => use_default_path = true,

        "-v" if !print_type => print_path = true,
        "-V" if !print_path => print_type = true,

        "-v" if print_type => {
          return Err(sherr!(InvalidOpt @ tk.span.clone(), "cannot specify both -v and -V"));
        }
        "-V" if print_path => {
          return Err(sherr!(InvalidOpt @ tk.span.clone(), "cannot specify both -v and -V"));
        }

        "--" => seen_dd = true,
        s if s.starts_with('-') => {
          return Err(sherr!(InvalidOpt @ tk.span.clone(), "invalid option: {s}"));
        }
        _ => rest.push(tk),
      }
    }

    if rest.is_empty() {
      return with_status(0);
    }

    *argv = rest;

    if use_default_path {
      let Some(default_path) = state::util::get_default_path() else {
        #[cfg(target_os = "android")]
        return Err(sherr!(ExecFail @ node.get_span(), "the -p flag is not supported on Android"));

        #[cfg(not(target_os = "android"))]
        return Err(sherr!(ExecFail @ node.get_span(), "unable to get default path"));
      };
      // TODO: Find a way to do this that doesn't involve forcing a full PATH rehash twice
      defer! {
        Shed::meta_mut(|m| m.rehash());
      }
      state::util::with_vars([("PATH".to_string(), default_path)], || {
        Shed::meta_mut(|m| m.rehash());
        Self::execute_inner(print_path, print_type, node, dispatcher)
      })
    } else {
      Self::execute_inner(print_path, print_type, node, dispatcher)
    }
  }
}

impl CommandBuiltin {
  fn execute_inner(
    print_path: bool,
    print_type: bool,
    node: Node,
    dispatcher: &mut Dispatcher,
  ) -> ShResult<()> {
    let NdRule::Command { argv, .. } = &node.class else {
      unreachable!()
    };
    if print_path {
      let Some(name) = argv.first() else {
        return with_status(2);
      };
      let name_str = name.as_str();
      match state::util::which_util(name_str) {
        Some(util) => match util.kind() {
          UtilKind::Alias => {
            let Some(alias) = Shed::logic(|l| l.get_alias(name_str)) else {
              return with_status(127);
            };
            outln!("alias {name_str}={}", as_var_val_display(alias.body()));
          }
          UtilKind::Function | UtilKind::Builtin => outln!("{name_str}"),
          UtilKind::Command(p) | UtilKind::File(p) => outln!("{}", p.display()),
        },
        None if KEYWORDS.contains(&name_str) => outln!("{name_str}"),
        None => return with_status(127),
      }

      return with_status(0);
    }
    if print_type {
      let Some(name) = argv.first() else {
        return with_status(2);
      };
      let name_str = name.as_str();
      match state::util::which_util(name_str) {
        Some(util) => match util.kind() {
          UtilKind::Alias => {
            let Some(alias) = Shed::logic(|l| l.get_alias(name_str)) else {
              return with_status(127);
            };
            outln!(
              "{name_str} is an alias for {}",
              as_var_val_display(alias.body())
            );
          }
          UtilKind::Function => outln!("{name_str} is a function"),
          UtilKind::Builtin => outln!("{name_str} is a shell builtin"),
          UtilKind::Command(p) | UtilKind::File(p) => {
            outln!("{name_str} is {}", p.display())
          }
        },
        None if KEYWORDS.contains(&name_str) => outln!("{name_str} is a shell keyword"),
        None => {
          errln!("command: {name_str}: not found");
          return with_status(127);
        }
      }

      return with_status(0);
    }

    // this one has to offload to the dispatcher
    dispatcher.exec_cmd(node)
  }
}

#[cfg(test)]
pub mod tests {
  use crate::{
    assert_status_eq,
    builtin::{source_builtin_completions, source_builtin_scripts},
    eval::execute::exec_nonint,
    state,
    tests::testutil::{TestGuard, has_cmd, test_input},
  };

  // You can never be too sure!!!!!!
  #[test]
  fn test_true() {
    let _g = TestGuard::new();
    test_input("true").unwrap();

    assert_eq!(state::Shed::get_status(), 0);
  }

  #[test]
  fn test_false() {
    let _g = TestGuard::new();
    test_input("false").unwrap();

    assert_eq!(state::Shed::get_status(), 1);
  }

  #[test]
  fn test_colon() {
    let _g = TestGuard::new();
    test_input(":").unwrap();

    assert_eq!(state::Shed::get_status(), 0);
  }

  #[test]
  fn builtin_scripts_pass() {
    let _g = TestGuard::new();
    source_builtin_scripts();
    assert_status_eq!(0);

    let failures: Vec<&str> = super::SCRIPTS
      .iter()
      .filter(|(path, src)| {
        crate::eval::execute::exec_nonint(src.to_string(), Some(path.to_string().into())).is_err()
      })
      .map(|(path, _)| *path)
      .collect();
    assert!(
      failures.is_empty(),
      "Functions failed to source: {failures:?}"
    );
  }

  #[test]
  fn builtin_help_flag_works() {
    let _g = TestGuard::new();
    exec_nonint("echo --help".into(), Some("builtin help test".into())).unwrap();
    assert_status_eq!(0);
  }

  // ===================== command builtin =====================

  #[test]
  fn command_bare_dispatches() {
    let g = TestGuard::new();
    test_input("command echo hello_dispatch").unwrap();
    let out = g.read_output();
    assert!(out.contains("hello_dispatch"), "got: {out:?}");
    assert_eq!(state::Shed::get_status(), 0);
  }

  #[test]
  fn command_v_builtin_prints_just_name() {
    let g = TestGuard::new();
    test_input("command -v echo").unwrap();
    let out = g.read_output();
    assert!(out.contains("echo"), "got: {out:?}");
    assert!(
      !out.contains('/'),
      "builtin should not print a path: {out:?}"
    );
    assert!(!out.contains("is"), "no -V-style prose for -v: {out:?}");
  }

  #[test]
  fn command_v_keyword_prints_just_name() {
    let g = TestGuard::new();
    test_input("command -v if").unwrap();
    let out = g.read_output();
    assert!(out.contains("if"), "got: {out:?}");
    assert!(
      !out.contains('/'),
      "keyword should not print a path: {out:?}"
    );
  }

  #[test]
  fn command_v_function_prints_just_name() {
    let g = TestGuard::new();
    test_input("myfn_for_cmdv() { :; }").unwrap();
    g.read_output();

    test_input("command -v myfn_for_cmdv").unwrap();
    let out = g.read_output();
    assert!(out.contains("myfn_for_cmdv"), "got: {out:?}");
    assert!(
      !out.contains('/'),
      "function should not print a path: {out:?}"
    );
  }

  #[test]
  fn command_v_alias_prints_alias_line() {
    let g = TestGuard::new();
    test_input("alias myalias_for_cmdv='ls -la'").unwrap();
    g.read_output();

    test_input("command -v myalias_for_cmdv").unwrap();
    let out = g.read_output();
    assert!(out.contains("alias myalias_for_cmdv="), "got: {out:?}");
    assert!(out.contains("ls -la"), "got: {out:?}");
  }

  #[test]
  fn command_v_external_prints_absolute_path() {
    if !has_cmd("cat") {
      return;
    }
    let g = TestGuard::new();
    test_input("command -v cat").unwrap();
    let out = g.read_output();
    assert!(out.contains("cat"), "got: {out:?}");
    assert!(out.contains('/'), "external should print a path: {out:?}");
  }

  #[test]
  fn command_v_not_found_is_silent_and_127() {
    let _g = TestGuard::new();
    let res = test_input("command -v __hopefully__not__a__command__");
    assert!(res.is_ok());
    assert_eq!(state::Shed::get_status(), 127);
  }

  #[test]
  #[allow(non_snake_case)]
  fn command_V_builtin_says_shell_builtin() {
    let g = TestGuard::new();
    test_input("command -V echo").unwrap();
    let out = g.read_output();
    assert!(out.contains("echo"), "got: {out:?}");
    assert!(out.contains("shell builtin"), "got: {out:?}");
  }

  #[test]
  #[allow(non_snake_case)]
  fn command_V_keyword_says_shell_keyword() {
    let g = TestGuard::new();
    test_input("command -V if").unwrap();
    let out = g.read_output();
    assert!(out.contains("if"), "got: {out:?}");
    assert!(out.contains("shell keyword"), "got: {out:?}");
  }

  #[test]
  #[allow(non_snake_case)]
  fn command_V_not_found_writes_stderr_and_127() {
    let g = TestGuard::new();
    test_input("command -V __hopefully__not__a__command__").unwrap();
    let out = g.read_output();
    assert!(out.contains("not found"), "got: {out:?}");
    assert_eq!(state::Shed::get_status(), 127);
  }

  #[test]
  #[allow(non_snake_case)]
  fn command_v_and_V_together_errors() {
    let _g = TestGuard::new();
    let _ = test_input("command -v -V echo");
    assert_ne!(state::Shed::get_status(), 0);
  }

  #[test]
  #[allow(non_snake_case)]
  fn command_V_and_v_together_errors() {
    let _g = TestGuard::new();
    let _ = test_input("command -V -v echo");
    assert_ne!(state::Shed::get_status(), 0);
  }

  #[test]
  fn command_double_dash_terminates_option_parsing() {
    // If `--` works, `-V` here is the command_name (not a flag), so
    // dispatching it should fail with 127 (no such command). If `--`
    // is broken, `-V` would be parsed as a flag and the missing
    // command_name path would set a different exit status.
    let _g = TestGuard::new();
    test_input("command -- -V").unwrap();
    assert_eq!(state::Shed::get_status(), 127);
  }

  #[test]
  fn command_invalid_flag_errors() {
    let _g = TestGuard::new();
    let _ = test_input("command -Z something");
    assert_ne!(state::Shed::get_status(), 0);
  }

  #[test]
  fn command_p_restores_path_after_invocation() {
    if !has_cmd("cat") {
      return;
    }
    let g = TestGuard::new();
    // Set a sentinel PATH that wouldn't normally contain `cat`,
    // then run `command -p` which should temporarily switch to the
    // system default PATH to find it, then restore /sentinel afterwards.
    test_input("export PATH=/sentinel_path_xyz").unwrap();
    g.read_output();
    test_input("command -p cat /dev/null").unwrap();
    g.read_output();
    test_input("echo \"PATH_NOW=$PATH\"").unwrap();
    let out = g.read_output();
    assert!(
      out.contains("PATH_NOW=/sentinel_path_xyz"),
      "PATH was not restored after `command -p`: got {out:?}",
    );
  }

  #[test]
  fn builtin_completions_pass() {
    let _g = TestGuard::new();
    source_builtin_completions();
    assert_status_eq!(0);
    let failures: Vec<&str> = super::COMPLETIONS
      .iter()
      .filter(|(name, src)| {
        crate::eval::execute::exec_nonint(src.to_string(), Some(format!("{name} comp").into()))
          .is_err()
      })
      .map(|(name, _)| *name)
      .collect();
    assert!(
      failures.is_empty(),
      "Completions failed to source: {failures:?}"
    );
  }
}
