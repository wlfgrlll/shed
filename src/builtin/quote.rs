use crate::{
  ShResult, Shed,
  builtin::getopt::OptSpec,
  eval::lex::{LexFlags, LexStream},
  expand, outln, procio,
  state::vars::{VarFlags, VarKind},
  util::with_status,
};

use super::getopt::Opt;

pub(super) struct Quote;
impl super::Builtin for Quote {
  fn execute(&self, args: super::BuiltinArgs) -> ShResult<()> {
    let parts: Vec<String> = args
      .argv
      .iter()
      .map(|(s, _)| expand::shell_quote(s))
      .collect();
    outln!("{}", parts.join(" "));
    with_status(0)
  }
}

enum UnquoteTarget {
  Array(String),
  Var(String),
}

pub(super) struct Unquote;
impl super::Builtin for Unquote {
  fn opts(&self) -> Vec<super::getopt::OptSpec> {
    vec![
      OptSpec::single_arg('a'),
      OptSpec::single_arg("array"),
      OptSpec::single_arg('v'),
      OptSpec::single_arg("var"),
    ]
  }
  fn execute(&self, mut args: super::BuiltinArgs) -> ShResult<()> {
    log::debug!("entered unquote execute()");
    let input = if args.argv.is_empty() || args.has_stdin() {
      if args.has_stdin() {
        args.take_stdin().unwrap()
      } else {
        procio::read_input()?
      }
    } else {
      super::join_raw_args(args.argv).0
    };
    let mut target = None;

    for opt in &args.opts {
      match opt {
        Opt::LongWithArg(flag, arg) => match flag.as_str() {
          "array" => target = Some(UnquoteTarget::Array(arg.clone())),
          "var" => target = Some(UnquoteTarget::Var(arg.clone())),
          _ => {}
        },
        Opt::ShortWithArg('a', arg) => target = Some(UnquoteTarget::Array(arg.clone())),
        Opt::ShortWithArg('v', arg) => target = Some(UnquoteTarget::Var(arg.clone())),
        _ => {}
      }
    }

    let tokens = LexStream::new(input.into(), LexFlags::empty());
    let mut fields = vec![];

    for tk in tokens {
      let tk = tk?;

      fields.extend(tk.expand_to_words()?);
    }

    match target {
      None => {
        for word in fields {
          outln!("{word}")
        }
      }
      Some(UnquoteTarget::Array(name)) => {
        let var = VarKind::Arr(fields.into());
        Shed::vars_mut(|v| v.set_var(&name, var, VarFlags::empty()))?;
      }
      Some(UnquoteTarget::Var(name)) => {
        let var = VarKind::Str(fields.join(" "));
        Shed::vars_mut(|v| v.set_var(&name, var, VarFlags::empty()))?;
      }
    }

    with_status(0)
  }
}
