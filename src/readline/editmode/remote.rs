use super::{
  CmdReplay, E as KeyEvent, EditCmd, EditMode, ModeReport, Shed, state::terminal::CursorStyle,
};

pub(crate) struct RemoteMode;

impl EditMode for RemoteMode {
  fn handle_key(&mut self, key: KeyEvent) -> Option<EditCmd> {
    Shed::meta_mut(|m| m.notify_key_event(key)).ok()?;
    None
  }

  fn is_repeatable(&self) -> bool {
    false
  }

  fn as_replay(&self) -> Option<CmdReplay> {
    None
  }

  fn cursor_style(&self) -> String {
    CursorStyle::Beam(false).to_string()
  }

  fn pending_seq(&self) -> Option<String> {
    None
  }

  fn clamp_cursor(&self) -> bool {
    false
  }

  fn report_mode(&self) -> ModeReport {
    super::ModeReport::Remote
  }
}
