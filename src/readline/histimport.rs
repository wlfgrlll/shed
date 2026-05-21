use std::{
  path::PathBuf,
  time::{Duration, SystemTime, UNIX_EPOCH},
};

use regex::Regex;

use super::{
  history::HistEntry,
  match_loop, sherr,
  util::{ShResult, ends_with_unescaped},
};

pub fn import_history(path: PathBuf) -> ShResult<Vec<HistEntry>> {
  let content = std::fs::read(&path)
    .map(|bytes| String::from_utf8_lossy(&bytes).to_string())
    .map_err(|e| sherr!(ParseErr, "Failed to read history file: {e}"))?;

  let filename = path
    .file_name()
    .and_then(|n| n.to_str())
    .ok_or_else(|| sherr!(ParseErr, "Invalid history file name"))?;

  match filename {
    ".bash_history" => try_import_bash(&content),
    ".zsh_history" => try_import_zsh(&content),
    "fish_history" => try_import_fish(&content),
    _ => try_import_bash(&content)
      .or_else(|_| try_import_zsh(&content))
      .or_else(|_| try_import_fish(&content))
      .map_err(|_| sherr!(ParseErr, "Unknown history format")),
  }
}

fn try_import_bash(content: &str) -> ShResult<Vec<HistEntry>> {
  let mut lines = content.lines().peekable();
  let mut entries = vec![];
  let timestamp_pat = Regex::new(r#"^#(\d+)$"#).unwrap();

  while let Some(line) = lines.next() {
    if let Some(caps) = timestamp_pat.captures(line) {
      let secs: u64 = caps[1].parse().unwrap_or(0);
      let timestamp = UNIX_EPOCH + Duration::from_secs(secs);
      if let Some(cmd) = lines.next() {
        entries.push(HistEntry {
          timestamp,
          command: cmd.to_string(),
          ..HistEntry::default()
        });
      }
    } else {
      entries.push(HistEntry {
        command: line.to_string(),
        ..HistEntry::default()
      });
    }
  }

  Ok(entries)
}

fn collect_continuation<'a>(first: &'a str, lines: &mut impl Iterator<Item = &'a str>) -> String {
  let mut parts = vec![];
  let mut line = first;
  loop {
    parts.push(line.strip_suffix('\\').unwrap_or(line));
    if !ends_with_unescaped(line, "\\") {
      break;
    }
    if let Some(next) = lines.next() {
      line = next;
    } else {
      break;
    }
  }
  parts.join("\n")
}

fn try_import_zsh(content: &str) -> ShResult<Vec<HistEntry>> {
  let mut lines = content.lines().peekable();
  let mut entries = vec![];

  while let Some(line) = lines.next() {
    if line.starts_with(": ")
      && let Some((meta, cmd_start)) = &line[2..].split_once(';')
    {
      let timestamp = meta
        .split_once(':')
        .and_then(|(ts, _)| ts.parse::<u64>().ok())
        .map(|secs| UNIX_EPOCH + Duration::from_secs(secs))
        .unwrap_or(SystemTime::now());

      entries.push(HistEntry {
        timestamp,
        command: collect_continuation(cmd_start, &mut lines),
        ..HistEntry::default()
      });
    } else {
      entries.push(HistEntry {
        command: collect_continuation(line, &mut lines),
        ..HistEntry::default()
      });
    }
  }

  Ok(entries)
}

fn expand_fish_cmd(cmd: &str) -> String {
  let mut out = String::new();
  let mut chars = cmd.chars();

  match_loop!(chars.next() => ch, {
    '\\' => {
      let Some(esc_ch) = chars.next() else {
        out.push('\\');
        break;
      };
      match esc_ch {
        'n' => out.push('\n'),
        '\\' => out.push('\\'),
        _ => {
          out.push('\\');
          out.push(esc_ch);
        }
      }
    }
    _ => out.push(ch)
  });

  out
}

fn try_import_fish(content: &str) -> ShResult<Vec<HistEntry>> {
  let mut entries = vec![];
  let mut lines = content.lines();

  while let Some(line) = lines.next() {
    if let Some(cmd) = line.strip_prefix("- cmd: ") {
      let timestamp = lines
        .next()
        .and_then(|l| l.trim().strip_prefix("when: "))
        .and_then(|ts| ts.parse::<u64>().ok())
        .map(|secs| UNIX_EPOCH + Duration::from_secs(secs))
        .unwrap_or(SystemTime::now());

      entries.push(HistEntry {
        timestamp,
        command: expand_fish_cmd(cmd),
        ..HistEntry::default()
      });
    }
  }

  Ok(entries)
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::io::Write;

  /// Write `content` to a tempfile with a chosen file name; returns
  /// (TempDir guard, full path).
  fn write_hist_file(name: &str, content: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join(name);
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(content.as_bytes()).unwrap();
    (dir, path)
  }

  fn secs_since_epoch(ts: SystemTime) -> u64 {
    ts.duration_since(UNIX_EPOCH).unwrap().as_secs()
  }

  // ===================== try_import_bash =====================

  #[test]
  fn bash_unprefixed_lines_become_entries_without_timestamps() {
    let content = "echo one\necho two\necho three\n";
    let entries = try_import_bash(content).unwrap();
    assert_eq!(entries.len(), 3);
    assert_eq!(entries[0].command, "echo one");
    assert_eq!(entries[1].command, "echo two");
    assert_eq!(entries[2].command, "echo three");
  }

  #[test]
  fn bash_timestamp_comment_attaches_to_next_line() {
    let content = "#1700000000\necho stamped\n";
    let entries = try_import_bash(content).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].command, "echo stamped");
    assert_eq!(secs_since_epoch(entries[0].timestamp), 1_700_000_000);
  }

  #[test]
  fn bash_timestamp_at_end_with_no_following_command_is_dropped() {
    // Pin the current behavior: a trailing `#N` with nothing after
    // simply produces no entry (the `if let Some(cmd) = lines.next()`
    // sees None).
    let content = "echo first\n#1700000000\n";
    let entries = try_import_bash(content).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].command, "echo first");
  }

  #[test]
  fn bash_mixed_timestamped_and_plain_lines() {
    let content = "echo a\n#1700000100\necho b\necho c\n";
    let entries = try_import_bash(content).unwrap();
    assert_eq!(entries.len(), 3);
    assert_eq!(entries[0].command, "echo a");
    assert_eq!(entries[1].command, "echo b");
    assert_eq!(secs_since_epoch(entries[1].timestamp), 1_700_000_100);
    assert_eq!(entries[2].command, "echo c");
  }

  // ===================== try_import_zsh =====================

  #[test]
  fn zsh_extended_format_parses_timestamp_and_command() {
    let content = ": 1700000200:0;echo zsh_one\n";
    let entries = try_import_zsh(content).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].command, "echo zsh_one");
    assert_eq!(secs_since_epoch(entries[0].timestamp), 1_700_000_200);
  }

  #[test]
  fn zsh_plain_line_becomes_entry_without_metadata() {
    let content = "echo plain\n";
    let entries = try_import_zsh(content).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].command, "echo plain");
  }

  #[test]
  fn zsh_backslash_continuation_joins_lines() {
    // A trailing unescaped `\` continues the command to the next line.
    let content = ": 1700000300:0;echo first \\\necho second\n";
    let entries = try_import_zsh(content).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].command, "echo first \necho second");
  }

  #[test]
  fn zsh_extended_format_with_malformed_timestamp_falls_back_to_now() {
    // The unwrap_or(SystemTime::now()) branch fires when timestamp
    // parsing fails. We just verify the entry is still produced and
    // the command is right.
    let content = ": notanumber:0;echo malformed\n";
    let entries = try_import_zsh(content).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].command, "echo malformed");
  }

  // ===================== try_import_fish =====================

  #[test]
  fn fish_cmd_and_when_pair_produces_entry() {
    let content = "- cmd: echo fish_one\n  when: 1700000400\n";
    let entries = try_import_fish(content).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].command, "echo fish_one");
    assert_eq!(secs_since_epoch(entries[0].timestamp), 1_700_000_400);
  }

  #[test]
  fn fish_cmd_without_when_falls_back_to_now() {
    let content = "- cmd: echo no_when\nother junk\n";
    let entries = try_import_fish(content).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].command, "echo no_when");
  }

  #[test]
  fn fish_multiple_entries_parsed_in_order() {
    let content = "- cmd: one\n  when: 100\n- cmd: two\n  when: 200\n- cmd: three\n  when: 300\n";
    let entries = try_import_fish(content).unwrap();
    assert_eq!(entries.len(), 3);
    assert_eq!(entries[0].command, "one");
    assert_eq!(entries[1].command, "two");
    assert_eq!(entries[2].command, "three");
  }

  #[test]
  fn fish_lines_without_cmd_prefix_are_skipped() {
    // Random metadata interspersed with entries.
    let content = "# header\n- cmd: real\n  when: 100\n  paths:\n    - foo\n";
    let entries = try_import_fish(content).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].command, "real");
  }

  // ===================== expand_fish_cmd =====================

  #[test]
  fn expand_fish_cmd_passthrough() {
    assert_eq!(expand_fish_cmd("plain text"), "plain text");
  }

  #[test]
  fn expand_fish_cmd_backslash_n_becomes_newline() {
    assert_eq!(expand_fish_cmd("a\\nb"), "a\nb");
  }

  #[test]
  fn expand_fish_cmd_escaped_backslash_collapses_to_single() {
    assert_eq!(expand_fish_cmd("a\\\\b"), "a\\b");
  }

  #[test]
  fn expand_fish_cmd_unknown_escape_preserved_verbatim() {
    assert_eq!(expand_fish_cmd("a\\xb"), "a\\xb");
  }

  #[test]
  fn expand_fish_cmd_trailing_backslash_kept_as_literal() {
    // Backslash at end of string — chars.next() returns None, the
    // function pushes the lone backslash and breaks.
    assert_eq!(expand_fish_cmd("foo\\"), "foo\\");
  }

  // ===================== collect_continuation =====================

  #[test]
  fn collect_continuation_returns_single_line_unchanged() {
    let mut lines = std::iter::empty::<&str>();
    assert_eq!(
      collect_continuation("simple line", &mut lines),
      "simple line"
    );
  }

  #[test]
  fn collect_continuation_joins_escaped_lines() {
    let rest = vec!["second", "third"];
    let mut iter = rest.into_iter();
    let result = collect_continuation("first\\", &mut iter);
    // First line had a trailing `\` so the next iter line joins on a
    // newline; the second line doesn't end with `\` so we stop.
    assert_eq!(result, "first\nsecond");
  }

  #[test]
  fn collect_continuation_stops_at_iter_exhaustion() {
    // Trailing `\` but no more lines — the loop breaks via the
    // `if let Some(next) = ... else { break }` arm.
    let mut iter = std::iter::empty::<&str>();
    let result = collect_continuation("only\\", &mut iter);
    assert_eq!(result, "only");
  }

  // ===================== import_history dispatch =====================

  #[test]
  fn dispatch_by_bash_history_filename() {
    let (_dir, path) = write_hist_file(".bash_history", "echo bash_entry\n");
    let entries = import_history(path).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].command, "echo bash_entry");
  }

  #[test]
  fn dispatch_by_zsh_history_filename() {
    let (_dir, path) = write_hist_file(".zsh_history", ": 1700000500:0;echo zsh_entry\n");
    let entries = import_history(path).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].command, "echo zsh_entry");
  }

  #[test]
  fn dispatch_by_fish_history_filename() {
    let (_dir, path) = write_hist_file(
      "fish_history",
      "- cmd: echo fish_entry\n  when: 1700000600\n",
    );
    let entries = import_history(path).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].command, "echo fish_entry");
  }

  #[test]
  fn dispatch_unknown_filename_falls_back_to_bash_first() {
    // Unknown filenames try bash → zsh → fish. Bash accepts any text,
    // so it always wins on the fallback chain.
    let (_dir, path) = write_hist_file("random_name", "echo fallback\n");
    let entries = import_history(path).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].command, "echo fallback");
  }

  #[test]
  fn missing_file_errors() {
    let path = PathBuf::from("/path/that/definitely/does/not/exist/zzz");
    assert!(import_history(path).is_err());
  }
}
