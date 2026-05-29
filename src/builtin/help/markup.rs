use std::{iter::Peekable, ops::Range, str::Chars};

use super::{expand::markers, match_loop, util::QuoteState};

pub const TAG_SEQ: &str = "\x1b[1;33m"; // bold yellow - searchable tags
pub const REF_SEQ: &str = "\x1b[4;36m"; // underline cyan - cross-references
pub const SEARCH_RES_SEQ: &str = "\x1b[1;7m"; // bold inverse - search result highlight
pub const SEARCH_FOCUS_SEQ: &str = "\x1b[1;36;7m"; // bold inverse cyan - search result focus
pub const RESET_SEQ: &str = "\x1b[0m";
pub const HEADER_SEQ: &str = "\x1b[1;35m"; // bold magenta - section headers
pub const CODE_SEQ: &str = "\x1b[32m"; // green - inline code
pub const KEYWORD_1_SEQ: &str = "\x1b[1;32m"; // bold green - {keyword}
pub const KEYWORD_2_SEQ: &str = "\x1b[3;37m"; // italic white - [optional]

#[derive(Debug)]
pub struct MarkedSpan {
  prefix_seq: Range<usize>,
  content: Range<usize>,
  postfix_seq: Range<usize>,
}

impl MarkedSpan {
  pub fn new(prefix_seq: Range<usize>, content: Range<usize>, postfix_seq: Range<usize>) -> Self {
    Self {
      prefix_seq,
      content,
      postfix_seq,
    }
  }

  pub fn prefix_range(&self) -> Range<usize> {
    self.prefix_seq.clone()
  }
  pub fn content<'a>(&self, source: &'a str) -> &'a str {
    &source[self.content.clone()]
  }
  pub fn line_no(&self, source: &str) -> usize {
    source[..self.prefix_seq.start]
      .chars()
      .filter(|c| *c == '\n')
      .count()
  }

  pub fn line_start(&self, source: &str) -> usize {
    source[..self.prefix_seq.start]
      .rfind('\n')
      .map(|pos| pos + 1)
      .unwrap_or(0)
  }

  pub fn rel_to_line(&self, source: &str) -> (Range<usize>, Range<usize>, Range<usize>) {
    let offset = self.line_start(source);
    (
      self.prefix_seq.clone().start - offset..self.prefix_seq.clone().end - offset,
      self.content.clone().start - offset..self.content.clone().end - offset,
      self.postfix_seq.clone().start - offset..self.postfix_seq.clone().end - offset,
    )
  }
}

#[derive(Debug)]
pub struct StyledHelp {
  content: String,
  ref_targets: Vec<Option<String>>,
}

impl StyledHelp {
  pub fn new(content: &str) -> Self {
    Self {
      content: style_help_content(content),
      ref_targets: extract_ref_targets(content),
    }
  }
  pub fn content(&self) -> &str {
    &self.content
  }
  pub fn take_ref_targets(&mut self) -> Vec<Option<String>> {
    std::mem::take(&mut self.ref_targets)
  }

  pub fn find_markers(&self, marker: &str) -> Vec<MarkedSpan> {
    let mut markers = vec![];
    let mut cursor = 0;

    while let Some(pos) = self.content[cursor..].find(marker) {
      let abs_pos = cursor + pos;
      let prefix_end = abs_pos + marker.len();

      let Some(end) = self.content[prefix_end..].find(RESET_SEQ) else {
        break;
      };

      let postfix_start = prefix_end + end;
      let postfix_end = postfix_start + RESET_SEQ.len();

      markers.push(MarkedSpan::new(
        abs_pos..prefix_end,
        prefix_end..postfix_start,
        postfix_start..postfix_end,
      ));

      cursor = postfix_end;
    }

    markers
  }
}

pub fn style_help_content(raw: &str) -> String {
  expand_help(&unescape_help(raw))
}

/// Consume a cross-reference target
///
/// Cross-references can define a target like `|some text|(some-target)`
fn consume_ref_alias(chars: &mut Peekable<Chars>) -> Option<String> {
  if chars.peek() != Some(&'(') {
    return None;
  }
  chars.next(); // '('
  let mut target = String::new();
  let mut closed = false;
  match_loop!(chars.next() => ch, {
    ')' => { closed = true; break; }
    '\\' if chars.peek() == Some(&')') => {
      target.push(')');
      chars.next();
    }
    _ => target.push(ch),
  });
  if closed && !target.is_empty() {
    Some(target)
  } else {
    None
  }
}

fn extract_ref_targets(source: &str) -> Vec<Option<String>> {
  let mut targets = vec![];
  let mut chars = source.chars().peekable();
  let mut qt_state = QuoteState::default();

  match_loop!(chars.next() => ch, {
    '\\' => {
      chars.next(); // consume escaped char
    }
    '\n' => {
      qt_state = QuoteState::default();
    }
    '"' => qt_state.toggle_double(),
    '\'' => qt_state.toggle_single(),
    _ if qt_state.in_quote() => {}
    '|' if chars.peek().is_some_and(|c| !c.is_whitespace()) => {
      // find the closer
      match_loop!(chars.next() => inner, {
        '|' => break,
        '\\' => { chars.next(); }
        _ => {}
      });
      // parse any reference targets
      targets.push(consume_ref_alias(&mut chars));
    }
    _ => {}
  });
  targets
}

fn expand_help(raw: &str) -> String {
  let mut result = String::new();
  let mut chars = raw.chars();

  match_loop!(chars.next() => ch, {
    markers::RESET => result.push_str(RESET_SEQ),
    markers::TAG => result.push_str(TAG_SEQ),
    markers::REFERENCE => result.push_str(REF_SEQ),
    markers::HEADER => result.push_str(HEADER_SEQ),
    markers::CODE => result.push_str(CODE_SEQ),
    markers::KEYWORD_1 => result.push_str(KEYWORD_1_SEQ),
    markers::KEYWORD_2 => result.push_str(KEYWORD_2_SEQ),
    _ => result.push(ch),
  });
  result
}

fn unescape_help(raw: &str) -> String {
  let mut result = String::new();
  let mut chars = raw.chars().peekable();
  let mut qt_state = QuoteState::default();

  let find_closer = |closer: char, res: &mut String, chars: &mut Peekable<Chars>| {
    match_loop!(chars.next() => next_ch, {
      _ if next_ch == closer => {
        res.push(markers::RESET);
        break;
      }
      '\\' => {
        match chars.peek() {
          Some(ch) if *ch == closer || *ch == '\\' => {
            res.push(chars.next().unwrap());
          }
          _ => res.push(next_ch),
        }
      }
      _ => res.push(next_ch),
    });
  };

  match_loop!(chars.next() => ch, {
    '\\' => {
      if let Some(next_ch) = chars.next() {
        result.push(next_ch);
      }
    }
    '\n' => {
      result.push(ch);
      qt_state = QuoteState::default();
    }
    '"' => {
      result.push(ch);
      qt_state.toggle_double();
    }
    '\'' => {
      result.push(ch);
      qt_state.toggle_single();
    }
    _ if qt_state.in_quote() || chars.peek().is_none_or(|ch| ch.is_whitespace()) => {
      result.push(ch);
    }
    '*' => {
      let lookahead: String = chars.clone()
        .take_while(|c| !c.is_whitespace() && *c != '\n')
        .collect();
      if lookahead.contains('*') {
        result.push(markers::TAG);
        find_closer('*', &mut result, &mut chars);
      } else {
        result.push(ch);
      }
    }
    '|' => {
      result.push(markers::REFERENCE);
      find_closer('|', &mut result, &mut chars);

      // run this here just to skip any (targets)
      // the return of this function is handled in
      // extract_ref_targets, we just drop it here
      consume_ref_alias(&mut chars);
    }
    '#' => {
      result.push(markers::HEADER);
      find_closer('#', &mut result, &mut chars);
    }
    '`' => {
      result.push(markers::CODE);
      find_closer('`', &mut result, &mut chars);
    }
    '{' => {
      result.push(markers::KEYWORD_2);
      find_closer('}', &mut result, &mut chars);
    }
    '[' => {
      result.push(markers::KEYWORD_2);
      find_closer(']', &mut result, &mut chars);
    }
    '~' => {
      let mut tilde_count = 1;
      while tilde_count != 3 && chars.peek() == Some(&'~') {
        chars.next();
        tilde_count += 1;
      }
      if tilde_count != 3 {
        result.push_str(&"~".repeat(tilde_count));
      } else {
        match_loop!(chars.next() => ch, {
          '~' => {
            tilde_count = 1;
            while tilde_count != 3 && chars.peek() == Some(&'~') {
              chars.next();
              tilde_count += 1;
            }
            if tilde_count != 3 {
              result.push_str(&"~".repeat(3 + tilde_count));
            } else {
              result.push(markers::RESET);
              break
            }
          }
          _ => result.push(ch),
        })
      }
    }
    _ => result.push(ch),
  });
  result
}

#[cfg(test)]
mod tests {
  use super::*;

  // `extract_ref_targets` probed directly — no need for full StyledHelp.

  #[test]
  fn extract_plain_ref_yields_none() {
    assert_eq!(extract_ref_targets("see |foo|"), vec![None]);
  }

  #[test]
  fn extract_aliased_ref_yields_some() {
    assert_eq!(
      extract_ref_targets("see |this text|(real-tag)"),
      vec![Some("real-tag".into())]
    );
  }

  #[test]
  fn extract_mixed_keeps_order() {
    assert_eq!(
      extract_ref_targets("|a| then |b text|(b-tag) then |c|"),
      vec![None, Some("b-tag".into()), None]
    );
  }

  #[test]
  fn extract_empty_paren_is_not_an_alias() {
    // `|t|()` still counts as a ref but with no target override.
    assert_eq!(extract_ref_targets("|t|()"), vec![None]);
  }

  #[test]
  fn extract_unterminated_paren_is_not_an_alias() {
    // No closing `)`, so the alias parse fails. Still one ref.
    assert_eq!(extract_ref_targets("|t|(abc"), vec![None]);
  }

  #[test]
  fn extract_escaped_close_paren_in_target() {
    assert_eq!(extract_ref_targets("|t|(a\\)b)"), vec![Some("a)b".into())]);
  }

  #[test]
  fn extract_pipe_followed_by_whitespace_is_literal() {
    // `| ` is not a reference opener (matches unescape_help's predicate).
    assert_eq!(extract_ref_targets("a | b"), vec![]);
  }

  #[test]
  fn extract_pipe_inside_quotes_is_literal() {
    assert_eq!(extract_ref_targets("\"a|b|c\""), vec![]);
  }

  #[test]
  fn extract_newline_resets_quote_state() {
    // Open double-quote without close, then newline -- quote state
    // resets, so the `|x|` on line 2 IS a reference.
    assert_eq!(extract_ref_targets("\"unclosed\n|x|"), vec![None]);
  }

  // Cross-pass invariant: count from extract_ref_targets must equal
  // count from find_markers(REF_SEQ). Catches drift if anyone touches
  // the opener predicate in either function.

  fn ref_count_invariant(src: &str) {
    let mut styled = StyledHelp::new(src);
    let from_render = styled.find_markers(REF_SEQ).len();
    let from_source = styled.take_ref_targets().len();
    assert_eq!(
      from_render, from_source,
      "ref count desync on input {src:?}: render={from_render} source={from_source}"
    );
  }

  #[test]
  fn invariant_plain_refs() {
    ref_count_invariant("|a| |b| |c|");
  }

  #[test]
  fn invariant_aliased_refs() {
    ref_count_invariant("|a|(x) |b|(y) |c|(z)");
  }

  #[test]
  fn invariant_mixed() {
    ref_count_invariant("|a| |b|(y) |c| |d|(z) |e|");
  }

  #[test]
  fn invariant_literal_pipes_dont_count() {
    ref_count_invariant("a | b, then |real| and \"|quoted|\"");
  }
}
