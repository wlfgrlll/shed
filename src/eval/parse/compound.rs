use ariadne::Label;
use std::rc::Rc;

use super::{
  CaseNode, CondNode, LoopKind, NdFlags, NdRule, Node, ParseStream, ShResult, Tk, TkFlags, TkRule,
  util::split_for_arith_tk,
};

impl ParseStream {
  pub(super) fn parse_func_def(&mut self) -> ShResult<Option<Node>> {
    let mut node_tks: Vec<Tk> = vec![];

    let has_func_kw = self.check_keyword("function");

    if has_func_kw {
      node_tks.push(self.next_tk().unwrap());
    }

    if !self.check_flags(TkFlags::FUNCNAME) {
      if has_func_kw {
        bail!(
          self,
          node_tks,
          "Expected function name after 'function' keyword"
        );
      } else {
        return Ok(None);
      }
    }

    let name_tk = self.next_tk().unwrap();
    node_tks.push(name_tk.clone());
    let name = name_tk.clone();
    let name_raw: Rc<str> = name.as_str().into();

    self.catch_separator(&mut node_tks);
    let mut src = name_tk.span.span_source().clone();
    src.rename(name_raw.clone());
    // Push a placeholder context so child nodes inherit it
    self.context.push_back((
      src.clone(),
      Label::new(name_tk.span.clone().with_name(name_raw.clone()))
        .with_message(format!("in function '{}' defined here", name_raw.clone())),
    ));

    let Some(mut compound_cmd) = self.parse_compound()? else {
      self.context.pop_back();
      bail!(
        self,
        node_tks,
        "Expected a compound command after function name"
      );
    };
    node_tks.extend(compound_cmd.tokens.clone());
    self.parse_redir(&mut compound_cmd.redirs, &mut node_tks)?;
    let body = Box::new(compound_cmd);
    // Replace placeholder with full-span label
    self.context.pop_back();

    Ok(Some(node!(self, node_tks, NdRule::FuncDef { name, body })))
  }
  pub(super) fn parse_subsh(&mut self) -> ShResult<Option<Node>> {
    let mut node_tks = vec![];
    let mut body = vec![];
    let mut body_tks = vec![];
    let mut redirs = vec![];

    if *self.next_tk_class() != TkRule::SubshStart {
      return Ok(None);
    }
    node_tks.push(self.next_tk().unwrap());
    self.catch_separator(&mut node_tks);

    loop {
      if *self.next_tk_class() == TkRule::SubshEnd {
        node_tks.push(self.next_tk().unwrap());
        break;
      }
      if let Some(node) = self.parse_conjunction()? {
        node_tks.extend(node.tokens.clone());
        body_tks.extend(node.tokens.clone());
        body.push(node);
      } else if *self.next_tk_class() != TkRule::SubshEnd {
        let next = self.peek_tk().cloned();
        let err = match next {
          Some(tk) => Err(parse_err!(
            self,
            node_tks,
            "Unexpected token '{}' in subshell body",
            tk.as_str()
          )),
          None => Err(parse_err!(
            self,
            node_tks,
            "Unexpected end of input while parsing subshell body"
          )),
        };
        self.panic_mode(&mut node_tks);
        return err;
      }
      self.catch_separator(&mut node_tks);
      if !self.next_tk_is_some() {
        bail!(
          self,
          node_tks,
          "Expected a closing parenthesis for this subshell"
        );
      }
    }

    let body = Box::new(node!(
      self,
      body_tks,
      NdRule::List { commands: body },
      vec![]
    ));

    self.parse_redir(&mut redirs, &mut node_tks)?;

    Ok(Some(node!(
      self,
      node_tks,
      NdRule::Subshell { body },
      redirs
    )))
  }
  pub(super) fn parse_brc_grp(&mut self, from_func_def: bool) -> ShResult<Option<Node>> {
    let mut node_tks = vec![];
    let mut body = vec![];
    let mut body_tks = vec![];
    let mut redirs = vec![];

    if *self.next_tk_class() != TkRule::BraceGrpStart {
      return Ok(None);
    }
    node_tks.push(self.next_tk().unwrap());

    self.catch_separator(&mut node_tks);

    loop {
      if *self.next_tk_class() == TkRule::BraceGrpEnd {
        node_tks.push(self.next_tk().unwrap());
        break;
      }
      if let Some(node) = self.parse_conjunction()? {
        node_tks.extend(node.tokens.clone());
        body_tks.extend(node.tokens.clone());
        body.push(node);
      } else if *self.next_tk_class() != TkRule::BraceGrpEnd {
        let next = self.peek_tk().cloned();
        let err = match next {
          Some(tk) => Err(parse_err!(
            self,
            node_tks,
            "Unexpected token '{}' in brace group body",
            tk.as_str()
          )),
          None => Err(parse_err!(
            self,
            node_tks,
            "Unexpected end of input while parsing brace group body"
          )),
        };
        self.panic_mode(&mut node_tks);
        return err;
      }
      self.catch_separator(&mut node_tks);
      if !self.next_tk_is_some() {
        bail!(
          self,
          node_tks,
          "Expected a closing brace for this brace group"
        );
      }
    }

    let body = Box::new(node!(
      self,
      body_tks,
      NdRule::List { commands: body },
      vec![]
    ));

    if !from_func_def {
      self.parse_redir(&mut redirs, &mut node_tks)?;
    }

    Ok(Some(node!(
      self,
      node_tks,
      NdRule::BraceGrp { body },
      redirs
    )))
  }
  pub(super) fn parse_case(&mut self) -> ShResult<Option<Node>> {
    // Needs a pattern token
    // Followed by any number of CaseNodes
    let mut node_tks: Vec<Tk> = vec![];

    let mut case_blocks: Vec<CaseNode> = vec![];
    let redirs = vec![];

    if !self.check_keyword("case") || !self.next_tk_is_some() {
      return Ok(None);
    }
    node_tks.push(self.next_tk().unwrap());

    let pat_err = parse_err!(self, node_tks, "Expected a pattern after 'case' keyword")
      .with_note("Patterns can be raw text, or anything that gets substituted with raw text");

    let Some(pat_tk) = self.next_tk() else {
      self.panic_mode(&mut node_tks);
      return Err(pat_err);
    };

    if pat_tk.span.as_str() == "in" {
      return Err(pat_err);
    }

    let pattern: Tk = pat_tk;

    node_tks.push(pattern.clone());

    if !self.check_keyword("in") || !self.next_tk_is_some() {
      bail!(self, node_tks, "Expected 'in' after case variable name");
    }
    node_tks.push(self.next_tk().unwrap());

    self.catch_separator(&mut node_tks);

    loop {
      let leading_paren = matches!(self.next_tk_class(), TkRule::SubshStart);
      if leading_paren {
        // optional leading paren, push and continue
        node_tks.push(self.next_tk().unwrap());
      }

      let mut patterns: Vec<Tk> = vec![];
      loop {
        let Some(word) = self.next_tk() else {
          bail!(self, node_tks, "Expected a case pattern here");
        };
        if matches!(word.class, TkRule::SubshEnd | TkRule::Sep | TkRule::Eoi)
          || word.flags.contains(TkFlags::KEYWORD)
        {
          self.panic_mode(&mut node_tks);
          return Err(parse_err!(self, vec![word], "Expected a case pattern here"));
        }
        node_tks.push(word.clone());
        patterns.push(word);

        match self.next_tk_class() {
          TkRule::Pipe => {
            node_tks.push(self.next_tk().unwrap()); // consume '|'
            // loop back for next alternative
          }
          TkRule::SubshEnd => break,
          _ => {
            bail!(self, node_tks, "Expected '|' or ')' after case pattern");
          }
        }
      }

      // Consume the closing ')'.
      node_tks.push(self.next_tk().unwrap());
      self.block_depth += 1;

      let mut found_end = false;
      while self.check_separator() {
        let sep = self.peek_tk().unwrap();
        if sep.has_double_semi() {
          node_tks.push(self.next_tk().unwrap());
          found_end = true;
          self.block_depth -= 1;
          break;
        } else {
          node_tks.push(self.next_tk().unwrap());
        }
      }
      let mut arm_commands = vec![];
      let mut arm_tks = vec![];

      while !found_end {
        let Some(conj) = self.parse_conjunction()? else {
          break;
        };
        arm_tks.extend(conj.tokens.clone());

        let trailing_dbl_semi = conj
          .tokens
          .iter()
          .rev()
          .take_while(|tk| matches!(tk.class, TkRule::Sep))
          .any(|tk| tk.has_double_semi());

        arm_commands.push(conj);

        if trailing_dbl_semi {
          found_end = true;
          self.block_depth -= 1;
        }
      }

      let arm_body = node!(
        self,
        arm_tks,
        NdRule::List {
          commands: arm_commands
        }
      );

      let case_node = CaseNode {
        patterns,
        body: Box::new(arm_body),
      };
      case_blocks.push(case_node);

      self.catch_separator(&mut node_tks);

      if self.check_keyword("esac") {
        node_tks.push(self.next_tk().unwrap());
        self.assert_separator(&mut node_tks)?;
        break;
      }

      if !self.next_tk_is_some() {
        bail!(
          self,
          node_tks,
          "Expected 'esac' to close this case statement"
        );
      }
    }

    Ok(Some(node!(
      self,
      node_tks,
      NdRule::CaseNode {
        pattern,
        case_blocks
      },
      redirs
    )))
  }
  pub(super) fn parse_time(&mut self) -> ShResult<Option<Node>> {
    let mut node_tks: Vec<Tk> = vec![];

    if !self.check_keyword("time") || !self.next_tk_is_some() {
      return Ok(None);
    }
    node_tks.push(self.next_tk().unwrap());

    let Some(mut cmd) = self.parse_block(true)? else {
      bail!(self, node_tks, "Expected a command after 'time'");
    };

    cmd.walk_tree(&mut |n| n.flags |= NdFlags::NOT_ERR);

    node_tks.extend(cmd.tokens.clone());
    self.catch_separator(&mut node_tks);
    Ok(Some(node!(
      self,
      node_tks,
      NdRule::Timed { cmd: Box::new(cmd) }
    )))
  }
  pub(super) fn parse_func_keyword(&mut self) -> ShResult<Option<Node>> {
    if !self.check_keyword("function") || !self.next_tk_is_some() {
      return Ok(None);
    }
    self.parse_func_def()
  }
  pub(super) fn parse_arith(&mut self) -> ShResult<Option<Node>> {
    let mut node_tks: Vec<Tk> = vec![];
    let mut redirs = vec![];

    if !self.check_flags(TkFlags::IS_ARITH) || !self.next_tk_is_some() {
      return Ok(None);
    }
    let arith_tk = self.next_tk().unwrap();
    node_tks.push(arith_tk.clone());

    self.parse_redir(&mut redirs, &mut node_tks)?;

    if matches!(self.next_tk_class(), TkRule::Str) {
      bail!(
        self,
        node_tks,
        "Unexpected argument after arithmetic command"
      );
    }

    Ok(Some(node!(
      self,
      node_tks,
      NdRule::Arithmetic { body: arith_tk },
      redirs
    )))
  }
  pub(super) fn parse_negate(&mut self) -> ShResult<Option<Node>> {
    let mut node_tks: Vec<Tk> = vec![];

    if !self.check_keyword("!") || !self.next_tk_is_some() {
      return Ok(None);
    }

    node_tks.push(self.next_tk().unwrap());

    let Some(mut cmd) = self.parse_block(true)? else {
      bail!(self, node_tks, "Expected a command after '!'");
    };
    cmd.walk_tree(&mut |n| n.flags |= NdFlags::NOT_ERR); // disable set -e for negated commands

    node_tks.extend(cmd.tokens.clone());
    self.catch_separator(&mut node_tks);

    Ok(Some(node!(
      self,
      node_tks,
      NdRule::Negate { cmd: Box::new(cmd) }
    )))
  }
  pub(super) fn parse_if(&mut self) -> ShResult<Option<Node>> {
    // Needs at last one 'if-then',
    // Any number of 'elif-then',
    // Zero or one 'else'
    let mut node_tks: Vec<Tk> = vec![];
    let mut cond_nodes: Vec<CondNode> = vec![];
    let mut else_block: Option<Node> = None;
    let mut redirs = vec![];

    if !self.check_keyword("if") || !self.next_tk_is_some() {
      return Ok(None);
    }
    node_tks.push(self.next_tk().unwrap());

    loop {
      self.block_depth += 1;
      let prefix_keywrd = if cond_nodes.is_empty() { "if" } else { "elif" };
      let Some(mut cond) = self.parse_cmd_list()? else {
        if prefix_keywrd == "elif" {
          self.block_depth -= 1;
        }
        bail!(self, node_tks, "Expected a command after '{prefix_keywrd}'");
      };
      node_tks.extend(cond.tokens.clone());
      cond.walk_tree(&mut |n| n.flags |= NdFlags::NOT_ERR); // disable set -e for condition commands

      if !self.check_keyword("then") || !self.next_tk_is_some() {
        bail!(
          self,
          node_tks,
          "Expected 'then' after '{prefix_keywrd}' condition"
        );
      }
      node_tks.push(self.next_tk().unwrap());
      self.catch_separator(&mut node_tks);

      let Some(body) = self.parse_cmd_list()? else {
        bail!(self, node_tks, "Expected a command after 'then'");
      };
      node_tks.extend(body.tokens.clone());

      let cond_node = CondNode {
        cond: Box::new(cond),
        body: Box::new(body),
      };
      cond_nodes.push(cond_node);

      self.catch_separator(&mut node_tks);
      if !self.check_keyword("elif") || !self.next_tk_is_some() {
        break;
      } else {
        self.block_depth -= 1;
        node_tks.push(self.next_tk().unwrap());
        self.catch_separator(&mut node_tks);
      }
    }

    self.catch_separator(&mut node_tks);
    if self.check_keyword("else") {
      self.block_depth -= 1;
      node_tks.push(self.next_tk().unwrap());
      let mut already_added = false;

      if self.check_separator() || self.next_tk_is_some() {
        already_added = true;
        self.block_depth += 1;
      }

      self.catch_separator(&mut node_tks);

      let Some(body) = self.parse_cmd_list()? else {
        bail!(self, node_tks, "Expected a command after 'else'");
      };
      else_block = Some(body);

      if !already_added {
        self.block_depth += 1;
      }
    }

    self.catch_separator(&mut node_tks);
    if !self.check_keyword("fi") || !self.next_tk_is_some() {
      bail!(self, node_tks, "Expected 'fi' after if statement");
    }
    node_tks.push(self.next_tk().unwrap());
    self.block_depth -= 1;

    self.parse_redir(&mut redirs, &mut node_tks)?;

    self.assert_separator(&mut node_tks)?;

    Ok(Some(node!(
      self,
      node_tks,
      NdRule::IfNode {
        cond_nodes,
        else_block: else_block.map(Box::new)
      },
      redirs
    )))
  }
  pub(super) fn parse_for_arith(&mut self, mut node_tks: Vec<Tk>) -> ShResult<Option<Node>> {
    let mut redirs = vec![];

    let arith_tk = self.next_tk().unwrap(); // we checked already
    node_tks.push(arith_tk.clone());
    let (init, cond, step) = split_for_arith_tk(arith_tk)?;
    self.catch_separator(&mut node_tks);

    if !self.check_keyword("do") || !self.next_tk_is_some() {
      bail!(
        self,
        node_tks,
        "Expected 'do' after for loop arithmetic expression"
      );
    }
    node_tks.push(self.next_tk().unwrap());
    self.catch_separator(&mut node_tks);

    let Some(body) = self.parse_cmd_list()? else {
      bail!(self, node_tks, "Expected a command after 'do' in this loop");
    };

    self.catch_separator(&mut node_tks);
    if !self.check_keyword("done") || !self.next_tk_is_some() {
      bail!(self, node_tks, "Expected 'done' after for loop body");
    }
    node_tks.push(self.next_tk().unwrap());

    self.parse_redir(&mut redirs, &mut node_tks)?;

    Ok(Some(node!(
      self,
      node_tks,
      NdRule::ForArith {
        init,
        cond,
        step,
        body: Box::new(body)
      },
      redirs
    )))
  }
  pub(super) fn parse_for_arr(&mut self, mut node_tks: Vec<Tk>) -> ShResult<Option<Node>> {
    let mut vars: Vec<Tk> = vec![];
    let mut arr: Vec<Tk> = vec![];
    let mut redirs = vec![];

    while let Some(tk) = self.next_tk() {
      node_tks.push(tk.clone());
      if tk.as_str() == "in" {
        break;
      } else {
        vars.push(tk.clone());
      }
    }

    while let Some(tk) = self.next_tk() {
      node_tks.push(tk.clone());
      if tk.class == TkRule::Sep {
        break;
      } else {
        arr.push(tk.clone());
      }
    }

    if vars.is_empty() {
      bail!(self, node_tks, "Expected a variable name for this for loop");
    }
    if !self.check_keyword("do") || !self.next_tk_is_some() {
      bail!(
        self,
        node_tks,
        "Expected 'do' after for loop variable and array"
      );
    }
    node_tks.push(self.next_tk().unwrap());
    self.catch_separator(&mut node_tks);

    let Some(body) = self.parse_cmd_list()? else {
      bail!(self, node_tks, "Expected a command after 'do' in this loop");
    };

    self.catch_separator(&mut node_tks);
    if !self.check_keyword("done") || !self.next_tk_is_some() {
      bail!(self, node_tks, "Expected 'done' after for loop body");
    }
    node_tks.push(self.next_tk().unwrap());

    self.parse_redir(&mut redirs, &mut node_tks)?;

    Ok(Some(node!(
      self,
      node_tks,
      NdRule::ForNode {
        vars,
        arr,
        body: Box::new(body)
      },
      redirs
    )))
  }
  pub(super) fn parse_for(&mut self) -> ShResult<Option<Node>> {
    let mut node_tks: Vec<Tk> = vec![];

    if !self.check_keyword("for") || !self.next_tk_is_some() {
      return Ok(None);
    }
    node_tks.push(self.next_tk().unwrap());

    if self.check_flags(TkFlags::IS_ARITH) {
      self.parse_for_arith(node_tks)
    } else {
      self.parse_for_arr(node_tks)
    }
  }
  pub(super) fn parse_loop(&mut self) -> ShResult<Option<Node>> {
    // Requires a single CondNode and a LoopKind

    let mut node_tks = vec![];
    let mut redirs = vec![];

    if (!self.check_keyword("while") && !self.check_keyword("until")) || !self.next_tk_is_some() {
      return Ok(None);
    }
    let loop_tk = self.next_tk().unwrap();
    let loop_kind: LoopKind = loop_tk
      .span
      .as_str()
      .parse() // LoopKind implements FromStr
      .unwrap();

    node_tks.push(loop_tk);
    self.catch_separator(&mut node_tks);

    let Some(mut cond) = self.parse_cmd_list()? else {
      bail!(self, node_tks, "Expected a command after '{loop_kind}'");
    };
    node_tks.extend(cond.tokens.clone());
    cond.walk_tree(&mut |n| n.flags |= NdFlags::NOT_ERR); // disable set -e for condition commands

    if !self.check_keyword("do") || !self.next_tk_is_some() {
      bail!(
        self,
        node_tks,
        "Expected 'do' after '{loop_kind}' condition"
      );
    }
    node_tks.push(self.next_tk().unwrap());
    self.catch_separator(&mut node_tks);

    let Some(body) = self.parse_cmd_list()? else {
      bail!(self, node_tks, "Expected a command after 'do' in this loop");
    };

    self.catch_separator(&mut node_tks);
    if !self.check_keyword("done") || !self.next_tk_is_some() {
      bail!(self, node_tks, "Expected 'done' after loop body");
    }
    node_tks.push(self.next_tk().unwrap());

    self.parse_redir(&mut redirs, &mut node_tks)?;

    self.assert_separator(&mut node_tks)?;

    let cond_node = CondNode {
      cond: Box::new(cond),
      body: Box::new(body),
    };

    Ok(Some(node!(
      self,
      node_tks,
      NdRule::LoopNode {
        kind: loop_kind,
        cond_node
      },
      redirs
    )))
  }
}

#[cfg(test)]
mod parse_for_arith_tests {
  //! End-to-end tests for C-style arithmetic `for` loops, which take
  //! the parse_for_arith branch of compound parsing.

  use crate::state;
  use crate::tests::testutil::{TestGuard, test_input};

  #[test]
  fn basic_arith_for_loop_runs_n_iterations() {
    let g = TestGuard::new();
    test_input("for (( i=0; i<3; i=i+1 )); do echo $i; done").unwrap();
    let out = g.read_output();
    assert_eq!(out, "0\n1\n2\n", "got: {out:?}");
    assert_eq!(state::Shed::get_status(), 0);
  }

  #[test]
  fn arith_for_loop_with_empty_body_runs() {
    let _g = TestGuard::new();
    test_input("for (( i=0; i<3; i=i+1 )); do :; done").unwrap();
    assert_eq!(state::Shed::get_status(), 0);
  }

  #[test]
  fn arith_for_loop_with_falsy_initial_cond_skips_body() {
    let g = TestGuard::new();
    test_input("for (( i=5; i<5; i=i+1 )); do echo hit; done").unwrap();
    let out = g.read_output();
    assert_eq!(out, "");
  }

  // Note: missing `do` / `done` cause parse errors that print but
  // don't propagate to `$?` (exec_input's parser branch prints errors
  // then returns Ok). They're observable via stderr but not status,
  // so we don't include them as direct branch tests.

  #[test]
  fn arith_for_loop_with_arithmetic_in_body() {
    let g = TestGuard::new();
    test_input("total=0; for (( i=1; i<=3; i=i+1 )); do total=$((total+i)); done; echo $total")
      .unwrap();
    let out = g.read_output();
    assert!(out.contains("6"), "expected 1+2+3=6, got: {out:?}");
  }

  #[test]
  fn arith_for_loop_with_redirect_on_done() {
    let _g = TestGuard::new();
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("out.txt");
    test_input(format!(
      "for (( i=0; i<2; i=i+1 )); do echo $i; done > {}",
      path.display()
    ))
    .unwrap();
    let content = std::fs::read_to_string(&path).unwrap();
    assert_eq!(content, "0\n1\n");
  }
}

#[cfg(test)]
mod compound_parse_error_tests {
  //! Targets uncovered error paths in compound parsing. We use
  //! `get_ast` which returns Err when parsing fails; for happy-path
  //! coverage of normally-reached-but-missed branches, we run the
  //! input end-to-end via `test_input`.

  use crate::tests::testutil::{TestGuard, get_ast, test_input};

  // ─── subshell body errors ──────────────────────────────────────────

  #[test]
  fn subshell_with_leading_pipe_errors() {
    // `parse_conjunction` returns None when next is an operator that
    // can't start a command. The else-branch error fires.
    let _g = TestGuard::new();
    assert!(get_ast("( | echo foo )").is_err());
  }

  #[test]
  fn unclosed_subshell_errors() {
    let _g = TestGuard::new();
    assert!(get_ast("(echo foo").is_err());
  }

  // ─── brace group body errors ───────────────────────────────────────

  #[test]
  fn brace_group_with_leading_pipe_errors() {
    let _g = TestGuard::new();
    assert!(get_ast("{ | echo foo; }").is_err());
  }

  #[test]
  fn unclosed_brace_group_errors() {
    let _g = TestGuard::new();
    assert!(get_ast("{ echo foo").is_err());
  }

  // ─── case parsing errors ───────────────────────────────────────────

  #[test]
  fn bare_case_with_no_pattern_token_errors() {
    let _g = TestGuard::new();
    assert!(get_ast("case").is_err());
  }

  #[test]
  fn case_immediately_followed_by_in_keyword_errors() {
    // Hits the explicit `pat_tk.span.as_str() == "in"` check.
    let _g = TestGuard::new();
    assert!(get_ast("case in foo) ;; esac").is_err());
  }

  #[test]
  fn case_missing_in_after_variable_errors() {
    let _g = TestGuard::new();
    assert!(get_ast("case x foo) ;; esac").is_err());
  }

  // ─── case double-semi happy path (the *normal* break) ──────────────

  #[test]
  fn case_with_empty_arm_takes_double_semi_break() {
    // `;;` immediately after the pattern — the `if sep.has_double_semi()`
    // branch in the inner `while check_separator` loop fires.
    let g = TestGuard::new();
    test_input("case foo in foo) ;; esac").unwrap();
    let out = g.read_output();
    assert_eq!(out, "");
  }

  // ─── parse_time happy path ─────────────────────────────────────────

  #[test]
  fn time_wraps_a_command() {
    // Whole function body — `time` keyword consumed, inner command
    // parsed via parse_block(true), flags walked.
    let g = TestGuard::new();
    test_input("time echo hello_from_time").unwrap();
    assert!(g.read_output().contains("hello_from_time"));
  }

  #[test]
  fn time_with_no_following_command_errors() {
    let _g = TestGuard::new();
    assert!(get_ast("time").is_err());
  }

  // ─── parse_arith happy path ────────────────────────────────────────

  #[test]
  fn standalone_arithmetic_command_parses() {
    // (( expr )) as a standalone command — exercises parse_arith
    // from check_flags(IS_ARITH) through to the Arithmetic node.
    let _g = TestGuard::new();
    test_input("(( 1 + 2 ))").unwrap();
  }

  #[test]
  fn arithmetic_command_with_trailing_arg_errors() {
    // The `matches!(self.next_tk_class(), TkRule::Str)` check after
    // parse_redir fires.
    let _g = TestGuard::new();
    assert!(get_ast("(( 1 + 2 )) extra_arg").is_err());
  }

  // ─── negation error ────────────────────────────────────────────────

  #[test]
  fn bare_bang_with_no_command_errors() {
    let _g = TestGuard::new();
    assert!(get_ast("!").is_err());
  }

  // ─── if-then missing ───────────────────────────────────────────────

  #[test]
  fn if_without_then_errors() {
    let _g = TestGuard::new();
    assert!(get_ast("if echo foo; echo bar; fi").is_err());
  }

  // ─── C-style for: missing do / commands / done ─────────────────────

  #[test]
  fn arith_for_without_do_errors() {
    let _g = TestGuard::new();
    assert!(get_ast("for (( i=0; i<3; i=i+1 )); echo $i; done").is_err());
  }

  #[test]
  fn arith_for_with_empty_body_errors() {
    // parse_cmd_list returns None when next is the `done` keyword.
    let _g = TestGuard::new();
    assert!(get_ast("for (( i=0; i<3; i=i+1 )); do done").is_err());
  }

  #[test]
  fn arith_for_without_done_errors() {
    let _g = TestGuard::new();
    assert!(get_ast("for (( i=0; i<3; i=i+1 )); do echo $i;").is_err());
  }

  // ─── array-style for: missing var / do / done / commands ───────────

  #[test]
  fn for_with_in_but_no_variable_errors() {
    // `for in 1 2 3` — first token after `for` is `in`, so vars stays
    // empty and the early bail at 637 fires.
    let _g = TestGuard::new();
    assert!(get_ast("for in 1 2 3; do echo $x; done").is_err());
  }

  #[test]
  fn for_arr_without_do_errors() {
    let _g = TestGuard::new();
    assert!(get_ast("for x in 1 2 3; echo $x; done").is_err());
  }

  #[test]
  fn for_arr_with_empty_body_errors() {
    let _g = TestGuard::new();
    assert!(get_ast("for x in 1 2 3; do done").is_err());
  }

  #[test]
  fn for_arr_without_done_errors() {
    let _g = TestGuard::new();
    assert!(get_ast("for x in 1 2 3; do echo $x;").is_err());
  }

  // ─── while/until: missing command after keyword / missing do ───────

  #[test]
  fn while_with_no_condition_command_errors() {
    let _g = TestGuard::new();
    assert!(get_ast("while ; do echo; done").is_err());
  }

  #[test]
  fn until_with_no_condition_command_errors() {
    let _g = TestGuard::new();
    assert!(get_ast("until ; do echo; done").is_err());
  }

  #[test]
  fn while_without_do_after_condition_errors() {
    let _g = TestGuard::new();
    assert!(get_ast("while true; echo; done").is_err());
  }

  #[test]
  fn until_without_do_after_condition_errors() {
    let _g = TestGuard::new();
    assert!(get_ast("until false; echo; done").is_err());
  }

  #[test]
  fn for_empty_array_succeeds() {
    let _g = TestGuard::new();
    assert!(get_ast("for i in; do true; done").is_ok())
  }
}
