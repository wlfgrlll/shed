use itertools::Itertools;
use nix::{errno::Errno, unistd::read};
use std::{
  collections::VecDeque,
  fmt::{Debug, Display},
  os::fd::BorrowedFd,
};

use super::{
  ShErr, ShResult,
  keys::{KeyCode, KeyEvent, ModKeys},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Rows(pub(crate) usize);
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Cols(pub(crate) usize);

#[derive(Debug, Clone)]
pub(crate) enum TermEvent {
  Key(KeyEvent),
  CursorPos(Rows, Cols),
  XtVersion(XtVersion),
  PrimaryDevAttr,
  KittyKbdFlags,
  Capabilities {
    name: String,
    _value: Option<String>,
  },
}

#[derive(Debug, Clone, Copy)]
enum DcsKind {
  XtGetCap,
  XtVersion,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct SemVer {
  major: Option<u32>,
  minor: Option<u32>,
  patch: Option<u32>,
}

impl Display for SemVer {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match (self.major, self.minor, self.patch) {
      (Some(major), Some(minor), Some(patch)) => write!(f, "{}.{}.{}", major, minor, patch),
      (Some(major), Some(minor), None) => write!(f, "{}.{}", major, minor),
      (Some(major), None, None) => write!(f, "{}", major),
      _ => write!(f, "unknown"),
    }
  }
}

macro_rules! semver {
  ($major:expr) => {
    SemVer {
      major: Some($major),
      minor: None,
      patch: None,
    }
  };
  ($major:expr, $minor:expr) => {
    SemVer {
      major: Some($major),
      minor: Some($minor),
      patch: None,
    }
  };
  ($major:expr, $minor:expr, $patch:expr) => {
    SemVer {
      major: Some($major),
      minor: Some($minor),
      patch: Some($patch),
    }
  };
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub(crate) enum XtVersion {
  Iterm2(SemVer),
  Tmux(SemVer),
  WezTerm,
  Unknown(String),
}

#[allow(dead_code)]
impl XtVersion {
  pub fn parse(raw: &str) -> Self {
    Self::parse_iterm2(raw)
      .or_else(|| Self::parse_tmux(raw))
      .or_else(|| Self::parse_wezterm(raw))
      .unwrap_or_else(|| Self::Unknown(raw.to_string()))
  }

  pub fn has_broken_kitty_kbd(&self) -> bool {
    let Self::Iterm2(ver) = self else {
      return false;
    };

    *ver < semver!(3, 5, 12)
  }

  pub fn needs_wezterm_workaround(&self) -> bool {
    matches!(self, Self::WezTerm)
  }

  pub fn supports_color_theme_reporting(&self) -> bool {
    let Self::Tmux(ver) = self else { return true };

    *ver >= semver!(3, 7)
  }

  fn parse_iterm2(raw: &str) -> Option<Self> {
    let (name, rest) = raw.split_once(' ')?;
    if name != "iTerm2" {
      return None;
    }
    let mut parts = rest.split('.');
    let major = parts.next()?.parse::<u32>().ok()?;
    let minor = parts.next()?.parse::<u32>().ok()?;
    let patch = parts.next()?.parse::<u32>().ok()?;
    Some(Self::Iterm2(semver!(major, minor, patch)))
  }

  fn parse_tmux(raw: &str) -> Option<Self> {
    let (name, rest) = raw.split_once(' ')?;
    if name != "tmux" {
      return None;
    }
    let mut parts = rest.split('.');
    // tmux minor may carry a trailing letter (e.g. "3.5a"); take leading digits only.
    let major = parse_leading_u32(parts.next()?)?;
    let minor = parse_leading_u32(parts.next()?)?;
    Some(Self::Tmux(semver!(major, minor)))
  }

  fn parse_wezterm(raw: &str) -> Option<Self> {
    raw.starts_with("WezTerm ").then_some(Self::WezTerm)
  }
}

fn parse_leading_u32(s: &str) -> Option<u32> {
  let end = s
    .bytes()
    .position(|b| !b.is_ascii_digit())
    .unwrap_or(s.len());
  if end == 0 {
    return None;
  }
  s[..end].parse().ok()
}

#[derive(Debug, Default, Clone)]
struct EventParser {
  events: VecDeque<TermEvent>,
  ss3_pending: bool,
  dcs_buf: Option<String>,
  dcs_kind: Option<DcsKind>,
  dcs_supported: bool,
}

impl EventParser {
  pub fn new() -> Self {
    Self {
      events: VecDeque::new(),
      ss3_pending: false,
      dcs_buf: None,
      dcs_kind: None,
      dcs_supported: false,
    }
  }

  pub fn push(&mut self, event: TermEvent) {
    self.events.push_back(event);
  }

  pub fn pop(&mut self) -> Option<TermEvent> {
    self.events.pop_front()
  }

  pub fn parse_term_cap(&mut self) {
    let Some(buf) = self.dcs_buf.take() else {
      log::trace!("parse_term_cap: no dcs_buf, skipping");
      return;
    };
    let supported = self.dcs_supported;
    self.dcs_kind = None;
    self.dcs_supported = false;

    // Only emit when the terminal reported the cap as supported.
    if !supported {
      log::debug!("XTGETTCAP response: cap unsupported (raw={buf:?})");
      return;
    }

    let (name_hex, value_hex) = match buf.split_once('=') {
      Some((n, v)) => (n, Some(v)),
      None => (buf.as_str(), None),
    };
    let Some(name) = Self::decode_hex(name_hex) else {
      log::debug!("XTGETTCAP response: failed to decode name hex {name_hex:?}");
      return;
    };
    let _value = value_hex.and_then(Self::decode_hex);

    log::debug!("XTGETTCAP response: name={name:?} value={_value:?}");
    self.push(TermEvent::Capabilities { name, _value });
  }

  pub fn parse_xtversion(&mut self) {
    let Some(buf) = self.dcs_buf.take() else {
      log::trace!("parse_xtversion: no dcs_buf, skipping");
      return;
    };
    self.dcs_kind = None;
    let xtver = XtVersion::parse(&buf);
    log::debug!("XTVERSION response: raw={buf:?} parsed={xtver:?}");
    self.push(TermEvent::XtVersion(xtver));
  }

  pub fn decode_hex(hex: &str) -> Option<String> {
    if !hex.len().is_multiple_of(2) {
      return None; // Invalid hex string
    }

    let bytes: Option<Vec<u8>> = hex
      .chars()
      .chunks(2)
      .into_iter()
      .map(|chunk| {
        let s: String = chunk.collect();
        u8::from_str_radix(&s, 16).ok()
      })
      .collect();
    bytes.map(|b| String::from_utf8_lossy(&b).into_owned())
  }
}

impl vte::Perform for EventParser {
  #[allow(clippy::single_match)]
  fn hook(&mut self, params: &vte::Params, intermediates: &[u8], _ignore: bool, action: char) {
    log::trace!("DCS hook: params={params:?}, intermediates={intermediates:?}, action={action:?}");
    let params: Vec<u16> = params
      .iter()
      .map(|p| p.first().copied().unwrap_or(0))
      .collect();

    match (intermediates, action) {
      ([b'+'], 'r') => {
        let first = params.first().copied().unwrap_or(0);
        self.dcs_supported = first == 1;
        self.dcs_kind = Some(DcsKind::XtGetCap);

        self.dcs_buf = Some(String::new());
        log::trace!(
          "DCS hook: XTGETTCAP introducer (supported={})",
          self.dcs_supported
        );
      }
      ([b'>'], '|') => {
        self.dcs_kind = Some(DcsKind::XtVersion);
        self.dcs_buf = Some(String::new());
        log::trace!("DCS hook: XTVERSION introducer");
      }

      _ => {
        log::trace!("DCS hook: unrecognized introducer ({intermediates:?}, {action:?})");
      }
    }
  }

  fn put(&mut self, byte: u8) {
    if let Some(buf) = self.dcs_buf.as_mut() {
      buf.push(byte as char);
    }
  }

  fn unhook(&mut self) {
    let Some(kind) = self.dcs_kind else { return };
    match kind {
      DcsKind::XtGetCap => self.parse_term_cap(),
      DcsKind::XtVersion => self.parse_xtversion(),
    }
  }

  fn print(&mut self, c: char) {
    // vte routes 0x7f (DEL) to print instead of execute
    if self.ss3_pending {
      self.ss3_pending = false;
      match c {
        'A' => {
          self.push(TermEvent::Key(KeyEvent(KeyCode::Up, ModKeys::empty())));
          return;
        }
        'B' => {
          self.push(TermEvent::Key(KeyEvent(KeyCode::Down, ModKeys::empty())));
          return;
        }
        'C' => {
          self.push(TermEvent::Key(KeyEvent(KeyCode::Right, ModKeys::empty())));
          return;
        }
        'D' => {
          self.push(TermEvent::Key(KeyEvent(KeyCode::Left, ModKeys::empty())));
          return;
        }
        'H' => {
          self.push(TermEvent::Key(KeyEvent(KeyCode::Home, ModKeys::empty())));
          return;
        }
        'F' => {
          self.push(TermEvent::Key(KeyEvent(KeyCode::End, ModKeys::empty())));
          return;
        }
        'P' => {
          self.push(TermEvent::Key(KeyEvent(KeyCode::F(1), ModKeys::empty())));
          return;
        }
        'Q' => {
          self.push(TermEvent::Key(KeyEvent(KeyCode::F(2), ModKeys::empty())));
          return;
        }
        'R' => {
          self.push(TermEvent::Key(KeyEvent(KeyCode::F(3), ModKeys::empty())));
          return;
        }
        'S' => {
          self.push(TermEvent::Key(KeyEvent(KeyCode::F(4), ModKeys::empty())));
          return;
        }
        _ => {}
      }
    }

    if c == '\x7f' {
      self.push(TermEvent::Key(KeyEvent(
        KeyCode::Backspace,
        ModKeys::empty(),
      )));
    } else {
      self.push(TermEvent::Key(KeyEvent(KeyCode::Char(c), ModKeys::empty())));
    }
  }

  fn execute(&mut self, byte: u8) {
    log::trace!("execute: {byte:#04x}");
    let event = match byte {
      0x00 => TermEvent::Key(KeyEvent(KeyCode::Char(' '), ModKeys::CTRL)), // Ctrl+Space / Ctrl+@
      0x09 => TermEvent::Key(KeyEvent(KeyCode::Tab, ModKeys::empty())),    // Tab (Ctrl+I)
      0x0a => TermEvent::Key(KeyEvent(KeyCode::Char('j'), ModKeys::CTRL)), // Ctrl+J (linefeed)
      0x0d => TermEvent::Key(KeyEvent(KeyCode::Enter, ModKeys::empty())), // Carriage return (Ctrl+M)
      0x1b => TermEvent::Key(KeyEvent(KeyCode::Esc, ModKeys::empty())),
      0x7f => TermEvent::Key(KeyEvent(KeyCode::Backspace, ModKeys::empty())),
      0x01..=0x1a => {
        // Ctrl+A through Ctrl+Z (excluding special cases above)
        let c = (b'a' + byte - 1) as char;
        TermEvent::Key(KeyEvent(KeyCode::Char(c), ModKeys::CTRL))
      }
      _ => return,
    };
    self.push(event);
  }

  fn csi_dispatch(
    &mut self,
    params: &vte::Params,
    intermediates: &[u8],
    _ignore: bool,
    action: char,
  ) {
    log::trace!(
      "CSI dispatch: params={params:?}, intermediates={intermediates:?}, action={action:?}"
    );
    let params: Vec<u16> = params
      .iter()
      .map(|p| p.first().copied().unwrap_or(0))
      .collect();

    let event = match (intermediates, action) {
      // Arrow keys: CSI A/B/C/D or CSI 1;mod A/B/C/D
      ([], 'R') => {
        let row = params.first().copied().unwrap_or(0) as usize;
        let col = params.get(1).copied().unwrap_or(0) as usize;
        TermEvent::CursorPos(Rows(row), Cols(col))
      }
      ([], 'A') => {
        let mods = params.get(1).map(ModKeys::from).unwrap_or(ModKeys::empty());
        TermEvent::Key(KeyEvent(KeyCode::Up, mods))
      }
      ([], 'B') => {
        let mods = params.get(1).map(ModKeys::from).unwrap_or(ModKeys::empty());
        TermEvent::Key(KeyEvent(KeyCode::Down, mods))
      }
      ([], 'C') => {
        let mods = params.get(1).map(ModKeys::from).unwrap_or(ModKeys::empty());
        TermEvent::Key(KeyEvent(KeyCode::Right, mods))
      }
      ([], 'D') => {
        let mods = params.get(1).map(ModKeys::from).unwrap_or(ModKeys::empty());
        TermEvent::Key(KeyEvent(KeyCode::Left, mods))
      }
      // Home/End: CSI H/F or CSI 1;mod H/F
      ([], 'H') => {
        let mods = params.get(1).map(ModKeys::from).unwrap_or(ModKeys::empty());
        TermEvent::Key(KeyEvent(KeyCode::Home, mods))
      }
      ([], 'F') => {
        let mods = params.get(1).map(ModKeys::from).unwrap_or(ModKeys::empty());
        TermEvent::Key(KeyEvent(KeyCode::End, mods))
      }
      // Shift+Tab: CSI Z
      ([], 'Z') => TermEvent::Key(KeyEvent(KeyCode::Tab, ModKeys::SHIFT)),
      // Special keys with tilde: CSI num ~ or CSI num;mod ~
      ([], '~') => {
        let key_num = params.first().copied().unwrap_or(0);
        let mods = params.get(1).map(ModKeys::from).unwrap_or(ModKeys::empty());
        let key = match key_num {
          1 | 7 => KeyCode::Home,
          2 => KeyCode::Insert,
          3 => KeyCode::Delete,
          4 | 8 => KeyCode::End,
          5 => KeyCode::PageUp,
          6 => KeyCode::PageDown,
          15 => KeyCode::F(5),
          17 => KeyCode::F(6),
          18 => KeyCode::F(7),
          19 => KeyCode::F(8),
          20 => KeyCode::F(9),
          21 => KeyCode::F(10),
          23 => KeyCode::F(11),
          24 => KeyCode::F(12),
          200 => KeyCode::BracketedPasteStart,
          201 => KeyCode::BracketedPasteEnd,
          _ => return,
        };
        TermEvent::Key(KeyEvent(key, mods))
      }
      ([], 'u') => {
        // kitty keyboard protocol: CSI code;mod;text u
        let codepoint = params.first().copied().unwrap_or(0);
        let mods = params.get(1).map(ModKeys::from).unwrap_or(ModKeys::empty());
        let text = params.get(2).copied().unwrap_or(codepoint);

        let (ch, mods) = if text != codepoint && mods.contains(ModKeys::SHIFT) {
          // Kitty reported something like 'Shift+7' and text is '&'
          // So we remove the SHIFT modifier and use the actual text

          (text, mods & !ModKeys::SHIFT)
        } else {
          (codepoint, mods)
        };

        let key = match ch {
          9 => KeyCode::Tab,
          13 => KeyCode::Enter,
          27 => KeyCode::Esc,
          127 => KeyCode::Backspace,
          _ => {
            if let Some(ch) = char::from_u32(codepoint as u32) {
              KeyCode::Char(ch)
            } else {
              return;
            }
          }
        };
        TermEvent::Key(KeyEvent(key, mods))
      }
      ([b'?'], 'c') => TermEvent::PrimaryDevAttr,
      ([b'?'], 'u') => TermEvent::KittyKbdFlags,
      // SGR mouse: CSI < button;x;y M/m (ignore mouse events for now)
      ([b'<'], dir @ ('M' | 'm')) => {
        if dir == 'm' {
          return;
        } // release event

        let button = params.first().copied().unwrap_or(0);
        match button {
          64 => TermEvent::Key(KeyEvent(KeyCode::ScrollUp, ModKeys::empty())),
          65 => TermEvent::Key(KeyEvent(KeyCode::ScrollDown, ModKeys::empty())),
          128 => TermEvent::Key(KeyEvent(KeyCode::Back, ModKeys::empty())),
          129 => TermEvent::Key(KeyEvent(KeyCode::Forward, ModKeys::empty())),
          _ => {
            let col = params.get(1).copied().unwrap_or(0) as usize;
            let row = params.get(2).copied().unwrap_or(0) as usize;

            match button {
              0 => TermEvent::Key(KeyEvent(KeyCode::LeftClick(row, col), ModKeys::empty())),
              1 => TermEvent::Key(KeyEvent(KeyCode::MiddleClick(row, col), ModKeys::empty())),
              2 => TermEvent::Key(KeyEvent(KeyCode::RightClick(row, col), ModKeys::empty())),
              35 => TermEvent::Key(KeyEvent(KeyCode::MousePos(row, col), ModKeys::empty())),
              _ => {
                // Other mouse events we don't care about
                return;
              }
            }
          }
        }
      }
      _ => return,
    };
    self.push(event);
  }

  fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, byte: u8) {
    log::trace!("ESC dispatch: intermediates={intermediates:?}, byte={byte:#04x}");
    // SS3 sequences
    if byte == b'O' {
      self.ss3_pending = true;
    }
  }
}

pub(crate) struct PollReader {
  parser: vte::Parser,
  collector: EventParser,
  byte_buf: VecDeque<u8>,
  pub verbatim_single: bool,
  pub verbatim: bool,
}

impl Clone for PollReader {
  fn clone(&self) -> Self {
    Self {
      parser: vte::Parser::new(),
      collector: self.collector.clone(),
      byte_buf: self.byte_buf.clone(),
      verbatim_single: self.verbatim_single,
      verbatim: self.verbatim,
    }
  }
}

impl Debug for PollReader {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("PollReader")
      .field("collector", &self.collector)
      .field("byte_buf", &self.byte_buf)
      .field("verbatim_single", &self.verbatim_single)
      .field("verbatim", &self.verbatim)
      .finish()
  }
}

impl PollReader {
  pub fn new() -> Self {
    Self {
      parser: vte::Parser::new(),
      collector: EventParser::new(),
      byte_buf: VecDeque::new(),
      verbatim_single: false,
      verbatim: false,
    }
  }

  pub fn handle_bracket_paste(&mut self) -> Option<KeyEvent> {
    let end_marker = b"\x1b[201~";
    let mut raw = vec![];
    while let Some(byte) = self.byte_buf.pop_front() {
      raw.push(byte);
      if raw.ends_with(end_marker) {
        // Strip the end marker from the raw sequence
        raw.truncate(raw.len() - end_marker.len());
        let paste = String::from_utf8_lossy(&raw).to_string();
        self.verbatim = false;
        return Some(KeyEvent(KeyCode::Verbatim(paste.into()), ModKeys::empty()));
      }
    }

    self.verbatim = true;
    self.byte_buf.extend(raw);
    None
  }

  pub fn read_one_verbatim(&mut self) -> Option<KeyEvent> {
    if self.byte_buf.is_empty() {
      return None;
    }
    let bytes: Vec<u8> = self.byte_buf.drain(..).collect();
    let verbatim_str = String::from_utf8_lossy(&bytes).to_string();
    Some(KeyEvent(
      KeyCode::Verbatim(verbatim_str.into()),
      ModKeys::empty(),
    ))
  }

  pub fn feed_bytes(&mut self, bytes: &[u8]) {
    self.byte_buf.extend(bytes);
  }

  pub fn read(&mut self, fd: BorrowedFd) -> ShResult<usize> {
    let mut buffer = [0u8; 1024];
    match read(fd, &mut buffer) {
      Ok(0) => {
        // EOF
        Err(ShErr::loop_break(0))
      }
      Ok(n) => {
        self.feed_bytes(&buffer[..n]);
        Ok(n)
      }
      Err(Errno::EINTR) => {
        // Interrupted, continue to handle signals
        Err(ShErr::loop_continue(0))
      }
      Err(e) => Err(e.into()),
    }
  }

  pub fn readkey(&mut self) -> Result<Option<KeyEvent>, ShErr> {
    if let Some(TermEvent::Key(event)) = self.read_event()? {
      Ok(Some(event))
    } else {
      Ok(None)
    }
  }

  pub(super) fn read_event(&mut self) -> Result<Option<TermEvent>, ShErr> {
    if self.verbatim_single {
      if let Some(key) = self.read_one_verbatim() {
        self.verbatim_single = false;
        return Ok(Some(TermEvent::Key(key)));
      }
      return Ok(None);
    }
    if self.verbatim {
      if let Some(paste) = self.handle_bracket_paste() {
        return Ok(Some(TermEvent::Key(paste)));
      }
      // If we're in verbatim mode but haven't seen the end marker yet, don't attempt to parse keys
      return Ok(None);
    } else if self.byte_buf.front() == Some(&b'\x1b') {
      if self.byte_buf.len() == 1 {
        // ESC is the only byte - emit standalone Escape
        self.byte_buf.pop_front();
        return Ok(Some(TermEvent::Key(KeyEvent(
          KeyCode::Esc,
          ModKeys::empty(),
        ))));
      }
      match self.byte_buf.get(1) {
        Some(b'[') | Some(b'O') | Some(b'P') | Some(b']') | Some(b'_') => {
          // Valid CSI/SS3/DCS/OSC/APC prefix - fall through to the parser below
        }
        Some(&b) if b >= 0x20 && b != 0x7f => {
          // ESC + printable char - interpret as Alt+<char>
          self.byte_buf.pop_front(); // consume ESC
          self.byte_buf.pop_front(); // consume the char
          let ch = b as char;
          return Ok(Some(TermEvent::Key(KeyEvent(
            KeyCode::Char(ch.to_ascii_uppercase()),
            ModKeys::ALT,
          ))));
        }
        _ => {
          // ESC + non-printable/unknown - emit standalone Escape
          self.byte_buf.pop_front();
          return Ok(Some(TermEvent::Key(KeyEvent(
            KeyCode::Esc,
            ModKeys::empty(),
          ))));
        }
      }
    }
    while let Some(byte) = self.byte_buf.pop_front() {
      self.parser.advance(&mut self.collector, &[byte]);
      if let Some(event) = self.collector.pop() {
        match event {
          TermEvent::Key(KeyEvent(KeyCode::BracketedPasteStart, _)) => {
            if let Some(paste) = self.handle_bracket_paste() {
              return Ok(Some(TermEvent::Key(paste)));
            } else {
              continue;
            }
          }
          _ => return Ok(Some(event)),
        }
      }
    }
    Ok(None)
  }
}

impl Default for PollReader {
  fn default() -> Self {
    Self::new()
  }
}
