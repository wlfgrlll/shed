use std::str::FromStr;

use crate::expand::escape::unescape_math;
use crate::expand::var::expand_raw;
use crate::state::{Shed, vars::VarFlags, vars::VarKind};
use crate::util::{ShErr, ShResult};
use crate::{match_loop, sherr};

#[derive(Debug, Clone)]
enum ArithOp {
  // math
  Add,
  Sub,
  Mul,
  Div,
  Mod,
  // comparison
  Lt,
  Gt,
  Le,
  Ge,
  Eq,
  Ne,
  // logical
  And,
  Or,
  // bitwise
  BitAnd,
  BitOr,
  BitXor,
  ShiftL,
  ShiftR,
  // assign
  Assign,
  PlusAssign,
  MinusAssign,
  MulAssign,
  DivAssign,
  ModAssign,
  BitAndAssign,
  BitOrAssign,
  BitXorAssign,
  ShiftLAssign,
  ShiftRAssign,
}

impl FromStr for ArithOp {
  type Err = ShErr;
  fn from_str(s: &str) -> Result<Self, Self::Err> {
    match s {
      "+" => Ok(Self::Add),
      "-" => Ok(Self::Sub),
      "*" => Ok(Self::Mul),
      "/" => Ok(Self::Div),
      "%" => Ok(Self::Mod),
      "<" => Ok(Self::Lt),
      ">" => Ok(Self::Gt),
      "<=" => Ok(Self::Le),
      ">=" => Ok(Self::Ge),
      "==" => Ok(Self::Eq),
      "!=" => Ok(Self::Ne),
      "&&" => Ok(Self::And),
      "||" => Ok(Self::Or),
      "&" => Ok(Self::BitAnd),
      "|" => Ok(Self::BitOr),
      "^" => Ok(Self::BitXor),
      "<<" => Ok(Self::ShiftL),
      ">>" => Ok(Self::ShiftR),
      "=" => Ok(Self::Assign),
      "+=" => Ok(Self::PlusAssign),
      "-=" => Ok(Self::MinusAssign),
      "*=" => Ok(Self::MulAssign),
      "/=" => Ok(Self::DivAssign),
      "%=" => Ok(Self::ModAssign),
      "&=" => Ok(Self::BitAndAssign),
      "|=" => Ok(Self::BitOrAssign),
      "^=" => Ok(Self::BitXorAssign),
      "<<=" => Ok(Self::ShiftLAssign),
      ">>=" => Ok(Self::ShiftRAssign),
      _ => Err(sherr!(ParseErr, "Unknown operator: '{s}'")),
    }
  }
}

#[derive(Debug, Clone)]
enum ArithTk {
  Num(i64),
  Op(ArithOp),
  Comma,
  LParen,
  RParen,
  Inc,      // ++ (raw, resolved to prefix/postfix during to_rpn)
  Dec,      // -- (raw, resolved to prefix/postfix during to_rpn)
  Not,      // !
  Neg,      // unary -
  UPlus,    // unary +
  BitNot,   // ~
  Question, // ? (lex token)
  Colon,    // : (lex token)
  Var(String),

  // RPN-only opcodes for control flow (jump-based short-circuit + ternary)
  JumpIfZero(usize),        // pop, if 0 jump to offset
  JumpIfZeroPeek(usize),    // peek, if 0 jump (used by &&)
  JumpIfNonZeroPeek(usize), // peek, if non-zero jump (used by ||)
  Jump(usize),              // unconditional jump
  Pop,                      // discard top of stack
  Nez,                      // replace top with 0 if it's 0, else 1

  // Ops-stack-only pending markers (carry indices to patch when flushed)
  PendingAnd(usize),         // jump_idx of the placeholder JIFZ_PEEK
  PendingOr(usize),          // jump_idx of the placeholder JIFNZ_PEEK
  PendingTernaryThen(usize), // jump_idx of the placeholder JIFZ (cond→else)
  PendingTernaryElse(usize), // jump_idx of the placeholder JMP (then→end)
}

// Stack value used during eval_rpn, keeps Var names alive for assignment targets
enum StackVal {
  Num(i64),
  Var(String),
}

impl StackVal {
  fn to_num(&self) -> ShResult<i64> {
    match self {
      StackVal::Num(n) => Ok(*n),
      StackVal::Var(name) => {
        let val = Shed::vars(|v| v.try_get_var(name)).unwrap_or_else(|| "0".into());
        val
          .parse::<i64>()
          .map_err(|_| sherr!(ParseErr, "Variable '{name}' does not contain an integer",))
      }
    }
  }
}

fn read_var_as_i64(name: &str) -> ShResult<i64> {
  let val = Shed::vars(|v| v.try_get_var(name)).unwrap_or_else(|| "0".into());
  val
    .parse::<i64>()
    .map_err(|_| sherr!(ParseErr, "Variable '{name}' does not contain an integer",))
}

impl ArithTk {
  pub fn tokenize(raw: &str) -> ShResult<Vec<Self>> {
    let mut tokens = Vec::new();
    let mut chars = raw.chars().peekable();
    // Track whether the last emitted token was an operand, to distinguish
    // unary minus from binary subtraction.
    let mut last_was_operand = false;

    match_loop!(chars.peek() => &ch => ch, {
      ' ' | '\t' => { chars.next(); }

      '0'..='9' => {
        let mut num = String::new();
        let first = chars.next().unwrap();
        num.push(first);

        // Hex (0x... / 0X...) or octal (0NNN); otherwise decimal.
        let parsed: i64 = if first == '0' && matches!(chars.peek(), Some('x' | 'X')) {
          chars.next(); // consume x/X
          let mut hex = String::new();
          while let Some(&d) = chars.peek() {
            if d.is_ascii_hexdigit() {
              hex.push(d);
              chars.next();
            } else {
              break;
            }
          }
          if hex.is_empty() {
            return Err(sherr!(ParseErr, "Invalid hex literal '0{}'", first));
          }
          i64::from_str_radix(&hex, 16).map_err(|_| sherr!(
            ParseErr, "Invalid hex literal: '0x{}'", hex,
          ))?
        } else if first == '0' && chars.peek().is_some_and(|d| d.is_ascii_digit()) {
          // Octal, collect remaining octal digits.
          let mut oct = String::new();
          while let Some(&d) = chars.peek() {
            if matches!(d, '0'..='7') {
              oct.push(d);
              chars.next();
            } else if d.is_ascii_digit() {
              return Err(sherr!(ParseErr, "Invalid digit '{}' in octal literal", d));
            } else {
              break;
            }
          }
          i64::from_str_radix(&oct, 8).map_err(|_| sherr!(
            ParseErr, "Invalid octal literal: '0{}'", oct,
          ))?
        } else {
          while let Some(&d) = chars.peek() {
            if d.is_ascii_digit() {
              num.push(d);
              chars.next();
            } else {
              break;
            }
          }
          num.parse::<i64>().map_err(|_| sherr!(
            ParseErr, "Invalid number in arithmetic expression: '{}'", num,
          ))?
        };

        tokens.push(Self::Num(parsed));
        last_was_operand = true;
      }

      '-' => {
        chars.next();
        if chars.peek() == Some(&'-') {
          chars.next();
          tokens.push(Self::Dec);
          // postfix Dec: last_was_operand stays true if it was; prefix Dec: next is a var
        } else if chars.peek() == Some(&'=') {
          chars.next();
          tokens.push(Self::Op(ArithOp::MinusAssign));
          last_was_operand = false;
        } else if last_was_operand {
          tokens.push(Self::Op(ArithOp::Sub));
          last_was_operand = false;
        } else {
          tokens.push(Self::Neg);
          // last_was_operand stays false, Neg is unary prefix
        }
      }

      '+' => {
        chars.next();
        if chars.peek() == Some(&'+') {
          chars.next();
          tokens.push(Self::Inc);
        } else if chars.peek() == Some(&'=') {
          chars.next();
          tokens.push(Self::Op(ArithOp::PlusAssign));
          last_was_operand = false;
        } else if last_was_operand {
          tokens.push(Self::Op(ArithOp::Add));
          last_was_operand = false;
        } else {
          tokens.push(Self::UPlus);
          // last_was_operand stays false, UPlus is unary prefix
        }
      }

      '*' => {
        chars.next();
        if chars.peek() == Some(&'=') {
          chars.next();
          tokens.push(Self::Op(ArithOp::MulAssign));
        } else {
          tokens.push(Self::Op(ArithOp::Mul));
        }
        last_was_operand = false;
      }

      '/' => {
        chars.next();
        if chars.peek() == Some(&'=') {
          chars.next();
          tokens.push(Self::Op(ArithOp::DivAssign));
        } else {
          tokens.push(Self::Op(ArithOp::Div));
        }
        last_was_operand = false;
      }

      '%' => {
        chars.next();
        if chars.peek() == Some(&'=') {
          chars.next();
          tokens.push(Self::Op(ArithOp::ModAssign));
        } else {
          tokens.push(Self::Op(ArithOp::Mod));
        }
        last_was_operand = false;
      }

      '<' => {
        chars.next();
        if chars.peek() == Some(&'=') {
          chars.next();
          tokens.push(Self::Op(ArithOp::Le));
        } else if chars.peek() == Some(&'<') {
          chars.next();
          if chars.peek() == Some(&'=') {
            chars.next();
            tokens.push(Self::Op(ArithOp::ShiftLAssign));
          } else {
            tokens.push(Self::Op(ArithOp::ShiftL));
          }
        } else {
          tokens.push(Self::Op(ArithOp::Lt));
        }
        last_was_operand = false;
      }

      '>' => {
        chars.next();
        if chars.peek() == Some(&'=') {
          chars.next();
          tokens.push(Self::Op(ArithOp::Ge));
        } else if chars.peek() == Some(&'>') {
          chars.next();
          if chars.peek() == Some(&'=') {
            chars.next();
            tokens.push(Self::Op(ArithOp::ShiftRAssign));
          } else {
            tokens.push(Self::Op(ArithOp::ShiftR));
          }
        } else {
          tokens.push(Self::Op(ArithOp::Gt));
        }
        last_was_operand = false;
      }

      '=' => {
        chars.next();
        if chars.peek() == Some(&'=') {
          chars.next();
          tokens.push(Self::Op(ArithOp::Eq));
        } else {
          tokens.push(Self::Op(ArithOp::Assign));
        }
        last_was_operand = false;
      }

      '!' => {
        chars.next();
        if chars.peek() == Some(&'=') {
          chars.next();
          tokens.push(Self::Op(ArithOp::Ne));
          last_was_operand = false;
        } else {
          tokens.push(Self::Not);
          last_was_operand = false;
        }
      }

      '&' => {
        chars.next();
        if chars.peek() == Some(&'&') {
          chars.next();
          tokens.push(Self::Op(ArithOp::And));
        } else if chars.peek() == Some(&'=') {
          chars.next();
          tokens.push(Self::Op(ArithOp::BitAndAssign));
        } else {
          tokens.push(Self::Op(ArithOp::BitAnd));
        }
        last_was_operand = false;
      }

      '|' => {
        chars.next();
        if chars.peek() == Some(&'|') {
          chars.next();
          tokens.push(Self::Op(ArithOp::Or));
        } else if chars.peek() == Some(&'=') {
          chars.next();
          tokens.push(Self::Op(ArithOp::BitOrAssign));
        } else {
          tokens.push(Self::Op(ArithOp::BitOr));
        }
        last_was_operand = false;
      }

      '^' => {
        chars.next();
        if chars.peek() == Some(&'=') {
          chars.next();
          tokens.push(Self::Op(ArithOp::BitXorAssign));
        } else {
          tokens.push(Self::Op(ArithOp::BitXor));
        }
        last_was_operand = false;
      }

      '~' => {
        chars.next();
        tokens.push(Self::BitNot);
        // last_was_operand stays false, BitNot is unary prefix
      }

      ',' => {
        tokens.push(Self::Comma);
        chars.next();
        last_was_operand = false;
      }

      '?' => {
        tokens.push(Self::Question);
        chars.next();
        last_was_operand = false;
      }

      ':' => {
        tokens.push(Self::Colon);
        chars.next();
        last_was_operand = false;
      }

      '(' => {
        tokens.push(Self::LParen);
        chars.next();
        last_was_operand = false;
      }

      ')' => {
        tokens.push(Self::RParen);
        chars.next();
        last_was_operand = true;
      }

      _ if ch.is_alphabetic() || ch == '_' => {
        chars.next();
        let mut var_name = ch.to_string();
        while let Some(ch) = chars.peek() {
          match ch {
            _ if ch.is_alphabetic() || *ch == '_' || ch.is_ascii_digit() => {
              var_name.push(*ch);
              chars.next();
            }
            _ => break,
          }
        }
        tokens.push(Self::Var(var_name));
        last_was_operand = true;
      }

      _ => {
        return Err(sherr!(
          ParseErr,
          "Unexpected character in arithmetic expression: '{ch}'",
        ));
      }
    });

    Ok(tokens)
  }

  fn to_rpn(tokens: Vec<ArithTk>) -> ShResult<Vec<ArithTk>> {
    let mut output: Vec<ArithTk> = Vec::new();
    let mut ops: Vec<ArithTk> = Vec::new();
    let mut tokens = tokens.into_iter().peekable();

    fn precedence(tk: &ArithTk) -> usize {
      match tk {
        ArithTk::Comma => 0,
        // Pending markers participate in flushing at their op's precedence.
        ArithTk::PendingTernaryElse(_) => 1,
        ArithTk::PendingOr(_) => 2,
        ArithTk::PendingAnd(_) => 3,
        ArithTk::Op(op) => match op {
          ArithOp::Assign
          | ArithOp::PlusAssign
          | ArithOp::MinusAssign
          | ArithOp::MulAssign
          | ArithOp::DivAssign
          | ArithOp::ModAssign
          | ArithOp::BitAndAssign
          | ArithOp::BitOrAssign
          | ArithOp::BitXorAssign
          | ArithOp::ShiftLAssign
          | ArithOp::ShiftRAssign => 1,
          ArithOp::Or => 2,
          ArithOp::And => 3,
          ArithOp::BitOr => 4,
          ArithOp::BitXor => 5,
          ArithOp::BitAnd => 6,
          ArithOp::Eq | ArithOp::Ne => 7,
          ArithOp::Lt | ArithOp::Gt | ArithOp::Le | ArithOp::Ge => 8,
          ArithOp::ShiftL | ArithOp::ShiftR => 9,
          ArithOp::Add | ArithOp::Sub => 10,
          ArithOp::Mul | ArithOp::Div | ArithOp::Mod => 11,
        },
        ArithTk::Not | ArithTk::Neg | ArithTk::UPlus | ArithTk::BitNot => 12,
        _ => 0,
      }
    }

    fn is_right_assoc(tk: &ArithTk) -> bool {
      matches!(
        tk,
        ArithTk::Not
          | ArithTk::Neg
          | ArithTk::UPlus
          | ArithTk::BitNot
          | ArithTk::PendingTernaryElse(_)
          | ArithTk::Op(
            ArithOp::Assign
              | ArithOp::PlusAssign
              | ArithOp::MinusAssign
              | ArithOp::MulAssign
              | ArithOp::DivAssign
              | ArithOp::ModAssign
              | ArithOp::BitAndAssign
              | ArithOp::BitOrAssign
              | ArithOp::BitXorAssign
              | ArithOp::ShiftLAssign
              | ArithOp::ShiftRAssign
          )
      )
    }

    // Pop one op off the ops stack, translating control-flow markers into
    // their final RPN form (and patching the corresponding jump placeholders).
    fn pop_to_output(ops: &mut Vec<ArithTk>, output: &mut Vec<ArithTk>) -> ShResult<()> {
      match ops.pop().unwrap() {
        ArithTk::PendingAnd(jump_idx) => {
          // After RHS of && is built. Patch JIFZ_PEEK to land at the Nez we
          // emit now, so the short-circuit jump produces 0 directly.
          let target = output.len();
          output.push(ArithTk::Nez);
          output[jump_idx] = ArithTk::JumpIfZeroPeek(target);
        }
        ArithTk::PendingOr(jump_idx) => {
          let target = output.len();
          output.push(ArithTk::Nez);
          output[jump_idx] = ArithTk::JumpIfNonZeroPeek(target);
        }
        ArithTk::PendingTernaryElse(jump_idx) => {
          // After else-arm. Patch the JMP that skips past the else.
          let target = output.len();
          output[jump_idx] = ArithTk::Jump(target);
        }
        ArithTk::PendingTernaryThen(_) => {
          return Err(sherr!(
            ParseErr,
            "'?' without matching ':' in arithmetic expression"
          ));
        }
        ArithTk::Question => {
          return Err(sherr!(
            ParseErr,
            "'?' without matching ':' in arithmetic expression"
          ));
        }
        other => output.push(other),
      }
      Ok(())
    }

    // True if `top` is a barrier that flushing must stop at.
    fn is_barrier(top: &ArithTk) -> bool {
      matches!(top, ArithTk::LParen | ArithTk::Question)
    }

    fn flush_ops(
      ops: &mut Vec<ArithTk>,
      output: &mut Vec<ArithTk>,
      until_paren: bool,
    ) -> ShResult<()> {
      while let Some(top) = ops.last() {
        if matches!(top, ArithTk::LParen) {
          break;
        }
        pop_to_output(ops, output)?;
      }
      if until_paren {
        ops.pop(); // remove the LParen
      }
      Ok(())
    }

    match_loop!(tokens.next() => token, {
      ArithTk::Num(_) => output.push(token),

      ArithTk::Var(ref var) => {
        // Check for postfix inc/dec
        if tokens.peek().is_some_and(|tk| matches!(tk, ArithTk::Inc | ArithTk::Dec)) {
          let op = tokens.next().unwrap();
          let val = read_var_as_i64(var)?;
          let delta: i64 = if matches!(op, ArithTk::Inc) { 1 } else { -1 };
          Shed::vars_mut(|v| v.set_var(var, VarKind::Str((val + delta).to_string()), VarFlags::empty())).unwrap();
          output.push(ArithTk::Num(val)); // push old value (postfix)
        } else {
          output.push(token); // keep as Var, may be assignment target
        }
      }

      op @ (ArithTk::Inc | ArithTk::Dec) => {
        let Some(ArithTk::Var(_)) = tokens.peek() else {
          return Err(sherr!(
            ParseErr,
            "Expected variable name after '{}' operator",
            if matches!(op, ArithTk::Inc) { "++" } else { "--" },
          ));
        };
        let Some(ArithTk::Var(var)) = tokens.next() else { unreachable!() };
        let val = read_var_as_i64(&var)?;
        let delta: i64 = if matches!(op, ArithTk::Inc) { 1 } else { -1 };
        let new_val = val + delta;
        Shed::vars_mut(|v| v.set_var(&var, VarKind::Str(new_val.to_string()), VarFlags::empty())).unwrap();
        output.push(ArithTk::Num(new_val)); // push new value (prefix)
      }

      ArithTk::Not | ArithTk::Neg | ArithTk::UPlus | ArithTk::BitNot => {
        // Unary right-associative
        // push to ops stack
        ops.push(token);
      }

      ArithTk::Comma => {
        // Lowest-precedence binary op
        // push to ops stack so both operands
        // are fully evaluated before Comma is applied. Question is a barrier
        // (commas inside the then-arm of a ternary stay there).
        while let Some(top) = ops.last() {
          if is_barrier(top) { break; }
          pop_to_output(&mut ops, &mut output)?;
        }
        ops.push(ArithTk::Comma);
      }

      ArithTk::Op(ref op) => {
        // Intercept && and || for short-circuit emission.
        if matches!(op, ArithOp::And | ArithOp::Or) {
          // Flush higher-precedence ops first (LHS finished).
          let cur_prec = if matches!(op, ArithOp::And) { 3 } else { 2 };
          while let Some(top) = ops.last() {
            if is_barrier(top) { break; }
            let top_prec = precedence(top);
            if top_prec >= cur_prec {
              pop_to_output(&mut ops, &mut output)?;
            } else {
              break;
            }
          }
          // Reserve placeholder jump and push pending marker.
          let jump_idx = output.len();
          if matches!(op, ArithOp::And) {
            output.push(ArithTk::JumpIfZeroPeek(0));
            output.push(ArithTk::Pop);
            ops.push(ArithTk::PendingAnd(jump_idx));
          } else {
            output.push(ArithTk::JumpIfNonZeroPeek(0));
            output.push(ArithTk::Pop);
            ops.push(ArithTk::PendingOr(jump_idx));
          }
        } else {
          let right_assoc = is_right_assoc(&token);
          let cur_prec = precedence(&token);
          while let Some(top) = ops.last() {
            if is_barrier(top) { break; }
            let top_prec = precedence(top);
            if top_prec > cur_prec || (top_prec == cur_prec && !right_assoc) {
              pop_to_output(&mut ops, &mut output)?;
            } else {
              break;
            }
          }
          ops.push(token);
        }
      }

      ArithTk::Question => {
        // Cond is fully built. Flush higher-prec ops, then emit a placeholder
        // JIFZ that will jump to the else-arm once we know its position.
        let cur_prec = 1;
        while let Some(top) = ops.last() {
          if is_barrier(top) { break; }
          let top_prec = precedence(top);
          if top_prec > cur_prec {
            pop_to_output(&mut ops, &mut output)?;
          } else {
            break;
          }
        }
        let jump_idx = output.len();
        output.push(ArithTk::JumpIfZero(0));
        ops.push(ArithTk::PendingTernaryThen(jump_idx));
      }

      ArithTk::Colon => {
        // Flush ops down to the matching PendingTernaryThen.
        while let Some(top) = ops.last() {
          if matches!(top, ArithTk::PendingTernaryThen(_)) { break; }
          if matches!(top, ArithTk::LParen) {
            return Err(sherr!(ParseErr, "Unexpected ':' before matching '?' in arithmetic"));
          }
          pop_to_output(&mut ops, &mut output)?;
        }
        let Some(ArithTk::PendingTernaryThen(jifz_idx)) = ops.pop() else {
          return Err(sherr!(ParseErr, "':' without matching '?' in arithmetic expression"));
        };
        // Emit a placeholder JMP (then→end), then patch the JIFZ to point at
        // the start of the else-arm (right after this JMP).
        let jmp_idx = output.len();
        output.push(ArithTk::Jump(0));
        output[jifz_idx] = ArithTk::JumpIfZero(output.len());
        ops.push(ArithTk::PendingTernaryElse(jmp_idx));
      }

      ArithTk::LParen => ops.push(token),

      ArithTk::RParen => flush_ops(&mut ops, &mut output, true)?,

      // Control-flow opcodes are only emitted by to_rpn, never lexed.
      ArithTk::JumpIfZero(_)
      | ArithTk::JumpIfZeroPeek(_)
      | ArithTk::JumpIfNonZeroPeek(_)
      | ArithTk::Jump(_)
      | ArithTk::Pop
      | ArithTk::Nez
      | ArithTk::PendingAnd(_)
      | ArithTk::PendingOr(_)
      | ArithTk::PendingTernaryThen(_)
      | ArithTk::PendingTernaryElse(_) => {
        unreachable!("control-flow opcodes are never produced by the tokenizer")
      }
    });

    while !ops.is_empty() {
      pop_to_output(&mut ops, &mut output)?;
    }

    Ok(output)
  }

  pub fn eval_rpn(tokens: Vec<ArithTk>) -> ShResult<i64> {
    let mut stack: Vec<StackVal> = Vec::new();

    macro_rules! pop_num {
      () => {
        stack
          .pop()
          .ok_or_else(|| sherr!(ParseErr, "Missing operand in arithmetic expression"))?
          .to_num()?
      };
    }

    macro_rules! pop_var {
      () => {
        match stack
          .pop()
          .ok_or_else(|| sherr!(ParseErr, "Missing operand in arithmetic expression"))?
        {
          StackVal::Var(name) => name,
          StackVal::Num(_) => return Err(sherr!(ParseErr, "Assignment target must be a variable")),
        }
      };
    }

    let mut i = 0;
    while i < tokens.len() {
      match tokens[i].clone() {
        ArithTk::Num(n) => stack.push(StackVal::Num(n)),

        ArithTk::Var(name) => stack.push(StackVal::Var(name)),

        // Control flow
        // set i directly and continue (skip auto-increment).
        ArithTk::Jump(target) => {
          i = target;
          continue;
        }
        ArithTk::JumpIfZero(target) => {
          let val = pop_num!();
          if val == 0 {
            i = target;
            continue;
          }
        }
        ArithTk::JumpIfZeroPeek(target) => {
          let val = stack
            .last()
            .ok_or_else(|| sherr!(ParseErr, "Empty stack at conditional jump"))?
            .to_num()?;
          if val == 0 {
            i = target;
            continue;
          }
        }
        ArithTk::JumpIfNonZeroPeek(target) => {
          let val = stack
            .last()
            .ok_or_else(|| sherr!(ParseErr, "Empty stack at conditional jump"))?
            .to_num()?;
          if val != 0 {
            i = target;
            continue;
          }
        }
        ArithTk::Pop => {
          stack.pop();
        }
        ArithTk::Nez => {
          let val = pop_num!();
          stack.push(StackVal::Num(if val != 0 { 1 } else { 0 }));
        }

        ArithTk::Not => {
          let val = pop_num!();
          stack.push(StackVal::Num(if val == 0 { 1 } else { 0 }));
        }

        ArithTk::Neg => {
          let val = pop_num!();
          stack.push(StackVal::Num(-val));
        }

        ArithTk::UPlus => {
          let val = pop_num!();
          stack.push(StackVal::Num(val));
        }

        ArithTk::BitNot => {
          let val = pop_num!();
          stack.push(StackVal::Num(!val));
        }

        ArithTk::Comma => {
          // Discard LHS, keep RHS already on stack
          let rhs = stack
            .pop()
            .ok_or_else(|| sherr!(ParseErr, "Missing operand after ','"))?;
          let _lhs = stack
            .pop()
            .ok_or_else(|| sherr!(ParseErr, "Missing operand before ','"))?;
          stack.push(rhs);
        }

        ArithTk::Op(op) => {
          match op {
            // Assignment ops
            // LHS must be a Var
            ArithOp::Assign => {
              let rhs = pop_num!();
              let lhs = pop_var!();
              Shed::vars_mut(|v| v.set_var(&lhs, VarKind::Str(rhs.to_string()), VarFlags::empty()))
                .unwrap();
              stack.push(StackVal::Num(rhs));
            }
            ArithOp::PlusAssign => {
              let rhs = pop_num!();
              let lhs = pop_var!();
              let new_val = read_var_as_i64(&lhs)? + rhs;
              Shed::vars_mut(|v| {
                v.set_var(&lhs, VarKind::Str(new_val.to_string()), VarFlags::empty())
              })
              .unwrap();
              stack.push(StackVal::Num(new_val));
            }
            ArithOp::MinusAssign => {
              let rhs = pop_num!();
              let lhs = pop_var!();
              let new_val = read_var_as_i64(&lhs)? - rhs;
              Shed::vars_mut(|v| {
                v.set_var(&lhs, VarKind::Str(new_val.to_string()), VarFlags::empty())
              })
              .unwrap();
              stack.push(StackVal::Num(new_val));
            }
            ArithOp::MulAssign => {
              let rhs = pop_num!();
              let lhs = pop_var!();
              let new_val = read_var_as_i64(&lhs)? * rhs;
              Shed::vars_mut(|v| {
                v.set_var(&lhs, VarKind::Str(new_val.to_string()), VarFlags::empty())
              })
              .unwrap();
              stack.push(StackVal::Num(new_val));
            }
            ArithOp::DivAssign => {
              let rhs = pop_num!();
              if rhs == 0 {
                return Err(sherr!(InternalErr, "Division by zero"));
              }
              let lhs = pop_var!();
              let new_val = read_var_as_i64(&lhs)? / rhs;
              Shed::vars_mut(|v| {
                v.set_var(&lhs, VarKind::Str(new_val.to_string()), VarFlags::empty())
              })
              .unwrap();
              stack.push(StackVal::Num(new_val));
            }
            ArithOp::ModAssign => {
              let rhs = pop_num!();
              if rhs == 0 {
                return Err(sherr!(InternalErr, "Modulo by zero"));
              }
              let lhs = pop_var!();
              let new_val = read_var_as_i64(&lhs)? % rhs;
              Shed::vars_mut(|v| {
                v.set_var(&lhs, VarKind::Str(new_val.to_string()), VarFlags::empty())
              })
              .unwrap();
              stack.push(StackVal::Num(new_val));
            }

            // Binary math
            ArithOp::Add => {
              let rhs = pop_num!();
              let lhs = pop_num!();
              stack.push(StackVal::Num(lhs + rhs));
            }
            ArithOp::Sub => {
              let rhs = pop_num!();
              let lhs = pop_num!();
              stack.push(StackVal::Num(lhs - rhs));
            }
            ArithOp::Mul => {
              let rhs = pop_num!();
              let lhs = pop_num!();
              stack.push(StackVal::Num(lhs * rhs));
            }
            ArithOp::Div => {
              let rhs = pop_num!();
              let lhs = pop_num!();
              if rhs == 0 {
                return Err(sherr!(InternalErr, "Division by zero"));
              }
              stack.push(StackVal::Num(lhs / rhs));
            }
            ArithOp::Mod => {
              let rhs = pop_num!();
              let lhs = pop_num!();
              if rhs == 0 {
                return Err(sherr!(InternalErr, "Modulo by zero"));
              }
              stack.push(StackVal::Num(lhs % rhs));
            }

            // Comparison (result is 1 or 0)
            ArithOp::Lt => {
              let rhs = pop_num!();
              let lhs = pop_num!();
              stack.push(StackVal::Num(if lhs < rhs { 1 } else { 0 }));
            }
            ArithOp::Gt => {
              let rhs = pop_num!();
              let lhs = pop_num!();
              stack.push(StackVal::Num(if lhs > rhs { 1 } else { 0 }));
            }
            ArithOp::Le => {
              let rhs = pop_num!();
              let lhs = pop_num!();
              stack.push(StackVal::Num(if lhs <= rhs { 1 } else { 0 }));
            }
            ArithOp::Ge => {
              let rhs = pop_num!();
              let lhs = pop_num!();
              stack.push(StackVal::Num(if lhs >= rhs { 1 } else { 0 }));
            }
            ArithOp::Eq => {
              let rhs = pop_num!();
              let lhs = pop_num!();
              stack.push(StackVal::Num(if lhs == rhs { 1 } else { 0 }));
            }
            ArithOp::Ne => {
              let rhs = pop_num!();
              let lhs = pop_num!();
              stack.push(StackVal::Num(if lhs != rhs { 1 } else { 0 }));
            }

            // && and || are decomposed into JIFZ_PEEK/JIFNZ_PEEK + Pop + Nez at
            // to_rpn time (see the ArithTk::Op handler). They never reach eval.
            ArithOp::And | ArithOp::Or => {
              return Err(sherr!(
                ParseErr,
                "Internal: && / || should have been decomposed before eval"
              ));
            }

            // Bitwise
            ArithOp::BitAnd => {
              let rhs = pop_num!();
              let lhs = pop_num!();
              stack.push(StackVal::Num(lhs & rhs));
            }
            ArithOp::BitOr => {
              let rhs = pop_num!();
              let lhs = pop_num!();
              stack.push(StackVal::Num(lhs | rhs));
            }
            ArithOp::BitXor => {
              let rhs = pop_num!();
              let lhs = pop_num!();
              stack.push(StackVal::Num(lhs ^ rhs));
            }
            ArithOp::ShiftL => {
              let rhs = pop_num!();
              let lhs = pop_num!();
              stack.push(StackVal::Num(lhs.wrapping_shl(rhs as u32)));
            }
            ArithOp::ShiftR => {
              let rhs = pop_num!();
              let lhs = pop_num!();
              stack.push(StackVal::Num(lhs.wrapping_shr(rhs as u32)));
            }

            // Bitwise/shift compound assignment
            ArithOp::BitAndAssign => {
              let rhs = pop_num!();
              let lhs = pop_var!();
              let new_val = read_var_as_i64(&lhs)? & rhs;
              Shed::vars_mut(|v| {
                v.set_var(&lhs, VarKind::Str(new_val.to_string()), VarFlags::empty())
              })
              .unwrap();
              stack.push(StackVal::Num(new_val));
            }
            ArithOp::BitOrAssign => {
              let rhs = pop_num!();
              let lhs = pop_var!();
              let new_val = read_var_as_i64(&lhs)? | rhs;
              Shed::vars_mut(|v| {
                v.set_var(&lhs, VarKind::Str(new_val.to_string()), VarFlags::empty())
              })
              .unwrap();
              stack.push(StackVal::Num(new_val));
            }
            ArithOp::BitXorAssign => {
              let rhs = pop_num!();
              let lhs = pop_var!();
              let new_val = read_var_as_i64(&lhs)? ^ rhs;
              Shed::vars_mut(|v| {
                v.set_var(&lhs, VarKind::Str(new_val.to_string()), VarFlags::empty())
              })
              .unwrap();
              stack.push(StackVal::Num(new_val));
            }
            ArithOp::ShiftLAssign => {
              let rhs = pop_num!();
              let lhs = pop_var!();
              let new_val = read_var_as_i64(&lhs)?.wrapping_shl(rhs as u32);
              Shed::vars_mut(|v| {
                v.set_var(&lhs, VarKind::Str(new_val.to_string()), VarFlags::empty())
              })
              .unwrap();
              stack.push(StackVal::Num(new_val));
            }
            ArithOp::ShiftRAssign => {
              let rhs = pop_num!();
              let lhs = pop_var!();
              let new_val = read_var_as_i64(&lhs)?.wrapping_shr(rhs as u32);
              Shed::vars_mut(|v| {
                v.set_var(&lhs, VarKind::Str(new_val.to_string()), VarFlags::empty())
              })
              .unwrap();
              stack.push(StackVal::Num(new_val));
            }
          }
        }

        ArithTk::Inc
        | ArithTk::Dec
        | ArithTk::LParen
        | ArithTk::RParen
        | ArithTk::Question
        | ArithTk::Colon
        | ArithTk::PendingAnd(_)
        | ArithTk::PendingOr(_)
        | ArithTk::PendingTernaryThen(_)
        | ArithTk::PendingTernaryElse(_) => {
          return Err(sherr!(
            ParseErr,
            "Unexpected token during arithmetic evaluation: '{:?}'",
            tokens[i],
          ));
        }
      }
      i += 1;
    }

    if stack.len() != 1 {
      return Err(sherr!(ParseErr, "Invalid arithmetic expression"));
    }

    stack.pop().unwrap().to_num()
  }
}

/// Evaluate an arithmetic expression string, returning the result.
/// The caller is responsible for stripping any `((...))` or `(...)` wrappers.
pub fn expand_arithmetic(expr: &str) -> ShResult<String> {
  let unescaped = unescape_math(expr)?;
  let expanded = expand_raw(&mut unescaped.chars().peekable())?;
  let tokens = ArithTk::tokenize(&expanded)?;
  let rpn = ArithTk::to_rpn(tokens)?;
  let result = ArithTk::eval_rpn(rpn)?;
  Ok(result.to_string())
}

/// Strip `((...))` or `(...)` wrappers and evaluate. Convenience for call sites
/// that receive the raw token including its delimiters.
pub fn expand_arithmetic_wrapped(raw: &str) -> ShResult<String> {
  let mut expr = raw;

  if expr.starts_with("((") {
    expr = &expr[2..];
  } else if expr.starts_with('(') {
    expr = &expr[1..];
  }

  if expr.ends_with("))") {
    expr = &expr[..expr.len() - 2];
  } else if expr.ends_with(')') {
    expr = &expr[..expr.len() - 1];
  }

  expand_arithmetic(expr)
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::state::{Shed, vars::VarFlags, vars::VarKind};
  use crate::tests::testutil::TestGuard;

  fn arith(s: &str) -> f64 {
    // Tests pass raw expressions - no outer ((...)) wrapper stripping
    expand_arithmetic(s).unwrap().parse::<f64>().unwrap()
  }

  // ===================== Basic math =====================

  #[test]
  fn arith_addition() {
    assert_eq!(arith("(1+2)"), 3.0);
  }

  #[test]
  fn arith_subtraction() {
    assert_eq!(arith("(10-3)"), 7.0);
  }

  #[test]
  fn arith_multiplication() {
    assert_eq!(arith("(3*4)"), 12.0);
  }

  #[test]
  fn arith_division() {
    assert_eq!(arith("(10/2)"), 5.0);
  }

  #[test]
  fn arith_modulo() {
    assert_eq!(arith("(10%3)"), 1.0);
  }

  #[test]
  fn arith_precedence() {
    assert_eq!(arith("(2+3*4)"), 14.0);
  }

  #[test]
  fn arith_parens() {
    assert_eq!(arith("(2+3)*4"), 20.0);
  }

  #[test]
  fn arith_nested_parens() {
    assert_eq!(arith("(1+2)*(3+4)"), 21.0);
  }

  #[test]
  fn arith_spaces() {
    assert_eq!(arith("( 1 + 2 )"), 3.0);
  }

  #[test]
  fn arith_unary_neg() {
    assert_eq!(arith("(-5)"), -5.0);
  }

  #[test]
  fn arith_unary_neg_in_expr() {
    assert_eq!(arith("(10 + -3)"), 7.0);
  }

  // ===================== Comparison =====================

  #[test]
  fn arith_lt_true() {
    assert_eq!(arith("(3 < 5)"), 1.0);
  }

  #[test]
  fn arith_lt_false() {
    assert_eq!(arith("(5 < 3)"), 0.0);
  }

  #[test]
  fn arith_eq_true() {
    assert_eq!(arith("(4 == 4)"), 1.0);
  }

  #[test]
  fn arith_ne_true() {
    assert_eq!(arith("(3 != 4)"), 1.0);
  }

  #[test]
  fn arith_le_equal() {
    assert_eq!(arith("(5 <= 5)"), 1.0);
  }

  // ===================== Logical =====================

  #[test]
  fn arith_logical_and_true() {
    assert_eq!(arith("(1 && 1)"), 1.0);
  }

  #[test]
  fn arith_logical_and_false() {
    assert_eq!(arith("(1 && 0)"), 0.0);
  }

  #[test]
  fn arith_logical_or_true() {
    assert_eq!(arith("(0 || 1)"), 1.0);
  }

  #[test]
  fn arith_not_true() {
    assert_eq!(arith("(!0)"), 1.0);
  }

  #[test]
  fn arith_not_false() {
    assert_eq!(arith("(!1)"), 0.0);
  }

  // ===================== Assignment =====================

  #[test]
  fn arith_assign() {
    let _g = TestGuard::new();
    arith("(x = 5)");
    let val = Shed::vars(|v| v.try_get_var("x")).unwrap();
    assert_eq!(val, "5");
  }

  #[test]
  fn arith_plus_assign() {
    let _g = TestGuard::new();
    Shed::vars_mut(|v| v.set_var("x", VarKind::Str("3".into()), VarFlags::empty())).unwrap();
    arith("(x += 2)");
    let val = Shed::vars(|v| v.try_get_var("x")).unwrap();
    assert_eq!(val, "5");
  }

  #[test]
  fn arith_chained_assign() {
    let _g = TestGuard::new();
    arith("(a = b = 7)");
    let a = Shed::vars(|v| v.try_get_var("a")).unwrap();
    let b = Shed::vars(|v| v.try_get_var("b")).unwrap();
    assert_eq!(a, "7");
    assert_eq!(b, "7");
  }

  // ===================== Inc/Dec =====================

  #[test]
  fn arith_postfix_inc() {
    let _g = TestGuard::new();
    Shed::vars_mut(|v| v.set_var("i", VarKind::Str("5".into()), VarFlags::empty())).unwrap();
    let result = arith("(i++)");
    assert_eq!(result, 5.0); // returns old value
    let val = Shed::vars(|v| v.try_get_var("i")).unwrap();
    assert_eq!(val, "6");
  }

  #[test]
  fn arith_prefix_inc() {
    let _g = TestGuard::new();
    Shed::vars_mut(|v| v.set_var("i", VarKind::Str("5".into()), VarFlags::empty())).unwrap();
    let result = arith("(++i)");
    assert_eq!(result, 6.0); // returns new value
    let val = Shed::vars(|v| v.try_get_var("i")).unwrap();
    assert_eq!(val, "6");
  }

  // ===================== Comma =====================

  #[test]
  fn arith_comma_returns_last() {
    let _g = TestGuard::new();
    // (j=2, j+1) should set j=2 and return 3
    let result = arith("(j=2, j+1)");
    assert_eq!(result, 3.0);
    let val = Shed::vars(|v| v.try_get_var("j")).unwrap();
    assert_eq!(val, "2");
  }

  #[test]
  fn arith_nested_comma() {
    let _g = TestGuard::new();
    // i=(j=2,j+1) sets j=2, evaluates j+1=3, assigns i=3
    arith("(i=(j=2,j+1))");
    let i = Shed::vars(|v| v.try_get_var("i")).unwrap();
    let j = Shed::vars(|v| v.try_get_var("j")).unwrap();
    assert_eq!(i, "3");
    assert_eq!(j, "2");
  }

  // ===================== Variable reads =====================

  #[test]
  fn arith_with_variable() {
    let _g = TestGuard::new();
    Shed::vars_mut(|v| v.set_var("x", VarKind::Str("5".into()), VarFlags::empty())).unwrap();
    assert_eq!(arith("(x + 3)"), 8.0);
  }

  #[test]
  fn arith_undefined_var_is_zero() {
    let _g = TestGuard::new();
    assert_eq!(arith("(undef_var + 1)"), 1.0);
  }
}
