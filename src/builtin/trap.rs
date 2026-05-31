use super::{
  Shed, errln,
  expand::as_var_val_display,
  outln,
  state::logic::TrapTarget,
  util::{ShResult, ShResultExt, with_status},
};

pub(super) struct Trap;
impl super::Builtin for Trap {
  fn is_special(&self) -> bool {
    true
  }

  fn execute(&self, args: super::BuiltinArgs) -> ShResult<()> {
    if args.argv.is_empty() {
      Shed::logic(|l| -> ShResult<()> {
        for l in l.traps() {
          let target = l.0;
          let command = as_var_val_display(l.1);
          outln!("trap -- {command} {target}");
        }
        Ok(())
      })?;
      return with_status(0);
    } else if args.argv.len() == 1 {
      errln!("usage: trap <COMMAND> [SIGNAL...]");
      return with_status(1);
    }

    let mut arg_vec = args.argv.into_iter();
    let command = arg_vec.next().unwrap().0;
    let mut targets = vec![];

    for (arg, span) in arg_vec {
      let target = arg.parse::<TrapTarget>().promote_err(span)?;
      targets.push(target);
    }

    for target in targets {
      if &command == "-" {
        Shed::logic_mut(|l| l.remove_trap(target));
      } else {
        Shed::logic_mut(|l| l.insert_trap(target, command.clone()));
      }
    }

    with_status(0)
  }
}

#[cfg(test)]
mod tests {
  use crate::state::logic::TrapTarget;
  use crate::state::{self, Shed};
  use crate::tests::testutil::{TestGuard, test_input};
  use nix::sys::signal::Signal;
  use std::str::FromStr;

  // ===================== Pure: TrapTarget parsing =====================

  #[test]
  fn parse_exit() {
    assert_eq!(TrapTarget::from_str("EXIT").unwrap(), TrapTarget::Exit);
  }

  #[test]
  fn parse_err() {
    assert_eq!(TrapTarget::from_str("ERR").unwrap(), TrapTarget::Error);
  }

  #[test]
  fn parse_signal_int() {
    assert_eq!(
      TrapTarget::from_str("INT").unwrap(),
      TrapTarget::Signal(Signal::SIGINT)
    );
  }

  #[test]
  fn parse_signal_term() {
    assert_eq!(
      TrapTarget::from_str("TERM").unwrap(),
      TrapTarget::Signal(Signal::SIGTERM)
    );
  }

  #[test]
  fn parse_signal_usr1() {
    assert_eq!(
      TrapTarget::from_str("USR1").unwrap(),
      TrapTarget::Signal(Signal::SIGUSR1)
    );
  }

  #[test]
  fn parse_invalid() {
    assert!(TrapTarget::from_str("BOGUS").is_err());
  }

  // ===================== Pure: Display round-trip =====================

  #[test]
  fn display_exit() {
    assert_eq!(TrapTarget::Exit.to_string(), "EXIT");
  }

  #[test]
  fn display_err() {
    assert_eq!(TrapTarget::Error.to_string(), "ERR");
  }

  #[test]
  fn display_signal_roundtrip() {
    for name in &[
      "INT", "QUIT", "TERM", "USR1", "USR2", "ALRM", "CHLD", "WINCH",
    ] {
      let target = TrapTarget::from_str(name).unwrap();
      assert_eq!(target.to_string(), *name);
    }
  }

  // ===================== Integration: registration =====================

  #[test]
  fn trap_registers_exit() {
    let _g = TestGuard::new();
    test_input("trap 'echo bye' EXIT").unwrap();
    let cmd = Shed::logic(|l| l.get_trap(TrapTarget::Exit));
    assert_eq!(cmd.unwrap(), "echo bye");
  }

  #[test]
  fn trap_registers_signal() {
    let _g = TestGuard::new();
    test_input("trap 'echo caught' INT").unwrap();
    let cmd = Shed::logic(|l| l.get_trap(TrapTarget::Signal(Signal::SIGINT)));
    assert_eq!(cmd.unwrap(), "echo caught");
  }

  #[test]
  fn trap_multiple_signals() {
    let _g = TestGuard::new();
    test_input("trap 'handle' INT TERM").unwrap();
    let int = Shed::logic(|l| l.get_trap(TrapTarget::Signal(Signal::SIGINT)));
    let term = Shed::logic(|l| l.get_trap(TrapTarget::Signal(Signal::SIGTERM)));
    assert_eq!(int.unwrap(), "handle");
    assert_eq!(term.unwrap(), "handle");
  }

  #[test]
  fn trap_remove() {
    let _g = TestGuard::new();
    test_input("trap 'echo hi' EXIT").unwrap();
    assert!(Shed::logic(|l| l.get_trap(TrapTarget::Exit)).is_some());
    test_input("trap - EXIT").unwrap();
    assert!(Shed::logic(|l| l.get_trap(TrapTarget::Exit)).is_none());
  }

  #[test]
  fn trap_display() {
    let guard = TestGuard::new();
    test_input("trap 'echo bye' EXIT").unwrap();
    test_input("trap").unwrap();
    let out = guard.read_output();
    assert!(out.contains("echo bye"));
    assert!(out.contains("EXIT"));
  }

  // ===================== Error cases =====================

  #[test]
  fn trap_single_arg_usage() {
    let _g = TestGuard::new();
    // Single arg prints usage and sets status 1
    test_input("trap 'echo hi'").unwrap();
    assert_eq!(state::Shed::get_status(), 1);
  }

  #[test]
  fn trap_invalid_signal() {
    let _g = TestGuard::new();
    test_input("trap 'echo hi' BOGUS").ok();
    assert_ne!(state::Shed::get_status(), 0);
  }

  // ===================== Status =====================

  #[test]
  fn trap_status_zero() {
    let _g = TestGuard::new();
    test_input("trap 'echo bye' EXIT").unwrap();
    assert_eq!(state::Shed::get_status(), 0);
  }
}
