use ariadne::Label;
use std::rc::Rc;

use super::{
  CaseNode, CondNode, ConjunctOp, LoopKind, NdFlags, NdRule, Node, ParseStream, ShResult,
  TEST_UNARY_OPS, TestCase, Tk, TkFlags, TkRule, node::TestCaseBuilder, util::split_for_arith_tk,
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
  pub(super) fn parse_test(&mut self) -> ShResult<Option<Node>> {
    let mut node_tks: Vec<Tk> = vec![];
    let mut cases: Vec<TestCase> = vec![];
    if !self.check_keyword("[[") || !self.next_tk_is_some() {
      return Ok(None);
    }
    node_tks.push(self.next_tk().unwrap());
    let mut case_builder = TestCaseBuilder::new();
    while let Some(tk) = self.next_tk() {
      node_tks.push(tk.clone());
      if tk.as_str() == "]]" {
        if case_builder.can_build() {
          let case = case_builder.build_and_take();
          cases.push(case);
          break;
        } else if cases.is_empty() {
          return Err(parse_err!(self, node_tks, "Malformed test call"));
        } else {
          break;
        }
      }
      if case_builder.is_empty() {
        match tk.as_str() {
          _ if TEST_UNARY_OPS.contains(&tk.as_str()) => {
            case_builder = case_builder.with_operator(tk.clone())
          }
          _ => case_builder = case_builder.with_lhs(tk.clone()),
        }
        continue;
      } else if case_builder.wants_rhs() {
        case_builder = case_builder.with_rhs(tk.clone());
        continue;
      } else if case_builder.wants_operator() {
        // we got lhs, then rhs -> treat it as operator maybe?
        case_builder = case_builder.with_operator(tk.clone());
        continue;
      } else if let TkRule::And | TkRule::Or = tk.class {
        if case_builder.can_build() {
          if case_builder.conjunct().is_some() {
            return Err(parse_err!(
              self,
              node_tks,
              "Invalid placement for logical operator in test"
            ));
          }
          let op = match tk.class {
            TkRule::And => ConjunctOp::And,
            TkRule::Or => ConjunctOp::Or,
            _ => unreachable!(),
          };
          case_builder = case_builder.with_conjunction(op);
          let case = case_builder.build_and_take();
          cases.push(case);
          continue;
        } else {
          return Err(parse_err!(
            self,
            node_tks,
            "Invalid placement for logical operator in test"
          ));
        }
      }
      if case_builder.can_build() {
        let case = case_builder.build_and_take();
        cases.push(case);
      }
    }
    self.catch_separator(&mut node_tks);

    Ok(Some(node!(self, node_tks, NdRule::Test { cases })))
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
      if !self.check_case_pattern() || !self.next_tk_is_some() {
        bail!(self, node_tks, "Expected a case pattern here");
      }
      let case_pat_tk = self.next_tk().unwrap();
      node_tks.push(case_pat_tk.clone());
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
        pattern: case_pat_tk,
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
    // the 'time' keyword does not have it's own NdRule. This is because it does not alter execution in any meaningful way.
    // All it does here is set the REPORT_TIME flag on the node it wraps. Then we just return the node itself.
    // Also, this is an unchecked context for the purpose of 'set -e' errors.
    cmd.walk_tree(&mut |n| n.flags |= NdFlags::NOT_ERR | NdFlags::REPORT_TIME);

    node_tks.extend(cmd.tokens.clone());
    self.catch_separator(&mut node_tks);
    Ok(Some(cmd))
  }
  pub(super) fn parse_func_keyword(&mut self) -> ShResult<Option<Node>> {
    if !self.check_keyword("function") || !self.next_tk_is_some() {
      return Ok(None);
    }

    let Some(func_def) = self.parse_func_def()? else {
      bail!(
        self,
        vec![],
        "Malformed function definition after 'function' keyword"
      );
    };

    Ok(Some(func_def))
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
    if arr.is_empty() {
      bail!(self, node_tks, "Expected an array for this for loop");
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
