use std::{
  env,
  path::{Path, PathBuf},
};

use super::{
  ShResult,
  getopt::{Opt, OptSpec},
  outln, sherr,
  state::util,
  try_var, var, with_status,
};

pub(super) struct Cd;
impl super::Builtin for Cd {
  fn opts(&self) -> Vec<OptSpec> {
    vec![OptSpec::flag('P'), OptSpec::flag('L')]
  }
  fn execute(&self, args: super::BuiltinArgs) -> ShResult<()> {
    let mut resolve_syms = false;
    let mut try_cd_path = false;
    let mut print_dir = false;

    for opt in &args.opts {
      match opt {
        Opt::Short('P') => resolve_syms = true,
        Opt::Short('L') => resolve_syms = false,
        _ => return Err(sherr!(ParseErr @ args.span, "Invalid option: {opt}")),
      }
    }

    let (mut new_dir, arg_span) = if let Some((arg, span)) = args.argv.into_iter().next() {
      match arg.as_str() {
        "-" => {
          let old_pwd = get_old_pwd();
          print_dir = true;
          (old_pwd, Some(span))
        }
        _ => {
          try_cd_path = !arg.starts_with(['/', '.']);
          (PathBuf::from(arg), Some(span))
        }
      }
    } else {
      (
        PathBuf::from(util::get_home_str().unwrap_or(String::from("/"))),
        None,
      )
    };

    let span = arg_span.unwrap_or(args.span);

    if try_cd_path && let Some(found) = search_cd_path(&new_dir) {
      print_dir = true;
      new_dir = found;
    }

    let logical_pwd = if !resolve_syms {
      let base = if new_dir.is_absolute() {
        PathBuf::new()
      } else {
        try_var!("PWD")
          .map(PathBuf::from)
          .or_else(|| std::env::current_dir().ok())
          .unwrap_or_else(|| PathBuf::from("/"))
      };
      Some(
        util::lex_normalize_path(&base.join(&new_dir))
          .display()
          .to_string(),
      )
    } else {
      None
    };

    if resolve_syms && let Ok(canon) = std::fs::canonicalize(&new_dir) {
      new_dir = canon;
    }

    if !new_dir.exists() {
      return Err(sherr!(ExecFail @ span.clone(), "Directory not found"));
    }
    if !new_dir.is_dir() {
      return Err(sherr!(ExecFail @ span.clone(), "Not a directory"));
    }
    if let Err(e) = util::change_dir_with_pwd(new_dir, logical_pwd) {
      return Err(sherr!(ExecFail @ span.clone(), "Failed to change directory: {e}"));
    }

    if print_dir {
      let mut dir = env::current_dir()?.display().to_string();
      if let Some(home) = util::get_home_str()
        && let Some(home_dir) = dir.strip_prefix(&home)
      {
        dir = format!("~{home_dir}");
      }

      outln!("{dir}");
    }

    with_status(0)
  }
}

fn search_cd_path(new_dir: impl AsRef<Path>) -> Option<PathBuf> {
  let path = var!("CDPATH");
  let mut paths = path
    .split(':')
    .filter(|p| !p.trim().is_empty())
    .map(PathBuf::from);

  paths.find_map(|p| p.join(&new_dir).is_dir().then(|| p.join(&new_dir)))
}

fn get_old_pwd() -> PathBuf {
  try_var!("OLDPWD")
    .or_else(|| util::get_home_str().or_else(|| Some(String::from("/"))))
    .map(PathBuf::from)
    .unwrap()
}

#[cfg(test)]
pub mod tests {
  use std::env;
  use std::fs;

  use tempfile::TempDir;

  use crate::var;
  use crate::{
    state::{
      self, Shed,
      vars::{VarFlags, VarKind},
    },
    tests::testutil::{TestGuard, canon, test_input},
  };

  // ===================== Basic Navigation =====================

  #[test]
  fn cd_simple() {
    let _g = TestGuard::new();
    let old_dir = env::current_dir().unwrap();
    let temp_dir = TempDir::new().unwrap();

    test_input(format!("cd {}", temp_dir.path().display())).unwrap();

    let new_dir = env::current_dir().unwrap();
    assert_ne!(old_dir, new_dir);

    assert_eq!(
      new_dir.display().to_string(),
      canon(temp_dir.path()).display().to_string()
    );
  }

  #[test]
  fn cd_no_args_goes_home() {
    let _g = TestGuard::new();
    let temp_dir = TempDir::new().unwrap();
    Shed::vars_mut(|v| {
      v.set_var(
        "HOME",
        VarKind::Str(temp_dir.path().display().to_string()),
        VarFlags::empty(),
      )
    })
    .unwrap();

    test_input("cd").unwrap();

    let cwd = env::current_dir().unwrap();
    assert_eq!(
      cwd.display().to_string(),
      canon(temp_dir.path()).display().to_string()
    );
  }

  #[test]
  fn cd_relative_path() {
    let _g = TestGuard::new();
    let temp_dir = TempDir::new().unwrap();
    let sub = temp_dir.path().join("child");
    fs::create_dir(&sub).unwrap();

    test_input(format!("cd {}", temp_dir.path().display())).unwrap();
    test_input("cd child").unwrap();

    let cwd = env::current_dir().unwrap();
    assert_eq!(cwd.display().to_string(), canon(&sub).display().to_string());
  }

  // ===================== Environment =====================

  #[test]
  fn cd_status_zero_on_success() {
    let _g = TestGuard::new();
    let temp_dir = TempDir::new().unwrap();

    test_input(format!("cd {}", temp_dir.path().display())).unwrap();

    assert_eq!(state::Shed::get_status(), 0);
  }

  // ===================== Error Cases =====================

  #[test]
  fn cd_nonexistent_dir_fails() {
    let _g = TestGuard::new();
    test_input("cd /nonexistent_path_that_does_not_exist_xyz").ok();
    assert_ne!(state::Shed::get_status(), 0);
  }

  #[test]
  fn cd_file_not_directory_fails() {
    let _g = TestGuard::new();
    let temp_dir = TempDir::new().unwrap();
    let file_path = temp_dir.path().join("afile.txt");
    fs::write(&file_path, "hello").unwrap();

    test_input(format!("cd {}", file_path.display())).ok();
    assert_ne!(state::Shed::get_status(), 0);
  }

  // ===================== Multiple cd =====================

  #[test]
  fn cd_multiple_times() {
    let _g = TestGuard::new();
    let dir_a = TempDir::new().unwrap();
    let dir_b = TempDir::new().unwrap();

    test_input(format!("cd {}", dir_a.path().display())).unwrap();
    assert_eq!(
      env::current_dir().unwrap().display().to_string(),
      canon(dir_a.path()).display().to_string()
    );

    test_input(format!("cd {}", dir_b.path().display())).unwrap();
    assert_eq!(
      env::current_dir().unwrap().display().to_string(),
      canon(dir_b.path()).display().to_string()
    );
  }

  #[test]
  fn cd_nested_subdirectories() {
    let _g = TestGuard::new();
    let temp_dir = TempDir::new().unwrap();
    let deep = temp_dir.path().join("a").join("b").join("c");
    fs::create_dir_all(&deep).unwrap();

    test_input(format!("cd {}", deep.display())).unwrap();
    assert_eq!(
      env::current_dir().unwrap().display().to_string(),
      canon(&deep).display().to_string()
    );
  }

  // ===================== Autocmd Integration =====================

  #[test]
  fn cd_fires_post_change_dir_autocmd() {
    let guard = TestGuard::new();
    let temp_dir = TempDir::new().unwrap();

    test_input("autocmd post-change-dir 'echo cd-hook-fired'").unwrap();
    guard.read_output();

    test_input(format!("cd {}", temp_dir.path().display())).unwrap();
    let out = guard.read_output();
    assert!(out.contains("cd-hook-fired"));
  }

  #[test]
  fn cd_fires_pre_change_dir_autocmd() {
    let guard = TestGuard::new();
    let temp_dir = TempDir::new().unwrap();

    test_input("autocmd pre-change-dir 'echo pre-cd'").unwrap();
    guard.read_output();

    test_input(format!("cd {}", temp_dir.path().display())).unwrap();
    let out = guard.read_output();
    assert!(out.contains("pre-cd"));
  }

  // ===================== OLDPWD / cd - =====================

  #[test]
  fn cd_sets_oldpwd() {
    let _g = TestGuard::new();
    let dir_a = TempDir::new().unwrap();
    let dir_b = TempDir::new().unwrap();

    test_input(format!("cd {}", dir_a.path().display())).unwrap();
    test_input(format!("cd {}", dir_b.path().display())).unwrap();

    // -L semantics: OLDPWD preserves the path the user typed, not the
    // canonical form. On macOS `/var/folders/...` is a symlink to
    // `/private/var/folders/...` so comparing against `canon(...)` would
    // wrongly canonicalize it.
    let oldpwd = var!("OLDPWD");
    assert_eq!(oldpwd, dir_a.path().display().to_string());
  }

  #[test]
  fn cd_sets_pwd_var() {
    let _g = TestGuard::new();
    let temp_dir = TempDir::new().unwrap();

    test_input(format!("cd {}", temp_dir.path().display())).unwrap();

    // -L semantics: $PWD reflects what the user typed, not the canonical
    // kernel cwd. The kernel cwd can differ if any component of the input
    // path is a symlink (e.g. macOS's `/var` → `/private/var`).
    let pwd = var!("PWD");
    assert_eq!(pwd, temp_dir.path().display().to_string());
  }

  #[test]
  fn cd_hyphen_goes_to_oldpwd() {
    let _g = TestGuard::new();
    let dir_a = TempDir::new().unwrap();
    let dir_b = TempDir::new().unwrap();

    test_input(format!("cd {}", dir_a.path().display())).unwrap();
    test_input(format!("cd {}", dir_b.path().display())).unwrap();
    test_input("cd -").unwrap();

    let cwd = env::current_dir().unwrap();
    assert_eq!(
      cwd.display().to_string(),
      canon(dir_a.path()).display().to_string()
    );
  }

  #[test]
  fn cd_hyphen_toggles() {
    let _g = TestGuard::new();
    let dir_a = TempDir::new().unwrap();
    let dir_b = TempDir::new().unwrap();

    test_input(format!("cd {}", dir_a.path().display())).unwrap();
    test_input(format!("cd {}", dir_b.path().display())).unwrap();
    test_input("cd -").unwrap();
    test_input("cd -").unwrap();

    let cwd = env::current_dir().unwrap();
    assert_eq!(
      cwd.display().to_string(),
      canon(dir_b.path()).display().to_string()
    );
  }

  // ===================== CDPATH =====================

  #[test]
  fn cd_uses_cdpath() {
    let _g = TestGuard::new();
    let base = TempDir::new().unwrap();
    let target = base.path().join("mydir");
    fs::create_dir(&target).unwrap();

    Shed::vars_mut(|v| {
      v.set_var(
        "CDPATH",
        VarKind::Str(base.path().display().to_string()),
        VarFlags::EXPORT,
      )
    })
    .unwrap();
    test_input("cd mydir").unwrap();

    let cwd = env::current_dir().unwrap();
    assert_eq!(
      cwd.display().to_string(),
      canon(&target).display().to_string()
    );
  }

  #[test]
  fn cd_cdpath_skips_nonexistent() {
    let _g = TestGuard::new();
    let base = TempDir::new().unwrap();
    let target = base.path().join("realdir");
    fs::create_dir(&target).unwrap();

    Shed::vars_mut(|v| {
      v.set_var(
        "CDPATH",
        VarKind::Str(format!("/nonexistent_cdpath_xyz:{}", base.path().display())),
        VarFlags::EXPORT,
      )
    })
    .unwrap();
    test_input("cd realdir").unwrap();

    let cwd = env::current_dir().unwrap();
    assert_eq!(
      cwd.display().to_string(),
      canon(&target).display().to_string()
    );
  }

  #[test]
  fn cd_cdpath_not_used_for_absolute() {
    let _g = TestGuard::new();
    let target = TempDir::new().unwrap();
    let decoy = TempDir::new().unwrap();

    Shed::vars_mut(|v| {
      v.set_var(
        "CDPATH",
        VarKind::Str(decoy.path().display().to_string()),
        VarFlags::EXPORT,
      )
    })
    .unwrap();
    test_input(format!("cd {}", target.path().display())).unwrap();

    let cwd = env::current_dir().unwrap();
    assert_eq!(
      cwd.display().to_string(),
      canon(target.path()).display().to_string()
    );
  }

  #[test]
  fn cd_cdpath_not_used_for_dot() {
    let _g = TestGuard::new();
    let temp_dir = TempDir::new().unwrap();
    let sub = temp_dir.path().join("child");
    fs::create_dir(&sub).unwrap();

    test_input(format!("cd {}", temp_dir.path().display())).unwrap();

    let decoy = TempDir::new().unwrap();
    Shed::vars_mut(|v| {
      v.set_var(
        "CDPATH",
        VarKind::Str(decoy.path().display().to_string()),
        VarFlags::EXPORT,
      )
    })
    .unwrap();
    test_input("cd ./child").unwrap();

    let cwd = env::current_dir().unwrap();
    assert_eq!(cwd.display().to_string(), canon(&sub).display().to_string());
  }

  // ===================== -P option =====================

  #[test]
  fn cd_p_resolves_symlinks() {
    let _g = TestGuard::new();
    let temp_dir = TempDir::new().unwrap();
    let real_dir = temp_dir.path().join("real");
    let link_dir = temp_dir.path().join("link");
    fs::create_dir(&real_dir).unwrap();
    std::os::unix::fs::symlink(&real_dir, &link_dir).unwrap();

    test_input(format!("cd -P {}", link_dir.display())).unwrap();

    let cwd = env::current_dir().unwrap();
    let canonical_real = fs::canonicalize(&real_dir).unwrap();
    assert_eq!(
      cwd.display().to_string(),
      canonical_real.display().to_string()
    );
  }

  // ===================== -L (default) symlink preservation =====================

  #[test]
  fn cd_l_preserves_symlink_in_pwd() {
    // The bug from #73: by default `cd` should NOT resolve symlinks when
    // setting $PWD. The kernel cwd is canonical (no avoiding that), but
    // $PWD should reflect what the user typed.
    let _g = TestGuard::new();
    let temp_dir = TempDir::new().unwrap();
    let real_dir = temp_dir.path().join("real");
    let link_dir = temp_dir.path().join("link");
    fs::create_dir(&real_dir).unwrap();
    std::os::unix::fs::symlink(&real_dir, &link_dir).unwrap();

    test_input(format!("cd {}", link_dir.display())).unwrap();

    let pwd = var!("PWD");
    assert_eq!(pwd, link_dir.display().to_string());
  }

  #[test]
  fn cd_l_dotdot_pops_lexically() {
    // After `cd /a/symlink-to-b`, `cd ..` with -L should land in /a (the
    // parent of the symlink path), not in the parent of the real dir.
    let _g = TestGuard::new();
    let temp_dir = TempDir::new().unwrap();
    let real = temp_dir.path().join("real");
    let link = temp_dir.path().join("link");
    fs::create_dir(&real).unwrap();
    std::os::unix::fs::symlink(&real, &link).unwrap();

    test_input(format!("cd {}", link.display())).unwrap();
    test_input("cd ..").unwrap();

    let pwd = var!("PWD");
    assert_eq!(pwd, temp_dir.path().display().to_string());
  }

  #[test]
  fn cd_l_normalizes_dotdot_in_input() {
    let _g = TestGuard::new();
    let temp_dir = TempDir::new().unwrap();
    let sub = temp_dir.path().join("sub");
    fs::create_dir(&sub).unwrap();

    let weird = format!("{}/sub/../sub", temp_dir.path().display());
    test_input(format!("cd {weird}")).unwrap();

    let pwd = var!("PWD");
    assert_eq!(pwd, sub.display().to_string());
  }

  #[test]
  fn cd_p_pwd_is_canonical() {
    // Sanity: with -P, $PWD matches the kernel cwd (symlinks resolved).
    let _g = TestGuard::new();
    let temp_dir = TempDir::new().unwrap();
    let real = temp_dir.path().join("real");
    let link = temp_dir.path().join("link");
    fs::create_dir(&real).unwrap();
    std::os::unix::fs::symlink(&real, &link).unwrap();

    test_input(format!("cd -P {}", link.display())).unwrap();

    let pwd = var!("PWD");
    assert_eq!(pwd, fs::canonicalize(&real).unwrap().display().to_string());
  }
}
