use std::cell::Cell;

use super::{ShResult, sherr, system_msg, ui, var};

thread_local! {
  static IN_META_MUT: Cell<bool> = const { Cell::new(false) };
}

pub(crate) fn init() -> ShResult<()> {
  log::set_logger(&Flog).map_err(|e| sherr!(InternalErr, "Failed to set logger: {e}"))?;
  update_log_level();
  Ok(())
}

pub(crate) fn update_log_level() {
  let level_raw = var!("SHED_LOG");
  let level = level_raw
    .parse::<log::LevelFilter>()
    .unwrap_or(log::LevelFilter::Error);
  log::set_max_level(level);
}

struct Flog;
impl log::Log for Flog {
  fn enabled(&self, metadata: &log::Metadata) -> bool {
    metadata.level() <= log::max_level()
  }
  fn log(&self, record: &log::Record) {
    if !self.enabled(record.metadata()) {
      return;
    }

    let re_entering = IN_META_MUT.with(|f| f.replace(true));
    if re_entering {
      return;
    }

    scopeguard::defer! {
      IN_META_MUT.with(|f| f.set(false));
    }

    let level = ui::stylize_loglevel(record.level());
    let args = record.args();

    let line = if let Some(file) = record.file()
      && let Some(line) = record.line()
    {
      format!("[{level} {file}:{line}] {args}")
    } else {
      format!("[{level}] {args}")
    };

    system_msg!("{line}")
  }
  fn flush(&self) {}
}
