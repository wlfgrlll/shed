use crate::expand::arithmetic::expand_arithmetic_wrapped;
use crate::parse::execute::exec_nonint;
use crate::prelude::*;
use crate::procio::{Redir, RedirGuard, RedirSet, RedirSpec, RedirType, pipes_high, read_fd_to_string};
use crate::sherr;
use crate::state::{self, write_meta};
use crate::util::error::{ShErrKind, ShResult};

pub fn expand_proc_sub(raw: &str, is_input: bool) -> ShResult<String> {
  let (rpipe, wpipe) = pipes_high()?;
  let rpipe_raw = rpipe.as_raw_fd();
  let wpipe_raw = wpipe.as_raw_fd();

  let (proc_fd, register_fd, redir_type, path) = match is_input {
    false => (
      wpipe,
      rpipe,
      RedirType::Output,
      format!("/proc/self/fd/{}", rpipe_raw),
    ),
    true => (
      rpipe,
      wpipe,
      RedirType::Input,
      format!("/proc/self/fd/{}", wpipe_raw),
    ),
  };

  let target_fd = match redir_type {
    RedirType::Input => 0,
    RedirType::Output => 1,
    _ => unreachable!(),
  };

  match unsafe { fork()? } {
    ForkResult::Child => {
      drop(register_fd);

      let redir: RedirSet = RedirSpec::dup(proc_fd.as_raw_fd(), target_fd, RedirType::Output).into();
      let _guard = redir.apply()?;

      if let Err(e) = exec_nonint(raw.to_string(), Some("process_sub".into())) {
        e.print_error();
        exit(1);
      }
      exit(0);
    }
    ForkResult::Parent {..} => {
      write_meta(|m| m.save_procsub_fd(register_fd));
      // Do not wait; process may run in background
      Ok(path)
    }
  }
}

/// Get the command output of a given command input as a String
pub fn expand_cmd_sub(raw: &str) -> ShResult<String> {
  if raw.starts_with('(') && raw.ends_with(')') {
    return expand_arithmetic_wrapped(raw);
  }
  let (rpipe, wpipe) = pipes_high()?;

  match unsafe { fork()? } {
    ForkResult::Child => {
      let redir: RedirSet = RedirSpec::dup(wpipe.as_raw_fd(), 1, RedirType::Output).into();
      let _guard = redir.apply()?;

      if let Err(e) = exec_nonint(raw.to_string(), Some("command_sub".into())) {
        if let ShErrKind::CleanExit(code) = e.kind() {
          std::process::exit(*code);
        }
        e.print_error();
        unsafe { libc::_exit(1) };
      }
      let status = state::get_status();
      unsafe { libc::_exit(status) };
    }
    ForkResult::Parent { child } => {
      drop(wpipe);

      // Read output first (before waiting) to avoid deadlock if
      // child fills pipe buffer
      let output = read_fd_to_string(rpipe)?;

      // Wait for child with EINTR retry
      let status = loop {
        match waitpid(child, Some(WtFlag::WSTOPPED)) {
          Ok(status) => break status,
          Err(Errno::EINTR) => continue,
          Err(e) => return Err(e.into()),
        }
      };

      match status {
        WtStat::Exited(_, code) => {
          state::set_status(code);
          Ok(output.trim_end_matches('\n').to_string())
        }
        _ => Err(sherr!(InternalErr, "Command sub failed")),
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::tests::testutil::TestGuard;

  // ===================== Command Substitution (TestGuard) =====================

  #[test]
  fn cmd_sub_echo() {
    let _guard = TestGuard::new();
    let result = expand_cmd_sub("echo hello").unwrap();
    assert_eq!(result, "hello");
  }

  #[test]
  fn cmd_sub_trailing_newlines_stripped() {
    let _guard = TestGuard::new();
    let result = expand_cmd_sub("printf 'hello\\n\\n'").unwrap();
    assert_eq!(result, "hello");
  }

  #[test]
  fn cmd_sub_arithmetic() {
    let result = expand_cmd_sub("(1+2)").unwrap();
    assert_eq!(result, "3");
  }
}
