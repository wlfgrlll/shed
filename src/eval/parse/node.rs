use crate::two_way_display;

use super::{
  LabelCtx,
  lex::{Span, SpanSource, Tk},
  procio::RedirSpec,
};
use ariadne::{Label, Span as AriadneSpan};
use bitflags::bitflags;

pub(crate) const TEST_UNARY_OPS: [&str; 23] = [
  "-a", "-b", "-c", "-d", "-e", "-f", "-g", "-h", "-L", "-k", "-n", "-p", "-r", "-s", "-S", "-t",
  "-u", "-w", "-x", "-z", "-O", "-G", "-N",
];

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
      NdRule::ForNode {
        vars: _,
        arr: _,
        ref mut body,
      } => {
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
          let CaseNode { pattern: _, body } = block;
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
      NdRule::Subshell { ref mut body } | NdRule::BraceGrp { ref mut body } => {
        body.walk_tree(f);
      }
      NdRule::FuncDef {
        name: _,
        ref mut body,
      } => {
        body.walk_tree(f);
      }
      NdRule::Negate { ref mut cmd } => {
        cmd.walk_tree(f);
      }
      NdRule::Arithmetic { .. } | NdRule::Test { .. } | NdRule::Assignment { .. } => (), // No nodes to check
    }
  }
  pub fn propagate_context(&mut self, ctx: (SpanSource, Label<Span>)) {
    self.walk_tree(&mut |nd| nd.context.push_back(ctx.clone()));
  }
  pub fn get_span(&self) -> Span {
    let Some(first_tk) = self.tokens.first() else {
      unreachable!()
    };
    let Some(last_tk) = self.tokens.last() else {
      unreachable!()
    };

    Span::from_span_source(
      first_tk.span.range().start..last_tk.span.range().end,
      first_tk.span.span_source().clone(),
    ).at(first_tk.span.pos())
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
    const REPORT_TIME   = 1 << 7; // whether this node should be reported by the time keyword
  }
}

#[derive(Clone, Debug)]
pub(crate) struct CondNode {
  pub cond: Box<Node>,
  pub body: Box<Node>,
}

#[derive(Clone, Debug)]
pub(crate) struct CaseNode {
  pub pattern: Tk,
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
pub(crate) enum TestCase {
  Unary {
    operator: Tk,
    operand: Tk,
    conjunct: Option<ConjunctOp>,
  },
  Binary {
    lhs: Tk,
    operator: Tk,
    rhs: Tk,
    conjunct: Option<ConjunctOp>,
  },
}

#[derive(Default, Clone, Debug)]
pub(crate) struct TestCaseBuilder {
  lhs: Option<Tk>,
  operator: Option<Tk>,
  rhs: Option<Tk>,
  conjunct: Option<ConjunctOp>,
}

impl TestCaseBuilder {
  pub fn new() -> Self {
    Self::default()
  }
  pub fn is_empty(&self) -> bool {
    self.lhs.is_none() && self.operator.is_none() && self.rhs.is_none() && self.conjunct.is_none()
  }
  pub fn wants_operator(&self) -> bool {
    self.operator.is_none() && (self.lhs.is_some())
  }
  pub fn wants_rhs(&self) -> bool {
    self.rhs.is_none() && self.operator.is_some()
  }
  pub fn with_lhs(self, lhs: Tk) -> Self {
    let Self {
      lhs: _,
      operator,
      rhs,
      conjunct,
    } = self;
    Self {
      lhs: Some(lhs),
      operator,
      rhs,
      conjunct,
    }
  }
  pub fn with_rhs(self, rhs: Tk) -> Self {
    let Self {
      lhs,
      operator,
      rhs: _,
      conjunct,
    } = self;
    Self {
      lhs,
      operator,
      rhs: Some(rhs),
      conjunct,
    }
  }
  pub fn with_operator(self, operator: Tk) -> Self {
    let Self {
      lhs,
      operator: _,
      rhs,
      conjunct,
    } = self;
    Self {
      lhs,
      operator: Some(operator),
      rhs,
      conjunct,
    }
  }
  pub fn with_conjunction(self, conjunction: ConjunctOp) -> Self {
    let Self {
      lhs,
      operator,
      rhs,
      conjunct: _,
    } = self;
    Self {
      lhs,
      operator,
      rhs,
      conjunct: Some(conjunction),
    }
  }
  pub fn can_build(&self) -> bool {
    self.operator.is_some() && self.rhs.is_some()
  }
  pub fn build_and_take(&mut self) -> TestCase {
    if self.lhs.is_some() {
      TestCase::Binary {
        lhs: self.lhs.take().unwrap(),
        operator: self.operator.take().unwrap(),
        rhs: self.rhs.take().unwrap(),
        conjunct: self.conjunct.take(),
      }
    } else {
      TestCase::Unary {
        operator: self.operator.take().unwrap(),
        operand: self.rhs.take().unwrap(),
        conjunct: self.conjunct.take(),
      }
    }
  }

  pub(crate) fn conjunct(&self) -> Option<ConjunctOp> {
    self.conjunct
  }
}

#[derive(Clone, Debug)]
pub(crate) enum AssignKind {
  Eq,
  PlusEq,
  MinusEq,
  MultEq,
  DivEq,
}

/// Flat NdRule names used mainly for debugging
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum NdKind {
  List,
  IfNode,
  LoopNode,
  ForNode,
  ForArith,
  Arithmetic,
  CaseNode,
  Command,
  Pipeline,
  Conjunction,
  Assignment,
  BraceGrp,
  Subsh,
  Negate,
  Test,
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
      Self::ForArith { .. } => NdKind::ForArith,
      Self::Arithmetic { .. } => NdKind::Arithmetic,
      Self::CaseNode { .. } => NdKind::CaseNode,
      Self::Command { .. } => NdKind::Command,
      Self::Pipeline { .. } => NdKind::Pipeline,
      Self::Conjunction { .. } => NdKind::Conjunction,
      Self::Assignment { .. } => NdKind::Assignment,
      Self::BraceGrp { .. } => NdKind::BraceGrp,
      Self::Test { .. } => NdKind::Test,
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
  Test {
    cases: Vec<TestCase>,
  },
  FuncDef {
    name: Tk,
    body: Box<Node>,
  },
}
