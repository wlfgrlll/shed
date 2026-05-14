use crate::{
  readline::editmode::EditMode,
  state::{terminal::CursorStyle, util::write_meta},
};

pub struct RemoteMode;

impl EditMode for RemoteMode {
  fn handle_key(
    &mut self,
    key: crate::keys::KeyEvent,
  ) -> Option<crate::readline::editcmd::EditCmd> {
    write_meta(|m| m.notify_key_event(key)).ok()?;
    None
  }

  fn is_repeatable(&self) -> bool {
    false
  }

  fn as_replay(&self) -> Option<super::CmdReplay> {
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

  fn report_mode(&self) -> super::ModeReport {
    super::ModeReport::Remote
  }
}
