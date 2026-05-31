use std::collections::HashSet;

use scopeguard::guard;

use super::{
  super::state::scopes::ScopeStack,
  Shed,
  eval::{execute::exec_nonint, lex::Span},
};

// ============================================================================
// ScopeGuard - RAII variable scope management
// ============================================================================

/// Execute commands registered by `defer`
/// Drop variables registered by `local`
fn guard_drop(_: ()) {
  let mut deferred = Shed::vars_mut(|v| v.cur_scope_mut().take_deferred_cmds());

  while let Some(cmd) = deferred.pop() {
    if let Err(e) = exec_nonint(cmd, Some("defer".into())) {
      e.print_error();
    }
  }

  Shed::vars_mut(ScopeStack::ascend);
}

/// Descend into a new variable scope, with a new argv that shadows the previous one.
///
/// The `local` builtin uses this scope to store its variables.
/// The `defer` builtin registers commands to run when this drops.
pub fn scope_guard(args: Option<Vec<(String, Span)>>) -> impl Drop {
  let arg_vec = args.map(|a| a.into_iter().map(|(s, _)| s).collect::<Vec<_>>());
  Shed::vars_mut(|v| v.descend(arg_vec));
  guard((), guard_drop)
}

/// Descend into a new variable scope, without using a new argv
/// This is used for stuff like brace groups,
///
/// The `local` builtin uses this scope to store its variables.
/// The `defer` builtin registers commands to run when this drops.
pub fn shared_scope_guard() -> impl Drop {
  Shed::vars_mut(|v| v.descend(None));
  guard((), guard_drop)
}

// ============================================================================
// VarCtxGuard - RAII variable context cleanup
// ============================================================================

pub fn var_ctx_guard(
  vars: HashSet<String>,
) -> scopeguard::ScopeGuard<HashSet<String>, impl FnOnce(HashSet<String>)> {
  guard(vars, |vars| {
    Shed::vars_mut(|v| {
      for var in &vars {
        v.unset_var(var).ok();
      }
    });
  })
}

// ============================================================================
// RedirGuard - RAII I/O redirection restoration
// ============================================================================
