use ariadne::Span;

use crate::{
  getopt::{Opt, OptSpec},
  outln,
  parse::lex::KEYWORDS,
  sherr,
  state::{self, Shed},
  util::{ShResult, with_status},
};

pub(super) struct Type;
impl super::Builtin for Type {
  fn opts(&self) -> Vec<crate::getopt::OptSpec> {
    vec![OptSpec::flag('s')]
  }
  fn execute(&self, args: super::BuiltinArgs) -> ShResult<()> {
    let mut status = 0;
    let short = args.opts.contains(&Opt::Short('s'));

    for (arg, span) in args.argv {
      if let Some(util) = state::util::which_util(&arg) {
        match util.kind() {
          state::meta::UtilKind::Alias => {
            let alias = Shed::logic(|v| v.get_alias(&arg)).unwrap();
            let (line, col) = alias.source().line_and_col();
            let name = alias.source().source().name();
            if short {
              outln!("alias");
            } else {
              outln!(
                "{arg} is an alias for '{alias_body}' defined at {name}:{ln}:{co}",
                ln = line + 1,
                co = col + 1,
                alias_body = alias.body(),
              );
            }
          }
          state::meta::UtilKind::Function => {
            let func = Shed::logic(|v| v.get_func(&arg)).unwrap();
            let (line, col) = func.source.line_and_col();
            let name = func.source.source().name();
            if short {
              outln!("function");
            } else {
              outln!(
                "{arg} is a function defined at {name}:{ln}:{co}",
                ln = line + 1,
                co = col + 1,
                name = name,
              );
            }
          }
          state::meta::UtilKind::Builtin => {
            if short {
              outln!("builtin");
            } else {
              outln!("{arg} is a shell builtin");
            }
          }
          state::meta::UtilKind::Command(path_buf) | state::meta::UtilKind::File(path_buf) => {
            if short {
              outln!("external");
            } else {
              outln!("{arg} is {}", path_buf.display());
            }
          }
        };
      } else if KEYWORDS.contains(&arg.as_str()) {
        if short {
          outln!("keyword");
        } else {
          outln!("{arg} is a shell keyword");
        }
      } else if let Some(var) = Shed::vars(|v| v.try_get_var_meta(arg.as_str())) {
        if short {
          match var.kind() {
            state::vars::VarKind::Str(_) => outln!("string"),
            state::vars::VarKind::Int(_) => outln!("integer"),
            state::vars::VarKind::Arr(_) => outln!("array"),
            state::vars::VarKind::AssocArr(_) => outln!("assoc_array"),
          }
        } else {
          match var.kind() {
            state::vars::VarKind::Str(_) => outln!("{arg} is a string variable"),
            state::vars::VarKind::Int(_) => outln!("{arg} is an integer variable"),
            state::vars::VarKind::Arr(_) => outln!("{arg} is an array variable"),
            state::vars::VarKind::AssocArr(_) => outln!("{arg} is an associative array"),
          }
        }
      } else {
        sherr!(
          NotFound @ span,
          "'{arg}' is not a command, function, or alias",
        )
        .print_error();

        status = 1;
      }
    }

    with_status(status)
  }
}

#[cfg(test)]
mod tests {
  use crate::state::{self};
  use crate::tests::testutil::{TestGuard, test_input};

  // ===================== Builtins =====================

  #[test]
  fn type_builtin_echo() {
    let guard = TestGuard::new();
    test_input("type echo").unwrap();
    let out = guard.read_output();
    assert!(out.contains("echo"));
    assert!(
      out.contains("shell builtin"),
      "Expected 'shell builtin' in output, got: {out}"
    );
  }

  #[test]
  fn type_builtin_cd() {
    let guard = TestGuard::new();
    test_input("type cd").unwrap();
    let out = guard.read_output();
    assert!(out.contains("cd"));
    assert!(out.contains("shell builtin"));
  }

  // ===================== Keywords =====================

  #[test]
  fn type_keyword_if() {
    let guard = TestGuard::new();
    test_input("type if").unwrap();
    let out = guard.read_output();
    assert!(out.contains("if"));
    assert!(out.contains("shell keyword"));
  }

  #[test]
  fn type_keyword_for() {
    let guard = TestGuard::new();
    test_input("type for").unwrap();
    let out = guard.read_output();
    assert!(out.contains("for"));
    assert!(out.contains("shell keyword"));
  }

  // ===================== Functions =====================

  #[test]
  fn type_function() {
    let guard = TestGuard::new();
    test_input("myfn() { echo hi; }").unwrap();
    guard.read_output();

    test_input("type myfn").unwrap();
    let out = guard.read_output();
    assert!(out.contains("myfn"));
    assert!(out.contains("function"));
  }

  // ===================== Aliases =====================

  #[test]
  fn type_alias() {
    let guard = TestGuard::new();
    test_input("alias ll='ls -la'").unwrap();
    guard.read_output();

    test_input("type ll").unwrap();
    let out = guard.read_output();
    assert!(out.contains("ll"));
    assert!(out.contains("alias"));
    assert!(out.contains("ls -la"));
  }

  // ===================== External commands =====================

  #[test]
  fn type_external_command() {
    let guard = TestGuard::new();
    // /bin/cat or /usr/bin/cat should exist on any Unix system
    test_input("type cat").unwrap();
    let out = guard.read_output();
    assert!(out.contains("cat"));
    assert!(out.contains("is"));
    assert!(out.contains("/")); // Should show a path
  }

  // ===================== Not found =====================

  #[test]
  fn type_not_found() {
    let _g = TestGuard::new();
    let result = test_input("type __hopefully____not_______a____command__");
    assert!(result.is_ok());
    assert_eq!(state::Shed::get_status(), 1);
  }

  // ===================== Priority order =====================

  #[test]
  fn type_function_shadows_builtin() {
    let guard = TestGuard::new();
    // Define a function named 'echo' - should shadow the builtin
    test_input("echo() { true; }").unwrap();
    guard.read_output();

    test_input("type echo").unwrap();
    let out = guard.read_output();
    assert!(out.contains("function"));
  }

  #[test]
  fn type_alias_shadows_external() {
    let guard = TestGuard::new();
    test_input("alias cat='echo meow'").unwrap();
    guard.read_output();

    test_input("type cat").unwrap();
    let out = guard.read_output();
    // alias check comes before external PATH scan
    assert!(out.contains("alias"));
  }

  // ===================== Status =====================

  #[test]
  fn type_status_zero_on_found() {
    let _g = TestGuard::new();
    test_input("type echo").unwrap();
    assert_eq!(state::Shed::get_status(), 0);
  }
}
