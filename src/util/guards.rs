use std::collections::HashSet;

use scopeguard::guard;

use crate::parse::execute::exec_nonint;
use crate::parse::lex::Span;
use crate::state::write_vars;

// ============================================================================
// ScopeGuard - RAII variable scope management
// ============================================================================

fn guard_drop(_: ()) {
  let mut deferred = write_vars(|v| v.cur_scope_mut().take_deferred_cmds());

  while let Some(cmd) = deferred.pop() {
    if let Err(e) = exec_nonint(cmd, Some("defer".into())) {
      e.print_error();
    }
  }

  write_vars(|v| v.ascend());
}

/// Descend into a new variable scope, with a new argv that shadows the previous one.
///
/// The `local` builtin uses this scope to store its variables.
/// The `defer` builtin registers commands to run when this drops.
pub fn scope_guard(args: Option<Vec<(String, Span)>>) -> impl Drop {
  let argv = args.map(|a| a.into_iter().map(|(s, _)| s).collect::<Vec<_>>());
  write_vars(|v| v.descend(argv));
  guard((), guard_drop)
}

/// Descend into a new variable scope.
///
/// The `local` builtin uses this scope to store its variables.
/// The `defer` builtin registers commands to run when this drops.
pub fn shared_scope_guard() -> impl Drop {
  write_vars(|v| v.descend(None));
  guard((), guard_drop)
}

// ============================================================================
// VarCtxGuard - RAII variable context cleanup
// ============================================================================

pub fn var_ctx_guard(
  vars: HashSet<String>,
) -> scopeguard::ScopeGuard<HashSet<String>, impl FnOnce(HashSet<String>)> {
  guard(vars, |vars| {
    write_vars(|v| {
      for var in &vars {
        v.unset_var(var).ok();
      }
    });
  })
}

// ============================================================================
// RedirGuard - RAII I/O redirection restoration
// ============================================================================
