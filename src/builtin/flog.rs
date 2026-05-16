use std::str::FromStr;

use super::{
  ShResult, Shed,
  expand::markers,
  getopt::{Opt, OptSpec},
  join_raw_arg_iter, match_loop, sherr,
  util::{flog, stylize_loglevel},
  with_status,
};

pub struct Flog;
impl super::Builtin for Flog {
  fn opts(&self) -> Vec<super::getopt::OptSpec> {
    vec![OptSpec::single_arg('p'), OptSpec::single_arg("prefix")]
  }
  fn execute(&self, args: super::BuiltinArgs) -> ShResult<()> {
    let span = args.span();
    let source = span.span_source().name();
    let (line, col) = span.line_and_col();

    let mut argv = args.argv.into_iter();

    let Some((first, span)) = argv.next() else {
      return Err(sherr!(ExecFail, "Usage: flog <LEVEL> <MESSAGE>"));
    };
    let level = first.to_ascii_uppercase();
    let Some(level) = log::Level::from_str(&level).ok() else {
      return Err(sherr!(ExecFail @ span, "Invalid log level"));
    };

    flog::update_log_level();

    if level > log::max_level() {
      return with_status(0);
    }

    let level = stylize_loglevel(level);

    let mut prefix_fmt = "[{level}]".to_string();

    for opt in args.opts {
      match &opt {
        Opt::ShortWithArg('p', arg) => {
          prefix_fmt = arg.to_string();
        }
        Opt::LongWithArg(flag, arg) => {
          if flag.as_str() == "prefix" {
            prefix_fmt = arg.to_string();
          }
        }
        _ => {}
      }
    }

    let (rest, _) = join_raw_arg_iter(argv);
    let formatted = Self::expand_prefix_fmt(&prefix_fmt, &level, source, line, col);

    let out = format!("{formatted} {rest}");

    Shed::meta_mut(|m| m.post_system_message(out));

    with_status(0)
  }
}

impl Flog {
  fn expand_prefix_fmt(fmt: &str, level: &str, source: &str, line: usize, col: usize) -> String {
    let mut chars = fmt.chars();
    let mut out = String::new();
    match_loop!(chars.next() => ch, {
      '\\' => {
        out.push(ch);
        if let Some(next_ch) = chars.next() {
          out.push(next_ch);
        }
      }
      '{' => {
        let mut fmt_arg = String::new();

        match_loop!(chars.next() => ch, {
          '}' => break,
          _ => fmt_arg.push(ch),
        });

        match fmt_arg.as_str() {
          "level" => out.push_str(level),
          "line" => out.push_str(&line.to_string()),
          "col" => out.push_str(&col.to_string()),
          "source" => {
            let source = source.replace('%', &format!("{}",markers::ESCAPE));
            out.push_str(&source);
          }
          _ => out.push_str("{fmt_arg}"),
        }
      }
      _ => out.push(ch),
    });

    out = chrono::Local::now()
      .format(&out)
      .to_string()
      .replace(markers::ESCAPE, "%");

    out
  }
}
