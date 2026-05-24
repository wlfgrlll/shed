use std::os::fd::AsRawFd;

use super::{
  ShErrKind, ShResult, Shed,
  arithmetic::expand_arithmetic_wrapped,
  eval::execute::exec_nonint,
  procio::{RedirSet, RedirSpec, RedirType, pipes_high, pipes_high_no_cloexec, read_fd_to_string},
  sherr, state,
};

use nix::errno::Errno;
use nix::sys::wait::{WaitPidFlag as WtFlag, WaitStatus as WtStat, waitpid};
use nix::unistd::{ForkResult, fork};

pub fn expand_proc_sub(raw: &str, is_input: bool) -> ShResult<String> {
  let (rpipe, wpipe) = pipes_high_no_cloexec()?;
  let rpipe_raw = rpipe.as_raw_fd();
  let wpipe_raw = wpipe.as_raw_fd();

  let (proc_fd, register_fd, redir_type, path) = match is_input {
    false => (
      wpipe,
      rpipe,
      RedirType::Output,
      format!("/dev/fd/{}", rpipe_raw),
    ),
    true => (
      rpipe,
      wpipe,
      RedirType::Input,
      format!("/dev/fd/{}", wpipe_raw),
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

      let redir: RedirSet =
        RedirSpec::dup(proc_fd.as_raw_fd(), target_fd, RedirType::Output).into();
      let _guard = redir.apply()?;

      if let Err(e) = exec_nonint(raw.to_string(), Some("process_sub".into())) {
        e.print_error();
        unsafe { nix::libc::_exit(1) };
      }
      unsafe { nix::libc::_exit(0) };
    }
    ForkResult::Parent { .. } => {
      Shed::meta_mut(|m| m.save_procsub_fd(register_fd));
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
      Shed::term_mut(|t| t.detach_tty()); // close tty fd
      let redir: RedirSet = RedirSpec::dup(wpipe.as_raw_fd(), 1, RedirType::Output).into();
      let _redir_guard = redir.apply()?;

      if let Err(e) = exec_nonint(raw.to_string(), Some("command_sub".into())) {
        if let ShErrKind::CleanExit(code) = e.kind() {
          std::process::exit(*code);
        }
        e.print_error();
        unsafe { nix::libc::_exit(1) };
      }
      let status = state::Shed::get_status();
      unsafe { nix::libc::_exit(status) };
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
          state::Shed::set_status(code);
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

  #[test]
  fn cmd_sub_only_final_newline_is_stripped() {
    // Internal newlines must survive; just the trailing run is removed.
    let _g = TestGuard::new();
    let result = expand_cmd_sub("printf 'a\\nb\\nc\\n'").unwrap();
    assert_eq!(result, "a\nb\nc");
  }

  #[test]
  fn cmd_sub_empty_output() {
    let _g = TestGuard::new();
    let result = expand_cmd_sub("true").unwrap();
    assert_eq!(result, "");
  }

  #[test]
  fn cmd_sub_sets_status_to_child_exit_code() {
    // `(exit N)` would hit the arithmetic fast-path; use a bare
    // command that genuinely exits with the desired status.
    let _g = TestGuard::new();
    expand_cmd_sub("false").unwrap();
    assert_eq!(state::Shed::get_status(), 1);
  }

  #[test]
  fn cmd_sub_zero_status_on_success() {
    let _g = TestGuard::new();
    expand_cmd_sub("true").unwrap();
    assert_eq!(state::Shed::get_status(), 0);
  }

  #[test]
  fn cmd_sub_arithmetic_distinguished_from_subshell_grouping() {
    // The outer-parens-check fast-path routes "(N+M)" to the arithmetic
    // expander, not to fork+exec. Verify by giving an arithmetic input
    // that wouldn't be valid as a shell command.
    let result = expand_cmd_sub("(10*5)").unwrap();
    assert_eq!(result, "50");
  }

  #[test]
  fn cmd_sub_large_output_does_not_deadlock() {
    // Parent reads from the pipe before waitpid; otherwise a child
    // writing more than the pipe buffer would block forever. Build
    // the payload via shell-only string doubling + `echo` (builtin,
    // so no execve / ARG_MAX involvement) — no PATH dependency.
    let _g = TestGuard::new();
    // 2^18 = 262144 chars — comfortably above a typical 64KB pipe buf.
    let result = expand_cmd_sub(
      "s=x; for _ in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18; do s=$s$s; done; echo \"$s\"",
    )
    .unwrap();
    assert_eq!(result.len(), 1 << 18);
    assert!(result.chars().all(|c| c == 'x'));
  }

  // ===================== expand_proc_sub =====================

  #[test]
  fn proc_sub_input_returns_dev_fd_path() {
    // is_input=true: path points at the writer fd we hold open in the
    // parent (so we could write through it); the format is the
    // /dev/fd/N path.
    let _g = TestGuard::new();
    let path = expand_proc_sub("echo hello", true).unwrap();
    assert!(
      path.starts_with("/dev/fd/"),
      "expected /dev/fd/... path, got: {path:?}"
    );
  }

  #[test]
  fn proc_sub_output_returns_dev_fd_path() {
    // is_input=false: path points at the reader fd; same shape.
    let _g = TestGuard::new();
    let path = expand_proc_sub("cat > /dev/null", false).unwrap();
    assert!(
      path.starts_with("/dev/fd/"),
      "expected /dev/fd/... path, got: {path:?}"
    );
  }

  #[test]
  fn proc_sub_input_path_is_readable_with_command_output() {
    // <(cmd) — reading from the returned path should yield the
    // command's stdout. This exercises the full plumbing: dup target
    // fd 1 in the child, parent reads via /dev/fd.
    let _g = TestGuard::new();
    let path = expand_proc_sub("echo proc_sub_marker_xyz", false).unwrap();
    // Open the path and read; the child writes 'proc_sub_marker_xyz\n'.
    let content = std::fs::read_to_string(&path).unwrap();
    assert!(content.contains("proc_sub_marker_xyz"), "got: {content:?}");
  }
}
