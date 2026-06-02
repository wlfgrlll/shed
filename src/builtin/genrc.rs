use super::{
  ShResult,
  getopt::{Opt, OptSpec},
  outln,
  state::{
    shopt::ShoptSource,
    util::{GenRcConfig, compose_rc},
  },
  util::with_status,
};

/// `genrc` — print an rc file built from the current shell state to
/// stdout. Used to (re)generate `~/.shedrc` after a shopt rename, or to
/// inspect the live config in re-sourceable form.
pub struct GenRc;

impl super::Builtin for GenRc {
  fn opts(&self) -> Vec<OptSpec> {
    vec![
      OptSpec::flag('s'), // shopts
      OptSpec::flag('a'), // alias
      OptSpec::flag('k'), // keymaps
      OptSpec::flag('A'), // autocmds
      OptSpec::flag('f'), // functions
      OptSpec::flag('c'), // completions
      OptSpec::flag("default"),
      OptSpec::flag("no-comments"),
    ]
  }

  fn strict_opts(&self) -> bool {
    true
  }

  fn execute(&self, args: super::BuiltinArgs) -> ShResult<()> {
    let mut config = GenRcConfig::default();
    let mut use_defaults = false;
    let mut no_comments = false;

    let mut want_shopts = false;
    let mut want_aliases = false;
    let mut want_keymaps = false;
    let mut want_autocmds = false;
    let mut want_functions = false;
    let mut want_completions = false;
    let mut any_section_flag = false;

    for opt in args.opts {
      match opt {
        Opt::Short('s') => {
          want_shopts = true;
          any_section_flag = true;
        }
        Opt::Short('a') => {
          want_aliases = true;
          any_section_flag = true;
        }
        Opt::Short('k') => {
          want_keymaps = true;
          any_section_flag = true;
        }
        Opt::Short('A') => {
          want_autocmds = true;
          any_section_flag = true;
        }
        Opt::Short('f') => {
          want_functions = true;
          any_section_flag = true;
        }
        Opt::Short('c') => {
          want_completions = true;
          any_section_flag = true;
        }
        Opt::Long(name) if name == "default" => use_defaults = true,
        Opt::Long(name) if name == "no-comments" => no_comments = true,
        _ => {}
      }
    }

    if any_section_flag {
      // Everything defaults to off and is opted back in by the section
      // flags the user passed.
      config.include_shopts = want_shopts;
      config.include_aliases = want_aliases;
      config.include_keymaps = want_keymaps;
      config.include_autocmds = want_autocmds;
      config.include_functions = want_functions;
      config.include_completions = want_completions;
    }

    if use_defaults {
      config.source = ShoptSource::Defaults;
    }
    if no_comments {
      config.include_comments = false;
    }

    for line in compose_rc(&config) {
      outln!("{}", line);
    }

    with_status(0)
  }
}
