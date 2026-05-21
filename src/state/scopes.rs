use std::{
  collections::{HashMap, VecDeque, hash_map::Entry},
  time::{Duration, Instant},
};

use super::{
  ShResult, Shed, sherr,
  vars::{ArrIndex, ShellParam, Var, VarFlags, VarKind, VarName, VarTab},
};

#[derive(Clone, Default, Debug)]
pub struct ScopeStack {
  // ALWAYS keep one scope.
  // The bottom scope is the global variable space.
  // Scopes that come after that are pushed in functions,
  // and only contain variables that are defined using `local`.
  scopes: Vec<VarTab>,
  depth: u32,

  // Global parameters such as $!, $$, etc
  global_params: HashMap<ShellParam, String>,
}

impl ScopeStack {
  pub fn new() -> Self {
    let mut new = Self::default();
    new.scopes.push(VarTab::new());
    let shell_name = std::env::args()
      .next()
      .unwrap_or_else(|| "shed".to_string());
    new.global_params.insert(ShellParam::ShellName, shell_name);
    new
  }
  pub fn descend(&mut self, argv: Option<Vec<String>>) {
    let mut new_vars = VarTab::bare();
    if let Some(argv) = argv {
      for arg in argv {
        new_vars.bpush_arg(arg);
      }
    }
    self.scopes.push(new_vars);
    self.depth += 1;
  }
  pub fn ascend(&mut self) {
    if self.depth >= 1 {
      self.scopes.pop();
      self.depth -= 1;
    }
  }
  pub fn depth(&self) -> u32 {
    self.depth
  }
  pub fn cur_scope(&self) -> &VarTab {
    self.scopes.last().unwrap()
  }
  pub fn cur_scope_mut(&mut self) -> &mut VarTab {
    self.scopes.last_mut().unwrap()
  }
  pub fn sh_argv(&self) -> &VecDeque<String> {
    for scope in self.scopes.iter().rev() {
      let argv = scope.sh_argv();
      if !argv.is_empty() {
        return argv;
      }
    }

    self.cur_scope().sh_argv()
  }
  pub fn sh_argv_scope_mut(&mut self) -> &mut VarTab {
    let idx = self
      .scopes
      .iter()
      .rposition(|s| !s.sh_argv().is_empty())
      .unwrap_or(0);
    self.scopes.get_mut(idx).unwrap()
  }
  pub fn sh_argv_scope(&self) -> &VarTab {
    let idx = self
      .scopes
      .iter()
      .rposition(|s| !s.sh_argv().is_empty())
      .unwrap_or(0);
    self.scopes.get(idx).unwrap()
  }
  pub fn unset_var(&mut self, var_name: &str) -> ShResult<()> {
    for scope in self.scopes.iter_mut().rev() {
      if scope.var_exists(var_name) {
        return scope.unset_var(var_name);
      }
    }
    Err(sherr!(ExecFail, "Variable '{}' not found", var_name,))
  }
  pub fn export_var(&mut self, var_name: &str) {
    for scope in self.scopes.iter_mut().rev() {
      if scope.var_exists(var_name) {
        scope.export_var(var_name);
        return;
      }
    }
  }
  pub fn var_exists(&self, var_name: &str) -> bool {
    for scope in self.scopes.iter().rev() {
      if scope.var_exists(var_name) {
        return true;
      }
    }
    if let Ok(param) = var_name.parse::<ShellParam>() {
      return self.global_params.contains_key(&param);
    }
    false
  }
  pub fn flatten_vars(&self) -> HashMap<String, Var> {
    let mut flat_vars = HashMap::new();
    for scope in self.scopes.iter() {
      for (var_name, var) in scope.vars() {
        flat_vars.insert(var_name.clone(), var.clone());
      }
    }
    for var in std::env::vars() {
      if let Entry::Vacant(e) = flat_vars.entry(var.0) {
        e.insert(Var::new(VarKind::Str(var.1), VarFlags::EXPORT));
      }
    }

    flat_vars
  }
  pub fn set_var(&mut self, var_name: &str, val: VarKind, flags: VarFlags) -> ShResult<()> {
    if flags.contains(VarFlags::LOCAL) {
      return self.set_var_local(var_name, val, flags);
    }
    // Dynamic scoping: walk scopes from innermost to outermost,
    // update the nearest scope that already has this variable
    for scope in self.scopes.iter_mut().rev() {
      if scope.var_exists(var_name) {
        return scope.set_var(var_name, val, flags);
      }
    }
    // Not found in any scope - create in global scope
    self.set_var_global(var_name, val, flags)
  }

  /// Mutate the value of an existing variable in place, finding it in the
  /// nearest scope that owns it and preserving its existing flags. Falls
  /// back to creating a new global if the name is unbound. Use this for
  /// compound-assignment paths (`arr+=`, `n+=1`, etc.) where the var is
  /// being updated rather than declared — naive `set_var` with a recovered
  /// LOCAL flag would shadow the original in whatever scope happens to be
  /// current (e.g. a `for`-loop body).
  pub fn update_var(&mut self, var_name: &str, val: VarKind) -> ShResult<()> {
    for scope in self.scopes.iter_mut().rev() {
      if scope.var_exists(var_name) {
        return scope.set_var(var_name, val, VarFlags::empty());
      }
    }
    self.set_var_global(var_name, val, VarFlags::empty())
  }

  /// Indexed counterpart to `update_var`: writes a single element of an
  /// existing array, in the scope that owns the array. Falls back to
  /// creating in global scope if no binding exists.
  pub fn update_var_indexed(&mut self, var_name: &str, idx: ArrIndex, val: String) -> ShResult<()> {
    for scope in self.scopes.iter_mut().rev() {
      if scope.var_exists(var_name) {
        return scope.set_index(var_name, idx, val);
      }
    }
    let Some(scope) = self.scopes.first_mut() else {
      return Ok(());
    };
    scope.set_index(var_name, idx, val)
  }
  pub fn set_var_indexed(
    &mut self,
    var_name: &str,
    idx: ArrIndex,
    val: String,
    flags: VarFlags,
  ) -> ShResult<()> {
    if flags.contains(VarFlags::LOCAL) {
      let Some(scope) = self.scopes.last_mut() else {
        return Ok(());
      };
      return scope.set_index(var_name, idx, val);
    }
    // Dynamic scoping: find nearest scope with this variable
    for scope in self.scopes.iter_mut().rev() {
      if scope.var_exists(var_name) {
        return scope.set_index(var_name, idx, val);
      }
    }
    // Not found - create in global scope
    let Some(scope) = self.scopes.first_mut() else {
      return Ok(());
    };
    scope.set_index(var_name, idx, val)
  }
  fn set_var_global(&mut self, var_name: &str, val: VarKind, flags: VarFlags) -> ShResult<()> {
    let Some(scope) = self.scopes.first_mut() else {
      return Ok(());
    };
    scope.set_var(var_name, val, flags)
  }
  fn set_var_local(&mut self, var_name: &str, val: VarKind, flags: VarFlags) -> ShResult<()> {
    let Some(scope) = self.scopes.last_mut() else {
      return Ok(());
    };
    scope.set_var(var_name, val, flags)
  }
  pub fn get_magic_var(&self, var_name: &str) -> Option<String> {
    match var_name {
      "SECONDS" => {
        let shell_time = Shed::meta(|m| m.shell_time());
        let secs = Instant::now().duration_since(shell_time).as_secs();
        Some(secs.to_string())
      }
      "EPOCHREALTIME" => {
        let epoch = std::time::SystemTime::now()
          .duration_since(std::time::UNIX_EPOCH)
          .unwrap_or(Duration::from_secs(0))
          .as_secs_f64();
        Some(epoch.to_string())
      }
      "EPOCHSECONDS" => {
        let epoch = std::time::SystemTime::now()
          .duration_since(std::time::UNIX_EPOCH)
          .unwrap_or(Duration::from_secs(0))
          .as_secs();
        Some(epoch.to_string())
      }
      "RANDOM" => {
        let random = rand::random_range(0..32768);
        Some(random.to_string())
      }
      "?" => Some(Shed::get_status().to_string()),
      "-" => {
        let mut set_string = String::new();
        Shed::shopts(|o| {
          if o.set.allexport {
            set_string.push('a');
          }
          if o.set.notify {
            set_string.push('b');
          }
          if o.set.noclobber {
            set_string.push('C');
          }
          if o.set.errexit {
            set_string.push('e');
          }
          if o.set.noglob {
            set_string.push('f');
          }
          if o.set.hashall {
            set_string.push('h');
          }
          if Shed::term(|t| t.interactive()) {
            set_string.push('i');
          }
          if o.set.monitor {
            set_string.push('m');
          }
          if o.set.noexec {
            set_string.push('n');
          }
          if o.set.nounset {
            set_string.push('u');
          }
          if o.set.verbose {
            set_string.push('v');
          }
          if o.set.xtrace {
            set_string.push('x');
          }
        });
        (!set_string.is_empty()).then_some(set_string)
      }
      _ => None,
    }
  }
  pub fn try_get_arr_elems(&self, var_name: &str) -> ShResult<Vec<String>> {
    for scope in self.scopes.iter().rev() {
      if scope.var_exists(var_name)
        && let Some(var) = scope.vars().get(var_name)
      {
        match var.kind() {
          VarKind::Arr(items) => {
            return Ok(items.iter().cloned().collect());
          }
          _ => {
            return Err(sherr!(ExecFail, "Variable '{}' is not an array", var_name,));
          }
        }
      }
    }
    Err(sherr!(ExecFail, "Variable '{}' not found", var_name,))
  }
  pub fn get_arr_elems(&self, var_name: &str) -> Vec<String> {
    self.try_get_arr_elems(var_name).unwrap_or_default()
  }
  pub fn get_arr_mut(&mut self, var_name: &str) -> ShResult<&mut VecDeque<String>> {
    for scope in self.scopes.iter_mut().rev() {
      if scope.var_exists(var_name)
        && let Some(var) = scope.vars_mut().get_mut(var_name)
      {
        match var.kind_mut() {
          VarKind::Arr(items) => return Ok(items),
          _ => {
            return Err(sherr!(ExecFail, "Variable '{}' is not an array", var_name,));
          }
        }
      }
    }
    Err(sherr!(ExecFail, "Variable '{var_name}' not found"))
  }
  pub fn index_var(&self, var_name: &str, idx: ArrIndex) -> ShResult<String> {
    self.index_var_sliced(var_name, idx, None, None)
  }

  pub fn index_var_sliced(
    &self,
    var_name: &str,
    idx: ArrIndex,
    slice_start: Option<usize>,
    slice_len: Option<usize>,
  ) -> ShResult<String> {
    for scope in self.scopes.iter().rev() {
      if scope.var_exists(var_name)
        && let Some(var) = scope.vars().get(var_name)
      {
        let idx = idx.clone().resolve_for(var.kind())?;
        match var.kind() {
          VarKind::Arr(items) => {
            match idx {
              ArrIndex::AllSplit => {
                let arg_sep = crate::expand::markers::ARG_SEP.to_string();
                let start = slice_start.unwrap_or(0);
                let end = start + slice_len.unwrap_or(items.len().saturating_sub(start));
                let sliced = &items
                  .iter()
                  .skip(start)
                  .take(end - start)
                  .cloned()
                  .collect::<Vec<_>>();
                return Ok(sliced.join(&arg_sep));
              }
              ArrIndex::AllJoined => {
                let ifs = self
                  .try_get_var("IFS")
                  .unwrap_or_else(|| " \t\n".to_string())
                  .chars()
                  .next()
                  .unwrap_or(' ')
                  .to_string();
                let start = slice_start.unwrap_or(0);
                let end = start + slice_len.unwrap_or(items.len().saturating_sub(start));
                let sliced = &items
                  .iter()
                  .skip(start)
                  .take(end - start)
                  .cloned()
                  .collect::<Vec<_>>();
                return Ok(sliced.join(&ifs));
              }
              ArrIndex::ArgCount => {
                return Ok(items.len().to_string());
              }
              _ => {}
            }
            let idx = match idx {
              ArrIndex::Literal(n) => n,
              ArrIndex::FromBack(n) => {
                if items.len() >= n {
                  items.len() - n
                } else {
                  return Err(sherr!(
                    ExecFail,
                    "Index {} out of bounds for array '{}'",
                    n,
                    var_name,
                  ));
                }
              }
              _ => {
                return Err(sherr!(
                  ExecFail,
                  "Cannot index all elements of array '{}'",
                  var_name,
                ));
              }
            };

            if let Some(item) = items.get(idx) {
              return Ok(item.clone());
            } else {
              return Err(sherr!(
                ExecFail,
                "Index {} out of bounds for array '{}'",
                idx,
                var_name,
              ));
            }
          }
          VarKind::AssocArr(items) => match idx {
            ArrIndex::AllSplit => {
              let arg_sep = crate::expand::markers::ARG_SEP.to_string();
              let values: Vec<String> = items.iter().map(|(_, v)| v.clone()).collect();
              return Ok(values.join(&arg_sep));
            }
            ArrIndex::AllJoined => {
              let ifs = self
                .try_get_var("IFS")
                .unwrap_or_else(|| " \t\n".to_string())
                .chars()
                .next()
                .unwrap_or(' ')
                .to_string();
              let values: Vec<String> = items.iter().map(|(_, v)| v.clone()).collect();
              return Ok(values.join(&ifs));
            }
            ArrIndex::ArgCount => {
              return Ok(items.len().to_string());
            }
            ArrIndex::Key(key) => {
              for (k, v) in items {
                if k == &key {
                  return Ok(v.clone());
                }
              }
              return Ok(String::new());
            }
            _ => unreachable!("resolve_for guarantees Key/AllSplit/AllJoined/ArgCount on AssocArr"),
          },
          _ => {
            return Err(sherr!(ExecFail, "Variable '{}' is not an array", var_name,));
          }
        }
      }
    }
    Ok("".into())
  }

  pub fn get_array_keys(&self, var_name: &str, joined: bool) -> ShResult<String> {
    for scope in self.scopes.iter().rev() {
      if scope.var_exists(var_name)
        && let Some(var) = scope.vars().get(var_name)
      {
        match var.kind() {
          VarKind::Arr(items) => {
            let indices: Vec<String> = (0..items.len()).map(|i| i.to_string()).collect();
            let sep = if joined {
              self
                .try_get_var("IFS")
                .unwrap_or_else(|| " \t\n".to_string())
                .chars()
                .next()
                .unwrap_or(' ')
                .to_string()
            } else {
              crate::expand::markers::ARG_SEP.to_string()
            };
            return Ok(indices.join(&sep));
          }
          VarKind::AssocArr(items) => {
            let keys: Vec<String> = items.iter().map(|(k, _)| k.clone()).collect();
            let sep = if joined {
              self
                .try_get_var("IFS")
                .unwrap_or_else(|| " \t\n".to_string())
                .chars()
                .next()
                .unwrap_or(' ')
                .to_string()
            } else {
              crate::expand::markers::ARG_SEP.to_string()
            };
            return Ok(keys.join(&sep));
          }
          _ => {
            return Err(sherr!(ExecFail, "Variable '{}' is not an array", var_name,));
          }
        }
      }
    }
    Ok("".into())
  }

  pub fn try_get_var(&self, var_name: &str) -> Option<String> {
    if let Some(magic) = self.get_magic_var(var_name) {
      Some(magic)
    } else if let Ok(param) = var_name.parse::<ShellParam>() {
      let val = self.get_param(param);
      (!val.is_empty()).then_some(val)
    } else {
      for scope in self.scopes.iter().rev() {
        if scope.var_exists(var_name) {
          return Some(scope.get_var(var_name));
        }
      }

      None
    }
  }
  /// Resolve a pre-parsed VarName, handling array indexes and slicing if present.
  pub fn resolve_var(&self, var: &VarName) -> Option<String> {
    if let Some(idx) = var.index() {
      self
        .index_var_sliced(var.name(), idx.clone(), var.slice_start(), var.slice_len())
        .ok()
    } else {
      self.try_get_var(var.name())
    }
  }
  pub fn take_var(&mut self, var_name: &str) -> String {
    let var = self.get_var(var_name);
    self.unset_var(var_name).ok();
    var
  }
  pub fn get_var(&self, var_name: &str) -> String {
    self.try_get_var(var_name).unwrap_or_default()
  }
  pub fn get_var_meta(&self, var_name: &str) -> Var {
    self.try_get_var_meta(var_name).unwrap_or_default()
  }
  pub fn try_get_var_meta(&self, var_name: &str) -> Option<Var> {
    for scope in self.scopes.iter().rev() {
      if scope.var_exists(var_name) {
        return scope.try_get_var_meta(var_name);
      }
    }
    None
  }

  pub fn try_get_var_kind(&self, var_name: &str) -> Option<VarKind> {
    for scope in self.scopes.iter().rev() {
      if scope.var_exists(var_name)
        && let Some(var) = scope.vars().get(var_name)
      {
        return Some(var.kind().clone());
      }
    }
    None
  }
  pub fn all_vars(&self) -> HashMap<String, Var> {
    let mut vars = HashMap::new();
    for scope in self.scopes.iter() {
      for (k, v) in scope.vars() {
        vars.insert(k.to_string(), v.clone());
      }
    }
    vars
  }
  #[cfg(test)]
  pub fn get_var_flags(&self, var_name: &str) -> Option<VarFlags> {
    for scope in self.scopes.iter().rev() {
      if scope.var_exists(var_name) {
        return scope.get_var_flags(var_name);
      }
    }
    None
  }
  pub fn get_param(&self, param: ShellParam) -> String {
    if param.is_global()
      && let Some(val) = self.global_params.get(&param)
    {
      return val.clone();
    }
    // Positional params are scope-local; only check the current scope
    if matches!(
      param,
      ShellParam::Pos(_) | ShellParam::AllArgs | ShellParam::AllArgsStr | ShellParam::ArgCount
    ) {
      let scope = self.sh_argv_scope();
      return scope.get_param(param);
    }
    for scope in self.scopes.iter().rev() {
      let val = scope.get_param(param);
      if !val.is_empty() {
        return val;
      }
    }
    // Fallback to empty string
    "".into()
  }
  /// Set a shell parameter
  pub fn set_param(&mut self, param: ShellParam, val: &str) {
    match param {
      ShellParam::ShPid | ShellParam::Status | ShellParam::LastJob | ShellParam::ShellName => {
        self.global_params.insert(param, val.to_string());
      }
      ShellParam::Pos(_) | ShellParam::AllArgs | ShellParam::AllArgsStr | ShellParam::ArgCount => {
        let scope = self.sh_argv_scope_mut();
        scope.set_param(param, val);
      }
    }
  }
}

#[cfg(test)]
mod index_var_sliced_tests {
  use super::*;
  use crate::state::Shed;
  use crate::state::vars::{ArrIndex, VarFlags, VarKind};
  use crate::tests::testutil::TestGuard;
  use std::collections::VecDeque;

  fn set_arr(name: &str, items: &[&str]) {
    let vec: VecDeque<String> = items.iter().map(|s| s.to_string()).collect();
    Shed::vars_mut(|v| {
      v.set_var(name, VarKind::Arr(vec), VarFlags::empty())
        .unwrap();
    });
  }

  fn set_assoc(name: &str, pairs: &[(&str, &str)]) {
    let vec: Vec<(String, String)> = pairs
      .iter()
      .map(|(k, v)| (k.to_string(), v.to_string()))
      .collect();
    Shed::vars_mut(|v| {
      v.set_var(name, VarKind::AssocArr(vec), VarFlags::empty())
        .unwrap();
    });
  }

  fn set_str(name: &str, val: &str) {
    Shed::vars_mut(|v| {
      v.set_var(name, VarKind::Str(val.into()), VarFlags::empty())
        .unwrap();
    });
  }

  fn index(
    name: &str,
    idx: ArrIndex,
    slice_start: Option<usize>,
    slice_len: Option<usize>,
  ) -> ShResult<String> {
    Shed::vars(|v| v.index_var_sliced(name, idx, slice_start, slice_len))
  }

  // ─── Arr: positional index ────────────────────────────────────────

  #[test]
  fn arr_literal_index() {
    let _g = TestGuard::new();
    set_arr("arr", &["a", "b", "c"]);
    assert_eq!(index("arr", ArrIndex::Literal(1), None, None).unwrap(), "b");
  }

  #[test]
  fn arr_literal_out_of_bounds_errors() {
    let _g = TestGuard::new();
    set_arr("arr", &["a", "b"]);
    assert!(index("arr", ArrIndex::Literal(99), None, None).is_err());
  }

  #[test]
  fn arr_from_back_index() {
    let _g = TestGuard::new();
    set_arr("arr", &["a", "b", "c", "d"]);
    // FromBack(1) → items.len() - 1 = 3 → last element "d"
    assert_eq!(
      index("arr", ArrIndex::FromBack(1), None, None).unwrap(),
      "d"
    );
  }

  #[test]
  fn arr_from_back_overflows_errors() {
    let _g = TestGuard::new();
    set_arr("arr", &["a", "b"]);
    // FromBack(99) — items.len() (2) < 99 → ExecFail.
    assert!(index("arr", ArrIndex::FromBack(99), None, None).is_err());
  }

  // ─── Arr: ArgCount ────────────────────────────────────────────────

  #[test]
  fn arr_arg_count_returns_length() {
    let _g = TestGuard::new();
    set_arr("arr", &["x", "y", "z", "w"]);
    assert_eq!(index("arr", ArrIndex::ArgCount, None, None).unwrap(), "4");
  }

  #[test]
  fn arr_arg_count_on_empty_array() {
    let _g = TestGuard::new();
    set_arr("arr", &[]);
    assert_eq!(index("arr", ArrIndex::ArgCount, None, None).unwrap(), "0");
  }

  // ─── Arr: AllSplit / AllJoined ────────────────────────────────────

  #[test]
  fn arr_all_split_joins_with_arg_sep() {
    let _g = TestGuard::new();
    set_arr("arr", &["a", "b", "c"]);
    // ARG_SEP is the marker char that splits later. We just check that
    // all values appear.
    let result = index("arr", ArrIndex::AllSplit, None, None).unwrap();
    assert!(result.contains('a'));
    assert!(result.contains('b'));
    assert!(result.contains('c'));
  }

  #[test]
  fn arr_all_joined_uses_first_char_of_ifs() {
    let _g = TestGuard::new();
    set_str("IFS", ",xy");
    set_arr("arr", &["a", "b", "c"]);
    // First char of IFS is ',', so values join with ','.
    assert_eq!(
      index("arr", ArrIndex::AllJoined, None, None).unwrap(),
      "a,b,c"
    );
  }

  #[test]
  fn arr_all_joined_defaults_to_space_when_ifs_empty() {
    let _g = TestGuard::new();
    set_str("IFS", "");
    set_arr("arr", &["a", "b", "c"]);
    // Empty IFS → first-char is '\0'?  Actually IFS=""  → next().unwrap_or(' ').
    // Looking at code: `.chars().next().unwrap_or(' ')`. An empty string
    // yields None from next() so we get ' '.
    assert_eq!(
      index("arr", ArrIndex::AllJoined, None, None).unwrap(),
      "a b c"
    );
  }

  // ─── Arr: slicing ────────────────────────────────────────────────

  #[test]
  fn arr_all_split_with_slice_skips_and_takes() {
    let _g = TestGuard::new();
    set_arr("arr", &["a", "b", "c", "d", "e"]);
    // start=1, len=2 → ["b","c"]
    let result = index("arr", ArrIndex::AllSplit, Some(1), Some(2)).unwrap();
    assert!(result.contains('b'));
    assert!(result.contains('c'));
    assert!(!result.contains('a'));
    assert!(!result.contains('d'));
  }

  #[test]
  fn arr_all_joined_with_slice() {
    let _g = TestGuard::new();
    set_str("IFS", "-");
    set_arr("arr", &["a", "b", "c", "d", "e"]);
    assert_eq!(
      index("arr", ArrIndex::AllJoined, Some(2), Some(2)).unwrap(),
      "c-d"
    );
  }

  #[test]
  fn arr_all_joined_with_slice_start_only() {
    let _g = TestGuard::new();
    set_str("IFS", "-");
    set_arr("arr", &["a", "b", "c", "d"]);
    // start=1, no len → take rest
    assert_eq!(
      index("arr", ArrIndex::AllJoined, Some(1), None).unwrap(),
      "b-c-d"
    );
  }

  // ─── AssocArr ────────────────────────────────────────────────────

  #[test]
  fn assoc_arr_key_lookup() {
    let _g = TestGuard::new();
    set_assoc("amap", &[("apple", "red"), ("banana", "yellow")]);
    assert_eq!(
      index("amap", ArrIndex::Key("apple".into()), None, None).unwrap(),
      "red"
    );
  }

  #[test]
  fn assoc_arr_missing_key_returns_empty_string() {
    let _g = TestGuard::new();
    set_assoc("amap", &[("a", "1")]);
    assert_eq!(
      index("amap", ArrIndex::Key("missing".into()), None, None).unwrap(),
      ""
    );
  }

  #[test]
  fn assoc_arr_arg_count() {
    let _g = TestGuard::new();
    set_assoc("amap", &[("a", "1"), ("b", "2"), ("c", "3")]);
    assert_eq!(index("amap", ArrIndex::ArgCount, None, None).unwrap(), "3");
  }

  #[test]
  fn assoc_arr_all_joined_with_ifs() {
    let _g = TestGuard::new();
    set_str("IFS", "+");
    set_assoc("amap", &[("a", "1"), ("b", "2")]);
    let result = index("amap", ArrIndex::AllJoined, None, None).unwrap();
    // Iteration order is preserved, values joined by '+'.
    assert_eq!(result, "1+2");
  }

  #[test]
  fn assoc_arr_all_split() {
    let _g = TestGuard::new();
    set_assoc("amap", &[("a", "1"), ("b", "2")]);
    let result = index("amap", ArrIndex::AllSplit, None, None).unwrap();
    assert!(result.contains('1'));
    assert!(result.contains('2'));
  }

  // ─── Scalar var errors ───────────────────────────────────────────

  #[test]
  fn scalar_var_is_not_indexable_errors() {
    let _g = TestGuard::new();
    set_str("scalar", "hello");
    assert!(index("scalar", ArrIndex::Literal(0), None, None).is_err());
  }

  // ─── Missing var returns empty string ────────────────────────────

  #[test]
  fn missing_var_returns_empty_string() {
    let _g = TestGuard::new();
    assert_eq!(
      index("no_such_var_xyz_qqq", ArrIndex::Literal(0), None, None).unwrap(),
      ""
    );
  }
}

#[cfg(test)]
mod get_array_keys_tests {
  use super::*;
  use crate::state::Shed;
  use crate::state::vars::{VarFlags, VarKind};
  use crate::tests::testutil::TestGuard;
  use std::collections::VecDeque;

  fn set_arr(name: &str, items: &[&str]) {
    let vec: VecDeque<String> = items.iter().map(|s| s.to_string()).collect();
    Shed::vars_mut(|v| {
      v.set_var(name, VarKind::Arr(vec), VarFlags::empty())
        .unwrap();
    });
  }

  fn set_assoc(name: &str, pairs: &[(&str, &str)]) {
    let vec: Vec<(String, String)> = pairs
      .iter()
      .map(|(k, v)| (k.to_string(), v.to_string()))
      .collect();
    Shed::vars_mut(|v| {
      v.set_var(name, VarKind::AssocArr(vec), VarFlags::empty())
        .unwrap();
    });
  }

  fn set_str(name: &str, val: &str) {
    Shed::vars_mut(|v| {
      v.set_var(name, VarKind::Str(val.into()), VarFlags::empty())
        .unwrap();
    });
  }

  fn get_keys(name: &str, joined: bool) -> ShResult<String> {
    Shed::vars(|v| v.get_array_keys(name, joined))
  }

  #[test]
  fn arr_keys_are_sequential_indices() {
    let _g = TestGuard::new();
    set_arr("arr", &["a", "b", "c", "d"]);
    // joined=true → first char of IFS (default ' ').
    let out = get_keys("arr", true).unwrap();
    assert_eq!(out, "0 1 2 3");
  }

  #[test]
  fn empty_arr_returns_empty_string() {
    let _g = TestGuard::new();
    set_arr("arr_empty", &[]);
    assert_eq!(get_keys("arr_empty", true).unwrap(), "");
  }

  #[test]
  fn arr_joined_false_uses_arg_sep_marker() {
    let _g = TestGuard::new();
    set_arr("arr", &["x", "y"]);
    let out = get_keys("arr", false).unwrap();
    // Separator is the ARG_SEP marker char between the two indices.
    let sep = crate::expand::markers::ARG_SEP.to_string();
    assert_eq!(out, format!("0{sep}1"));
  }

  #[test]
  fn assoc_keys_returned_in_insertion_order() {
    let _g = TestGuard::new();
    set_assoc("h", &[("foo", "1"), ("bar", "2"), ("baz", "3")]);
    let out = get_keys("h", true).unwrap();
    // Joined with space (default IFS first char).
    assert_eq!(out, "foo bar baz");
  }

  #[test]
  fn assoc_joined_false_uses_arg_sep_marker() {
    let _g = TestGuard::new();
    set_assoc("h", &[("k1", "a"), ("k2", "b")]);
    let out = get_keys("h", false).unwrap();
    let sep = crate::expand::markers::ARG_SEP.to_string();
    assert_eq!(out, format!("k1{sep}k2"));
  }

  #[test]
  fn scalar_var_errors() {
    let _g = TestGuard::new();
    set_str("scalar", "value");
    assert!(get_keys("scalar", true).is_err());
  }

  #[test]
  fn missing_var_returns_empty_string() {
    let _g = TestGuard::new();
    assert_eq!(
      get_keys("no_such_var_for_get_array_keys", true).unwrap(),
      ""
    );
  }

  #[test]
  fn custom_ifs_first_char_is_used_when_joined() {
    let _g = TestGuard::new();
    set_arr("arr", &["a", "b", "c"]);
    Shed::vars_mut(|v| {
      v.set_var("IFS", VarKind::Str(":/".into()), VarFlags::empty())
        .unwrap();
    });
    // First char of IFS is ':'.
    assert_eq!(get_keys("arr", true).unwrap(), "0:1:2");
  }
}
