use super::{
  getopt::OptSpec,
  outln,
  readline::stash::{Stash, StashOpts},
  util::{ShResult, ShResultExt},
};

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
