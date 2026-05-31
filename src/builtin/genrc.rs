use super::{
  ShResult, Shed,
  getopt::{Opt, OptSpec},
  outln, sherr,
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
      OptSpec::single_arg("only"),
      OptSpec::flag("default"),
      OptSpec::flag("no-autocmds"),
      OptSpec::flag("no-keymaps"),
      OptSpec::flag("no-completions"),
      OptSpec::flag("no-functions"),
      OptSpec::flag("no-aliases"),
      OptSpec::flag("no-comments"),
      OptSpec::single_arg("shopt"),
    ]
  }

  fn strict_opts(&self) -> bool {
    true
  }

  fn execute(&self, args: super::BuiltinArgs) -> ShResult<()> {
    let span = args.span();
    let mut config = GenRcConfig::default();
    let mut shopt_filter: Vec<String> = vec![];
    let mut only_sections: Option<Vec<String>> = None;

    for opt in args.opts {
      match opt {
        Opt::Long(name) if name == "default" => config.source = ShoptSource::Defaults,
        Opt::Long(name) if name == "no-autocmds" => config.include_autocmds = false,
        Opt::Long(name) if name == "no-keymaps" => config.include_keymaps = false,
        Opt::Long(name) if name == "no-completions" => config.include_completions = false,
        Opt::Long(name) if name == "no-functions" => config.include_functions = false,
        Opt::Long(name) if name == "no-aliases" => config.include_aliases = false,
        Opt::Long(name) if name == "no-comments" => config.include_comments = false,
        Opt::LongWithArg(name, arg) if name == "shopt" => {
          let valid = Shed::shopts(|o| o.get(&arg)).ok().flatten().is_some();

          if !valid {
            return Err(sherr!(InvalidOpt @ span, "invalid shopt name: {arg}"));
          }

          shopt_filter.push(arg);
        }
        Opt::LongWithArg(name, arg) if name == "only" => {
          // Comma separated list of categories
          let list: Vec<String> = arg
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
          only_sections = Some(list);
        }
        _ => {}
      }
    }

    if !shopt_filter.is_empty() {
      config.shopt_filter = Some(shopt_filter);
    }

    if let Some(sections) = only_sections {
      const VALID: &[&str] = &[
        "shopts",
        "aliases",
        "functions",
        "completions",
        "autocmds",
        "keymaps",
      ];
      for name in &sections {
        if !VALID.contains(&name.as_str()) {
          return Err(
            sherr!(
              InvalidOpt @ span,
              "unknown section in --only: '{name}'",
            )
            .with_note(format!("valid sections: {}", VALID.join(", "))),
          );
        }
      }
      // `--only` acts as an intersection: anything not listed gets
      // turned off. Any `--no-X` flags already applied above remain
      // in force (they can subtract further but never add).
      let want = |s: &str| sections.iter().any(|n| n == s);
      config.include_shopts &= want("shopts");
      config.include_aliases &= want("aliases");
      config.include_functions &= want("functions");
      config.include_completions &= want("completions");
      config.include_autocmds &= want("autocmds");
      config.include_keymaps &= want("keymaps");
    }

    for line in compose_rc(&config) {
      outln!("{}", line);
    }

    with_status(0)
  }
}
