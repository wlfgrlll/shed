use std::env;

use crate::{
  outln,
  util::{ShResult, with_status},
};

pub(super) struct Pwd;
impl super::Builtin for Pwd {
  fn execute(&self, _args: super::BuiltinArgs) -> ShResult<()> {
    let curr_dir = env::current_dir().unwrap().display().to_string();

    outln!("{curr_dir}");

    with_status(0)
  }
}

#[cfg(test)]
mod tests {
  use crate::state;
  use crate::tests::testutil::{TestGuard, test_input};
  use std::env;
  use tempfile::TempDir;

  #[test]
  fn pwd_prints_cwd() {
    let guard = TestGuard::new();
    let cwd = env::current_dir().unwrap();

    test_input("pwd").unwrap();
    let out = guard.read_output();
    assert_eq!(out.trim(), cwd.display().to_string());
  }

  #[test]
  fn pwd_after_cd() {
    let guard = TestGuard::new();
    let tmp = TempDir::new().unwrap();

    test_input(format!("cd {}", tmp.path().display())).unwrap();
    guard.read_output();

    test_input("pwd").unwrap();
    let out = guard.read_output();
    assert_eq!(out.trim(), tmp.path().display().to_string());
  }

  #[test]
  fn pwd_status_zero() {
    let _g = TestGuard::new();
    test_input("pwd").unwrap();
    assert_eq!(state::get_status(), 0);
  }
}
