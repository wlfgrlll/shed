use std::{collections::HashSet, fs::OpenOptions, io::Write, path::PathBuf};

use itertools::Itertools;
use nix::libc::STDIN_FILENO;
use scopeguard::defer;

use super::{
  Line, Lines, MotionKind, Pos, ShResult, autocmd,
  editcmd::{Anchor, Cmd, EditCmd, ReadSrc, StashArgs, StashListArg, Verb, WriteDest},
  editmode::{AddressRange, ExNdRule, ExNode, SubFlags},
  eval::{
    execute::{exec_int, exec_nonint},
    lex::TkFlags,
  },
  motion, ordered,
  procio::{RedirSet, RedirSpec, capture_command},
  stash::{Stash, StashedCmd},
  state::{Shed, vars::VarFlags, vars::VarKind},
  status_msg, system_msg,
};
use crate::util::{format_size, var_ctx_guard};
use crate::verb;

impl super::LineBuf {
  pub(super) fn dispatch_ex_node(&mut self, cmd: &EditCmd) -> ShResult<()> {
    let EditCmd {
      verb: Some(Cmd(_, Verb::ExCmd(node))),
      ..
    } = cmd
    else {
      return Ok(());
    };
    let ExNode { address, kind } = node;
    let address = address.clone();

    match kind {
      ExNdRule::Delete /*--------------------*/ => self.ex_delete(cmd, address),
      ExNdRule::Yank /*======================*/ => self.ex_yank(cmd, address),
      ExNdRule::Put(anchor) /*---------------*/ => self.ex_put(cmd, *anchor, address),
      ExNdRule::Edit(paths) /*---------------*/ => self.ex_edit(paths),
      ExNdRule::Read(read_src) /*============*/ => self.ex_read(read_src),
      ExNdRule::Write(write_dest) /*---------*/ => self.ex_write(write_dest),
      ExNdRule::RepeatSubstitute /*==========*/ => self.repeat_substitute(cmd),
      ExNdRule::RepeatGlobal /*--------------*/ => self.repeat_global(cmd),
      ExNdRule::Shell(sh_cmd) /*=============*/ => self.ex_shell_cmd(cmd, sh_cmd),
      ExNdRule::Stash(stash_args) /*---------*/ => self.ex_stash(stash_args),
      ExNdRule::Substitute { pat, repl, flags } => self.ex_substitute(cmd, pat, repl, *flags, address.clone()),
      ExNdRule::Global { negated, pat, nested } => self.ex_global(cmd, *negated, pat, nested, address),

      ExNdRule::Normal {..} /*---------------*/ |
      ExNdRule::Quit /*======================*/ => unreachable!(/* handled in readline/mod.rs */),
    }
  }

  fn ex_global(
    &mut self,
    cmd: &EditCmd,
    negated: bool,
    pat: &str,
    nested: &ExNode,
    range: Option<AddressRange>,
  ) -> ShResult<()> {
    log::debug!(
      "ex_global entry: negated={negated} pat={pat:?} range={range:?} nested.kind={:?}",
      nested.kind
    );
    let range = range.unwrap_or_else(AddressRange::all_lines);
    let constraint = range.as_motion();
    log::debug!("ex_global: resolved constraint = {constraint:?}");
    let lines = self.get_matching_lines(&constraint, pat, negated)?;
    log::debug!("ex_global: matched lines = {lines:?}");

    let nested_cmd = EditCmd {
      verb: Some(verb!(Verb::ExCmd(nested.clone()))),
      ..cmd.clone()
    };

    for line in lines.into_iter().rev() {
      log::debug!("ex_global: dispatching nested for row {line}");
      self.set_cursor(Pos { row: line, col: 0 });
      self.dispatch_ex_node(&nested_cmd)?;
    }

    self.last_global = Some(cmd.clone());
    Ok(())
  }

  fn ex_substitute(
    &mut self,
    cmd: &EditCmd,
    old: &str,
    new: &str,
    flags: SubFlags,
    range: Option<AddressRange>,
  ) -> ShResult<()> {
    let line_nums = self.lines_for_address(range.as_ref())?;

    let re = match Shed::meta_mut(|m| m.get_regex(old.to_string())) {
      Ok(re) => re,
      Err(e) => {
        status_msg!("{e}");
        return Ok(());
      }
    };

    // TODO: implement flag logic
    let mut changes: Vec<(usize, Lines)> = vec![];
    let lines = self
      .lines
      .iter()
      .enumerate()
      .filter(|(i, _)| line_nums.contains(i));

    for (i, line) in lines {
      let s = line.to_string();
      let res = if flags.contains(SubFlags::GLOBAL) {
        re.replace_all(&s, new)
      } else {
        re.replace(&s, new)
      };
      let lines = Lines::to_lines(res);
      changes.push((i, lines));
    }

    for (i, change) in changes.into_iter().rev() {
      self.lines.remove(i);
      for (j, new_line) in change.0.into_iter().enumerate() {
        self.lines.insert(i + j, new_line);
      }
    }

    self.last_substitute = Some(cmd.clone());

    Ok(())
  }
  fn ex_stash(&mut self, args: &StashArgs) -> ShResult<()> {
    let Ok(stash) = Stash::new() else {
      status_msg!("Failed to access stash - database unreachable");
      return Ok(());
    };
    match args {
      StashArgs::Push(arg) => {
        if self.is_empty() {
          status_msg!("Buffer is empty, nothing to stash");
          return Ok(());
        }
        let name = arg.clone().filter(|a| !a.trim().is_empty());
        let buffer = self.joined();
        let (s, e) = (self.row(), self.col());

        stash.push(name, &buffer, (s, e))?;
        self.clear_buffer();
        self.clear_hint();
        self.set_cursor(Pos::new(0, 0));
      }
      StashArgs::Pop(arg) => {
        let stack_len = stash.stack_len();
        let idx = arg
          .as_ref()
          .map(|a| a.parse::<usize>())
          .transpose()
          .ok()
          .flatten()
          .unwrap_or(stack_len.saturating_sub(1));

        let StashedCmd {
          name: _,
          buffer,
          cursor_pos,
        } = match stash.pop(idx) {
          Ok(ent) => match ent {
            Some(ent) => {
              status_msg!("stash: Popped stash entry");
              ent
            }
            None => {
              if stack_len == 0 {
                status_msg!("stash: Stash is empty, nothing to pop");
              } else {
                status_msg!("stash: No stash entry at index '{idx}'");
              }
              return Ok(());
            }
          },
          Err(e) => {
            status_msg!("stash: Failed to pop stash entry: {e}");
            return Ok(());
          }
        };

        self.set_buffer(buffer);

        let cursor_pos = match self.parse_pos(&cursor_pos) {
          Ok(pos) => pos,
          Err(e) => {
            status_msg!("Failed to parse cursor position from stash: {e}");
            Pos { row: 0, col: 0 }
          }
        };

        self.set_cursor(cursor_pos);
      }
      StashArgs::Drop(arg) => {
        let idx = arg
          .as_ref()
          .map(|a| a.parse::<usize>())
          .transpose()
          .ok()
          .flatten()
          .unwrap_or(0);
        let stack_len = stash.stack_len();

        match stash.pop(idx).ok().flatten() {
          Some(_) => {
            status_msg!("stash: Dropped stash entry");
          }
          None => {
            if stack_len == 0 {
              status_msg!("stash: Stash is empty, nothing to drop");
            } else {
              status_msg!("stash: No stash entry at index '{idx}'");
            }
          }
        }
      }
      StashArgs::Apply(arg) => {
        let stack_len = stash.stack_len();
        let name = arg
          .clone()
          .unwrap_or(stack_len.saturating_sub(1).to_string());

        let Some(StashedCmd {
          name,
          buffer,
          cursor_pos,
        }) = stash.get(&name)?
        else {
          if let Ok(idx) = name.parse::<usize>() {
            if stack_len == 0 {
              status_msg!("stash: Stash is empty");
            } else {
              status_msg!("stash: No stash entry at index '{idx}'");
            }
          } else {
            status_msg!("stash: No stash entry named '{name}'");
          }
          return Ok(());
        };

        if let Some(name) = name {
          status_msg!("stash: Applied stash entry '{}'", name);
        }

        self.set_buffer(buffer);

        let cursor_pos = match self.parse_pos(&cursor_pos) {
          Ok(pos) => pos,
          Err(e) => {
            status_msg!("Failed to parse cursor position from stash: {e}");
            Pos { row: 0, col: 0 }
          }
        };

        self.set_cursor(cursor_pos);
      }
      StashArgs::Insert(arg) => {
        let stack_len = stash.stack_len();
        let name = arg
          .clone()
          .unwrap_or(stack_len.saturating_sub(1).to_string());

        let Some(StashedCmd {
          name: _,
          buffer,
          cursor_pos,
        }) = stash.get(&name)?
        else {
          if let Ok(idx) = name.parse::<usize>() {
            if stack_len == 0 {
              status_msg!("stash: Stash is empty");
            } else {
              status_msg!("stash: No stash entry at index '{idx}'");
            }
          } else {
            status_msg!("stash: No stash entry named '{name}'");
          }
          return Ok(());
        };

        let lines = Lines::to_lines(&buffer);
        let num_lines = lines.len();
        let line_range = self.row()..self.row() + num_lines;

        self.insert_lines_at(self.cursor.pos, lines);

        let cursor_offset = match self.parse_pos(&cursor_pos) {
          Ok(pos) => pos,
          Err(e) => {
            system_msg!("Failed to parse cursor position from stash: {e}");
            Pos { row: 0, col: 0 }
          }
        };
        self.cursor.pos = self.cursor.pos + cursor_offset;
        self.fix_cursor();
        if Shed::shopts(|o| o.line.auto_indent) {
          self.equalize_rows(line_range.collect());
        }
      }
      StashArgs::Swap(_) => todo!(),
      StashArgs::List(arg) => {
        let output = match arg {
          Some(StashListArg::Stack) => {
            stash.list(/*named_only:*/ false, /*stack_only:*/ true)
          }
          Some(StashListArg::Named) => {
            stash.list(/*named_only:*/ true, /*stack_only:*/ false)
          }
          None => stash.list(/*named_only:*/ false, /*stack_only:*/ false),
        };
        if output.trim().is_empty() {
          match arg {
            Some(StashListArg::Named) => {
              status_msg!("stash: No named stash entries");
            }
            Some(StashListArg::Stack) => {
              status_msg!("stash: Stack is empty");
            }
            None => {
              status_msg!("stash: No stash entries");
            }
          }
        } else {
          for line in output.lines() {
            system_msg!("{line}");
          }
        }
      }
    }

    Ok(())
  }

  fn repeat_global(&mut self, cmd: &EditCmd) -> ShResult<()> {
    let Some(saved) = self.last_global.clone() else {
      return Ok(());
    };
    let Some(merged) = merge_repeat_addr(cmd, &saved) else {
      return Ok(());
    };
    self.exec_cmd(merged)
  }

  fn repeat_substitute(&mut self, cmd: &EditCmd) -> ShResult<()> {
    let Some(saved) = self.last_substitute.clone() else {
      return Ok(());
    };
    let Some(merged) = merge_repeat_addr(cmd, &saved) else {
      return Ok(());
    };
    self.exec_cmd(merged)
  }

  fn ex_write(&mut self, dest: &WriteDest) -> ShResult<()> {
    match dest {
      WriteDest::FileAppend(path_buf) | WriteDest::File(path_buf) => {
        let Ok(mut file) = (if matches!(dest, WriteDest::File(_)) {
          OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(path_buf)
        } else {
          OpenOptions::new().create(true).append(true).open(path_buf)
        }) else {
          system_msg!("Failed to open file {}", path_buf.display());
          return Ok(());
        };
        let joined = self.joined();
        let bytes = joined.as_bytes();
        let lines = bytes.iter().filter(|b| **b == b'\n').count();
        let len = bytes.len() as u64;
        let size = format_size(len);

        if let Err(e) = file.write_all(bytes) {
          system_msg!("Failed to write to file {}: {e}", path_buf.display());
        }

        status_msg!("Wrote {lines} lines [{size}] to '{}'", path_buf.display());

        return Ok(());
      }
      WriteDest::Cmd(cmd) => {
        let buf = self.joined();
        let spec = RedirSpec::Buffer {
          fd: STDIN_FILENO,
          buf,
          flags: TkFlags::empty(),
        };

        let redirs = RedirSet::from(spec);
        let _guard = redirs.apply()?;

        autocmd!(PreCmd);
        {
          defer!(autocmd!(PostCmd));
          exec_nonint(cmd.to_string(), Some("ex write".into()))?;
        }
      }
    }
    Ok(())
  }

  fn ex_read(&mut self, src: &ReadSrc) -> ShResult<()> {
    let contents = match src {
      ReadSrc::File(path_buf) => {
        if !path_buf.is_file() {
          system_msg!("{} is not a file", path_buf.display());
          return Ok(());
        }
        let Ok(contents) = std::fs::read_to_string(path_buf) else {
          system_msg!("Failed to read file {}", path_buf.display());
          return Ok(());
        };
        let line_count = contents.lines().count();
        let byte_count = contents.len();
        let size = format_size(byte_count as u64);
        status_msg!(
          "Read {line_count} lines [{size}] from '{}'",
          path_buf.display()
        );
        contents
      }
      ReadSrc::Cmd(cmd) => {
        autocmd!(PreCmd);
        defer!(autocmd!(PostCmd));
        match capture_command(cmd, None) {
          Ok(out) => out,
          Err(e) => {
            e.print_error();
            return Ok(());
          }
        }
      }
    };

    let new_lines = Lines::to_lines(&contents);
    self.insert_lines_at(self.cursor.pos, new_lines);
    self.indent_cache = None;
    Ok(())
  }

  fn ex_edit(&mut self, paths: &[PathBuf]) -> ShResult<()> {
    if Shed::vars(|v| v.try_get_var("EDITOR")).is_none() {
      system_msg!("$EDITOR is unset. Aborting edit.");
      Ok(())
    } else {
      let args = paths.iter().map(|p| format!("{}", p.display())).join(" ");
      let input = format!("$EDITOR {args}");

      exec_int(input, Some("ex edit".into()))
    }
  }

  fn ex_shell_cmd(&mut self, cmd: &EditCmd, sh_cmd: &str) -> ShResult<()> {
    let Some(MotionKind::Line {
      start,
      end,
      inclusive,
    }) = self.eval_motion(cmd)?
    else {
      self.run_shell_cmd(sh_cmd, None)?;
      return Ok(());
    };
    let (s, mut e) = ordered(start, end);
    if !inclusive {
      e = e.saturating_sub(1);
    }
    // Clamp to last valid row in case the motion over-reached.
    e = e.min(self.lines.len().saturating_sub(1));
    let lines = self.lines.drain(s..=e).collect::<Vec<_>>();
    if self.lines.is_empty() {
      self.lines.push(Line::default());
    }
    let input = format!("{}\n", Lines(lines).join());
    let output = self.run_shell_cmd(sh_cmd, Some(&input))?;
    let new_lines = Lines::to_lines(output.unwrap_or_default());
    self.lines.0.splice(s..s, new_lines.0);

    Ok(())
  }

  fn run_shell_cmd(&mut self, sh_cmd: &str, stdin: Option<&str>) -> ShResult<Option<String>> {
    let mut vars = HashSet::new();
    vars.insert("BUFFER".into());
    vars.insert("CURSOR".into());
    vars.insert("ANCHOR".into());
    let _guard = var_ctx_guard(vars);

    let mut buf = self.joined();
    let cursor_raw = self.cursor_to_flat();
    let mut cursor = cursor_raw.to_string();
    let mut anchor = self.anchor_to_flat();

    Shed::vars_mut(|v| -> ShResult<()> {
      v.set_var("BUFFER", VarKind::Str(buf.clone()), VarFlags::EXPORT)?;
      v.set_var("CURSOR", VarKind::Str(cursor.to_string()), VarFlags::EXPORT)?;
      if let Some(anchor) = anchor {
        v.set_var("ANCHOR", VarKind::Str(anchor.to_string()), VarFlags::EXPORT)?;
      }
      Ok(())
    })?;

    autocmd!(PreCmd);
    let output = if let Some(stdin) = stdin {
      defer!(autocmd!(PostCmd));
      Some(capture_command(sh_cmd, Some(stdin))?)
    } else {
      defer!(autocmd!(PostCmd));
      let _guard = Shed::term_mut(|t| t.cooked_mode_guard());
      exec_int(sh_cmd.to_string(), Some("<ex-mode-cmd>".into()))?;
      None
    };

    let mut new_anchor = None;

    let keys = Shed::vars_mut(|v| {
      buf = v.take_var("BUFFER");
      cursor = v.take_var("CURSOR");
      if anchor.is_some() {
        new_anchor = Some(v.take_var("ANCHOR"));
      }
      v.take_var("KEYS")
    });

    self.set_buffer(buf);

    if let Some(new_anchor) = new_anchor {
      if let Ok(pos) = self.parse_pos(&new_anchor) {
        anchor = Some(self.pos_to_flat(pos));
      } else {
        log::warn!(
          "Invalid anchor position returned from shell command: '{}'",
          new_anchor
        );
        anchor = None;
      }
    }

    if let Ok(pos) = self.parse_pos(&cursor) {
      self.set_cursor(pos);
    } else {
      log::warn!(
        "Invalid cursor position returned from shell command: '{}'",
        cursor
      );
      self.set_cursor_from_flat(cursor_raw);
    }

    if let Some(anchor) = anchor
      && anchor != cursor_raw
      && self.select_mode.is_some()
    {
      self.set_anchor_from_flat(anchor);
    }
    if !keys.is_empty() {
      Shed::meta_mut(|m| m.set_pending_widget_keys(&keys))
    }
    Ok(output)
  }

  fn ex_delete(&mut self, cmd: &EditCmd, range: Option<AddressRange>) -> ShResult<()> {
    let range = range.unwrap_or_default();
    self.replace_verb(cmd, Verb::Delete, range)
  }
  fn ex_yank(&mut self, cmd: &EditCmd, range: Option<AddressRange>) -> ShResult<()> {
    let range = range.unwrap_or_default();
    self.replace_verb(cmd, Verb::Yank, range)
  }
  fn ex_put(&mut self, cmd: &EditCmd, anchor: Anchor, range: Option<AddressRange>) -> ShResult<()> {
    let range = range.unwrap_or_default();
    self.replace_verb(cmd, Verb::Put(anchor), range)
  }

  fn replace_verb(&mut self, cmd: &EditCmd, verb: Verb, range: AddressRange) -> ShResult<()> {
    // TODO: this can probably be performed way before we get here
    let verb = Some(verb!(verb));
    let motion = Some(motion!(range.as_motion()));

    let new_cmd = EditCmd {
      verb,
      motion,
      ..cmd.clone()
    };
    self.exec_verb(&new_cmd)
  }

  /// Recursively determines the set of lines an ExNode operates on.
  ///
  /// - Leaf nodes (Normal, Delete, Substitute, etc.) use the node's address
  ///   (current row if None).
  /// - Global nodes resolve their pattern within the node's address scope.
  /// - Nested Globals INTERSECT: `:g/A/g/B/cmd` operates on lines matching A AND B.
  ///   Each layer of Global narrows the set.
  pub fn lines_for_ex_node(&self, node: &ExNode) -> ShResult<Vec<usize>> {
    match &node.kind {
      ExNdRule::Global {
        negated,
        pat,
        nested,
      } => {
        let range = node.address.clone().unwrap_or_else(AddressRange::all_lines);
        let constraint = range.as_motion();
        let outer = self.get_matching_lines(&constraint, pat, *negated)?;

        // If the nested node also narrows the set (another Global), intersect.
        // Otherwise the outer match is the answer.
        if matches!(nested.kind, ExNdRule::Global { .. }) {
          let inner = self.lines_for_ex_node(nested)?;
          let outer_set: HashSet<usize> = outer.iter().copied().collect();
          Ok(
            inner
              .into_iter()
              .filter(|l| outer_set.contains(l))
              .collect(),
          )
        } else {
          Ok(outer)
        }
      }
      _ => self.lines_for_address(node.address.as_ref()),
    }
  }

  pub fn lines_for_address(&self, addr: Option<&AddressRange>) -> ShResult<Vec<usize>> {
    match addr {
      None => Ok(vec![self.row()]),
      Some(AddressRange::Single(a)) => {
        let line = self.resolve_line_addr(a)?.unwrap_or(self.row());
        Ok(vec![line])
      }
      Some(AddressRange::Range(s, e)) => {
        let s = self.resolve_line_addr(s)?.unwrap_or(self.row());
        let e = self.resolve_line_addr(e)?.unwrap_or(self.row());
        let (s, e) = ordered(s, e);
        Ok((s..=e).collect())
      }
    }
  }
}

/// Build a repeat command that uses the saved verb's *kind* (pat/repl/flags
/// or pattern/nested) but the NEW cmd's address. Without this, `:%s` after a
/// `:s/foo/bar/` would replay the old substitute on the OLD scope (current
/// line) instead of the new `%` scope.
fn merge_repeat_addr(new_cmd: &EditCmd, saved: &EditCmd) -> Option<EditCmd> {
  // Pull the saved kind
  let Some(Cmd(_, Verb::ExCmd(saved_node))) = saved.verb.as_ref() else {
    return None;
  };
  // Pull the new address (if any)
  let new_address = match new_cmd.verb.as_ref() {
    Some(Cmd(_, Verb::ExCmd(node))) => node.address.clone(),
    _ => None,
  };

  let new_node = ExNode {
    address: new_address,
    kind: saved_node.kind.clone(),
  };

  Some(EditCmd {
    verb: Some(verb!(Verb::ExCmd(new_node))),
    ..new_cmd.clone()
  })
}
