use std::rc::Rc;

use super::{
  Shed,
  expand::as_var_val_display,
  getopt::{Opt, OptSpec},
  outln, sherr,
  state::{self, meta::MetaTab, meta::Utility},
  util::{ShResult, with_status},
};

pub(super) struct Hash;
impl super::Builtin for Hash {
  fn opts(&self) -> Vec<OptSpec> {
    vec![OptSpec::flag('r'), OptSpec::flag("refresh")]
  }
  fn execute(&self, args: super::BuiltinArgs) -> ShResult<()> {
    let mut refresh = false;
    let mut clear = false;

    for opt in &args.opts {
      match opt {
        Opt::Short('r') => clear = true,
        Opt::Long(s) if s == "refresh" => refresh = true,
        _ => return Err(sherr!(ParseErr, "Invalid hash option: {opt:?}").promote(args.span())),
      }
    }

    if args.argv.is_empty() && args.opts.is_empty() {
      let cmds: Vec<Rc<Utility>> = Shed::meta(|m| m.cached_utils().collect());
      for cmd in cmds {
        if let state::meta::UtilKind::Command(path) = cmd.kind() {
          let path = as_var_val_display(&path.to_string_lossy());
          let name = cmd.name();
          outln!("{name}={path}");
        }
      }
    }

    Shed::meta_mut(|m| {
      if clear {
        m.clear_cache();
      }
      if refresh {
        m.rehash();
      }
    });

    let path_cmds = MetaTab::get_cmds_in_path();

    Shed::meta_mut(|m| {
      for (arg, span) in args.argv {
        if let Some(cmd) = path_cmds.iter().find(|cmd| cmd.name() == arg) {
          m.cache_util(Rc::clone(cmd));
        } else {
          return Err(sherr!(NotFound, "Command not found: {arg}").promote(span));
        }
      }
      Ok(())
    })?;

    with_status(0)
  }
}
