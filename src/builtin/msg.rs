use super::{
  Shed,
  getopt::{Opt, OptSpec},
  join_raw_args, outln, sherr, status_msg, system_msg,
  util::{ShResult, with_status},
};

pub(super) struct Msg;
impl super::Builtin for Msg {
  fn opts(&self) -> Vec<OptSpec> {
    vec![
      OptSpec::flag('s'),
      OptSpec::flag('S'),
      OptSpec::flag("status"),
      OptSpec::flag("system"),
    ]
  }
  fn execute(&self, args: super::BuiltinArgs) -> ShResult<()> {
    let mut system = false;
    let mut status = false;

    for opt in args.opts {
      match opt {
        Opt::Short('S') => system = true,
        Opt::Short('s') => status = true,
        Opt::Long(o) if o.as_str() == "system" => system = true,
        Opt::Long(o) if o.as_str() == "status" => status = true,
        _ => {
          return Err(sherr!(ExecFail, "msg: Unexpected flag '{opt}'",));
        }
      }
    }

    if args.argv.is_empty() {
      // argv is empty, maybe they want us to list past messages?
      let history = if system {
        Shed::system_msg_hist()
      } else {
        Shed::status_msg_hist()
      };

      for msg in history {
        let formatted = msg.with_timestamp();
        outln!("{formatted}");
      }
    }

    let (msg, _span) = join_raw_args(args.argv);

    if system {
      system_msg!("{msg}");
    }

    // defaults to status messages if no flag is provided, but if both are provided we post to both
    if status || !system {
      status_msg!("{msg}");
    }

    with_status(0)
  }
}
