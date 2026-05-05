use std::iter::Peekable;
use std::str::Chars;

use nix::unistd::{Uid, User};

use crate::expand::PARAMETERS;
use crate::expand::escape::escape_str;
use crate::expand::param::perform_param_expansion;
use crate::expand::subshell::{expand_cmd_sub, expand_proc_sub};
use crate::match_loop;
use crate::parse::lex::is_hard_sep;
use crate::prelude::*;
use crate::readline::markers;
use crate::sherr;
use crate::state::{read_shopts, read_vars};
use crate::util::error::ShResult;

pub fn expand_raw_inner(
  chars: &mut Peekable<Chars<'_>>,
  expand_cmd_subs: bool,
) -> ShResult<String> {
  let mut result = String::new();

  match_loop!(chars.next() => ch, {
    markers::TILDE_SUB => {
      let mut username = String::new();
      while chars.peek().is_some_and(|ch| *ch != '/') {
        let ch = chars.next().unwrap();
        username.push(ch);
      }

      let (home, expanded) = if username.is_empty() {
        // standard '~' expansion
        (env::var("HOME").unwrap_or_default(), true)
      } else if let Ok(result) = User::from_name(&username)
        && let Some(user) = result
      {
        // username expansion like '~user'
        (user.dir.to_string_lossy().to_string(), true)
      } else if let Ok(id) = username.parse::<u32>()
        && let Ok(result) = User::from_uid(Uid::from_raw(id))
          && let Some(user) = result
      {
        // uid expansion like '~1000'
        // shed only feature btw B)
        (user.dir.to_string_lossy().to_string(), true)
      } else {
        (format!("~{username}"), false)
      };

      if expanded {
        result.push(markers::DUB_QUOTE);
        result.push_str(&home);
        result.push(markers::DUB_QUOTE);
      } else {
        result.push_str(&home);
      }
    }
    markers::PROC_SUB_OUT if expand_cmd_subs => {
      let mut inner = String::new();
      match_loop!(chars.next() => ch, {
        markers::PROC_SUB_OUT => break,
        _ => inner.push(ch),
      });
      let fd_path = expand_proc_sub(&inner, false)?;
      result.push_str(&fd_path);
    }
    markers::PROC_SUB_IN if expand_cmd_subs => {
      let mut inner = String::new();
      match_loop!(chars.next() => ch, {
        markers::PROC_SUB_IN => break,
        _ => inner.push(ch),
      });
      let fd_path = expand_proc_sub(&inner, true)?;
      result.push_str(&fd_path);
    }
    markers::VAR_SUB => {
      let expanded = expand_var(chars, expand_cmd_subs)?;
      result.push_str(&expanded);
    }
    _ => result.push(ch),
  });

  Ok(result)
}

pub fn expand_raw(chars: &mut Peekable<Chars<'_>>) -> ShResult<String> {
  expand_raw_inner(chars, true)
}

pub fn expand_var(chars: &mut Peekable<Chars<'_>>, expand_cmd_subs: bool) -> ShResult<String> {
  let mut var_name = String::new();
  let mut brace_depth: i32 = 0;
  let mut inner_brace_depth: i32 = 0;
  match_loop!(chars.peek() => &ch => ch, {
    markers::SUBSH if var_name.is_empty() => {
      chars.next(); // now safe to consume
      let mut subsh_body = String::new();
      let mut found_end = false;
      match_loop!(chars.next() => c, {
        markers::SUBSH => {
          found_end = true;
          break;
        }
        _ => subsh_body.push(c),
      });
      if !found_end {
        // if there isnt a closing SUBSH, we are probably in some tab completion context
        // and we got passed some unfinished input. Just treat it as literal text
        return Ok(format!("$({subsh_body}"));
      }
      if expand_cmd_subs {
        let expanded = expand_cmd_sub(&subsh_body)?;
        return Ok(expanded);
      } else {
        return Ok(subsh_body);
      }
    }
    '{' if var_name.is_empty() && brace_depth == 0 => {
      chars.next(); // consume the brace
      brace_depth += 1;
    }
    '}' if brace_depth > 0 && inner_brace_depth == 0 => {
      chars.next(); // consume the brace
      let val = perform_param_expansion(&var_name)?;
      return Ok(val);
    }
    markers::ESCAPE if brace_depth > 0 => {
      chars.next();
      var_name.push(markers::ESCAPE);
      if let Some(next_ch) = chars.next() {
        var_name.push(next_ch);
      }
    }
    markers::DUB_QUOTE | markers::SNG_QUOTE if brace_depth > 0 => {
      let opener = ch;
      chars.next();
      var_name.push(opener);
      while let Some(&next_ch) = chars.peek() {
        chars.next();
        var_name.push(next_ch);
        if next_ch == opener {
          break;
        }
        if next_ch == markers::ESCAPE
        && let Some(esc_ch) = chars.next() {
          var_name.push(esc_ch);
        }
      }
    }
    ch if brace_depth > 0 => {
      chars.next(); // safe to consume
      if ch == '{' {
        inner_brace_depth += 1;
      }
      if ch == '}' {
        inner_brace_depth -= 1;
      }
      var_name.push(ch);
    }
    ch if var_name.is_empty() && (PARAMETERS.contains(&ch) || ch.is_ascii_digit()) => {
      chars.next();
      let parameter = ch.to_string();
      let val = read_vars(|v| v.get_var(&parameter));

      if (ch == '@' || ch == '*') && val.is_empty() {
        return Ok(markers::NULL_EXPAND.to_string());
      }

      return Ok(val);
    }
    ch if is_hard_sep(ch) || !(ch.is_alphanumeric() || ch == '_') => {
      let val = read_vars(|v| v.try_get_var(&var_name));
      if val.is_none() && read_shopts(|o| o.set.nounset) {
        return Err(sherr!(NotFound, "Variable '{var_name}' is not set"));
      }
      return Ok(val.unwrap_or_default());
    }
    _ => {
      chars.next();
      var_name.push(ch);
    }
  });
  if !var_name.is_empty() {
    let val = read_vars(|v| v.try_get_var(&var_name));
    if val.is_none() && read_shopts(|o| o.set.nounset) {
      return Err(sherr!(NotFound, "Variable '{var_name}' is not set"));
    }
    Ok(val.unwrap_or_default())
  } else {
    Ok(String::new())
  }
}

pub fn escape_glob(raw: &str, use_markers: bool) -> String {
  let esc_ch = if use_markers { markers::ESCAPE } else { '\\' };
  let mut out = String::new();
  let mut chars = raw.chars();
  match_loop!(chars.next() => ch, {
    _ if ch == esc_ch => {
      if let Some(nch) = chars.next() {
        out.push_str(&glob::Pattern::escape(&nch.to_string()));
      }
    }
    _ => out.push(ch),
  });

  out
}

pub fn restore_glob_prefix(pattern: &str, mut result: String) -> String {
  if pattern.starts_with("./")
    && !result.starts_with("./")
    && !result.starts_with('/')
  {
    result.insert_str(0, "./");
  }
  if pattern.ends_with('/') && !result.ends_with('/') {
    result.push('/');
  }
  result
}

pub fn expand_glob(raw: &str) -> ShResult<Vec<String>> {
  let mut words = vec![];

  if !raw.contains(['*', '?', '[']) || read_shopts(|o| o.set.noglob) {
    return Ok(vec![raw.to_string()]);
  }
  let escaped = escape_glob(raw, true);

  let opts = glob::MatchOptions {
    require_literal_leading_dot: !read_shopts(|s| s.core.dotglob),
    ..Default::default()
  };
  for entry in
    glob::glob_with(&escaped, opts).map_err(|_| sherr!(SyntaxErr, "Invalid glob pattern"))?
  {
    let entry = entry.map_err(|_| sherr!(SyntaxErr, "Invalid filename found in glob"))?;
    let entry_raw = entry
      .to_str()
      .ok_or_else(|| sherr!(SyntaxErr, "Non-UTF8 filename found in glob"))?;
    let escaped = escape_str(entry_raw, true);

    words.push(escaped)
  }
  Ok(words)
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::expand::escape::unescape_str;
  use crate::state::{VarFlags, VarKind, write_vars};
  use crate::tests::testutil::TestGuard;

  // ===================== Variable Expansion (TestGuard) =====================

  #[test]
  fn var_expansion_basic() {
    let _guard = TestGuard::new();
    write_vars(|v| v.set_var("MYVAR", VarKind::Str("hello".into()), VarFlags::NONE)).unwrap();

    let raw = unescape_str("$MYVAR");
    let result = expand_raw(&mut raw.chars().peekable()).unwrap();
    assert_eq!(result, "hello");
  }

  #[test]
  fn var_expansion_braced() {
    let _guard = TestGuard::new();
    write_vars(|v| v.set_var("FOO", VarKind::Str("bar".into()), VarFlags::NONE)).unwrap();

    let raw = unescape_str("${FOO}");
    let result = expand_raw(&mut raw.chars().peekable()).unwrap();
    assert_eq!(result, "bar");
  }

  #[test]
  fn var_expansion_unset_empty() {
    let _guard = TestGuard::new();

    let raw = unescape_str("$NONEXISTENT");
    let result = expand_raw(&mut raw.chars().peekable()).unwrap();
    assert_eq!(result, "");
  }

  #[test]
  fn var_expansion_concatenated() {
    let _guard = TestGuard::new();
    write_vars(|v| v.set_var("A", VarKind::Str("hello".into()), VarFlags::NONE)).unwrap();
    write_vars(|v| v.set_var("B", VarKind::Str("world".into()), VarFlags::NONE)).unwrap();

    let raw = unescape_str("${A}_${B}");
    let result = expand_raw(&mut raw.chars().peekable()).unwrap();
    assert_eq!(result, "hello_world");
  }

  // ===================== Tilde Expansion (TestGuard) =====================

  #[test]
  fn tilde_expansion_home() {
    let _guard = TestGuard::new();
    let home = std::env::var("HOME").unwrap();

    let raw = unescape_str("~/foo");
    let result = expand_raw(&mut raw.chars().peekable()).unwrap();
    assert_eq!(
      result,
      format!("{}{}{}/foo", markers::DUB_QUOTE, home, markers::DUB_QUOTE)
    );
  }

  #[test]
  fn tilde_expansion_bare() {
    let _guard = TestGuard::new();
    let home = std::env::var("HOME").unwrap();

    let raw = unescape_str("~");
    let result = expand_raw(&mut raw.chars().peekable()).unwrap();
    assert_eq!(
      result,
      format!("{}{}{}", markers::DUB_QUOTE, home, markers::DUB_QUOTE)
    );
  }

  // ===================== escape_glob =====================

  #[test]
  fn escape_glob_passthrough_when_no_escapes() {
    // No `\` chars → output equals input.
    assert_eq!(escape_glob("foo*bar", false), "foo*bar");
    assert_eq!(escape_glob("plain", false), "plain");
  }

  #[test]
  fn escape_glob_wraps_escaped_star() {
    // `\*` → `[*]` (glob-literal star)
    assert_eq!(escape_glob("foo\\*", false), "foo[*]");
  }

  #[test]
  fn escape_glob_wraps_escaped_question_mark() {
    assert_eq!(escape_glob("foo\\?", false), "foo[?]");
  }

  #[test]
  fn escape_glob_wraps_escaped_bracket() {
    assert_eq!(escape_glob("foo\\[bar", false), "foo[[]bar");
  }

  #[test]
  fn escape_glob_strips_non_meta_escapes() {
    // `\ ` (escaped space) becomes literal space — not a glob meta, so
    // bracket-wrap is unnecessary.
    assert_eq!(escape_glob("my\\ file", false), "my file");
  }

  #[test]
  fn escape_glob_drops_trailing_escape() {
    // Lone trailing `\` with nothing to escape — silently dropped.
    assert_eq!(escape_glob("foo\\", false), "foo");
  }

  #[test]
  fn escape_glob_with_marker_form() {
    // use_markers=true reads the ESCAPE marker char, not literal `\`.
    use crate::readline::markers;
    let input = format!("foo{}*", markers::ESCAPE);
    assert_eq!(escape_glob(&input, true), "foo[*]");
  }

  // ===================== expand_glob with escapes =====================

  #[test]
  fn expand_glob_matches_escaped_space() {
    // The original bug: `my\ *` should match a file named `my file.txt`.
    let _g = TestGuard::new();
    let tmp = std::env::temp_dir().join("shed_test_glob_escape");
    std::fs::create_dir_all(&tmp).ok();
    let target = tmp.join("my file.txt");
    std::fs::write(&target, "").unwrap();

    let saved_dir = std::env::current_dir().ok();
    std::env::set_current_dir(&tmp).unwrap();

    // After unescape_str, `my\ *` becomes `my{ESCAPE} *`.
    let unescaped = unescape_str("my\\ *");
    let result = expand_glob(&unescaped);

    if let Some(prev) = saved_dir {
      let _ = std::env::set_current_dir(prev);
    }
    std::fs::remove_dir_all(&tmp).ok();

    let result = result.expect("expand_glob should succeed").join(" ");
    // Glob expansion should match `my file.txt`. Result is escape-marker-
    // wrapped post-glob; check via strip_markers.
    use crate::readline::markers::strip_markers;
    let stripped = strip_markers(&result);
    assert!(
      stripped.contains("my file.txt"),
      "expected match for 'my\\ *'; got {stripped:?}"
    );
  }

  // ===================== Tk::expand glob tests (full pipeline) =====================

  /// Helper: drive the full expansion pipeline (unescape_str → expand_raw →
  /// split_words → expand_glob → strip ESCAPE) on a raw shell word.
  fn expand_words_in(dir: &std::path::Path, raw: &str) -> Vec<String> {
    use crate::expand::Expander;
    use crate::parse::lex::TkFlags;

    let saved = std::env::current_dir().ok();
    std::env::set_current_dir(dir).unwrap();
    let result = Expander::from_raw(raw, TkFlags::empty())
      .unwrap()
      .expand()
      .unwrap();
    if let Some(prev) = saved {
      let _ = std::env::set_current_dir(prev);
    }
    result
  }

  /// Build a tempdir populated with the given filenames.
  fn make_fixture(name: &str, files: &[&str]) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    for f in files {
      std::fs::File::create(dir.join(f)).unwrap();
    }
    dir
  }

  #[test]
  fn glob_quoted_prefix_unquoted_meta_matches() {
    // `"path/"*` should glob — only `*` is unquoted, the prefix is literal.
    // This is the cd-completion case.
    let _g = TestGuard::new();
    let dir = make_fixture("shed_glob_qprefix", &["alpha", "beta", "gamma"]);
    let pattern = format!(r#""{}/"*"#, dir.display());
    let words = expand_words_in(&dir, &pattern);
    let _ = std::fs::remove_dir_all(&dir);

    let mut got: Vec<String> = words
      .iter()
      .filter_map(|w| std::path::Path::new(w).file_name().map(|n| n.to_string_lossy().into_owned()))
      .collect();
    got.sort();
    assert_eq!(got, vec!["alpha", "beta", "gamma"]);
  }

  #[test]
  fn glob_fully_quoted_is_literal() {
    // `"*"` should be a literal `*` — no expansion.
    let _g = TestGuard::new();
    let dir = make_fixture("shed_glob_full_quote", &["a", "b"]);
    let words = expand_words_in(&dir, r#""*""#);
    let _ = std::fs::remove_dir_all(&dir);

    assert_eq!(words, vec!["*"]);
  }

  #[test]
  fn glob_squote_is_literal() {
    // `'*'` should be a literal `*` — no expansion.
    let _g = TestGuard::new();
    let dir = make_fixture("shed_glob_squote", &["a", "b"]);
    let words = expand_words_in(&dir, "'*'");
    let _ = std::fs::remove_dir_all(&dir);

    assert_eq!(words, vec!["*"]);
  }

  #[test]
  fn glob_backslash_escaped_is_literal() {
    // `\*` should be a literal `*`.
    let _g = TestGuard::new();
    let dir = make_fixture("shed_glob_bs_escape", &["a", "b"]);
    let words = expand_words_in(&dir, r"\*");
    let _ = std::fs::remove_dir_all(&dir);

    assert_eq!(words, vec!["*"]);
  }

  #[test]
  fn glob_unquoted_expands() {
    // Baseline: unquoted `*` globs as expected.
    let _g = TestGuard::new();
    let dir = make_fixture("shed_glob_unquoted", &["a.txt", "b.txt", "c.log"]);
    let words = expand_words_in(&dir, "*.txt");
    let _ = std::fs::remove_dir_all(&dir);

    let mut got = words;
    got.sort();
    assert_eq!(got, vec!["a.txt", "b.txt"]);
  }

  #[test]
  fn glob_quoted_prefix_with_subdir_unquoted_meta() {
    // `"a/"*.txt` — prefix quoted, suffix has unquoted glob meta.
    let _g = TestGuard::new();
    let outer = make_fixture("shed_glob_subdir", &[]);
    let sub = outer.join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::File::create(sub.join("a.txt")).unwrap();
    std::fs::File::create(sub.join("b.txt")).unwrap();

    let pattern = format!(r#""{}/sub/"*.txt"#, outer.display());
    let words = expand_words_in(&outer, &pattern);
    let _ = std::fs::remove_dir_all(&outer);

    let mut got: Vec<String> = words
      .iter()
      .filter_map(|w| std::path::Path::new(w).file_name().map(|n| n.to_string_lossy().into_owned()))
      .collect();
    got.sort();
    assert_eq!(got, vec!["a.txt", "b.txt"]);
  }
}
