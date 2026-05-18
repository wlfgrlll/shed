use std::os::unix::net::UnixStream;

use nix::{
  sys::signal::{Signal, kill},
  unistd::{Pid, write},
};

use crate::{
  Hint, LineData, Lines, ReadlineEvent, ShResult, Shed, ShedLine,
  state::{
    meta::{LineHeader, QueryHeader, SocketRequest, StatusHeader},
    vars::VarKind,
  },
  status_msg, system_msg,
  util::Pos,
  var,
};

pub(super) fn handle_socket_request(
  conn: UnixStream,
  request: SocketRequest,
  readline: &mut ShedLine,
) -> ShResult<Option<ReadlineEvent>> {
  match request {
    SocketRequest::PostSystemMessage(msg) => {
      system_msg!("{msg}");
      write(&conn, b"ok\n").ok();
    }
    SocketRequest::PostStatusMessage(msg) => {
      status_msg!("{msg}");
      write(&conn, b"ok\n").ok();
    }
    SocketRequest::Subscribe => {
      Shed::meta_mut(|m| m.push_subscriber(conn));
    }
    SocketRequest::RefreshPrompt => {
      kill(Pid::this(), Signal::SIGUSR1)?;
      write(&conn, b"ok\n").ok();
    }
    SocketRequest::LineGet(line_header) => {
      let LineData {
        buffer,
        cursor,
        anchor,
        hint,
        mode,
      } = readline.get_line_data();
      match line_header {
        LineHeader::Buffer => {
          write(&conn, buffer.as_bytes()).ok();
          write(&conn, b"\n").ok();
        }
        LineHeader::Cursor => {
          write(&conn, cursor.to_string().as_bytes()).ok();
          write(&conn, b"\n").ok();
        }
        LineHeader::Anchor => {
          if let Some(anchor) = anchor {
            write(&conn, anchor.to_string().as_bytes()).ok();
          }
          write(&conn, b"\n").ok();
        }
        LineHeader::Hint => {
          if let Some(hint) = hint {
            write(&conn, hint.as_bytes()).ok();
          }
          write(&conn, b"\n").ok();
        }
        LineHeader::Mode => {
          write(&conn, mode.to_string().as_bytes()).ok();
          write(&conn, b"\n").ok();
        }
      }
    }
    SocketRequest::LineSet(line_header, value) => match line_header {
      LineHeader::Buffer => {
        let joined = readline.editor().joined();
        let pos = readline.editor().cursor_to_flat();

        readline.editor_mut().edit(|this| {
          this.set_buffer(value.clone());
        });

        readline.history_mut().update_pending_cmd((&joined, pos));

        let hint = readline.history().get_hint();

        readline.editor_mut().set_hint(hint);
        readline.editor_mut().move_cursor_to_end();
        readline.set_needs_redraw(true);
      }
      LineHeader::Cursor => readline.editor_mut().with_hint(|this| {
        if let Some((row, col)) = value.split_once(':')
          && let Ok(row) = row.parse::<usize>()
          && let Ok(col) = col.parse::<usize>()
        {
          this.set_cursor(Pos::new(row, col));
        } else if let Ok(pos) = value.parse::<usize>() {
          this.set_cursor_from_flat(pos);
        }
      }),
      LineHeader::Hint => {
        readline
          .editor_mut()
          .set_hint(Some(Hint::Override(Lines::to_lines(value))));
      }
      LineHeader::Mode => {
        if !readline.try_swap_mode_from_str(&value) {
          return Ok(None);
        }
      }
      LineHeader::Anchor => {
        if let Some((row, col)) = value.split_once(':')
          && let Ok(row) = row.parse::<usize>()
          && let Ok(col) = col.parse::<usize>()
        {
          readline.editor_mut().set_anchor(Pos::new(row, col));
        } else if let Ok(pos) = value.parse::<usize>() {
          readline.editor_mut().set_anchor_from_flat(pos);
        }
      }
    },
    SocketRequest::LineSendKeys(events) => {
      if let Some(event) = readline.replay_keys(events, true)? {
        return Ok(Some(event));
      }
    }
    SocketRequest::Query(query_header) => match query_header {
      QueryHeader::Cwd => {
        let cwd = std::env::current_dir()?.to_string_lossy().to_string();
        write(&conn, cwd.as_bytes()).ok();
        write(&conn, b"\n").ok();
      }
      QueryHeader::GetVar(var) => {
        let var = var!(&var);
        write(&conn, var.as_bytes()).ok();
        write(&conn, b"\n").ok();
      }
      QueryHeader::SetVar(var, val, flags) => {
        Shed::vars_mut(|v| v.set_var(&var, VarKind::Str(val), flags)).ok();
        write(&conn, b"ok\n").ok();
      }
      QueryHeader::Status(headers) => {
        let mut responses = vec![];
        for header in headers {
          match header {
            StatusHeader::ExitCode => responses.push(Shed::get_status().to_string()),
            StatusHeader::CommandName => {
              if let Some(job) = Shed::meta(|m| m.last_job().cloned())
                && let Some(cmd) = job.name()
              {
                responses.push(cmd.to_string());
              } else {
                responses.push("".to_string());
              }
            }
            StatusHeader::Runtime => {
              let Some(dur) = Shed::meta_mut(|m| m.get_time()) else {
                responses.push("".to_string());
                continue;
              };
              responses.push(format!("{}", dur.as_millis()));
            }
            StatusHeader::Pid => {
              let Some(job) = Shed::meta_mut(|m| m.last_job().cloned()) else {
                responses.push("".to_string());
                continue;
              };
              responses.push(
                job
                  .get_pids()
                  .first()
                  .map(|p| p.to_string())
                  .unwrap_or_default(),
              );
            }
            StatusHeader::Pgid => {
              let Some(job) = Shed::meta_mut(|m| m.last_job().cloned()) else {
                responses.push("".to_string());
                continue;
              };
              responses.push(job.pgid().to_string());
            }
          }
        }
        let output = responses.join(" ");
        write(&conn, output.as_bytes()).ok();
        write(&conn, b"\n").ok();
      }
    },
  }
  Ok(None)
}
