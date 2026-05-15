use std::{path::Path, sync::atomic::Ordering};

use nix::{
  errno::Errno,
  unistd::{isatty, read},
};

use super::{
  ShResult, Shed, errln, expand_keymap, interactive, lifecycle,
  parse::execute::{exec_dash_c, exec_nonint},
  procio, sherr,
  signal::QUIT_CODE,
  state, status_msg,
};

pub fn dispatch_input(mut args: lifecycle::ShedArgs) -> ShResult<()> {
  match args.edit_script {
    true => {
      // in this arm, we interpret the input we are given as a sequence of keys
      // for the line editor to consume and execute
      let input = if let Some(ref cmd) = args.command {
        cmd.clone()
      } else if args.stdin || !isatty(procio::stdin_fileno()).unwrap_or(false) {
        read_input()?
      } else if !args.script_args.is_empty() {
        let path = args.script_args.remove(0);
        std::fs::read_to_string(path)?
      } else {
        // no input provided, just run interactively
        status_msg!("warning: --script was passed but no input was given");
        return interactive::shed_interactive(args, None);
      };

      let keys = expand_keymap(&input);
      interactive::shed_interactive(args, Some(keys))
    }
    false => {
      if let Some(cmd) = args.command {
        exec_dash_c(cmd, args.script_args)
      } else if args.stdin || !isatty(procio::stdin_fileno()).unwrap_or(false) {
        read_commands(args.script_args)
      } else if !args.script_args.is_empty() {
        let path = args.script_args.remove(0);
        run_script(path, args.script_args)
      } else {
        interactive::shed_interactive(args, None)
      }
    }
  }
}

pub(crate) fn read_commands(args: Vec<String>) -> ShResult<()> {
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
    match read(procio::stdin_fileno(), &mut read_buf) {
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

pub(crate) fn run_script<P: AsRef<Path>>(path: P, args: Vec<String>) -> ShResult<()> {
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
