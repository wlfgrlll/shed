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
      return Err(
        sherr!(
          ParseErr @ redir_tk.span.clone(),
          "Invalid redirection operator"
        )
        .with_context(context),
      );
    };
    let Some(next_tk) = next().filter(|tk| tk.class != TkRule::Eoi) else {
      return Err(
        sherr!(
          ParseErr @ redir_tk.span.clone(),
          "Expected a filename after this redirection",
        )
        .with_context(context),
      );
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
              Err(e) => {
                self.panic_mode(&mut node_tks);
                return Err(e);
              }
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
            pos += '='.len_utf8();
            val_range.start = pos;
            assign_kind = Some(AssignKind::MinusEq);
          }
          '+' if bracket_depth == 0 => {
            name_range.end = pos;
            pos += ch.len_utf8();
            let Some('=') = chars.next() else { return None };
            pos += '='.len_utf8();
            val_range.start = pos;
            assign_kind = Some(AssignKind::PlusEq);
          }
          '/' if bracket_depth == 0 => {
            name_range.end = pos;
            pos += ch.len_utf8();
            let Some('=') = chars.next() else { return None };
            pos += '='.len_utf8();
            val_range.start = pos;
            assign_kind = Some(AssignKind::DivEq);
          }
          '*' if bracket_depth == 0 => {
            name_range.end = pos;
            pos += ch.len_utf8();
            let Some('=') = chars.next() else { return None };
            pos += '='.len_utf8();
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
    if let Some(assign_kind) = assign_kind
      && !var_name.is_empty()
    {
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
    } else {
      None
    }
  }
}
