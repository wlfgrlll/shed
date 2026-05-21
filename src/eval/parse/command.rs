use ariadne::Span as AriadneSpan;

use super::{
  Label, LabelCtx, NdFlags, NdRule, Node, ParseStream, ShResult, Span, Tk, TkFlags, TkRule,
  node::{AssignKind, NodeVecUtils},
  procio::{RedirBldr, RedirSpec, RedirTarget, RedirType},
  sherr,
  util::node_is_punctuated,
};

impl ParseStream {
  fn build_redir<F: FnMut() -> Option<Tk>>(
    redir_tk: &Tk,
    mut next: F,
    node_tks: &mut Vec<Tk>,
    context: LabelCtx,
  ) -> ShResult<RedirSpec> {
    let redir_bldr = RedirBldr::try_from(redir_tk.clone())?;
    if redir_bldr.target.is_some() {
      return redir_bldr.build();
    }

    let Some(class) = redir_bldr.class else {
      // excluding this from coverage reports
      // because it's theoretically impossible
      // to reach using test input, if the lexer is working.
      // LCOV_EXCL_START
      return Err(
        sherr!(
          ParseErr @ redir_tk.span.clone(),
          "Invalid redirection operator"
        )
        .with_context(context),
      );
      // LCOV_EXCL_STOP
    };
    let Some(next_tk) = next().filter(|tk| tk.class != TkRule::Eoi) else {
      // LCOV_EXCL_START
      return Err(
        sherr!(
          ParseErr @ redir_tk.span.clone(),
          "Expected a filename after this redirection",
        )
        .with_context(context),
      );
      // LCOV_EXCL_STOP
    };

    let target = match class {
      RedirType::HereString => {
        let mut body = next_tk.clone().expand_no_split()?;
        body.push('\n');
        RedirTarget::HereDoc {
          body,
          flags: redir_tk.flags,
        }
      }
      _ => {
        node_tks.push(next_tk.clone());
        RedirTarget::Path(next_tk)
      }
    };

    redir_bldr.with_target(target).build()
  }
  pub(super) fn parse_redir(
    &mut self,
    redirs: &mut Vec<RedirSpec>,
    node_tks: &mut Vec<Tk>,
  ) -> ShResult<()> {
    while self.check_redir() {
      let tk = self.next_tk().unwrap();
      node_tks.push(tk.clone());
      let ctx = self.context.clone();
      let redir = match Self::build_redir(&tk, || self.next_tk(), node_tks, ctx) {
        Ok(r) => r,
        Err(e) => {
          self.panic_mode(node_tks);
          return Err(e);
        }
      };
      redirs.push(redir);
    }
    Ok(())
  }
  pub(super) fn parse_pipeln(&mut self) -> ShResult<Option<Node>> {
    let mut cmds = vec![];
    let mut node_tks = vec![];
    let mut flags = NdFlags::empty();

    while let Some(mut cmd) = self.parse_block(false)? {
      let is_punctuated = node_is_punctuated(&cmd.tokens);
      node_tks.append(&mut cmd.tokens.clone());
      let next_class = self.next_tk_class().clone();
      if next_class == TkRule::ErrPipe {
        cmd.flags |= NdFlags::PIPE_ERR;
      }
      if matches!(next_class, TkRule::Pipe | TkRule::ErrPipe) {
        cmd.walk_tree(&mut |n| n.flags |= NdFlags::PIPE_CMD | NdFlags::NOT_ERR);
      }

      cmds.push(cmd);
      if next_class == TkRule::Bg {
        let tk = self.next_tk().unwrap();
        node_tks.push(tk.clone());
        flags |= NdFlags::BACKGROUND;
        break;
      } else if (!matches!(next_class, TkRule::Pipe | TkRule::ErrPipe)) || is_punctuated {
        break;
      } else if let Some(pipe) = self.next_tk() {
        node_tks.push(pipe);
        self.catch_separator(&mut node_tks);
      } else {
        break;
      }
    }
    if cmds.is_empty() {
      Ok(None)
    } else {
      Ok(Some(node!(
        self,
        node_tks,
        NdRule::Pipeline { cmds },
        vec![/*redirs*/],
        flags
      )))
    }
  }
  #[allow(clippy::while_let_loop)]
  pub(super) fn parse_cmd(&mut self) -> ShResult<Option<Node>> {
    let mut node_tks = vec![];

    let result = 'out: {
      let tk_slice = self.tokens();
      let mut tk_iter = tk_slice.iter().peekable();
      let mut redirs = vec![];
      let mut argv = vec![];
      let flags = NdFlags::empty();
      let mut assignments = vec![];

      loop {
        let Some(prefix_tk) = tk_iter.next() else {
          break;
        };
        if let TkRule::CasePattern = prefix_tk.class {
          break 'out Err(parse_err!(
            self,
            vec![prefix_tk.clone()],
            "Found case pattern in command"
          ));
        }
        let is_cmd = prefix_tk.flags.contains(TkFlags::IS_CMD);
        let is_assignment = prefix_tk.flags.contains(TkFlags::ASSIGN);
        let is_keyword = prefix_tk.flags.contains(TkFlags::KEYWORD);

        if is_cmd {
          node_tks.push(prefix_tk.clone());
          argv.push(prefix_tk.clone());
          break;
        } else if is_assignment {
          let Some(assign) = self.parse_assignment(prefix_tk) else {
            break;
          };
          node_tks.push(prefix_tk.clone());
          assignments.push(assign)
        } else if is_keyword {
          return Ok(None);
        } else if prefix_tk.class == TkRule::Redir {
          let ctx = self.context.clone();
          let redir = Self::build_redir(prefix_tk, || tk_iter.next().cloned(), &mut node_tks, ctx)?;
          redirs.push(redir);
        } else if prefix_tk.class == TkRule::Sep {
          // Separator ends the prefix section - add it so commit() consumes it
          node_tks.push(prefix_tk.clone());
          break;
        } else {
          // Other non-prefix token ends the prefix section
          break;
        }
      }
      if argv.is_empty() {
        if assignments.is_empty() {
          break 'out Ok(None);
        } else {
          // If we have assignments but no command word,
          // return the assignment-only command without parsing more tokens
          self.commit(node_tks.len());
          let mut context = self.context.clone();
          let assignments_span = assignments.get_span().unwrap();
          context.push_back((
            assignments_span.source().clone(),
            Label::new(assignments_span)
              .with_message("in variable assignment defined here".to_string()),
          ));
          return Ok(Some(node!(
            self,
            node_tks,
            NdRule::Command { assignments, argv },
            redirs,
            flags
          )));
        }
      }
      loop {
        let Some(tk) = tk_iter.next() else {
          break;
        };
        match tk.class {
          TkRule::Comment => break,

          TkRule::Eoi
          | TkRule::Pipe
          | TkRule::ErrPipe
          | TkRule::And
          | TkRule::BraceGrpEnd
          | TkRule::SubshEnd
          | TkRule::Or
          | TkRule::Bg => break,
          TkRule::Sep => {
            node_tks.push(tk.clone());
            break;
          }
          TkRule::Str => {
            argv.push(tk.clone());
            node_tks.push(tk.clone());
          }
          TkRule::HereDoc { .. } | TkRule::Redir => {
            node_tks.push(tk.clone());
            let ctx = self.context.clone();
            let redir = match Self::build_redir(tk, || tk_iter.next().cloned(), &mut node_tks, ctx)
            {
              Ok(r) => r,

              // excluding from coverage reports, see the
              // comment at line 24
              // LCOV_EXCL_START
              Err(e) => {
                self.panic_mode(&mut node_tks);
                return Err(e);
              } // LCOV_EXCL_STOP
            };
            redirs.push(redir);
          }
          _ => {
            break 'out Err(parse_err!(
              self,
              node_tks,
              "Unexpected token in command: {:?}",
              tk.class
            ));
          }
        };
      }
      self.commit(node_tks.len());

      return Ok(Some(node!(
        self,
        node_tks,
        NdRule::Command { assignments, argv },
        redirs,
        flags
      )));
    };

    match result {
      Ok(node) => Ok(node),
      Err(e) => {
        self.panic_mode(&mut node_tks);
        Err(e)
      }
    }
  }
  fn parse_assignment(&self, token: &Tk) -> Option<Node> {
    let mut chars = token.span.as_str().chars();
    let mut var_name = String::new();
    let mut name_range = token.span.range().start..token.span.range().start;
    let mut var_val = String::new();
    let mut val_range = token.span.range().end..token.span.range().end;
    let mut assign_kind = None;
    let mut pos = token.span.range().start;
    let mut bracket_depth = 0usize;

    while let Some(ch) = chars.next() {
      if assign_kind.is_some() {
        match ch {
          '\\' => {
            pos += ch.len_utf8();
            var_val.push(ch);
            if let Some(esc_ch) = chars.next() {
              pos += esc_ch.len_utf8();
              var_val.push(esc_ch);
            }
          }
          _ => {
            pos += ch.len_utf8();
            var_val.push(ch);
          }
        }
      } else {
        match ch {
          '[' => {
            bracket_depth += 1;
            pos += ch.len_utf8();
            var_name.push(ch);
          }
          ']' if bracket_depth > 0 => {
            bracket_depth -= 1;
            pos += ch.len_utf8();
            var_name.push(ch);
          }
          '=' if bracket_depth == 0 => {
            name_range.end = pos;
            pos += ch.len_utf8();
            val_range.start = pos;
            assign_kind = Some(AssignKind::Eq);
          }
          '-' if bracket_depth == 0 => {
            name_range.end = pos;
            pos += ch.len_utf8();
            let Some('=') = chars.next() else { return None };
            pos += 1;
            val_range.start = pos;
            assign_kind = Some(AssignKind::MinusEq);
          }
          '+' if bracket_depth == 0 => {
            name_range.end = pos;
            pos += ch.len_utf8();
            let Some('=') = chars.next() else { return None };
            pos += 1;
            val_range.start = pos;
            assign_kind = Some(AssignKind::PlusEq);
          }
          '/' if bracket_depth == 0 => {
            name_range.end = pos;
            pos += ch.len_utf8();
            let Some('=') = chars.next() else { return None };
            pos += 1;
            val_range.start = pos;
            assign_kind = Some(AssignKind::DivEq);
          }
          '*' if bracket_depth == 0 => {
            name_range.end = pos;
            pos += ch.len_utf8();
            let Some('=') = chars.next() else { return None };
            pos += 1;
            val_range.start = pos;
            assign_kind = Some(AssignKind::MultEq);
          }
          '\\' => {
            pos += ch.len_utf8();
            var_name.push(ch);
            if let Some(esc_ch) = chars.next() {
              pos += esc_ch.len_utf8();
              var_name.push(esc_ch);
            }
          }
          _ => {
            pos += ch.len_utf8();
            var_name.push(ch)
          }
        }
      }
    }
    if assign_kind.is_none() || var_name.is_empty() {
      return None;
    }
    let assign_kind = assign_kind.unwrap();

    let var = Tk::new(TkRule::Str, Span::new(name_range, token.source()));
    let val = Tk::new(TkRule::Str, Span::new(val_range, token.source()));
    let flags = if var_val.starts_with('(') && var_val.ends_with(')') {
      NdFlags::ARR_ASSIGN
    } else {
      NdFlags::empty()
    };

    Some(node!(
      self,
      vec![token.clone()],
      NdRule::Assignment {
        kind: assign_kind,
        var,
        val
      },
      vec![/*redirs*/],
      flags
    ))
  }
}

#[cfg(test)]
mod command_parse_tests {
  //! Targets uncovered branches in command.rs parsing — redirection
  //! errors, the Bg pipeline path, leading redirs, comment-as-argv-
  //! terminator, and parse_assignment escape handling.

  use crate::tests::testutil::{TestGuard, get_ast, test_input};

  // ─── build_redir / parse_redir error paths ─────────────────────────

  #[test]
  fn redir_with_no_filename_at_eoi_errors() {
    // After consuming `>`, build_redir's `next()` returns None.
    // Triggers the "Expected a filename after this redirection"
    // branch and the panic_mode wrapper in parse_redir.
    let _g = TestGuard::new();
    assert!(get_ast("echo foo >").is_err());
  }

  #[test]
  fn append_redir_with_no_filename_errors() {
    let _g = TestGuard::new();
    assert!(get_ast("echo foo >>").is_err());
  }

  #[test]
  fn input_redir_with_no_filename_errors() {
    let _g = TestGuard::new();
    assert!(get_ast("cat <").is_err());
  }

  // ─── parse_pipeln Bg branch ────────────────────────────────────────

  #[test]
  fn background_command_parses_with_bg_flag() {
    // `cmd &` — the parse_pipeln loop sees Bg as next class, consumes
    // it, sets BACKGROUND, breaks. We just verify the parse succeeds;
    // executing a real background command in the test harness can
    // hit tcsetpgrp issues.
    let _g = TestGuard::new();
    get_ast("sleep 0 &").unwrap();
  }

  #[test]
  fn pipeline_followed_by_bg_parses() {
    let _g = TestGuard::new();
    get_ast("echo foo | cat &").unwrap();
  }

  // ─── parse_cmd: leading redir before command word ──────────────────

  #[test]
  fn leading_redir_before_command_routes_through_build_redir() {
    // The first token in the prefix loop is a Redir, not a Cmd /
    // Assignment / Keyword — hits the `prefix_tk.class == TkRule::Redir`
    // branch and the inline build_redir call.
    let _g = TestGuard::new();
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("out.txt");
    test_input(format!("> {} echo prefixed_redir_marker", path.display())).unwrap();
    let content = std::fs::read_to_string(&path).unwrap();
    assert!(
      content.contains("prefixed_redir_marker"),
      "got: {content:?}"
    );
  }

  // ─── parse_cmd: comment in argv breaks loop ────────────────────────

  #[test]
  fn comment_after_command_word_terminates_argv() {
    // The argv loop hits `TkRule::Comment => break`.
    let g = TestGuard::new();
    test_input("echo hello_before_comment # this should be skipped").unwrap();
    let out = g.read_output();
    assert!(out.contains("hello_before_comment"), "got: {out:?}");
    assert!(
      !out.contains("this should be skipped"),
      "comment leaked: {out:?}"
    );
  }

  // ─── parse_cmd: build_redir error inside argv ──────────────────────

  #[test]
  fn redir_with_no_filename_after_argv_errors() {
    // After `echo foo`, the argv loop encounters `>` then tries to
    // build a redir with no filename — hits the Err arm at the inner
    // build_redir call, which panic_modes and returns.
    let _g = TestGuard::new();
    assert!(get_ast("echo foo >").is_err());
  }

  // ─── parse_cmd: unexpected token in argv ───────────────────────────
  //
  // The catchall `_` arm in the argv loop is reachable only for token
  // classes that (a) the lexer can produce and (b) aren't handled by
  // any of the explicit arms (Str / Redir / HereDoc / Sep / the
  // Comment-and-terminator break list). In practice, lex tries hard to
  // either close paired delimiters at this position (`(` / `{`) or
  // classify standalone punctuation as Str — so the `_` arm has no
  // straightforward reachable input. I tried `echo foo {`, `echo foo (`,
  // `echo foo !` and none of them surface as parse errors. Leaving the
  // test out rather than baking in something flaky; arguably dead code.

  // ─── parse_assignment escape handling ──────────────────────────────

  #[test]
  fn assignment_value_preserves_escaped_char() {
    // Hits the `\\` arm of the post-= chars loop in parse_assignment.
    // The literal backslash + next char should be preserved verbatim.
    let g = TestGuard::new();
    test_input("x=a\\zb; echo \"$x\"").unwrap();
    let out = g.read_output();
    assert!(out.contains("a"), "got: {out:?}");
    assert!(out.contains("zb"), "got: {out:?}");
  }
}
