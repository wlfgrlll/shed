use std::path::PathBuf;

use super::{ShResult, sherr, state::util::source_file};

pub(super) struct Source;
impl super::Builtin for Source {
  fn is_special(&self) -> bool {
    true
  }

  fn execute(&self, args: super::BuiltinArgs) -> ShResult<()> {
    for (arg, span) in args.argv {
      let path = PathBuf::from(arg);

      if !path.exists() {
        return Err(sherr!(
          ExecFail @ span,
          "source: File '{}' not found", path.display(),
        ));
      } else if !path.is_file() {
        return Err(sherr!(
          ExecFail @ span,
          "source: Given path '{}' is not a file", path.display(),
        ));
      }

      source_file(path)?;
    }

    Ok(())
  }
}

#[cfg(test)]
pub mod tests {
  use std::io::Write;

  use crate::state::{self, Shed};
  use crate::tests::testutil::{TestGuard, test_input};
  use crate::var;
  use tempfile::{NamedTempFile, TempDir};

  #[test]
  fn source_simple() {
    let _g = TestGuard::new();
    let mut file = NamedTempFile::new().unwrap();
    let path = file.path().display().to_string();
    file.write_all(b"some_var=some_val").unwrap();

    test_input(format!("source {path}")).unwrap();
    let var = var!("some_var");
    assert_eq!(var, "some_val".to_string());
  }

  #[test]
  fn source_multiple_commands() {
    let _g = TestGuard::new();
    let mut file = NamedTempFile::new().unwrap();
    let path = file.path().display().to_string();
    file.write_all(b"x=1\ny=2\nz=3").unwrap();

    test_input(format!("source {path}")).unwrap();
    assert_eq!(var!("x"), "1");
    assert_eq!(var!("y"), "2");
    assert_eq!(var!("z"), "3");
  }

  #[test]
  fn source_defines_function() {
    let _g = TestGuard::new();
    let mut file = NamedTempFile::new().unwrap();
    let path = file.path().display().to_string();
    file.write_all(b"greet() { echo hi; }").unwrap();

    test_input(format!("source {path}")).unwrap();
    let func = Shed::logic(|l| l.get_func("greet"));
    assert!(func.is_some());
  }

  #[test]
  fn source_defines_alias() {
    let _g = TestGuard::new();
    let mut file = NamedTempFile::new().unwrap();
    let path = file.path().display().to_string();
    file.write_all(b"alias ll='ls -la'").unwrap();

    test_input(format!("source {path}")).unwrap();
    let alias = Shed::logic(|l| l.get_alias("ll"));
    assert!(alias.is_some());
  }

  #[test]
  fn source_output_captured() {
    let guard = TestGuard::new();
    let mut file = NamedTempFile::new().unwrap();
    let path = file.path().display().to_string();
    file.write_all(b"echo sourced").unwrap();

    test_input(format!("source {path}")).unwrap();
    let out = guard.read_output();
    assert!(out.contains("sourced"));
  }

  #[test]
  fn source_multiple_files() {
    let _g = TestGuard::new();
    let mut file1 = NamedTempFile::new().unwrap();
    let mut file2 = NamedTempFile::new().unwrap();
    let path1 = file1.path().display().to_string();
    let path2 = file2.path().display().to_string();
    file1.write_all(b"a=from_file1").unwrap();
    file2.write_all(b"b=from_file2").unwrap();

    test_input(format!("source {path1} {path2}")).unwrap();
    assert_eq!(var!("a"), "from_file1");
    assert_eq!(var!("b"), "from_file2");
  }

  // ===================== Dot syntax =====================

  #[test]
  fn source_dot_syntax() {
    let _g = TestGuard::new();
    let mut file = NamedTempFile::new().unwrap();
    let path = file.path().display().to_string();
    file.write_all(b"dot_var=dot_val").unwrap();

    test_input(format!(". {path}")).unwrap();
    assert_eq!(var!("dot_var"), "dot_val");
  }

  // ===================== Error cases =====================

  #[test]
  fn source_nonexistent_file() {
    let _g = TestGuard::new();
    test_input("source /tmp/__no_such_file_xyz__").ok();
    assert_ne!(state::Shed::get_status(), 0);
  }

  #[test]
  fn source_directory_fails() {
    let _g = TestGuard::new();
    let dir = TempDir::new().unwrap();
    test_input(format!("source {}", dir.path().display())).ok();
    assert_ne!(state::Shed::get_status(), 0);
  }

  // ===================== Status =====================

  #[test]
  fn source_status_zero() {
    let _g = TestGuard::new();
    let mut file = NamedTempFile::new().unwrap();
    let path = file.path().display().to_string();
    file.write_all(b"true").unwrap();

    test_input(format!("source {path}")).unwrap();
    assert_eq!(state::Shed::get_status(), 0);
  }

  #[test]
  fn source_top_level_return_exits_script_cleanly() {
    let _g = TestGuard::new();
    let mut file = NamedTempFile::new().unwrap();
    let path = file.path().display().to_string();
    file
      .write_all(b"before=set\nreturn 0\nafter=should_not_run\n")
      .unwrap();

    test_input(format!("source {path}")).unwrap();
    assert_eq!(var!("before"), "set");
    assert_eq!(var!("after"), "");
    assert_eq!(state::Shed::get_status(), 0);
  }

  #[test]
  fn source_top_level_return_propagates_status() {
    let _g = TestGuard::new();
    let mut file = NamedTempFile::new().unwrap();
    let path = file.path().display().to_string();
    file.write_all(b"return 42").unwrap();

    test_input(format!("source {path}")).unwrap();
    assert_eq!(state::Shed::get_status(), 42);
  }

  #[test]
  fn source_return_inside_conditional() {
    let _g = TestGuard::new();
    let mut file = NamedTempFile::new().unwrap();
    let path = file.path().display().to_string();
    file
      .write_all(
        b"GUARD=yes\n\
          if [ -n \"$GUARD\" ]; then return; fi\n\
          after=should_not_run\n",
      )
      .unwrap();

    test_input(format!("source {path}")).unwrap();
    assert_eq!(var!("after"), "");
  }
}
