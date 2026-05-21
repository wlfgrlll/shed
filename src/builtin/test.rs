use std::{collections::VecDeque, fs::metadata, os::fd::BorrowedFd, path::PathBuf, str::FromStr};

use super::{
  Shed,
  eval::{ConjunctOp, NdRule, Node, TEST_UNARY_OPS, TestCase},
  expand, sherr,
  state::{vars::VarFlags, vars::VarKind},
  util::{ShErr, ShResult},
};
use nix::{
  sys::stat::{self, SFlag},
  unistd::{AccessFlags, isatty},
};

#[derive(Debug, Clone)]
pub(crate) enum UnaryOp {
  Exists,                    // -e
  Directory,                 // -d
  File,                      // -f
  Symlink,                   // -h or -L
  Readable,                  // -r
  Writable,                  // -w
  Executable,                // -x
  NonEmpty,                  // -s
  NamedPipe,                 // -p
  Socket,                    // -S
  BlockSpecial,              // -b
  CharSpecial,               // -c
  Sticky,                    // -k
  UIDOwner,                  // -O
  GIDOwner,                  // -G
  ModifiedSinceStatusChange, // -N
  SetUID,                    // -u
  SetGID,                    // -g
  Terminal,                  // -t
  NonNull,                   // -n
  Null,                      // -z
}

impl FromStr for UnaryOp {
  type Err = ShErr;
  fn from_str(s: &str) -> Result<Self, Self::Err> {
    match s {
      "-e" => Ok(Self::Exists),
      "-d" => Ok(Self::Directory),
      "-f" => Ok(Self::File),
      "-h" | "-L" => Ok(Self::Symlink), // -h or -L
      "-r" => Ok(Self::Readable),
      "-w" => Ok(Self::Writable),
      "-x" => Ok(Self::Executable),
      "-s" => Ok(Self::NonEmpty),
      "-p" => Ok(Self::NamedPipe),
      "-S" => Ok(Self::Socket),
      "-b" => Ok(Self::BlockSpecial),
      "-c" => Ok(Self::CharSpecial),
      "-k" => Ok(Self::Sticky),
      "-O" => Ok(Self::UIDOwner),
      "-G" => Ok(Self::GIDOwner),
      "-N" => Ok(Self::ModifiedSinceStatusChange),
      "-u" => Ok(Self::SetUID),
      "-g" => Ok(Self::SetGID),
      "-t" => Ok(Self::Terminal),
      "-n" => Ok(Self::NonNull),
      "-z" => Ok(Self::Null),
      _ => Err(sherr!(SyntaxErr, "Invalid test operator")),
    }
  }
}

#[derive(Debug, Clone)]
pub(crate) enum TestOp {
  Unary(UnaryOp),
  StringEq,   // ==
  StringNeq,  // !=
  IntEq,      // -eq
  IntNeq,     // -ne
  IntGt,      // -gt
  IntLt,      // -lt
  IntGe,      // -ge
  IntLe,      // -le
  RegexMatch, // =~
}

impl FromStr for TestOp {
  type Err = ShErr;
  fn from_str(s: &str) -> Result<Self, Self::Err> {
    match s {
      "==" => Ok(Self::StringEq),
      "!=" => Ok(Self::StringNeq),
      "=~" => Ok(Self::RegexMatch),
      "-eq" => Ok(Self::IntEq),
      "-ne" => Ok(Self::IntNeq),
      "-gt" => Ok(Self::IntGt),
      "-lt" => Ok(Self::IntLt),
      "-ge" => Ok(Self::IntGe),
      "-le" => Ok(Self::IntLe),
      _ if TEST_UNARY_OPS.contains(&s) => Ok(Self::Unary(s.parse::<UnaryOp>()?)),
      _ => Err(sherr!(SyntaxErr, "Invalid test operator '{s}'")),
    }
  }
}

fn replace_posix_classes(pat: &str) -> String {
  pat
    .replace("[[:alnum:]]", r"[A-Za-z0-9]")
    .replace("[[:alpha:]]", r"[A-Za-z]")
    .replace("[[:blank:]]", r"[ \t]")
    .replace("[[:cntrl:]]", r"[\x00-\x1F\x7F]")
    .replace("[[:digit:]]", r"[0-9]")
    .replace("[[:graph:]]", r"[!-~]")
    .replace("[[:lower:]]", r"[a-z]")
    .replace("[[:print:]]", r"[\x20-\x7E]")
    .replace("[[:space:]]", r"[ \t\r\n\x0B\x0C]") // vertical tab (\x0B), form feed (\x0C)
    .replace("[[:upper:]]", r"[A-Z]")
    .replace("[[:xdigit:]]", r"[0-9A-Fa-f]")
}

pub fn double_bracket_test(node: Node) -> ShResult<bool> {
  let err_span = node.get_span();
  let NdRule::Test { cases } = node.class else {
    unreachable!()
  };
  let mut last_result = false;
  let mut conjunct_op: Option<ConjunctOp>;
  log::trace!("test cases: {:#?}", cases);

  for case in cases {
    let result = match case {
      TestCase::Unary {
        operator,
        operand,
        conjunct,
      } => {
        let operand = operand.expand_no_glob()?.get_words().join(" ");
        conjunct_op = conjunct;
        let TestOp::Unary(op) = TestOp::from_str(operator.as_str())? else {
          return Err(sherr!(
            SyntaxErr @ err_span,
            "Invalid unary operator",
          ));
        };
        match op {
          UnaryOp::Exists => {
            let path = PathBuf::from(operand.as_str());
            path.exists()
          }
          UnaryOp::Directory => {
            let path = PathBuf::from(operand.as_str());
            if path.exists() {
              path.metadata().unwrap().is_dir()
            } else {
              false
            }
          }
          UnaryOp::File => {
            let path = PathBuf::from(operand.as_str());
            if path.exists() {
              path.metadata().unwrap().is_file()
            } else {
              false
            }
          }
          UnaryOp::Symlink => {
            // symlink_metadata = lstat: returns the symlink's own metadata
            // rather than following to the target. `metadata()` here would
            // always report the *target's* file type and never see the
            // symlink itself.
            std::fs::symlink_metadata(operand.as_str())
              .map(|m| m.file_type().is_symlink())
              .unwrap_or(false)
          }
          UnaryOp::Readable => nix::unistd::access(operand.as_str(), AccessFlags::R_OK).is_ok(),
          UnaryOp::Writable => nix::unistd::access(operand.as_str(), AccessFlags::W_OK).is_ok(),
          UnaryOp::Executable => nix::unistd::access(operand.as_str(), AccessFlags::X_OK).is_ok(),
          UnaryOp::NonEmpty => match metadata(operand.as_str()) {
            Ok(meta) => meta.len() > 0,
            Err(_) => false,
          },
          UnaryOp::NamedPipe => match stat::stat(operand.as_str()) {
            Ok(stat) => SFlag::from_bits_truncate(stat.st_mode).contains(SFlag::S_IFIFO),
            Err(_) => false,
          },
          UnaryOp::Socket => match stat::stat(operand.as_str()) {
            Ok(stat) => SFlag::from_bits_truncate(stat.st_mode).contains(SFlag::S_IFSOCK),
            Err(_) => false,
          },
          UnaryOp::BlockSpecial => match stat::stat(operand.as_str()) {
            Ok(stat) => SFlag::from_bits_truncate(stat.st_mode).contains(SFlag::S_IFBLK),
            Err(_) => false,
          },
          UnaryOp::CharSpecial => match stat::stat(operand.as_str()) {
            Ok(stat) => SFlag::from_bits_truncate(stat.st_mode).contains(SFlag::S_IFCHR),
            Err(_) => false,
          },
          UnaryOp::Sticky => match stat::stat(operand.as_str()) {
            Ok(stat) => stat.st_mode & nix::libc::S_ISVTX != 0,
            Err(_) => false,
          },
          UnaryOp::UIDOwner => match stat::stat(operand.as_str()) {
            Ok(stat) => stat.st_uid == nix::unistd::geteuid().as_raw(),
            Err(_) => false,
          },

          UnaryOp::GIDOwner => match stat::stat(operand.as_str()) {
            Ok(stat) => stat.st_gid == nix::unistd::getegid().as_raw(),
            Err(_) => false,
          },

          UnaryOp::ModifiedSinceStatusChange => match stat::stat(operand.as_str()) {
            Ok(stat) => stat.st_mtime > stat.st_ctime,
            Err(_) => false,
          },

          UnaryOp::SetUID => match stat::stat(operand.as_str()) {
            Ok(stat) => stat.st_mode & nix::libc::S_ISUID != 0,
            Err(_) => false,
          },

          UnaryOp::SetGID => match stat::stat(operand.as_str()) {
            Ok(stat) => stat.st_mode & nix::libc::S_ISGID != 0,
            Err(_) => false,
          },

          UnaryOp::Terminal => match operand.as_str().parse::<i32>() {
            Ok(fd) => match isatty(unsafe { BorrowedFd::borrow_raw(fd) }) {
              Ok(b) => b,
              Err(e) => return Err(ShErr::from(e).promote(err_span)),
            },
            Err(_) => false,
          },
          UnaryOp::NonNull => !operand.is_empty(),
          UnaryOp::Null => operand.is_empty(),
        }
      }
      TestCase::Binary {
        lhs,
        operator,
        rhs,
        conjunct,
      } => {
        let lhs = lhs.expand_no_glob()?.get_words().join(" ");
        let rhs = rhs.expand_no_glob()?.get_words().join(" ");
        conjunct_op = conjunct;
        let test_op = operator.as_str().parse::<TestOp>()?;
        match test_op {
          TestOp::Unary(_) => {
            return Err(sherr!(
                SyntaxErr @ err_span,
                "Expected a binary operator in this test call; found a unary operator",
            ));
          }
          TestOp::StringEq => {
            let pattern = expand::glob_to_regex(rhs.trim(), true);
            pattern.is_match(lhs.trim())
          }
          TestOp::StringNeq => {
            let pattern = expand::glob_to_regex(rhs.trim(), true);
            !pattern.is_match(lhs.trim())
          }
          TestOp::IntNeq
          | TestOp::IntGt
          | TestOp::IntLt
          | TestOp::IntGe
          | TestOp::IntLe
          | TestOp::IntEq => {
            let err = sherr!(
              SyntaxErr @ err_span.clone(),
              "Expected an integer with '{operator}' operator"
            );
            let Ok(lhs) = lhs.trim().parse::<i32>() else {
              return Err(err);
            };
            let Ok(rhs) = rhs.trim().parse::<i32>() else {
              return Err(err);
            };
            match test_op {
              TestOp::IntNeq => lhs != rhs,
              TestOp::IntGt => lhs > rhs,
              TestOp::IntLt => lhs < rhs,
              TestOp::IntGe => lhs >= rhs,
              TestOp::IntLe => lhs <= rhs,
              TestOp::IntEq => lhs == rhs,
              _ => unreachable!(),
            }
          }
          TestOp::RegexMatch => {
            let cleaned = replace_posix_classes(&rhs);
            let re = Shed::meta_mut(|m| m.get_regex(cleaned))
              .map_err(|e| sherr!(SyntaxErr @ err_span.clone(), "Invalid regex: {e}"))?;

            if let Some(caps) = re.captures(&lhs) {
              let groups: VecDeque<String> = caps
                .iter()
                .map(|m| m.map(|mat| mat.as_str().to_string()).unwrap_or_default())
                .collect();

              Shed::vars_mut(|v| v.set_var("SHED_REMATCH", VarKind::Arr(groups), VarFlags::LOCAL))?;

              true
            } else {
              Shed::vars_mut(|v| v.unset_var("SHED_REMATCH")).ok();

              false
            }
          }
        }
      }
    };

    last_result = result;
    if let Some(op) = conjunct_op {
      match op {
        ConjunctOp::And if !last_result => break,
        ConjunctOp::Or if last_result => break,
        _ => {}
      }
    }
  }
  Ok(last_result)
}

#[cfg(test)]
mod tests {
  use crate::state;
  use crate::tests::testutil::{TestGuard, test_input};
  use std::fs;
  use tempfile::{NamedTempFile, TempDir};

  // ===================== Unary: file tests =====================

  #[test]
  fn test_exists_true() {
    let _g = TestGuard::new();
    let file = NamedTempFile::new().unwrap();
    test_input(format!("[[ -e {} ]]", file.path().display())).unwrap();
    assert_eq!(state::Shed::get_status(), 0);
  }

  #[test]
  fn test_exists_false() {
    let _g = TestGuard::new();
    test_input("[[ -e /tmp/__no_such_file_test_rs__ ]]").unwrap();
    assert_ne!(state::Shed::get_status(), 0);
  }

  #[test]
  fn test_is_directory() {
    let _g = TestGuard::new();
    let dir = TempDir::new().unwrap();
    test_input(format!("[[ -d {} ]]", dir.path().display())).unwrap();
    assert_eq!(state::Shed::get_status(), 0);
  }

  #[test]
  fn test_is_directory_false() {
    let _g = TestGuard::new();
    let file = NamedTempFile::new().unwrap();
    test_input(format!("[[ -d {} ]]", file.path().display())).unwrap();
    assert_ne!(state::Shed::get_status(), 0);
  }

  #[test]
  fn test_is_file() {
    let _g = TestGuard::new();
    let file = NamedTempFile::new().unwrap();
    test_input(format!("[[ -f {} ]]", file.path().display())).unwrap();
    assert_eq!(state::Shed::get_status(), 0);
  }

  #[test]
  fn test_is_file_false() {
    let _g = TestGuard::new();
    let dir = TempDir::new().unwrap();
    test_input(format!("[[ -f {} ]]", dir.path().display())).unwrap();
    assert_ne!(state::Shed::get_status(), 0);
  }

  #[test]
  fn test_readable() {
    let _g = TestGuard::new();
    let file = NamedTempFile::new().unwrap();
    test_input(format!("[[ -r {} ]]", file.path().display())).unwrap();
    assert_eq!(state::Shed::get_status(), 0);
  }

  #[test]
  fn test_writable() {
    let _g = TestGuard::new();
    let file = NamedTempFile::new().unwrap();
    test_input(format!("[[ -w {} ]]", file.path().display())).unwrap();
    assert_eq!(state::Shed::get_status(), 0);
  }

  #[test]
  fn test_non_empty_file() {
    let _g = TestGuard::new();
    let file = NamedTempFile::new().unwrap();
    fs::write(file.path(), "content").unwrap();
    test_input(format!("[[ -s {} ]]", file.path().display())).unwrap();
    assert_eq!(state::Shed::get_status(), 0);
  }

  #[test]
  fn test_empty_file() {
    let _g = TestGuard::new();
    let file = NamedTempFile::new().unwrap();
    test_input(format!("[[ -s {} ]]", file.path().display())).unwrap();
    assert_ne!(state::Shed::get_status(), 0);
  }

  // ===================== Unary: string tests =====================

  #[test]
  fn test_non_null_true() {
    let _g = TestGuard::new();
    test_input("[[ -n hello ]]").unwrap();
    assert_eq!(state::Shed::get_status(), 0);
  }

  #[test]
  fn test_non_null_empty() {
    let _g = TestGuard::new();
    test_input("[[ -n '' ]]").unwrap();
    assert_ne!(state::Shed::get_status(), 0);
  }

  #[test]
  fn test_null_true() {
    let _g = TestGuard::new();
    test_input("[[ -z '' ]]").unwrap();
    assert_eq!(state::Shed::get_status(), 0);
  }

  #[test]
  fn test_null_false() {
    let _g = TestGuard::new();
    test_input("[[ -z hello ]]").unwrap();
    assert_ne!(state::Shed::get_status(), 0);
  }

  // ===================== Binary: string comparison =====================

  #[test]
  fn test_string_eq() {
    let _g = TestGuard::new();
    test_input("[[ hello == hello ]]").unwrap();
    assert_eq!(state::Shed::get_status(), 0);
  }

  #[test]
  fn test_string_eq_false() {
    let _g = TestGuard::new();
    test_input("[[ hello == world ]]").unwrap();
    assert_ne!(state::Shed::get_status(), 0);
  }

  #[test]
  fn test_string_neq() {
    let _g = TestGuard::new();
    test_input("[[ hello != world ]]").unwrap();
    assert_eq!(state::Shed::get_status(), 0);
  }

  #[test]
  fn test_string_neq_false() {
    let _g = TestGuard::new();
    test_input("[[ hello != hello ]]").unwrap();
    assert_ne!(state::Shed::get_status(), 0);
  }

  #[test]
  fn test_string_glob_match() {
    let _g = TestGuard::new();
    test_input("[[ hello == hel* ]]").unwrap();
    assert_eq!(state::Shed::get_status(), 0);
  }

  #[test]
  fn test_string_glob_no_match() {
    let _g = TestGuard::new();
    test_input("[[ hello == wor* ]]").unwrap();
    assert_ne!(state::Shed::get_status(), 0);
  }

  // ===================== Binary: integer comparison =====================

  #[test]
  fn test_int_eq() {
    let _g = TestGuard::new();
    test_input("[[ 42 -eq 42 ]]").unwrap();
    assert_eq!(state::Shed::get_status(), 0);
  }

  #[test]
  fn test_int_eq_false() {
    let _g = TestGuard::new();
    test_input("[[ 42 -eq 43 ]]").unwrap();
    assert_ne!(state::Shed::get_status(), 0);
  }

  #[test]
  fn test_int_ne() {
    let _g = TestGuard::new();
    test_input("[[ 1 -ne 2 ]]").unwrap();
    assert_eq!(state::Shed::get_status(), 0);
  }

  #[test]
  fn test_int_gt() {
    let _g = TestGuard::new();
    test_input("[[ 10 -gt 5 ]]").unwrap();
    assert_eq!(state::Shed::get_status(), 0);
  }

  #[test]
  fn test_int_gt_false() {
    let _g = TestGuard::new();
    test_input("[[ 5 -gt 10 ]]").unwrap();
    assert_ne!(state::Shed::get_status(), 0);
  }

  #[test]
  fn test_int_lt() {
    let _g = TestGuard::new();
    test_input("[[ 5 -lt 10 ]]").unwrap();
    assert_eq!(state::Shed::get_status(), 0);
  }

  #[test]
  fn test_int_ge() {
    let _g = TestGuard::new();
    test_input("[[ 10 -ge 10 ]]").unwrap();
    assert_eq!(state::Shed::get_status(), 0);
  }

  #[test]
  fn test_int_le() {
    let _g = TestGuard::new();
    test_input("[[ 5 -le 5 ]]").unwrap();
    assert_eq!(state::Shed::get_status(), 0);
  }

  #[test]
  fn test_int_negative() {
    let _g = TestGuard::new();
    test_input("[[ -5 -lt 0 ]]").unwrap();
    assert_eq!(state::Shed::get_status(), 0);
  }

  #[test]
  fn test_int_non_integer_errors() {
    let _g = TestGuard::new();
    let result = test_input("[[ abc -eq 1 ]]");
    assert!(result.is_err());
  }

  // ===================== Binary: regex match =====================

  #[test]
  fn test_regex_match() {
    let _g = TestGuard::new();
    test_input("[[ hello123 =~ ^hello[0-9]+$ ]]").unwrap();
    assert_eq!(state::Shed::get_status(), 0);
  }

  #[test]
  fn test_regex_no_match() {
    let _g = TestGuard::new();
    test_input("[[ goodbye =~ ^hello ]]").unwrap();
    assert_ne!(state::Shed::get_status(), 0);
  }

  // ===================== Conjuncts =====================

  #[test]
  fn test_and_both_true() {
    let _g = TestGuard::new();
    test_input("[[ -n hello && -n world ]]").unwrap();
    assert_eq!(state::Shed::get_status(), 0);
  }

  #[test]
  fn no_glob_expansion() {
    let _g = TestGuard::new();
    test_input("[[ 'hello*' == hello* ]]").unwrap();
    assert_eq!(state::Shed::get_status(), 0);
  }

  #[test]
  fn test_and_first_false() {
    let _g = TestGuard::new();
    test_input("[[ -z hello && -n world ]]").unwrap();
    assert_ne!(state::Shed::get_status(), 0);
  }

  #[test]
  fn test_or_first_true() {
    let _g = TestGuard::new();
    test_input("[[ -n hello || -z hello ]]").unwrap();
    assert_eq!(state::Shed::get_status(), 0);
  }

  #[test]
  fn test_or_both_false() {
    let _g = TestGuard::new();
    test_input("[[ -z hello || -z world ]]").unwrap();
    assert_ne!(state::Shed::get_status(), 0);
  }

  // ===================== Pure: operator parsing =====================

  #[test]
  fn parse_unary_ops() {
    use super::UnaryOp;
    use std::str::FromStr;
    for op in [
      "-e", "-d", "-f", "-h", "-L", "-r", "-w", "-x", "-s", "-p", "-S", "-b", "-c", "-k", "-O",
      "-G", "-N", "-u", "-g", "-t", "-n", "-z",
    ] {
      assert!(UnaryOp::from_str(op).is_ok(), "failed to parse {op}");
    }
  }

  #[test]
  fn parse_invalid_unary_op() {
    use super::UnaryOp;
    use std::str::FromStr;
    assert!(UnaryOp::from_str("-Q").is_err());
  }

  #[test]
  fn parse_binary_ops() {
    use super::TestOp;
    use std::str::FromStr;
    for op in ["==", "!=", "=~", "-eq", "-ne", "-gt", "-lt", "-ge", "-le"] {
      assert!(TestOp::from_str(op).is_ok(), "failed to parse {op}");
    }
  }

  #[test]
  fn parse_invalid_binary_op() {
    use super::TestOp;
    use std::str::FromStr;
    assert!(TestOp::from_str("~=").is_err());
  }

  // ─── Symlink (-h / -L) ──────────────────────────────────────────────

  #[test]
  fn test_symlink_true() {
    let _g = TestGuard::new();
    let dir = TempDir::new().unwrap();
    let target = dir.path().join("target");
    fs::write(&target, b"hi").unwrap();
    let link = dir.path().join("link");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    test_input(format!("[[ -h {} ]]", link.display())).unwrap();
    assert_eq!(state::Shed::get_status(), 0);
  }

  #[test]
  fn test_symlink_false_on_regular_file() {
    let _g = TestGuard::new();
    let file = NamedTempFile::new().unwrap();
    test_input(format!("[[ -h {} ]]", file.path().display())).unwrap();
    assert_ne!(state::Shed::get_status(), 0);
  }

  #[test]
  fn test_symlink_capital_l_alias() {
    let _g = TestGuard::new();
    let dir = TempDir::new().unwrap();
    let target = dir.path().join("target");
    fs::write(&target, b"hi").unwrap();
    let link = dir.path().join("link");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    test_input(format!("[[ -L {} ]]", link.display())).unwrap();
    assert_eq!(state::Shed::get_status(), 0);
  }

  // ─── Executable (-x) ────────────────────────────────────────────────

  #[test]
  fn test_executable_true() {
    use std::os::unix::fs::PermissionsExt;
    let _g = TestGuard::new();
    let file = NamedTempFile::new().unwrap();
    let mut perms = fs::metadata(file.path()).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(file.path(), perms).unwrap();
    test_input(format!("[[ -x {} ]]", file.path().display())).unwrap();
    assert_eq!(state::Shed::get_status(), 0);
  }

  #[test]
  fn test_executable_false() {
    use std::os::unix::fs::PermissionsExt;
    let _g = TestGuard::new();
    let file = NamedTempFile::new().unwrap();
    let mut perms = fs::metadata(file.path()).unwrap().permissions();
    perms.set_mode(0o644);
    fs::set_permissions(file.path(), perms).unwrap();
    test_input(format!("[[ -x {} ]]", file.path().display())).unwrap();
    assert_ne!(state::Shed::get_status(), 0);
  }

  // ─── NamedPipe (-p) ─────────────────────────────────────────────────

  #[test]
  fn test_named_pipe_true() {
    let _g = TestGuard::new();
    let dir = TempDir::new().unwrap();
    let fifo = dir.path().join("myfifo");
    nix::unistd::mkfifo(&fifo, nix::sys::stat::Mode::S_IRWXU).unwrap();
    test_input(format!("[[ -p {} ]]", fifo.display())).unwrap();
    assert_eq!(state::Shed::get_status(), 0);
  }

  #[test]
  fn test_named_pipe_false_on_regular_file() {
    let _g = TestGuard::new();
    let file = NamedTempFile::new().unwrap();
    test_input(format!("[[ -p {} ]]", file.path().display())).unwrap();
    assert_ne!(state::Shed::get_status(), 0);
  }

  // ─── Socket (-S) ────────────────────────────────────────────────────

  #[test]
  fn test_socket_true() {
    let _g = TestGuard::new();
    let dir = TempDir::new().unwrap();
    let sock_path = dir.path().join("test.sock");
    let _listener = std::os::unix::net::UnixListener::bind(&sock_path).unwrap();
    test_input(format!("[[ -S {} ]]", sock_path.display())).unwrap();
    assert_eq!(state::Shed::get_status(), 0);
  }

  #[test]
  fn test_socket_false_on_regular_file() {
    let _g = TestGuard::new();
    let file = NamedTempFile::new().unwrap();
    test_input(format!("[[ -S {} ]]", file.path().display())).unwrap();
    assert_ne!(state::Shed::get_status(), 0);
  }

  // ─── BlockSpecial (-b) / CharSpecial (-c) ───────────────────────────

  #[test]
  fn test_block_special_false_on_regular_file() {
    let _g = TestGuard::new();
    let file = NamedTempFile::new().unwrap();
    test_input(format!("[[ -b {} ]]", file.path().display())).unwrap();
    assert_ne!(state::Shed::get_status(), 0);
  }

  #[test]
  fn test_char_special_true_dev_null() {
    let _g = TestGuard::new();
    // /dev/null is a character special device on every reasonable unix.
    test_input("[[ -c /dev/null ]]").unwrap();
    assert_eq!(state::Shed::get_status(), 0);
  }

  #[test]
  fn test_char_special_false_on_regular_file() {
    let _g = TestGuard::new();
    let file = NamedTempFile::new().unwrap();
    test_input(format!("[[ -c {} ]]", file.path().display())).unwrap();
    assert_ne!(state::Shed::get_status(), 0);
  }

  // ─── Sticky (-k) ────────────────────────────────────────────────────

  #[test]
  fn test_sticky_true_tmpdir() {
    let _g = TestGuard::new();
    // /tmp has the sticky bit set on every standard Linux setup.
    test_input("[[ -k /tmp ]]").unwrap();
    assert_eq!(state::Shed::get_status(), 0);
  }

  #[test]
  fn test_sticky_false_on_regular_file() {
    let _g = TestGuard::new();
    let file = NamedTempFile::new().unwrap();
    test_input(format!("[[ -k {} ]]", file.path().display())).unwrap();
    assert_ne!(state::Shed::get_status(), 0);
  }

  // ─── UIDOwner (-O) / GIDOwner (-G) ──────────────────────────────────

  #[test]
  fn test_uid_owner_true_on_self_created_file() {
    let _g = TestGuard::new();
    let file = NamedTempFile::new().unwrap();
    test_input(format!("[[ -O {} ]]", file.path().display())).unwrap();
    assert_eq!(state::Shed::get_status(), 0);
  }

  #[test]
  fn test_gid_owner_true_on_self_created_file() {
    let _g = TestGuard::new();
    let file = NamedTempFile::new().unwrap();
    test_input(format!("[[ -G {} ]]", file.path().display())).unwrap();
    assert_eq!(state::Shed::get_status(), 0);
  }

  #[test]
  fn test_uid_owner_false_on_nonexistent_path() {
    let _g = TestGuard::new();
    test_input("[[ -O /__nope_xyz_123 ]]").unwrap();
    assert_ne!(state::Shed::get_status(), 0);
  }

  // ─── SetUID (-u) / SetGID (-g) ──────────────────────────────────────

  #[test]
  fn test_setuid_false_on_regular_file() {
    let _g = TestGuard::new();
    let file = NamedTempFile::new().unwrap();
    test_input(format!("[[ -u {} ]]", file.path().display())).unwrap();
    assert_ne!(state::Shed::get_status(), 0);
  }

  #[test]
  fn test_setgid_false_on_regular_file() {
    let _g = TestGuard::new();
    let file = NamedTempFile::new().unwrap();
    test_input(format!("[[ -g {} ]]", file.path().display())).unwrap();
    assert_ne!(state::Shed::get_status(), 0);
  }

  // ─── ModifiedSinceStatusChange (-N) ─────────────────────────────────

  #[test]
  fn test_modified_since_ctime_false_on_fresh_file() {
    let _g = TestGuard::new();
    let file = NamedTempFile::new().unwrap();
    // A brand-new file has mtime == ctime, so mtime > ctime is false.
    test_input(format!("[[ -N {} ]]", file.path().display())).unwrap();
    assert_ne!(state::Shed::get_status(), 0);
  }

  // ─── Terminal (-t) ──────────────────────────────────────────────────

  #[test]
  fn test_terminal_false_on_pipe_stdin() {
    let _g = TestGuard::new();
    // TestGuard redirects stdin to a pipe, not a tty.
    test_input("[[ -t 0 ]]").unwrap();
    assert_ne!(state::Shed::get_status(), 0);
  }

  #[test]
  fn test_terminal_true_on_pty_stdout() {
    let _g = TestGuard::new();
    // TestGuard redirects stdout to the pty slave, which is a real tty.
    test_input("[[ -t 1 ]]").unwrap();
    assert_eq!(state::Shed::get_status(), 0);
  }

  #[test]
  fn test_terminal_non_numeric_operand_is_false() {
    let _g = TestGuard::new();
    test_input("[[ -t notanumber ]]").unwrap();
    assert_ne!(state::Shed::get_status(), 0);
  }

  // ─── Conjunction short-circuit ──────────────────────────────────────

  #[test]
  fn test_and_short_circuits_on_first_false() {
    // The second operand here would error if evaluated (bad integer),
    // but `&&` short-circuits.
    let _g = TestGuard::new();
    test_input("[[ -z nonempty && abc -eq 5 ]]").unwrap();
    // First clause is false (nonempty isn't empty), and short-circuit
    // means the bad-int clause is never reached → no error, status nonzero.
    assert_ne!(state::Shed::get_status(), 0);
  }

  #[test]
  fn test_or_short_circuits_on_first_true() {
    let _g = TestGuard::new();
    test_input("[[ -n nonempty || abc -eq 5 ]]").unwrap();
    assert_eq!(state::Shed::get_status(), 0);
  }

  // ─── Misc errors ────────────────────────────────────────────────────

  #[test]
  fn test_regex_invalid_pattern_errors() {
    let _g = TestGuard::new();
    // Unclosed character class — invalid regex. The handler returns
    // Err(SyntaxErr) which test_input surfaces as an Err.
    let result = test_input(r#"[[ abc =~ "[" ]]"#);
    assert!(
      result.is_err(),
      "invalid regex pattern should propagate an error"
    );
  }
}
