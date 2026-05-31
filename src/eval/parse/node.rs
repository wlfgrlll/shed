use crate::ShResult;

use super::{
  LabelCtx,
  lex::{Span, Tk, TkRule},
  procio::RedirSpec,
  two_way_display,
};
use ariadne::{Label, Span as AriadneSpan};
use bitflags::bitflags;

pub(crate) trait NodeVecUtils<Node> {
  fn get_span(&self) -> Option<Span>;
}

impl NodeVecUtils<Node> for Vec<Node> {
  fn get_span(&self) -> Option<Span> {
    if let Some(first_nd) = self.first()
      && let Some(last_nd) = self.last()
    {
      let first_start = first_nd.get_span().range().start;
      let last_end = last_nd.get_span().range().end;
      if first_start <= last_end {
        return Some(Span::new(
          first_start..last_end,
          first_nd.get_span().source().content(),
        ));
      }
    }
    None
  }
}

#[derive(Clone, Debug)]
pub(crate) struct Node {
  pub class: NdRule,
  pub flags: NdFlags,
  pub redirs: Vec<RedirSpec>,
  pub tokens: Vec<Tk>,
  pub context: LabelCtx,
}

impl Node {
  pub fn get_command(&self) -> Option<&Tk> {
    if let NdRule::Command {
      assignments: _,
      argv,
    } = &self.class
    {
      argv.iter().next()
    } else {
      None
    }
  }
  pub fn walk_tree<F: FnMut(&mut Node)>(&mut self, f: &mut F) {
    f(self);

    match self.class {
      NdRule::List { ref mut commands } => {
        for cmd in commands {
          cmd.walk_tree(f);
        }
      }
      NdRule::IfNode {
        ref mut cond_nodes,
        ref mut else_block,
      } => {
        for node in cond_nodes {
          let CondNode { cond, body } = node;
          cond.walk_tree(f);
          body.walk_tree(f);
        }

        if let Some(block) = else_block {
          block.walk_tree(f);
        }
      }
      NdRule::LoopNode {
        kind: _,
        ref mut cond_node,
      } => {
        let CondNode { cond, body } = cond_node;
        cond.walk_tree(f);
        body.walk_tree(f);
      }
      NdRule::ForNode { ref mut body, .. }
      | NdRule::TryNode { ref mut body, .. }
      | NdRule::FuncDef { ref mut body, .. }
      | NdRule::DeferNode { ref mut body }
      | NdRule::Subshell { ref mut body }
      | NdRule::BraceGrp { ref mut body } => {
        body.walk_tree(f);
      }
      NdRule::ForArith {
        ref mut init,
        ref mut cond,
        ref mut step,
        ref mut body,
      } => {
        if let Some(init) = init {
          init.walk_tree(f);
        }
        if let Some(cond) = cond {
          cond.walk_tree(f);
        }
        if let Some(step) = step {
          step.walk_tree(f);
        }
        body.walk_tree(f);
      }
      NdRule::CaseNode {
        pattern: _,
        ref mut case_blocks,
      } => {
        for block in case_blocks {
          let CaseNode { patterns: _, body } = block;
          body.walk_tree(f);
        }
      }
      NdRule::Command {
        ref mut assignments,
        argv: _,
      } => {
        for assign_node in assignments {
          assign_node.walk_tree(f);
        }
      }
      NdRule::Pipeline { ref mut cmds } => {
        for cmd_node in cmds {
          cmd_node.walk_tree(f);
        }
      }
      NdRule::Conjunction { ref mut elements } => {
        for node in elements.iter_mut() {
          let ConjunctNode { cmd, operator: _ } = node;
          cmd.walk_tree(f);
        }
      }
      NdRule::Timed { ref mut cmd } | NdRule::Negate { ref mut cmd } => {
        cmd.walk_tree(f);
      }
      NdRule::Arithmetic { .. } | NdRule::Assignment { .. } => (), // No nodes to check
    }
  }
  pub fn eager_expand(&mut self) -> ShResult<()> {
    let expand_tk = |tk: &mut Tk| -> ShResult<()> {
      *tk = std::mem::take(tk).expand()?;
      Ok(())
    };

    match &mut self.class {
      NdRule::Command { argv: tks, .. } | NdRule::ForNode { arr: tks, .. } => {
        for tk in tks {
          expand_tk(tk)?;
        }
      }
      NdRule::Assignment { val: tk, .. }
      | NdRule::CaseNode { pattern: tk, .. }
      | NdRule::Arithmetic { body: tk } => {
        expand_tk(tk)?;
      }

      _ => {}
    }

    Ok(())
  }
  pub fn propagate_context(&mut self, ctx: &(Span, Label<Span>)) {
    self.walk_tree(&mut |nd| nd.context.push_back(ctx.clone()));
  }
  pub fn get_span(&self) -> Span {
    let Some(first_tk) = self.tokens.first() else {
      unreachable!()
    };
    let last_tk = self
      .tokens
      .iter()
      .rev()
      .find(|tk| !matches!(tk.class, TkRule::Sep | TkRule::Eoi))
      .unwrap_or(first_tk);

    Span::from_span_source(
      first_tk.span.range().start..last_tk.span.range().end,
      first_tk.span.span_source().clone(),
    )
    .at(first_tk.span.pos())
  }
}

bitflags! {
#[derive(Clone,Copy,Debug)]
  pub struct NdFlags: u32 {
    const BACKGROUND    = 1 << 0;
    const FORK_BUILTINS = 1 << 1;
    const NO_FORK       = 1 << 2;
    const ARR_ASSIGN    = 1 << 3;
    const PIPE_ERR      = 1 << 4; // whether to include stderr in a pipe
    const NOT_ERR       = 1 << 5; // whether an error triggers ERR traps and set -e
    const PIPE_CMD      = 1 << 6; // is not the last command in a pipeline
    const NO_SPLIT      = 1 << 7; // don't split words, used in double bracket tests ('[[')
  }
}

#[derive(Clone, Debug)]
pub(crate) struct CondNode {
  pub cond: Box<Node>,
  pub body: Box<Node>,
}

#[derive(Clone, Debug)]
pub(crate) struct CaseNode {
  pub patterns: Vec<Tk>,
  pub body: Box<Node>,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum ConjunctOp {
  And,
  Or,
  Null,
}

#[derive(Clone, Debug)]
pub(crate) struct ConjunctNode {
  pub cmd: Box<Node>,
  pub operator: ConjunctOp,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum LoopKind {
  While,
  Until,
}

two_way_display!(LoopKind,
  While <=> "while";
  Until <=> "until";
);

#[derive(Clone, Debug)]
pub(crate) enum AssignKind {
  Eq,
  PlusEq,
  MinusEq,
  MultEq,
  DivEq,
}

/// Flat `NdRule` names used mainly for debugging
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum NdKind {
  List,
  IfNode,
  LoopNode,
  ForNode,
  ForArith,
  Arithmetic,
  CaseNode,
  TryNode,
  DeferNode,
  Command,
  Pipeline,
  Conjunction,
  Assignment,
  BraceGrp,
  Subsh,
  Negate,
  Timed,
  FuncDef,
}

impl NdRule {
  pub fn as_nd_kind(&self) -> NdKind {
    match self {
      Self::List { .. } => NdKind::List,
      Self::Negate { .. } => NdKind::Negate,
      Self::IfNode { .. } => NdKind::IfNode,
      Self::LoopNode { .. } => NdKind::LoopNode,
      Self::ForNode { .. } => NdKind::ForNode,
      Self::TryNode { .. } => NdKind::TryNode,
      Self::DeferNode { .. } => NdKind::DeferNode,
      Self::ForArith { .. } => NdKind::ForArith,
      Self::Arithmetic { .. } => NdKind::Arithmetic,
      Self::CaseNode { .. } => NdKind::CaseNode,
      Self::Command { .. } => NdKind::Command,
      Self::Pipeline { .. } => NdKind::Pipeline,
      Self::Conjunction { .. } => NdKind::Conjunction,
      Self::Assignment { .. } => NdKind::Assignment,
      Self::Timed { .. } => NdKind::Timed,
      Self::BraceGrp { .. } => NdKind::BraceGrp,
      Self::FuncDef { .. } => NdKind::FuncDef,
      Self::Subshell { .. } => NdKind::Subsh,
    }
  }
}

#[derive(Clone, Debug)]
pub(crate) enum NdRule {
  List {
    commands: Vec<Node>,
  },
  IfNode {
    cond_nodes: Vec<CondNode>,
    else_block: Option<Box<Node>>,
  },
  LoopNode {
    kind: LoopKind,
    cond_node: CondNode,
  },
  ForNode {
    vars: Vec<Tk>,
    arr: Vec<Tk>,
    body: Box<Node>,
  },
  TryNode {
    body: Box<Node>,
    err: Vec<Tk>,
  },
  DeferNode {
    body: Box<Node>,
  },
  ForArith {
    init: Option<Box<Node>>,
    cond: Option<Box<Node>>,
    step: Option<Box<Node>>,
    body: Box<Node>,
  },
  Arithmetic {
    body: Tk,
  },
  Negate {
    cmd: Box<Node>,
  },
  Timed {
    cmd: Box<Node>,
  },
  CaseNode {
    pattern: Tk,
    case_blocks: Vec<CaseNode>,
  },
  Command {
    assignments: Vec<Node>,
    argv: Vec<Tk>,
  },
  Pipeline {
    cmds: Vec<Node>,
  },
  Conjunction {
    elements: Vec<ConjunctNode>,
  },
  Assignment {
    kind: AssignKind,
    var: Tk,
    val: Tk,
  },
  Subshell {
    body: Box<Node>,
  },
  BraceGrp {
    body: Box<Node>,
  },
  FuncDef {
    name: Tk,
    body: Box<Node>,
  },
}
