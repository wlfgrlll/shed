use std::{iter::Peekable, str::Chars};

use super::{
  error::ShResult,
  eval::lex::{Span, Tk},
  expand::{read_hex, read_octal, read_stty_escape},
  match_loop, sherr,
};

/// Used to track whether the lexer is currently inside a quote, and if so, which type
#[derive(Default, Debug, PartialEq, Clone)]
pub enum QuoteState {
  #[default]
  Outside,
  Single,
  Double,
}

impl QuoteState {
  pub fn outside(&self) -> bool {
    matches!(self, QuoteState::Outside)
  }
  pub fn in_single(&self) -> bool {
    matches!(self, QuoteState::Single)
  }
  pub fn in_double(&self) -> bool {
    matches!(self, QuoteState::Double)
  }
  pub fn in_quote(&self) -> bool {
    !self.outside()
  }
  /// Toggles whether we are in a double quote. If self = QuoteState::Single or QuoteState::Backtick, this does nothing, since double quotes inside those quotes are just literal characters
  pub fn toggle_double(&mut self) {
    match self {
      QuoteState::Outside => *self = QuoteState::Double,
      QuoteState::Double => *self = QuoteState::Outside,
      _ => {}
    }
  }
  /// Toggles whether we are in a single quote. If self == QuoteState::Double or QuoteState::Backtick, this does nothing, since single quotes inside those quotes are just literal characters
  pub fn toggle_single(&mut self) {
    match self {
      QuoteState::Outside => *self = QuoteState::Single,
      QuoteState::Single => *self = QuoteState::Outside,
      _ => {}
    }
  }
}

/* - splitting functions
 * the splitting functions in std are fine, but don't cut it when quoting rules and escaping are involved
 * so we have to roll our own stuff. we can take a functional approach to to this that generalizes quite well
 */

pub fn split_tk(tk: &Tk, pat: &str) -> Vec<Tk> {
  let slice = tk.as_str();
  let base = tk.span.range().start;
  split_all_with(
    slice,
    |s| split_at_unescaped(s, pat),
    |start, end| {
      Tk::new(
        tk.class.clone(),
        Span::new(base + start..base + end, tk.source()),
      )
    },
  )
}

pub fn split_all_with<T, F, B>(slice: &str, segment_fn: F, mut build: B) -> Vec<T>
where
  F: Fn(&str) -> Option<(usize, usize)>,
  B: FnMut(usize, usize) -> T,
{
  let mut cursor = 0;
  let mut splits = vec![];
  while let Some((len, skip)) = segment_fn(&slice[cursor..]) {
    splits.push(build(cursor, cursor + len));
    cursor += len + skip;
  }
  if let Some(remaining) = slice.get(cursor..) {
    splits.push(build(cursor, cursor + remaining.len()));
  }
  splits
}

/// Splits a string at the first occurrence of a pattern, but only if the pattern is not escaped by a backslash
/// and not in quotes. Returns None if the pattern is not found or only found escaped.
pub fn split_at_unescaped(slice: &str, pat: &str) -> Option<(usize, usize)> {
  let mut chars = slice.char_indices().peekable();
  let mut qt_state = QuoteState::default();

  while let Some((i, ch)) = chars.next() {
    match ch {
      '\\' => {
        chars.next();
        continue;
      }
      '\'' => qt_state.toggle_single(),
      '"' => qt_state.toggle_double(),
      _ if qt_state.in_quote() => continue,
      _ => {}
    }

    if slice[i..].starts_with(pat) {
      return Some((i, pat.len()));
    }
  }

  None
}

pub fn pos_is_escaped(slice: &str, pos: usize) -> bool {
  let bytes = slice.as_bytes();
  let mut escaped = false;
  let mut i = pos;
  while i > 0 && bytes[i - 1] == b'\\' {
    escaped = !escaped;
    i -= 1;
  }
  escaped
}

pub fn rfind_unescaped(slice: &str, pat: char) -> Option<usize> {
  let mut last = None;
  let mut chars = slice.char_indices();
  while let Some((i, ch)) = chars.next() {
    if ch == '\\' {
      chars.next();
    } else if ch == pat {
      last = Some(i);
    }
  }
  last
}

pub fn ends_with_unescaped(slice: &str, pat: &str) -> bool {
  slice.ends_with(pat) && !pos_is_escaped(slice, slice.len() - pat.len())
}

pub fn has_unescaped(slice: &str, pat: &str) -> bool {
  split_at_unescaped(slice, pat).is_some()
}

pub fn scan_parens(chars: &mut Peekable<Chars>, pos: &mut usize, depth: usize) -> bool {
  scan_delims('(', chars, pos, depth).unwrap()
}

pub fn scan_braces(chars: &mut Peekable<Chars>, pos: &mut usize, depth: usize) -> bool {
  scan_delims('{', chars, pos, depth).unwrap()
}

fn scan_delims(
  opener: char,
  chars: &mut Peekable<Chars>,
  pos: &mut usize,
  mut depth: usize,
) -> ShResult<bool> {
  let closer = match opener {
    '(' => ')',
    '{' => '}',
    '[' => ']',
    '<' => '>',
    _ => {
      return Err(sherr!(
          ParseErr @ Span::new(*pos..*pos, "".into()),
          "Invalid opener '{opener}'",
      ));
    }
  };
  let mut qt = QuoteState::default();
  match_loop!(chars.next() => ch, {
    '\\' => {
      *pos += 1;
      if let Some(next_ch) = chars.next() {
        *pos += next_ch.len_utf8();
      }
    }
    '\'' => { *pos += 1; qt.toggle_single(); }
    '"' if !qt.in_single() => { *pos += 1; qt.toggle_double(); }
    _ if qt.in_quote() => *pos += ch.len_utf8(),
    _ if ch == opener => { *pos += 1; depth += 1; }
    _ if ch == closer => {
      *pos += 1;
      depth -= 1;
      if depth == 0 { break; }
    }
    _ => *pos += ch.len_utf8(),
  });
  Ok(depth == 0)
}

pub(crate) fn format_time(dur: std::time::Duration) -> String {
  const ETERNITY: u128 = f32::INFINITY as u128;
  let mut micros = dur.as_micros();
  let mut millis = 0;
  let mut seconds = 0;
  let mut minutes = 0;
  let mut hours = 0;
  let mut days = 0;
  let mut weeks = 0;
  let mut months = 0;
  let mut years = 0;
  let mut decades = 0;
  let mut centuries = 0;
  let mut millennia = 0;
  let mut epochs = 0;
  let mut aeons = 0;
  let mut eternities = 0;

  if micros >= 1000 {
    millis = micros / 1000;
    micros %= 1000;
  }
  if millis >= 1000 {
    seconds = millis / 1000;
    millis %= 1000;
  }
  if seconds >= 60 {
    minutes = seconds / 60;
    seconds %= 60;
  }
  if minutes >= 60 {
    hours = minutes / 60;
    minutes %= 60;
  }
  if hours >= 24 {
    days = hours / 24;
    hours %= 24;
  }
  if days >= 7 {
    weeks = days / 7;
    days %= 7;
  }
  if weeks >= 4 {
    months = weeks / 4;
    weeks %= 4;
  }
  if months >= 12 {
    years = months / 12;
    months %= 12;
  }
  if years >= 10 {
    decades = years / 10;
    years %= 10;
  }
  if decades >= 10 {
    centuries = decades / 10;
    decades %= 10;
  }
  if centuries >= 10 {
    millennia = centuries / 10;
    centuries %= 10;
  }
  if millennia >= 1000 {
    epochs = millennia / 1000;
    millennia %= 1000;
  }
  if epochs >= 1000 {
    aeons = epochs / 1000;
    epochs %= 1000;
  }
  if aeons == ETERNITY {
    eternities = aeons / ETERNITY;
    aeons %= ETERNITY;
  }

  // Format the result
  let mut result = Vec::new();
  if eternities > 0 {
    let mut string = format!("{} eternit", eternities);
    if eternities > 1 {
      string.push_str("ies");
    } else {
      string.push('y');
    }
    result.push(string)
  }
  if aeons > 0 {
    let mut string = format!("{} aeon", aeons);
    if aeons > 1 {
      string.push('s')
    }
    result.push(string)
  }
  if epochs > 0 {
    let mut string = format!("{} epoch", epochs);
    if epochs > 1 {
      string.push('s')
    }
    result.push(string)
  }
  if millennia > 0 {
    let mut string = format!("{} millenni", millennia);
    if millennia > 1 {
      string.push('a')
    } else {
      string.push_str("um")
    }
    result.push(string)
  }
  if centuries > 0 {
    let mut string = format!("{} centur", centuries);
    if centuries > 1 {
      string.push_str("ies")
    } else {
      string.push('y')
    }
    result.push(string)
  }
  if decades > 0 {
    let mut string = format!("{} decade", decades);
    if decades > 1 {
      string.push('s')
    }
    result.push(string)
  }
  if years > 0 {
    let mut string = format!("{} year", years);
    if years > 1 {
      string.push('s')
    }
    result.push(string)
  }
  if months > 0 {
    let mut string = format!("{} month", months);
    if months > 1 {
      string.push('s')
    }
    result.push(string)
  }
  if weeks > 0 {
    let mut string = format!("{} week", weeks);
    if weeks > 1 {
      string.push('s')
    }
    result.push(string)
  }
  if days > 0 {
    let mut string = format!("{} day", days);
    if days > 1 {
      string.push('s')
    }
    result.push(string)
  }
  if hours > 0 {
    let string = format!("{}h", hours);
    result.push(string);
  }
  if minutes > 0 {
    let string = format!("{}m", minutes);
    result.push(string);
  }
  if seconds > 0 {
    let string = format!("{}s", seconds);
    result.push(string);
  }
  if result.is_empty() && millis > 0 {
    let string = format!("{}ms", millis);
    result.push(string);
  }
  if result.is_empty() && micros > 0 {
    let string = format!("{}µs", micros);
    result.push(string);
  }

  result.join(" ")
}

pub fn format_size(bytes: u64) -> String {
  const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB", "PB", "EB"];
  let mut size = bytes as f64;
  let mut unit = 0;
  while size >= 1024.0 && unit < UNITS.len() - 1 {
    size /= 1024.0;
    unit += 1;
  }
  if unit == 0 {
    format!("{} {}", size as u64, UNITS[unit])
  } else {
    format!("{:.1} {}", size, UNITS[unit])
  }
}

pub fn format_mode(mode: u32) -> String {
  let mut out = String::new();
  let mut check_bit = |bit: u32, ch: char| {
    if mode & bit != 0 {
      out.push(ch);
    } else {
      out.push('-');
    }
  };
  check_bit(0o400, 'r');
  check_bit(0o200, 'w');
  check_bit(0o100, 'x');
  check_bit(0o040, 'r');
  check_bit(0o020, 'w');
  check_bit(0o010, 'x');
  check_bit(0o004, 'r');
  check_bit(0o002, 'w');
  check_bit(0o001, 'x');

  out
}

/// Expand standard ANSI-C escapes
pub fn expand_ansi_c(s: &str) -> String {
  let mut result = String::new();
  let mut chars = s.chars().peekable();

  while let Some(ch) = chars.next() {
    if ch != '\\' {
      result.push(ch);
      continue;
    }
    let Some(&next) = chars.peek() else {
      result.push(ch);
      break;
    };

    match next {
      'n' => {
        result.push('\n');
        chars.next();
      }
      't' => {
        result.push('\t');
        chars.next();
      }
      'r' => {
        result.push('\r');
        chars.next();
      }
      'a' => {
        result.push('\x07');
        chars.next();
      }
      'b' => {
        result.push('\x08');
        chars.next();
      }
      'c' => {
        chars.next();
        read_stty_escape(&mut chars, &mut result);
      }
      'e' | 'E' => {
        result.push('\x1B');
        chars.next();
      }
      'f' => {
        result.push('\x0C');
        chars.next();
      }
      'v' => {
        result.push('\x0B');
        chars.next();
      }
      'x' => {
        chars.next();
        read_hex(&mut chars, &mut result);
      }
      'o' => {
        chars.next();
        read_octal(&mut chars, &mut result, None);
      }
      _ if next.is_ascii_digit() => read_octal(&mut chars, &mut result, None),
      '\'' => {
        result.push('\'');
        chars.next();
      }
      '\\' => {
        result.push('\\');
        chars.next();
      }
      _ => {
        result.push(ch);
      }
    }
  }

  result
}

#[cfg(test)]
mod format_time_tests {
  use super::format_time;
  use std::time::Duration;

  // ─── single-unit base cases ──────────────────────────────────────

  #[test]
  fn zero_duration_is_empty_string() {
    assert_eq!(format_time(Duration::ZERO), "");
  }

  #[test]
  fn sub_millisecond_uses_microseconds() {
    assert_eq!(format_time(Duration::from_micros(500)), "500µs");
  }

  #[test]
  fn sub_second_uses_milliseconds() {
    assert_eq!(format_time(Duration::from_millis(250)), "250ms");
  }

  #[test]
  fn exact_second_uses_s_suffix() {
    assert_eq!(format_time(Duration::from_secs(1)), "1s");
  }

  #[test]
  fn one_minute() {
    assert_eq!(format_time(Duration::from_secs(60)), "1m");
  }

  #[test]
  fn one_hour() {
    assert_eq!(format_time(Duration::from_secs(3600)), "1h");
  }

  #[test]
  fn one_day_uses_day_word() {
    assert_eq!(format_time(Duration::from_secs(86_400)), "1 day");
  }

  #[test]
  fn one_week() {
    assert_eq!(format_time(Duration::from_secs(86_400 * 7)), "1 week");
  }

  #[test]
  fn one_month() {
    // shed defines a month as 4 weeks (28 days).
    assert_eq!(format_time(Duration::from_secs(86_400 * 7 * 4)), "1 month");
  }

  #[test]
  fn one_year() {
    // ... and a year as 12 months.
    assert_eq!(
      format_time(Duration::from_secs(86_400 * 7 * 4 * 12)),
      "1 year"
    );
  }

  #[test]
  fn one_decade() {
    assert_eq!(
      format_time(Duration::from_secs(86_400 * 7 * 4 * 12 * 10)),
      "1 decade"
    );
  }

  #[test]
  fn one_century() {
    assert_eq!(
      format_time(Duration::from_secs(86_400 * 7 * 4 * 12 * 100)),
      "1 century"
    );
  }

  // ─── singular vs plural ──────────────────────────────────────────

  #[test]
  fn plural_days() {
    assert_eq!(format_time(Duration::from_secs(86_400 * 2)), "2 days");
  }

  #[test]
  fn plural_weeks() {
    assert_eq!(format_time(Duration::from_secs(86_400 * 14)), "2 weeks");
  }

  #[test]
  fn plural_centuries() {
    assert_eq!(
      format_time(Duration::from_secs(86_400 * 7 * 4 * 12 * 200)),
      "2 centuries"
    );
  }

  // ─── combined output ─────────────────────────────────────────────

  #[test]
  fn combined_h_m_s() {
    // 1h 2m 3s = 3600 + 120 + 3 = 3723s
    assert_eq!(format_time(Duration::from_secs(3723)), "1h 2m 3s");
  }

  #[test]
  fn combined_day_and_hour() {
    // 1 day 5h = 86400 + 18000 = 104400s
    assert_eq!(format_time(Duration::from_secs(104_400)), "1 day 5h");
  }

  #[test]
  fn combined_week_and_day() {
    // 1 week 3 days = 7*86400 + 3*86400 = 10*86400
    assert_eq!(
      format_time(Duration::from_secs(86_400 * 10)),
      "1 week 3 days"
    );
  }

  // ─── sub-unit suppression ────────────────────────────────────────

  #[test]
  fn ms_suppressed_when_seconds_present() {
    // 1500ms = 1s + 500ms; only "1s" appears (ms only shows when
    // nothing else does).
    assert_eq!(format_time(Duration::from_millis(1500)), "1s");
  }

  #[test]
  fn micros_suppressed_when_millis_present() {
    // 1500µs = 1ms + 500µs; only "1ms" appears.
    assert_eq!(format_time(Duration::from_micros(1500)), "1ms");
  }

  // ─── regression tests for previously-buggy paths ────────────────

  #[test]
  fn thirteen_months_carries_one_month_not_thirteen() {
    // Regression: `months %= 12;` was previously `weeks %= 12;`, which
    // left `months` un-modulo'd and produced "1 year 13 months" instead.
    let dur = Duration::from_secs(86_400 * 7 * 4 * 13);
    assert_eq!(format_time(dur), "1 year 1 month");
  }

  #[test]
  fn singular_millennium_is_singular() {
    let dur = Duration::from_secs(86_400 * 7 * 4 * 12 * 1000);
    assert!(
      format_time(dur).contains("1 millennium"),
      "got {:?}",
      format_time(dur)
    );
  }

  #[test]
  fn plural_millennia_is_plural() {
    let dur = Duration::from_secs(86_400 * 7 * 4 * 12 * 2000);
    assert!(
      format_time(dur).contains("2 millennia"),
      "got {:?}",
      format_time(dur)
    );
  }
}

#[cfg(test)]
#[allow(non_snake_case)] // names preserve uppercase vs lowercase E
mod expand_ansi_c_tests {
  use super::expand_ansi_c;

  // ─── identity passthrough ─────────────────────────────────────────

  #[test]
  fn plain_text_unchanged() {
    assert_eq!(expand_ansi_c("hello world"), "hello world");
  }

  #[test]
  fn empty_string() {
    assert_eq!(expand_ansi_c(""), "");
  }

  // ─── named single-char escapes ───────────────────────────────────

  #[test]
  fn backslash_n_is_newline() {
    assert_eq!(expand_ansi_c("a\\nb"), "a\nb");
  }

  #[test]
  fn backslash_t_is_tab() {
    assert_eq!(expand_ansi_c("a\\tb"), "a\tb");
  }

  #[test]
  fn backslash_r_is_carriage_return() {
    assert_eq!(expand_ansi_c("a\\rb"), "a\rb");
  }

  #[test]
  fn backslash_a_is_bel() {
    assert_eq!(expand_ansi_c("\\a"), "\x07");
  }

  #[test]
  fn backslash_b_is_backspace() {
    assert_eq!(expand_ansi_c("\\b"), "\x08");
  }

  #[test]
  fn backslash_lower_e_is_escape() {
    assert_eq!(expand_ansi_c("\\e"), "\x1b");
  }

  #[test]
  fn backslash_upper_E_is_escape() {
    assert_eq!(expand_ansi_c("\\E"), "\x1b");
  }

  #[test]
  fn backslash_f_is_form_feed() {
    assert_eq!(expand_ansi_c("\\f"), "\x0c");
  }

  #[test]
  fn backslash_v_is_vertical_tab() {
    assert_eq!(expand_ansi_c("\\v"), "\x0b");
  }

  // ─── escaped quote and backslash ─────────────────────────────────

  #[test]
  fn backslash_single_quote_is_single_quote() {
    assert_eq!(expand_ansi_c("\\'"), "'");
  }

  #[test]
  fn backslash_backslash_is_single_backslash() {
    assert_eq!(expand_ansi_c("\\\\"), "\\");
  }

  // ─── \xNN — hex byte ─────────────────────────────────────────────

  #[test]
  fn hex_two_digits_decodes_byte() {
    assert_eq!(expand_ansi_c("\\x41"), "A");
  }

  #[test]
  fn hex_uppercase_digits() {
    assert_eq!(expand_ansi_c("\\xFF"), "\u{ff}");
  }

  #[test]
  fn hex_with_trailing_text() {
    // \x41 = 'A', then literal "BC"
    assert_eq!(expand_ansi_c("\\x41BC"), "ABC");
  }

  // ─── \oNNN — octal byte (with leading 'o') ──────────────────────

  #[test]
  fn octal_with_o_prefix() {
    // 'A' = octal 101
    assert_eq!(expand_ansi_c("\\o101"), "A");
  }

  // ─── \<digit>... — octal byte (no 'o' prefix) ────────────────────

  #[test]
  fn octal_digit_only() {
    assert_eq!(expand_ansi_c("\\101"), "A");
  }

  #[test]
  fn octal_short_form() {
    // \0 → null byte
    assert_eq!(expand_ansi_c("\\0"), "\0");
  }

  // ─── \c<char> — stty-style control char ──────────────────────────

  #[test]
  fn control_a() {
    assert_eq!(expand_ansi_c("\\cA"), "\x01"); // Ctrl+A
  }

  #[test]
  fn control_g_is_bel() {
    assert_eq!(expand_ansi_c("\\cG"), "\x07"); // Ctrl+G = BEL
  }

  #[test]
  fn control_lowercase_normalized_to_upper() {
    // \ca and \cA both produce Ctrl+A.
    assert_eq!(expand_ansi_c("\\ca"), "\x01");
  }

  #[test]
  fn control_question_mark_is_del() {
    assert_eq!(expand_ansi_c("\\c?"), "\x7f"); // DEL
  }

  #[test]
  fn control_invalid_target_preserves_literal() {
    // '0' is outside @..._ and isn't '?', so the escape isn't valid.
    // read_stty_escape pushes back "\\c" and leaves the '0' for the
    // outer loop to handle as a normal char.
    assert_eq!(expand_ansi_c("\\c0"), "\\c0");
  }

  // ─── unrecognized escape — preserves backslash ───────────────────

  #[test]
  fn unknown_escape_preserves_backslash() {
    assert_eq!(expand_ansi_c("\\z"), "\\z");
  }

  // ─── edge cases ──────────────────────────────────────────────────

  #[test]
  fn trailing_backslash_with_no_followup_kept() {
    // Bare `\` at end of string is kept as-is.
    assert_eq!(expand_ansi_c("foo\\"), "foo\\");
  }

  #[test]
  fn multiple_escapes_in_sequence() {
    assert_eq!(expand_ansi_c("\\t\\n\\r"), "\t\n\r");
  }

  #[test]
  fn mixed_escapes_and_literals() {
    assert_eq!(expand_ansi_c("line1\\nline2\\tcol2"), "line1\nline2\tcol2");
  }
}
