use std::path::PathBuf;

use super::{
  ShResult,
  crate_util::{ansi_from_description, format_time},
  match_loop, shopt, state,
  state::Shed,
  status_msg,
  subshell::expand_cmd_sub,
  var,
};

use nix::sys::wait::WaitStatus as WtStat;

#[derive(Debug)]
pub enum PromptTk {
  AsciiOct(i32),
  Text(String),
  AnsiSeq(String),
  Color(String),    // plain english color descriptions
  Function(String), // Expands to the output of any defined shell function
  RuntimeMillis,
  RuntimeFormatted,
  Pwd,
  PwdShort,
  Hostname,
  HostnameShort,
  ShellName,
  Username,
  PromptSymbol,
  JobCount,
}

fn tokenize_prompt(raw: &str) -> Vec<PromptTk> {
  let mut chars = raw.chars().peekable();
  let mut tk_text = String::new();
  let mut tokens = vec![];

  match_loop!(chars.next() => ch, {
    '\\' => {
      // Push any accumulated text as a token
      if !tk_text.is_empty() {
        tokens.push(PromptTk::Text(std::mem::take(&mut tk_text)));
      }

      // Handle the escape sequence
      if let Some(ch) = chars.next() {
        match ch {
          'w' => tokens.push(PromptTk::Pwd),
          'W' => tokens.push(PromptTk::PwdShort),
          'h' => tokens.push(PromptTk::HostnameShort),
          'H' => tokens.push(PromptTk::Hostname),
          's' => tokens.push(PromptTk::ShellName),
          'u' => tokens.push(PromptTk::Username),
          '$' => tokens.push(PromptTk::PromptSymbol),
          'n' => tokens.push(PromptTk::Text("\n".into())),
          'r' => tokens.push(PromptTk::Text("\r".into())),
          't' => tokens.push(PromptTk::RuntimeMillis),
          'j' => tokens.push(PromptTk::JobCount),
          'T' => tokens.push(PromptTk::RuntimeFormatted),
          '\\' => tokens.push(PromptTk::Text("\\".into())),
          '"' => tokens.push(PromptTk::Text("\"".into())),
          '\'' => tokens.push(PromptTk::Text("'".into())),
          'c' => {
            let Some('{') = chars.peek() else {
              tk_text.push_str("\\c");
              break;
            };
            chars.next(); // consume the '{'
            let mut desc = String::new();
            match_loop!(chars.next() => ch, {
              '}' => break,
              _ => desc.push(ch)
            });
            tokens.push(PromptTk::Color(desc));
          }
          '@' => {
            let mut func_name = String::new();
            let is_braced = chars.peek() == Some(&'{');
            let mut handled = false;
            match_loop!(chars.peek() => &ch => ch, {
              '}' if is_braced => {
                chars.next();
                handled = true;
                break;
              }
              'A'..='Z' | 'a'..='z' | '0'..='9' | '_' => {
                func_name.push(ch);
                chars.next();
              }
              _ => {
                handled = true;
                if is_braced {
                  // Invalid character in braced function name
                  tokens.push(PromptTk::Text(format!("\\@{{{func_name}")));
                } else {
                  // End of unbraced function name
                  let func_exists = Shed::logic(|l| l.get_func(&func_name).is_some());
                  if func_exists {
                    tokens.push(PromptTk::Function(func_name.clone()));
                  } else {
                    tokens.push(PromptTk::Text(format!("\\@{func_name}")));
                  }
                }
                break;
              }
            });
            // Handle end-of-input: function name collected but loop ended without pushing
            if !handled && !func_name.is_empty() {
              let func_exists = Shed::logic(|l| l.get_func(&func_name).is_some());
              if func_exists {
                tokens.push(PromptTk::Function(func_name));
              } else {
                tokens.push(PromptTk::Text(format!("\\@{func_name}")));
              }
            }
          }
          'e' => {
            if chars.next() == Some('[') {
              let mut params = String::new();

              // Collect parameters and final character
              match_loop!(chars.next() => ch, {
                '0'..='9' | ';' | '?' | ':' => params.push(ch), // Valid parameter characters
                'A'..='Z' | 'a'..='z' => {
                  // Final character (letter)
                  params.push(ch);
                  break;
                }
                _ => {
                  // Invalid character in ANSI sequence
                  tokens.push(PromptTk::Text(format!("\x1b[{params}")));
                  break;
                }
              });

              tokens.push(PromptTk::AnsiSeq(format!("\x1b[{params}")));
            } else {
              // Handle case where 'e' is not followed by '['
              tokens.push(PromptTk::Text("\\e".into()));
            }
          }
          '0'..='7' => {
            // Handle octal escape
            let mut octal_str = String::new();
            octal_str.push(ch);

            // Collect up to 2 more octal digits
            for _ in 0..2 {
              if let Some(&next_ch) = chars.peek() {
                if ('0'..='7').contains(&next_ch) {
                  octal_str.push(chars.next().unwrap());
                } else {
                  break;
                }
              } else {
                break;
              }
            }

            // Parse the octal string into an integer
            if let Ok(octal) = i32::from_str_radix(&octal_str, 8) {
              tokens.push(PromptTk::AsciiOct(octal));
            } else {
              // Fallback: treat as raw text
              tokens.push(PromptTk::Text(format!("\\{octal_str}")));
            }
          }
          _ => {
            // Unknown escape sequence: treat as raw text
            tokens.push(PromptTk::Text(format!("\\{ch}")));
          }
        }
      } else {
        // Handle trailing backslash
        tokens.push(PromptTk::Text("\\".into()));
      }
    }
    _ => {
      // Accumulate non-escape characters
      tk_text.push(ch);
    }
  });
  // Push any remaining text as a token
  if !tk_text.is_empty() {
    tokens.push(PromptTk::Text(tk_text));
  }

  tokens
}

pub fn expand_prompt(raw: &str) -> ShResult<String> {
  let mut tokens = tokenize_prompt(raw).into_iter();
  let mut result = String::new();

  match_loop!(tokens.next() => token, {
    PromptTk::Text(txt) => result.push_str(&txt),
    PromptTk::AnsiSeq(params) => result.push_str(&params),
    PromptTk::Color(c) => {
      match ansi_from_description(&c) {
        Ok(esc_seq) => result.push_str(&esc_seq.to_string()),
        Err(e) => status_msg!("{e}")
      }
    }
    PromptTk::RuntimeMillis => {
      if let Some(runtime) = Shed::meta_mut(|m| m.get_time()) {
        let runtime_millis = runtime.as_millis().to_string();
        result.push_str(&runtime_millis);
      }
    }
    PromptTk::RuntimeFormatted => {
      if let Some(runtime) = Shed::meta_mut(|m| m.get_time()) {
        let runtime_fmt = format_time(runtime);
        result.push_str(&runtime_fmt);
      }
    }
    PromptTk::Pwd => {
      let mut pwd = var!("PWD");
      let home = state::util::get_home_str().unwrap_or_default();
      if pwd.starts_with(&home) {
        pwd = pwd.replacen(&home, "~", 1);
      }
      result.push_str(&pwd);
    }
    PromptTk::PwdShort => {
      let mut pwd = var!("PWD");
      let home = state::util::get_home_str().unwrap_or_default();
      if pwd.starts_with(&home) {
        pwd = pwd.replacen(&home, "~", 1);
      }
      let pathbuf = PathBuf::from(&pwd);
      let mut segments = pathbuf.iter().count();
      let mut path_iter = pathbuf.iter();
      let max_segments = shopt!(prompt.trunc_prompt_path);
      while segments > max_segments {
        path_iter.next();
        segments -= 1;
      }
      let path_rebuilt: PathBuf = path_iter.collect();
      let mut path_rebuilt = path_rebuilt.to_str().unwrap().to_string();
      if path_rebuilt.starts_with(&home) {
        path_rebuilt = path_rebuilt.replacen(&home, "~", 1);
      }
      result.push_str(&path_rebuilt);
    }
    PromptTk::Hostname => {
      let hostname = var!("HOST");
      result.push_str(&hostname);
    }
    PromptTk::ShellName => result.push_str("shed"),
    PromptTk::Username => {
      let username = var!("USER");
      result.push_str(&username);
    }
    PromptTk::PromptSymbol => {
      let uid = var!("UID");
      let symbol = if &uid == "0" { '#' } else { '$' };
      result.push(symbol);
    }
    PromptTk::HostnameShort => {
      let hostname = var!("HOST");
      let mut segments = hostname.split('.');
      if let Some(first) = segments.next() {
        result.push_str(first);
      } else {
        result.push_str(&hostname);
      }
    }
    PromptTk::JobCount => {
      let count = Shed::jobs(|j| {
        j.jobs()
          .iter()
          .filter(|j| {
            j.as_ref().is_some_and(|j| {
              j.get_stats()
                .iter()
                .all(|st| matches!(st, WtStat::StillAlive))
            })
          })
        .count()
      });
      result.push_str(&count.to_string());
    }
    PromptTk::AsciiOct(n) => {
      if let Some(ch) = std::char::from_u32(n as u32) {
        result.push(ch);
      }
    }
    PromptTk::Function(f) => {
      let output = expand_cmd_sub(&f)?;
      result.push_str(&output);
    }
  });

  Ok(result)
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::time::Duration;

  // ===================== tokenize_prompt =====================

  #[test]
  fn prompt_username() {
    let tokens = tokenize_prompt("\\u");
    assert_eq!(tokens.len(), 1);
    assert!(matches!(tokens[0], PromptTk::Username));
  }

  #[test]
  fn prompt_hostname() {
    let tokens = tokenize_prompt("\\H");
    assert_eq!(tokens.len(), 1);
    assert!(matches!(tokens[0], PromptTk::Hostname));
  }

  #[test]
  fn prompt_pwd() {
    let tokens = tokenize_prompt("\\w");
    assert_eq!(tokens.len(), 1);
    assert!(matches!(tokens[0], PromptTk::Pwd));
  }

  #[test]
  fn prompt_pwd_short() {
    let tokens = tokenize_prompt("\\W");
    assert_eq!(tokens.len(), 1);
    assert!(matches!(tokens[0], PromptTk::PwdShort));
  }

  #[test]
  fn prompt_symbol() {
    let tokens = tokenize_prompt("\\$");
    assert_eq!(tokens.len(), 1);
    assert!(matches!(tokens[0], PromptTk::PromptSymbol));
  }

  #[test]
  fn prompt_newline() {
    let tokens = tokenize_prompt("\\n");
    assert_eq!(tokens.len(), 1);
    assert!(matches!(tokens[0], PromptTk::Text(ref t) if t == "\n"));
  }

  #[test]
  fn prompt_shell_name() {
    let tokens = tokenize_prompt("\\s");
    assert_eq!(tokens.len(), 1);
    assert!(matches!(tokens[0], PromptTk::ShellName));
  }

  #[test]
  fn prompt_literal_backslash() {
    let tokens = tokenize_prompt("\\\\");
    assert_eq!(tokens.len(), 1);
    assert!(matches!(tokens[0], PromptTk::Text(ref t) if t == "\\"));
  }

  #[test]
  fn prompt_mixed() {
    let tokens = tokenize_prompt("\\u@\\h \\w\\$ ");
    // \u, Text("@"), \h, Text(" "), \w, \$, Text(" ")
    assert_eq!(tokens.len(), 7);
    assert!(matches!(tokens[0], PromptTk::Username));
    assert!(matches!(tokens[1], PromptTk::Text(ref t) if t == "@"));
    assert!(matches!(tokens[2], PromptTk::HostnameShort));
    assert!(matches!(tokens[3], PromptTk::Text(ref t) if t == " "));
    assert!(matches!(tokens[4], PromptTk::Pwd));
    assert!(matches!(tokens[5], PromptTk::PromptSymbol));
    assert!(matches!(tokens[6], PromptTk::Text(ref t) if t == " "));
  }

  #[test]
  fn prompt_ansi_sequence() {
    let tokens = tokenize_prompt("\\e[31m");
    assert_eq!(tokens.len(), 1);
    assert!(matches!(tokens[0], PromptTk::AnsiSeq(ref s) if s == "\x1b[31m"));
  }

  #[test]
  fn prompt_octal() {
    let tokens = tokenize_prompt("\\141"); // 'a' in octal
    assert_eq!(tokens.len(), 1);
    assert!(matches!(tokens[0], PromptTk::AsciiOct(97)));
  }

  // ===================== format_cmd_runtime =====================

  #[test]
  fn runtime_millis() {
    let dur = Duration::from_millis(500);
    assert_eq!(format_time(dur), "500ms");
  }

  #[test]
  fn runtime_seconds() {
    let dur = Duration::from_secs(5);
    assert_eq!(format_time(dur), "5s");
  }

  #[test]
  fn runtime_minutes_and_seconds() {
    let dur = Duration::from_secs(125);
    assert_eq!(format_time(dur), "2m 5s");
  }

  #[test]
  fn runtime_hours() {
    let dur = Duration::from_secs(3661);
    assert_eq!(format_time(dur), "1h 1m 1s");
  }

  #[test]
  fn runtime_micros() {
    let dur = Duration::from_micros(500);
    assert_eq!(format_time(dur), "500µs");
  }
}
