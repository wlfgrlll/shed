use super::{
  getopt::{Opt, OptSpec},
  match_loop, outln,
  readline::stash::{Stash, StashedCmd},
  sherr,
  util::{ShResult, ShResultExt},
};

#[derive(Debug, Default)]
pub(crate) struct StashOpts {
  pub to_save: Vec<StashedCmd>,
  pub to_delete: Vec<String>,
  pub list: bool,
  pub only_named: bool,
  pub only_stack: bool,
}

impl StashOpts {
  pub fn from_opts(opts: Vec<Opt>) -> ShResult<Self> {
    let mut new = Self::default();
    let mut opt_iter = opts.into_iter();

    match_loop!(opt_iter.next() => opt, {
      Opt::ShortWithList('s',mut args) => {
        // length of 'args' is enforced by the opt spec
        let cursor = args.pop().unwrap();
        let buffer = args.pop().unwrap();
        let name = args.pop().unwrap();
        new.to_save.push(StashedCmd {
          name: Some(name),
          buffer,
          cursor_pos: cursor,
        });
      }
      Opt::LongWithList(opt, mut args) => {
        let "save" = opt.as_str() else {
          return Err(sherr!(ParseErr, "unexpected option {opt} in stash"))
        };

        // length of 'args' is enforced by the opt spec
        let cursor = args.pop().unwrap();
        let buffer = args.pop().unwrap();
        let name = args.pop().unwrap();
        new.to_save.push(StashedCmd {
          name: Some(name),
          buffer,
          cursor_pos: cursor,
        });
      }
      Opt::ShortWithArg('d', arg) => {
        new.to_delete.push(arg);
      }
      Opt::LongWithArg(opt, arg) => {
        match opt.as_str() {
          "delete" => new.to_delete.push(arg),
          _ => return Err(sherr!(ParseErr, "unexpected option {opt} in stash"))
        }
      }
      Opt::Long(arg) => {
        match arg.as_str() {
          "list" => new.list = true,
          "stack" => new.only_stack = true,
          "named" => new.only_named = true,
          _ => return Err(sherr!(ParseErr, "unexpected option {arg} in stash"))
        }
      }
      _ => return Err(sherr!(ParseErr, "unexpected option {opt} in stash"))
    });

    Ok(new)
  }
}

pub(super) struct StashBuiltin;
impl super::Builtin for StashBuiltin {
  fn opts(&self) -> Vec<OptSpec> {
    vec![
      OptSpec::exact_args('s', 3),
      OptSpec::exact_args("save", 3),
      OptSpec::single_arg('d'),
      OptSpec::single_arg("delete"),
      OptSpec::flag('l'),
      OptSpec::flag("list"),
      OptSpec::flag("stack"),
      OptSpec::flag("named"),
    ]
  }
  fn execute(&self, args: super::BuiltinArgs) -> ShResult<()> {
    let span = args.span();
    let is_empty = args.opts.is_empty();
    let stash_opts = StashOpts::from_opts(args.opts).promote_err(span.clone())?;
    let stash = Stash::new().promote_err(span.clone())?;

    for cmd in stash_opts.to_save {
      stash.stash_cmd(cmd).promote_err(span.clone())?;
    }

    for cmd in stash_opts.to_delete {
      stash.delete_cmd(&cmd).promote_err(span.clone())?;
    }

    if stash_opts.list || is_empty {
      let output = stash.list(stash_opts.only_named, stash_opts.only_stack);
      outln!("{output}");
    }

    Ok(())
  }
}
