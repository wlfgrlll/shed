use ariadne::Label;
use bitflags::bitflags;
use std::{
  collections::VecDeque,
  fmt::{self, Debug},
  rc::Rc,
};

pub(crate) mod node;
pub(crate) use node::{
  AssignKind, CaseNode, CondNode, ConjunctNode, ConjunctOp, LoopKind, NdFlags, NdRule, Node,
};

#[cfg(test)]
pub(crate) use node::NdKind;

#[macro_use]
mod macros;
mod command;
mod compound;
mod util;

#[cfg(test)]
pub mod tests;

use super::{
  lex::{self, LexFlags, LexStream, Span, Tk, TkFlags, TkRule, clean_input},
  procio, sherr, two_way_display,
  util::{self as crate_util, ShErr, ShResult},
};

/// The parsed AST along with the source input it parsed
///
/// Uses Rc<str> instead of &str because the reference has to stay alive
/// while errors are propagated upwards The string also has to stay alive in the
/// case of pre-parsed shell function nodes, which live in the logic table Using
/// &str for this use-case dramatically overcomplicates the code
#[derive(Clone, Debug)]
pub(crate) struct ParsedSrc {
  pub src: Rc<str>,
  pub name: Rc<str>,
  pub ast: Ast,
  pub lex_flags: LexFlags,
  pub parse_flags: ParseFlags,
  pub context: LabelCtx,

  /// Not used internally, used mainly for auto-indent in the line editor. Mirrors the field on ParseStream
  pub block_depth: usize,
}

impl ParsedSrc {
  pub fn new(src: Rc<str>) -> Self {
    let src = if src.contains("\\\n") || src.contains('\r') {
      clean_input(&src).as_str().into()
    } else {
      src
    };
    Self {
      src,
      name: "<stdin>".into(),
      ast: Ast::new(vec![]),
      lex_flags: LexFlags::empty(),
      parse_flags: ParseFlags::empty(),
      context: VecDeque::new(),
      block_depth: 0,
    }
  }
  pub fn with_name(mut self, name: Rc<str>) -> Self {
    self.name = name;
    self
  }
  pub fn with_lex_flags(mut self, flags: LexFlags) -> Self {
    self.lex_flags = flags;
    self
  }
  pub fn with_parse_flags(mut self, flags: ParseFlags) -> Self {
    self.parse_flags = flags;
    self
  }
  pub fn parse_src(&mut self) -> Result<(), Vec<ShErr>> {
    let mut tokens = vec![];
    let mut errors = vec![];
    let mut stream = LexStream::new(self.src.clone(), self.lex_flags).with_name(self.name.clone());

    while let Some(lex_result) = stream.next() {
      // inline what the previous .filter() did
      if lex_result
        .as_ref()
        .is_ok_and(|tk| matches!(tk.class, TkRule::Comment))
      {
        continue;
      }
      match lex_result {
        Ok(token) => tokens.push(token),
        Err(error) => {
          if self.lex_flags.contains(LexFlags::LEX_UNFINISHED) {
            errors.push(error)
          } else {
            return Err(vec![error]);
          }
        }
      }
    }

    let in_array = stream.in_array();

    let mut nodes = vec![];
    let parser = ParseStream::new(tokens, self.context.clone()).with_flags(self.parse_flags);

    for parse_result in parser {
      match parse_result {
        Ok(node) => {
          self.block_depth = 0;
          nodes.push(node)
        }
        Err((depth, error)) => {
          self.block_depth = depth;
          if self.parse_flags.contains(ParseFlags::ERR_RETURN) {
            if in_array {
              self.block_depth += 1;
            }
            return Err(vec![error]);
          } else {
            errors.push(error);
          }
        }
      }
    }

    if in_array {
      self.block_depth += 1;
    }

    if !errors.is_empty() {
      return Err(errors);
    }

    *self.ast.tree_mut() = nodes;
    Ok(())
  }
  pub fn extract_nodes(&mut self) -> Vec<Node> {
    std::mem::take(self.ast.tree_mut())
  }
}

#[derive(Default, Clone, Debug)]
pub(crate) struct Ast(Vec<Node>);

impl Ast {
  pub fn new(tree: Vec<Node>) -> Self {
    Self(tree)
  }
  pub fn tree_mut(&mut self) -> &mut Vec<Node> {
    &mut self.0
  }
}

pub(crate) type LabelCtx = VecDeque<(Span, Label<Span>)>;

bitflags! {
  #[derive(Clone,Copy,Debug,Default,PartialEq,Eq,Hash,PartialOrd,Ord)]
  pub(crate) struct ParseFlags: u32 {
    const INCOMPLETE = 1 << 0; // Whether to error
    const ERR_RETURN = 1 << 1; // Return on first error instead of continuing
  }
}

struct ParseStream {
  pub tokens: Vec<Tk>,
  pub cursor: usize,
  pub context: LabelCtx,
  pub flags: ParseFlags,

  /// Not used internally, used mainly for auto-indent in the line editor
  pub block_depth: usize,
}

impl Debug for ParseStream {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.debug_struct("ParseStream")
      .field("tokens", &self.tokens)
      .field("cursor", &self.cursor)
      .finish()
  }
}

impl ParseStream {
  pub fn new(tokens: Vec<Tk>, context: LabelCtx) -> Self {
    let tokens = tokens
      .into_iter()
      .filter(|tk| tk.class != TkRule::Comment)
      .collect();
    Self {
      tokens,
      cursor: 0,
      context,
      block_depth: 0,
      flags: ParseFlags::empty(),
    }
  }
  pub fn with_flags(mut self, flags: ParseFlags) -> Self {
    self.flags = flags;
    self
  }
  fn parse_cmd_list(&mut self) -> ShResult<Option<Node>> {
    let mut commands = vec![];
    let mut node_tks = vec![];
    while let Some(command) = self.parse_conjunction()? {
      node_tks.extend(command.tokens.clone());
      commands.push(command);
    }

    if commands.is_empty() {
      Ok(None)
    } else {
      Ok(Some(node!(self, node_tks, NdRule::List { commands })))
    }
  }
  fn parse_conjunction(&mut self) -> ShResult<Option<Node>> {
    let mut elements = vec![];
    let mut node_tks = vec![];

    while let Some(mut block) = self.parse_block(true)? {
      node_tks.append(&mut block.tokens.clone());
      self.catch_separator(&mut node_tks);

      let conjunct_op = match self.next_tk_class() {
        TkRule::And => ConjunctOp::And,
        TkRule::Or => ConjunctOp::Or,
        _ => ConjunctOp::Null,
      };

      if conjunct_op != ConjunctOp::Null {
        block.walk_tree(&mut |nd| nd.flags |= NdFlags::NOT_ERR);
      }

      let conjunction = ConjunctNode {
        cmd: Box::new(block),
        operator: conjunct_op,
      };

      elements.push(conjunction);

      if conjunct_op != ConjunctOp::Null {
        let Some(tk) = self.next_tk() else { break };
        node_tks.push(tk);
        self.catch_separator(&mut node_tks);
      }

      if conjunct_op == ConjunctOp::Null {
        break;
      }
    }

    if elements.is_empty() {
      Ok(None)
    } else {
      Ok(Some(node!(
        self,
        node_tks,
        NdRule::Conjunction { elements }
      )))
    }
  }
  /// This tries to match on different stuff that can appear in a command
  /// position Matches shell commands like if-then-fi, pipelines, etc.
  /// Ordered from specialized to general, with more generally matchable stuff
  /// appearing at the bottom The check_pipelines parameter is used to prevent
  /// left-recursion issues in self.parse_pipeln()
  fn parse_block(&mut self, check_pipelines: bool) -> ShResult<Option<Node>> {
    if !check_pipelines {
      self.block_depth += 1;
    }

    // You will live to see man made horrors beyond your comprehension
    let result = || -> ShResult<Option<Node>> {
      if check_pipelines {
        try_match!(self.parse_pipeln()?);
        Ok(None)
      } else {
        try_match!(self.parse_func_def()?);
        try_match!(self.parse_brc_grp(false /* from_func_def */)?);
        try_match!(self.parse_subsh()?);
        try_match!(self.parse_case()?);
        try_match!(self.parse_loop()?);
        try_match!(self.parse_for()?);

        // these aren't nested contexts
        // so we decrement the depth and descend into
        // yet another immediately-invoked closure
        // please stabilize the try block my rust developers!!
        self.block_depth -= 1;
        let r = || -> ShResult<Option<Node>> {
          try_match!(self.parse_if()?);
          try_match!(self.parse_try()?);
          try_match!(self.parse_negate()?);
          try_match!(self.parse_time()?);
          try_match!(self.parse_func_keyword()?);
          try_match!(self.parse_arith()?);
          try_match!(self.parse_cmd()?);
          Ok(None)
        }()?;
        self.block_depth += 1;

        Ok(r)
      }
    }()?;

    if !check_pipelines {
      self.block_depth -= 1;
    }

    Ok(result)
  }
  fn parse_compound(&mut self) -> ShResult<Option<Node>> {
    // parse only a compound command.
    // used by function definition
    // because any compound command is a valid
    // function body.
    //
    // also we don't increment block_depth here because it
    // already happened in parse_block() -> parse_func_def()

    let result = || -> ShResult<Option<Node>> {
      try_match!(self.parse_brc_grp(true /* from_func_def */)?);
      try_match!(self.parse_subsh()?);
      try_match!(self.parse_case()?);
      try_match!(self.parse_loop()?);
      try_match!(self.parse_for()?);
      try_match!(self.parse_try()?);
      try_match!(self.parse_if()?);

      Ok(None)
    }()?;

    Ok(result)
  }
  fn panic_mode(&mut self, node_tks: &mut Vec<Tk>) {
    while let Some(tk) = self.next_tk() {
      node_tks.push(tk.clone());
      if tk.class == TkRule::Sep {
        break;
      }
    }
  }
}

impl Iterator for ParseStream {
  type Item = Result<Node, (usize, ShErr)>; // (block_depth and error)
  fn next(&mut self) -> Option<Self::Item> {
    // Empty token vector or only Soi/Eoi tokens, nothing to do
    if self.is_empty() && self.len() == 1 && self.tokens().last().unwrap().class == TkRule::Eoi {
      return None;
    }
    while let Some(tk) = self.tokens().first() {
      if let TkRule::Eoi = tk.class {
        return None;
      }
      if let TkRule::Soi | TkRule::Sep = tk.class {
        self.next_tk();
      } else {
        break;
      }
    }
    let result = self.parse_cmd_list();
    match result {
      Ok(Some(node)) => Some(Ok(node)),
      Ok(None) => None,
      Err(e) => {
        let block_depth = self.block_depth;
        Some(Err((block_depth, e)))
      }
    }
  }
}
