use std::iter::Peekable;
use std::ops::Range;
use std::path::PathBuf;
use std::rc::Rc;
use std::str::CharIndices;
use std::vec::IntoIter;

use itertools::{Itertools, PeekingNext};

use crate::eval::lex::TkVecUtils;
use crate::eval::lex::{self, LexFlags, LexStream, Span, Tk};
use crate::key;
use crate::keys::KeyEvent;
use crate::match_loop;
use crate::readline::SimpleEditor;
use crate::readline::editcmd::{
  Anchor, CmdFlags, EditCmd, LineAddr, Motion, ReadSrc, RegisterName, StashArgs, StashListArg,
  Verb, WriteDest,
};
use crate::readline::editmode::{EditMode, ModeReport};
use crate::readline::history::History;
use crate::readline::linebuf::LineBuf;
use crate::state::terminal::CursorStyle;
use crate::util::ShResult;
use crate::{status_msg, verb};
use bitflags::bitflags;

bitflags! {
  #[derive(Debug,Clone,Copy,PartialEq,Eq)]
  pub struct SubFlags: u16 {
    const GLOBAL           = 1 << 0; // g
    const CONFIRM          = 1 << 1; // c (probably not implemented)
    const IGNORE_CASE      = 1 << 2; // i
    const NO_IGNORE_CASE   = 1 << 3; // I
    const SHOW_COUNT       = 1 << 4; // n
    const PRINT_RESULT     = 1 << 5; // p
    const PRINT_NUMBERED   = 1 << 6; // #
    const PRINT_LEFT_ALIGN = 1 << 7; // l
  }
}

#[derive(Debug)]
struct ExEditor {
  editor: SimpleEditor,
}

impl Default for ExEditor {
  fn default() -> Self {
    Self {
      editor: SimpleEditor::new(Some("ex_history")),
    }
  }
}

impl ExEditor {
  pub fn new(has_select: bool) -> Self {
    let mut editor = SimpleEditor::new(Some("ex_history"));
    if has_select {
      editor.buf = editor.buf.with_initial("'<,'>", 6);
    }
    Self { editor }
  }
  pub fn clear(&mut self) {
    *self = Self::default()
  }
  pub fn is_empty(&self) -> bool {
    self.editor.buf.is_empty()
  }
}

#[derive(Default, Debug)]
pub struct ViEx {
  pending_cmd: ExEditor,
}

impl ViEx {
  pub fn new(has_select: bool) -> Self {
    Self {
      pending_cmd: ExEditor::new(has_select),
    }
  }
}

impl EditMode for ViEx {
  // Ex mode can return errors, so we use this fallible method instead of the normal one
  fn handle_key_fallible(&mut self, key: KeyEvent) -> ShResult<Option<EditCmd>> {
    match key {
      key!('\r') | key!(Enter) => {
        let input = self.pending_cmd.editor.buf.joined();
        let res = match parse_ex_input(&input) {
          ExParseResult::Success(node) => {
            self.pending_cmd.clear();

            Ok(Some(EditCmd {
              register: RegisterName::default(),
              verb: Some(verb!(Verb::ExCmd(node))),
              motion: None,
              flags: CmdFlags::EXIT_CUR_MODE,
              raw_seq: input.clone(),
            }))
          }
          ExParseResult::Error(e) => {
            status_msg!("{e}");
            Ok(None)
          }
        };

        if let Some(hist) = self.history()
          && let Err(e) = hist.push(input)
        {
          status_msg!("Failed to save ex command to history: {e}");
        }

        res
      }
      key!(Ctrl + 'c') => {
        self.pending_cmd.clear();
        Ok(None)
      }
      key!(Backspace) if self.pending_cmd.is_empty() => Ok(Some(EditCmd {
        register: RegisterName::default(),
        verb: None,
        motion: None,
        flags: CmdFlags::EXIT_CUR_MODE,
        raw_seq: "".into(),
      })),
      key!(Esc) => Ok(Some(EditCmd {
        register: RegisterName::default(),
        verb: None,
        motion: None,
        flags: CmdFlags::EXIT_CUR_MODE,
        raw_seq: "".into(),
      })),
      _ => self.pending_cmd.editor.handle_key(key).map(|_| None),
    }
  }
  fn handle_key(&mut self, key: KeyEvent) -> Option<EditCmd> {
    let result = self.handle_key_fallible(key);
    result.ok().flatten()
  }
  fn is_repeatable(&self) -> bool {
    false
  }

  fn as_replay(&self) -> Option<super::CmdReplay> {
    None
  }

  fn editor(&mut self) -> Option<&mut LineBuf> {
    Some(&mut self.pending_cmd.editor.buf)
  }

  fn history(&mut self) -> Option<&mut History> {
    self.pending_cmd.editor.history.as_mut()
  }

  fn cursor_style(&self) -> String {
    CursorStyle::Underline(false).to_string()
  }

  fn is_input_mode(&self) -> bool {
    true
  }

  fn pending_seq(&self) -> Option<String> {
    Some(self.pending_cmd.editor.buf.joined())
  }

  fn pending_cursor(&self) -> Option<usize> {
    Some(self.pending_cmd.editor.buf.cursor_to_flat())
  }

  fn clamp_cursor(&self) -> bool {
    self.pending_cmd.editor.mode.clamp_cursor()
  }

  fn report_mode(&self) -> super::ModeReport {
    ModeReport::Ex
  }
}

fn parse_ex_input(input: &str) -> ExP<ExNode> {
  let lexer = ExLexer::new(input);
  let tokens = lexer.lex();
  let parser = ExParser::new(tokens);
  parser.parse()
}

bitflags! {
  struct ExLexFlags: u32 {
    const LEX_UNFINISHED = 1 << 0;
  }
}

const COMMANDS: &[(&str, ExTkRule)] = &[
  ("substitute", ExTkRule::Substitute),
  ("global", ExTkRule::Global),
  ("normal", ExTkRule::Normal),
  ("delete", ExTkRule::Delete),
  ("yank", ExTkRule::Yank),
  ("put", ExTkRule::Put),
  ("edit", ExTkRule::Edit),
  ("read", ExTkRule::Read),
  ("write", ExTkRule::Write),
  ("stash", ExTkRule::Stash),
  ("quit", ExTkRule::Quit),
  ("help", ExTkRule::Help),
];

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub enum ExTkRule {
  Number,
  Dot,
  Dollar,
  Percent,
  Comma,
  Offset,
  Mark,
  Bang,
  Append,
  NormalSeq,
  Pattern,
  PatternRev,
  Argument,

  Substitute,
  Global,
  Normal,
  Delete,
  Yank,
  Put,
  Edit,
  Read,
  Write,
  Stash,
  Quit,
  Help,
  Command,

  ShellTk(Tk), // delegate to LexStream after '!' appears
}

impl ExTkRule {
  pub fn is_addr(&self) -> bool {
    matches!(
      self,
      ExTkRule::Number
        | ExTkRule::Dot
        | ExTkRule::Dollar
        | ExTkRule::Percent
        | ExTkRule::Comma
        | ExTkRule::Offset
        | ExTkRule::Mark
        | ExTkRule::Pattern
    )
  }
  pub fn is_command(&self) -> bool {
    matches!(
      self,
      ExTkRule::Substitute
        | ExTkRule::Global
        | ExTkRule::Normal
        | ExTkRule::Delete
        | ExTkRule::Yank
        | ExTkRule::Put
        | ExTkRule::Edit
        | ExTkRule::Read
        | ExTkRule::Write
        | ExTkRule::Stash
        | ExTkRule::Quit
        | ExTkRule::Help
        | ExTkRule::Bang
        | ExTkRule::Command
    )
  }
}

#[derive(Debug, Clone)]
pub struct ExTk {
  class: ExTkRule,
  span: Span,
}

impl From<Tk> for ExTk {
  fn from(value: Tk) -> Self {
    let span = value.span.clone();
    ExTk {
      class: ExTkRule::ShellTk(value),
      span,
    }
  }
}

pub struct ExLexer<'a> {
  input: Rc<str>,
  chars: Peekable<CharIndices<'a>>,
  tokens: Vec<ExTk>,

  flags: ExLexFlags,
}

impl<'a> ExLexer<'a> {
  pub fn new(input: &'a str) -> Self {
    Self {
      input: input.into(),
      chars: input.char_indices().peekable(),
      tokens: vec![],
      flags: ExLexFlags::empty(),
    }
  }
  pub fn lex(mut self) -> Vec<ExTk> {
    if self.chars.peek().is_none() {
      return self.tokens;
    }

    self.ignoring_ws(&[Self::parse_address, Self::parse_cmd_name, Self::parse_args]);

    self.tokens
  }
  fn skip_whitespace(&mut self) {
    self
      .chars
      .peeking_take_while(|(_, ch)| ch.is_whitespace())
      .for_each(drop);
  }
  fn take_alphabetic(&mut self, start: usize) -> usize {
    let mut end = start;
    while let Some(&(i, ch)) = self.chars.peek()
      && ch.is_ascii_alphabetic()
    {
      self.chars.next();
      end = i + ch.len_utf8();
    }
    end
  }
  fn consume_digits(&mut self, mut end: usize) -> usize {
    while let Some(&(i, ch)) = self.chars.peek()
      && ch.is_ascii_digit()
    {
      self.chars.next();
      end = i + ch.len_utf8();
    }
    end
  }
  fn consume_pattern(&mut self, mut end: usize, delim: char) -> usize {
    match_loop!(self.chars.next() => (i,ch) => ch, {
      '\\' => {
        let Some(&(j,ch)) = self.chars.peek() else {
          break
        };
        self.chars.next();
        end = j + ch.len_utf8();
      }
      _ if ch == delim => {
        end = i + ch.len_utf8();
        return end
      }
      _ => end = i + ch.len_utf8(),
    });

    self.flags |= ExLexFlags::LEX_UNFINISHED;
    end
  }
  fn peek_pos(&mut self) -> Option<usize> {
    self.chars.peek().map(|(i, _)| *i)
  }
  fn peeking_next<F>(&mut self, accept: F) -> Option<(usize, char)>
  where
    F: FnOnce(&char) -> bool,
  {
    let &(_, ch) = self.chars.peek()?;
    accept(&ch).then(|| self.chars.next()).flatten()
  }
  fn get_span(&self, range: Range<usize>) -> Option<Span> {
    self.input.get(range.clone())?;

    Some(Span::new(range, self.input.clone()))
  }
  fn is_addr_opener(ch: &(usize, char)) -> bool {
    let ch = ch.1;
    ch.is_ascii_digit() || ".$+-'/?".contains(ch)
  }
  fn get_token(&self, range: Range<usize>, rule: ExTkRule) -> Option<ExTk> {
    let span = self.get_span(range)?;
    Some(ExTk { class: rule, span })
  }
  fn get_pattern_token(&self, range: Range<usize>) -> Option<ExTk> {
    // strip delimiter chars
    let span = self.get_span(range.start + 1..range.end.saturating_sub(1))?;
    Some(ExTk {
      class: ExTkRule::Pattern,
      span,
    })
  }
  /// Lets us skip whitespace between every individual parsing step.
  ///
  /// Takes a function pointer array and calls self.skip_whitespace() between
  /// function call.
  ///
  /// Panics if `funcs` is empty.
  fn ignoring_ws<T>(&mut self, funcs: &[fn(&mut Self) -> T]) -> T {
    debug_assert!(!funcs.is_empty());
    self.skip_whitespace();
    let mut res = None;
    for f in funcs {
      res = Some(f(self));
      self.skip_whitespace();
    }

    // if funcs is not empty, this is safe
    // and funcs is not empty as per the assertion above
    res.unwrap()
  }
  fn command(&self) -> Option<&ExTk> {
    self
      .tokens
      .iter()
      .rev()
      .find(|tk| tk.class.is_command() && !matches!(tk.class, ExTkRule::Bang))
      .or_else(|| {
        self
          .tokens
          .iter()
          .rev()
          .find(|tk| matches!(tk.class, ExTkRule::Bang))
      })
  }
  fn parse_substitute_arg(&mut self) {
    let Some((start, first)) = self.chars.peeking_next(|(_, ch)| !ch.is_alphanumeric()) else {
      return;
    };
    let delim = first;

    let end_before = self.consume_pattern(start, delim);
    let Some(before) = self.get_pattern_token(start..end_before) else {
      return;
    };
    self.tokens.push(before);

    if self.chars.peek().is_none() {
      return;
    }

    let end_after = self.consume_pattern(end_before, delim);
    let Some(after) = self.get_pattern_token(end_before.saturating_sub(1)..end_after) else {
      return;
    };
    self.tokens.push(after);

    if self.chars.peek().is_none() {
      return;
    }

    let end_flags = self.take_alphabetic(end_after);
    let Some(flags) = self.get_token(end_after..end_flags, ExTkRule::Argument) else {
      return;
    };
    self.tokens.push(flags);
  }
  fn parse_global_arg(&mut self) {
    let Some((start, first)) = self.chars.peeking_next(|(_, ch)| !ch.is_alphanumeric()) else {
      return;
    };
    let delim = first;

    let end_before = self.consume_pattern(start, delim);
    let Some(before) = self.get_pattern_token(start..end_before) else {
      return;
    };
    self.tokens.push(before);

    if self.chars.peek().is_none() {
      return;
    }

    // recursively parse the command
    self.ignoring_ws(&[Self::parse_address, Self::parse_cmd_name, Self::parse_args])
  }
  fn parse_normal_seq(&mut self) {
    // everything after this is parsed as a single literal arg
    let Some((start, ch)) = self.chars.next() else {
      return;
    };
    let mut end = start + ch.len_utf8();
    while let Some((i, ch)) = self.chars.next() {
      end = i + ch.len_utf8();
    }
    if let Some(tk) = self.get_token(start..end, ExTkRule::NormalSeq) {
      self.tokens.push(tk);
    }
  }
  fn parse_args(&mut self) {
    let Some(cmd) = self.command() else { return };

    match cmd.class.clone() {
      ExTkRule::Substitute => self.parse_substitute_arg(),
      ExTkRule::Global => self.parse_global_arg(),
      ExTkRule::Normal => self.parse_normal_seq(),
      ExTkRule::Bang => self.parse_shell_cmd(),
      rule => {
        // Some command like 'edit' or 'write' or something that just expects words
        // These are subject to shell expansion, so we use LexStream for these
        let Some(start_pos) = self.peek_pos() else {
          return;
        };
        let is_write = matches!(rule, ExTkRule::Write);
        let rest = self.chars.by_ref().map(|(_, ch)| ch).collect::<String>();
        let stream = LexStream::new(rest.into(), LexFlags::LEX_UNFINISHED)
          .filter_map(Result::ok)
          .filter_map(|tk| tk.filter_meta().then_some(tk))
          .map(ExTk::from);
        let mut pushed = false;

        for mut tk in stream {
          let inner = tk.span.range();
          tk.span = Span::new(
            (inner.start + start_pos)..(inner.end + start_pos),
            self.input.clone(),
          );
          if !pushed && is_write && tk.span.as_str() == ">>" {
            tk.class = ExTkRule::Append;
          }
          self.tokens.push(tk);
          pushed = true;
        }
      }
    }
  }
  fn classify_cmd(name: &str) -> ExTkRule {
    if name.is_empty() {
      return ExTkRule::Command;
    }
    for (full, rule) in COMMANDS {
      if full.starts_with(name) {
        return (*rule).clone();
      }
    }
    ExTkRule::Command
  }
  fn parse_cmd_name(&mut self) {
    let Some(&(start, first)) = self.chars.peek() else {
      return;
    };
    if !first.is_ascii_alphabetic() && first != '!' {
      return;
    }

    let mut end = start;
    match_loop!(self.chars.peek() => &(i,ch) => ch, {
      '!' => {
        let cmd_name = &self.input[start..i];
        let cmd_rule = Self::classify_cmd(cmd_name);

        if end > start {
          let Some(tk) = self.get_token(start..end, cmd_rule) else { return };
          self.tokens.push(tk);
        }

        let Some(tk) = self.get_token(i..i+1, ExTkRule::Bang) else { return };
        self.tokens.push(tk);
        self.chars.next();

        return
      }
      _ if ch.is_ascii_alphabetic() || ch == '_' => {
        self.chars.next();
        end = i + ch.len_utf8();
      }
      _ => break
    });

    let cmd_name = &self.input[start..end];
    let cmd_rule = Self::classify_cmd(cmd_name);

    let Some(tk) = self.get_token(start..end, cmd_rule) else {
      return;
    };
    self.tokens.push(tk);
  }
  fn parse_shell_cmd(&mut self) {
    let Some(start_pos) = self.peek_pos() else {
      return;
    };
    let mut rest = String::new();
    while let Some((_, ch)) = self.chars.next() {
      rest.push(ch)
    }

    let stream = LexStream::new(rest.into(), LexFlags::LEX_UNFINISHED)
      .filter_map(Result::ok)
      .filter_map(|tk| tk.filter_meta().then_some(tk))
      .map(ExTk::from);

    for mut tk in stream {
      // The inner LexStream's span points into `rest`; rebuild against the
      // outer input with absolute byte offsets.
      let inner = tk.span.range();
      tk.span = Span::new(
        (inner.start + start_pos)..(inner.end + start_pos),
        self.input.clone(),
      );
      self.tokens.push(tk);
    }
  }
  fn parse_address(&mut self) {
    if let Some((i, _)) = self.chars.peeking_next(|&(_, ch)| ch == '%')
      && let Some(tk) = self.get_token(i..i + 1, ExTkRule::Percent)
    {
      self.tokens.push(tk);
      return;
    }
    if !self.parse_one_addr() {
      return;
    }

    self.ignoring_ws(&[|this| {
      let Some((i, _)) = this.peeking_next(|c| *c == ',') else {
        return;
      };
      let Some(tk) = this.get_token(i..i + 1, ExTkRule::Comma) else {
        return;
      };
      this.tokens.push(tk);
    }]);

    self.parse_one_addr();
  }
  fn parse_one_addr(&mut self) -> bool {
    let Some((start, first)) = self.chars.peeking_next(Self::is_addr_opener) else {
      return false;
    };

    let (end, rule) = match first {
      '.' => (start + 1, ExTkRule::Dot),
      '$' => (start + 1, ExTkRule::Dollar),
      '0'..='9' => {
        let end = self.consume_digits(start + 1);
        (end, ExTkRule::Number)
      }
      '+' | '-' => {
        let end = self.consume_digits(start + 1);
        (end, ExTkRule::Offset)
      }
      '\'' => {
        let Some((i, c)) = self.chars.next() else {
          self.flags |= ExLexFlags::LEX_UNFINISHED;
          return false;
        };
        (i + c.len_utf8(), ExTkRule::Mark)
      }
      ch @ ('/' | '?') => {
        let rule = match ch {
          '?' => ExTkRule::PatternRev,
          _ => ExTkRule::Pattern,
        };
        let end = self.consume_pattern(start + 1, first);
        (end, rule)
      }
      _ => return false, // unreachable probably
    };

    if matches!(rule, ExTkRule::Pattern) {
      if let Some(tk) = self.get_pattern_token(start..end) {
        self.tokens.push(tk);
        true
      } else {
        false
      }
    } else {
      if let Some(tk) = self.get_token(start..end, rule) {
        self.tokens.push(tk);
        true
      } else {
        false
      }
    }
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExNdRule {
  Delete,
  Yank,
  Put(Anchor),
  Quit,

  Edit(Box<[PathBuf]>),
  Read(ReadSrc),
  Write(WriteDest),

  Substitute {
    pat: String,
    repl: String,
    flags: SubFlags,
  },
  Global {
    negated: bool,
    pat: String,
    nested: Box<ExNode>,
  },
  RepeatSubstitute,
  RepeatGlobal,

  Normal {
    seq: String,
  },
  Shell(String),

  Stash(StashArgs),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddressRange {
  Single(LineAddr),
  Range(LineAddr, LineAddr),
}

impl AddressRange {
  pub const fn all_lines() -> Self {
    Self::Range(LineAddr::Number(1), LineAddr::Last)
  }
  pub fn as_motion(&self) -> Motion {
    match self {
      AddressRange::Single(line) => Motion::Line(line.clone()),
      AddressRange::Range(s, e) => Motion::LineRange(s.clone(), e.clone()),
    }
  }
}

impl Default for AddressRange {
  fn default() -> Self {
    Self::Single(LineAddr::Current)
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExNode {
  pub address: Option<AddressRange>,
  pub kind: ExNdRule,
}

pub enum ExParseResult<T> {
  Success(T),
  Error(String),
}
use ExParseResult as ExP;

enum ExInnerParseResult<T> {
  Done(ExParseResult<T>),
  NoMatch,
}
use ExInnerParseResult as ExR;

impl<T> ExInnerParseResult<T> {
  pub fn success(value: T) -> Self {
    ExInnerParseResult::Done(ExP::Success(value))
  }
  pub fn error(msg: String) -> Self {
    ExInnerParseResult::Done(ExP::Error(msg))
  }
}

enum ExInnerPartialParseResult<T, P> {
  Full(T),
  Partial(P),
}
use ExInnerPartialParseResult as ExPR;

#[derive(Debug, Clone)]
pub struct ExParser {
  tokens: Peekable<IntoIter<ExTk>>,
}

impl ExParser {
  pub fn new(tokens: Vec<ExTk>) -> Self {
    let tokens = tokens.into_iter().peekable();
    Self { tokens }
  }
  pub fn parse(mut self) -> ExP<ExNode> {
    let address = match self.parse_address() {
      ExR::Done(ExP::Success(addr)) => Some(addr),
      ExR::Done(ExP::Error(msg)) => return ExP::Error(msg),
      ExR::NoMatch => None,
    };

    let kind = match self.parse_command() {
      ExR::Done(ExP::Success(cmd)) => cmd,
      ExR::Done(ExP::Error(msg)) => return ExP::Error(msg),
      ExR::NoMatch => return ExP::Error("expected command".into()),
    };

    ExP::Success(ExNode { address, kind })
  }

  fn parse_address(&mut self) -> ExR<AddressRange> {
    let Some(tk) = self.tokens.peeking_next(|tk| tk.class.is_addr()) else {
      return ExR::NoMatch;
    };

    let resolved = match Self::parse_one_address(&tk) {
      ExPR::Partial(addr) => addr,
      ExPR::Full(motion) => return ExR::success(motion),
    };

    if self
      .tokens
      .peeking_next(|tk| matches!(tk.class, ExTkRule::Comma))
      .is_none()
    {
      return ExR::success(AddressRange::Single(resolved));
    };

    let Some(end_tk) = self.tokens.peeking_next(|tk| tk.class.is_addr()) else {
      return ExR::error("expected second address after ','".into());
    };

    let resolved_end = match Self::parse_one_address(&end_tk) {
      ExPR::Partial(addr) => addr,
      ExPR::Full(motion) => return ExR::success(motion),
    };

    ExR::success(AddressRange::Range(resolved, resolved_end))
  }
  fn parse_one_address(tk: &ExTk) -> ExPR<AddressRange, LineAddr> {
    match tk.class {
      ExTkRule::Number => {
        let addr = tk.span.as_str().parse::<usize>().unwrap_or(1);
        ExPR::Partial(LineAddr::Number(addr))
      }
      ExTkRule::Dot => ExPR::Partial(LineAddr::Current),
      ExTkRule::Dollar => ExPR::Partial(LineAddr::Last),
      ExTkRule::Percent => ExPR::Full(AddressRange::all_lines()),
      ExTkRule::Offset => {
        let s = tk
          .span
          .as_str()
          .strip_prefix("+")
          .unwrap_or(tk.span.as_str());

        let offset = s.parse::<isize>().unwrap_or(1);
        ExPR::Partial(LineAddr::Offset(offset))
      }
      ExTkRule::Mark => {
        let mark_name = tk.span.as_str().chars().nth(1).unwrap();

        ExPR::Partial(LineAddr::Mark(mark_name))
      }
      ExTkRule::Pattern => {
        let pat = tk.span.as_str().to_string();
        ExPR::Partial(LineAddr::Pattern(pat))
      }
      ExTkRule::PatternRev => {
        let pat = tk.span.as_str().to_string();
        ExPR::Partial(LineAddr::PatternRev(pat))
      }
      _ => unreachable!(),
    }
  }
  fn parse_command(&mut self) -> ExR<ExNdRule> {
    let Some(tk) = self.tokens.peeking_next(|tk| tk.class.is_command()) else {
      return ExR::NoMatch;
    };

    match &tk.class {
      ExTkRule::Read | ExTkRule::Write => self.parse_read_write(&tk.class),
      ExTkRule::Substitute => self.parse_substitute(),
      ExTkRule::Global => self.parse_global(),
      ExTkRule::Normal => self.parse_normal(),
      ExTkRule::Edit => self.parse_edit(),
      ExTkRule::Stash => self.parse_stash(),
      ExTkRule::Help => self.parse_help(),
      ExTkRule::Bang => self.parse_shell(),
      ExTkRule::Delete => ExR::success(ExNdRule::Delete),
      ExTkRule::Yank => ExR::success(ExNdRule::Yank),
      ExTkRule::Put => ExR::success(ExNdRule::Put(Anchor::After)),
      ExTkRule::Quit => ExR::success(ExNdRule::Quit),
      ExTkRule::Command => ExR::error(format!("not an editor command: {}", tk.span.as_str())),
      _ => unreachable!(),
    }
  }
  fn parse_shell(&mut self) -> ExR<ExNdRule> {
    let mut args = vec![];
    while let Some(arg) = self.tokens.next() {
      // wrap in Tk so we can use get_span()
      args.push(Tk::new(lex::TkRule::Str, arg.span));
    }
    let args_raw = args
      .get_span() // extract total span of arg tokens
      .map(|s| s.as_str().to_string())
      .unwrap_or_default();

    ExR::success(ExNdRule::Shell(args_raw))
  }
  fn parse_help(&mut self) -> ExR<ExNdRule> {
    let mut args = vec![];
    while let Some(arg) = self.tokens.next() {
      args.push(Tk::new(lex::TkRule::Str, arg.span));
    }
    let args_raw = args
      .get_span() // extract total span of arg tokens
      .map(|s| s.as_str().to_string());

    let cmd = if let Some(args) = args_raw {
      ["help", &args].join(" ")
    } else {
      "help".to_string()
    };

    ExR::success(ExNdRule::Shell(cmd))
  }
  fn parse_normal(&mut self) -> ExR<ExNdRule> {
    self
      .tokens
      .peeking_next(|tk| matches!(tk.class, ExTkRule::Bang));

    let Some(tk) = self
      .tokens
      .peeking_next(|tk| matches!(tk.class, ExTkRule::NormalSeq))
    else {
      return ExR::error("expected normal command sequence after 'normal'".into());
    };

    let seq = tk.span.as_str().to_string();
    ExR::success(ExNdRule::Normal { seq })
  }
  fn parse_stash(&mut self) -> ExR<ExNdRule> {
    let arg_names = ["pop", "drop", "apply", "insert", "swap", "list"];
    let arg = self.tokens.next().map(|tk| tk.span.as_str().to_string());
    if arg.is_none() {
      return ExR::success(ExNdRule::Stash(StashArgs::Push(None)));
    } else if !arg_names
      .iter()
      .any(|name| name.starts_with(arg.as_ref().unwrap()))
    {
      return ExR::success(ExNdRule::Stash(StashArgs::Push(arg)));
    }

    let name = self.tokens.next().map(|tk| tk.span.as_str().to_string());
    let arg = arg.unwrap();
    match arg.as_str() {
      _ if arg.starts_with("pop") => ExR::success(ExNdRule::Stash(StashArgs::Pop(name))),
      _ if arg.starts_with("drop") => ExR::success(ExNdRule::Stash(StashArgs::Drop(name))),
      _ if arg.starts_with("apply") => ExR::success(ExNdRule::Stash(StashArgs::Apply(name))),
      _ if arg.starts_with("insert") => ExR::success(ExNdRule::Stash(StashArgs::Insert(name))),
      _ if arg.starts_with("swap") => ExR::success(ExNdRule::Stash(StashArgs::Swap(name))),
      _ if arg.starts_with("list") => {
        let target = name
          .map(|n| match n.as_str() {
            _ if "stack".starts_with(n.trim()) => Ok(Some(StashListArg::Stack)),
            _ if "named".starts_with(n.trim()) => Ok(Some(StashListArg::Named)),
            _ => Err(ExR::error(format!("invalid stash list argument: {}", n))),
          })
          .transpose();
        let target = match target {
          Ok(t) => t,
          Err(e) => return e,
        };

        ExR::success(ExNdRule::Stash(StashArgs::List(target.flatten())))
      }
      _ => ExR::error(format!("invalid stash argument: {}", arg)),
    }
  }
  fn parse_substitute(&mut self) -> ExR<ExNdRule> {
    let Some(pat) = self
      .tokens
      .peeking_next(|tk| matches!(tk.class, ExTkRule::Pattern))
    else {
      // Bare `:s` with no arguments — repeat the last substitute
      return ExR::success(ExNdRule::RepeatSubstitute);
    };
    let Some(repl) = self
      .tokens
      .peeking_next(|tk| matches!(tk.class, ExTkRule::Pattern))
    else {
      return ExR::error("expected replacement argument for substitute command".into());
    };
    let flags_tk = self
      .tokens
      .peeking_next(|tk| matches!(tk.class, ExTkRule::Argument));
    let pat = pat.span.as_str().to_string();
    let repl = repl.span.as_str().to_string();

    let mut flags = SubFlags::empty();
    if let Some(flags_tk) = flags_tk {
      let flags_str = flags_tk.span.as_str();
      for ch in flags_str.chars() {
        match ch {
          'g' => flags |= SubFlags::GLOBAL,
          'c' => flags |= SubFlags::CONFIRM,
          'i' => flags |= SubFlags::IGNORE_CASE,
          'I' => flags |= SubFlags::NO_IGNORE_CASE,
          'n' => flags |= SubFlags::SHOW_COUNT,
          'p' => flags |= SubFlags::PRINT_RESULT,
          '#' => flags |= SubFlags::PRINT_NUMBERED,
          'l' => flags |= SubFlags::PRINT_LEFT_ALIGN,
          _ => return ExR::error(format!("invalid substitute flag: {}", ch)),
        }
      }
    }

    ExR::success(ExNdRule::Substitute { pat, repl, flags })
  }
  fn parse_global(&mut self) -> ExR<ExNdRule> {
    let negated = self
      .tokens
      .peeking_next(|tk| matches!(tk.class, ExTkRule::Bang))
      .is_some();
    let Some(pat) = self
      .tokens
      .peeking_next(|tk| matches!(tk.class, ExTkRule::Pattern))
    else {
      // Bare `:g` with no arguments — repeat the last global
      return ExR::success(ExNdRule::RepeatGlobal);
    };

    // drain tokens into this
    let mut rest = vec![];
    while let Some(tk) = self.tokens.next() {
      rest.push(tk);
    }

    let nested_parser = ExParser::new(rest);
    let sub_node = match nested_parser.parse() {
      ExP::Success(cmd) => cmd,
      ExP::Error(msg) => return ExR::error(msg),
    };

    ExR::success(ExNdRule::Global {
      negated,
      pat: pat.span.as_str().to_string(),
      nested: Box::new(sub_node),
    })
  }
  fn parse_edit(&mut self) -> ExR<ExNdRule> {
    let mut args = vec![];
    while let Some(arg) = self.tokens.next() {
      let path = PathBuf::from(arg.span.as_str().to_string());
      args.push(path);
    }
    let args = args.into_boxed_slice();

    ExR::success(ExNdRule::Edit(args))
  }
  fn parse_read_write(&mut self, rule: &ExTkRule) -> ExR<ExNdRule> {
    let is_read = matches!(rule, ExTkRule::Read);
    let is_shell_arg = self
      .tokens
      .peeking_next(|tk| matches!(tk.class, ExTkRule::Bang))
      .is_some();

    if is_shell_arg {
      let mut args = vec![];
      for tk in &mut self.tokens {
        args.push(Tk::new(lex::TkRule::Str, tk.span.clone()));
      }
      let args_raw = args
        .get_span() // extract total span of arg tokens
        .map(|s| s.as_str().to_string())
        .unwrap_or_default();

      if is_read {
        ExR::success(ExNdRule::Read(ReadSrc::Cmd(args_raw)))
      } else {
        ExR::success(ExNdRule::Write(WriteDest::Cmd(args_raw)))
      }
    } else {
      let Some(mut arg) = self.tokens.next() else {
        return ExR::error(format!(
          "expected {} argument for read command",
          if is_shell_arg { "shell" } else { "file" }
        ));
      };
      let is_append = matches!(arg.class, ExTkRule::Append);
      if is_append && let Some(next) = self.tokens.next() {
        arg = next;
      }
      let arg = arg.span.as_str();

      if is_read {
        ExR::success(ExNdRule::Read(ReadSrc::File(PathBuf::from(arg))))
      } else if is_append {
        ExR::success(ExNdRule::Write(WriteDest::FileAppend(PathBuf::from(arg))))
      } else {
        ExR::success(ExNdRule::Write(WriteDest::File(PathBuf::from(arg))))
      }
    }
  }
}
