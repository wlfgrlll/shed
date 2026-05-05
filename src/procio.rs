use std::{ fmt::Debug, os::fd::AsFd, str::FromStr };

use crate::{
  expand::Expander, match_loop, parse::{
    execute::exec_nonint,
    lex::{Span, Tk, TkFlags},
  }, prelude::*, sherr, state::{self, read_shopts}, util::error::{ ShErr, ShErrKind, ShResult }
};

/// Minimum fd number for shell-internal file descriptors.
/// User-visible fds (0-9) are kept clear so `exec 3>&-` etc. work as expected.
pub const MIN_INTERNAL_FD: RawFd = 10;

/// Like `dup()`, but places the new fd at `MIN_INTERNAL_FD` or above so it
/// doesn't collide with user-managed fds.
fn dup_high(fd: BorrowedFd) -> nix::Result<OwnedFd> {
  let fd = fcntl(fd.as_raw_fd(), FcntlArg::F_DUPFD_CLOEXEC(MIN_INTERNAL_FD))?;
  unsafe { Ok(OwnedFd::from_raw_fd(fd)) }
}

fn move_high(fd: OwnedFd) -> nix::Result<OwnedFd> {
  let new_fd = dup_high(fd.as_fd())?;
  Ok(new_fd)
} // fd is closed here

/// SQLite opens files on its own and we cant call move_high on them.
///
/// Later on we will probably have to do something like using a custom VFS
/// to limit the fd numbers it can use, but for now this will do. I guess.
pub fn do_something_that_opens_fds_that_we_cant_access_hack<F,T>(min_fd: RawFd, something: F) -> T
where F: FnOnce() -> T {
  // these drop at the end of the function
  let _dummies = (3..min_fd).filter_map(|_| {
    // painful to write
    open("/dev/null", OFlag::O_RDONLY | OFlag::O_CLOEXEC, Mode::empty())
      .map(|fd| unsafe { OwnedFd::from_raw_fd(fd) })
      .ok()
  }).collect::<Vec<_>>();

  // now if this opens fds, they will be at least the value of min_fd
  something()
}

pub fn pipes_high() -> nix::Result<(OwnedFd,OwnedFd)> {
  let (r,w) = nix::unistd::pipe()?;
  Ok((move_high(r)?, move_high(w)?))
}

/// Basically just a fancy deferred dup2() call.
///
/// If constructed using Redir::close(), this will close the target fd when applied.
#[derive(Debug)]
pub struct Redir {
  fd: RawFd,
  from: Option<OwnedFd>,
  span: Option<Span>,
}

impl Redir {
  pub fn new(fd: RawFd, from: OwnedFd) -> Self {
    Self {
      fd,
      from: Some(from),
      span: None,
    }
  }
  pub fn close(fd: RawFd) -> Self {
    Self {
      fd,
      from: None,
      span: None,
    }
  }
  pub fn apply(&mut self) -> ShResult<()> {
    if let Some(from) = &self.from {
      nix::unistd::dup2(from.as_raw_fd(), self.fd)?;
    } else {
      nix::unistd::close(self.fd)?;
    }
    Ok(())
  }
  pub fn with_span(mut self, span: Span) -> Self {
    self.span = Some(span);
    self
  }
  pub fn target_fd(&self) -> RawFd {
    self.fd
  }
  pub fn source_fd(&self) -> Option<BorrowedFd<'_>> {
    self.from.as_ref().map(|fd| fd.as_fd())
  }
}

#[derive(Default, Debug)]
pub struct RedirBldr {
  pub fd: Option<RawFd>,
  pub class: Option<RedirType>,
  pub target: Option<RedirTarget>,
  pub span: Option<Span>,
}

impl RedirBldr {
  pub fn new() -> Self {
    Default::default()
  }
  pub fn with_fd(self, fd: RawFd) -> Self {
    Self {
      fd: Some(fd),
      ..self
    }
  }
  pub fn with_class(self, class: RedirType) -> Self {
    Self {
      class: Some(class),
      ..self
    }
  }
  pub fn with_target(self, target: RedirTarget) -> Self {
    Self {
      target: Some(target),
      ..self
    }
  }
  pub fn with_span(self, span: Span) -> Self {
    Self {
      span: Some(span),
      ..self
    }
  }
  pub fn build(self) -> ShResult<RedirSpec> {
    let Some(fd) = self.fd else {
      return Err(sherr!(ParseErr, "Redirection missing target fd").option_promote(self.span));
    };
    let Some(class) = self.class else {
      return Err(sherr!(ParseErr, "Redirection missing class").option_promote(self.span));
    };
    let Some(target) = self.target else {
      return Err(sherr!(ParseErr, "Redirection missing target").option_promote(self.span));
    };

    match target {
      RedirTarget::Path(path) if class.is_file_op() => {
        Ok(RedirSpec::file(fd, path, class))
      }
      RedirTarget::Close => {
        Ok(RedirSpec::close(fd))
      }
      RedirTarget::Fd(src_fd) if class.is_dup_op() => {
        Ok(RedirSpec::dup(src_fd, fd, class))
      }
      RedirTarget::HereDoc { body, flags } => {
        // Strip leading tabs per line BEFORE expansion (POSIX order).
        let mut buf = if flags.contains(TkFlags::TAB_HEREDOC) {
          let trailing_nl = body.ends_with('\n');
          let stripped: Vec<&str> = body.lines()
            .map(|line| line.trim_start_matches('\t'))
            .collect();
          let mut s = stripped.join("\n");
          if trailing_nl { s.push('\n'); }
          s
        } else {
          body
        };

        if flags.contains(TkFlags::IS_HEREDOC) && !flags.contains(TkFlags::LIT_HEREDOC) {
          buf = Expander::from_raw(&buf, flags)?
            .expand()?
            .into_iter()
            .next()
            .unwrap_or_default();
        }

        RedirSpec::buffer(fd, buf)
      }
      _ => Err(sherr!(ParseErr, "Invalid redirection target for redirection type").option_promote(self.span)),
    }
  }
}

impl FromStr for RedirBldr {
  type Err = ShErr;
  fn from_str(s: &str) -> Result<Self, Self::Err> {
    let mut chars = s.chars().peekable();
    let mut src_fd = String::new();
    let mut tgt_fd = String::new();
    let mut redir = RedirBldr::new();

    match_loop!(chars.next() => ch, {
      '>' => {
        redir = redir.with_class(RedirType::Output);
        if let Some('>') = chars.peek() {
          chars.next();
          redir = redir.with_class(RedirType::Append);
        } else if let Some('|') = chars.peek() {
          chars.next();
          redir = redir.with_class(RedirType::OutputForce);
        }
      }
      '<' => {
        redir = redir.with_class(RedirType::Input);
        let mut count = 0;

        if chars.peek() == Some(&'>') {
          chars.next(); // consume the '>'
          redir = redir.with_class(RedirType::ReadWrite);
        } else {
          while count < 2 && matches!(chars.peek(), Some('<')) {
            chars.next();
            count += 1;
          }
        }

        redir = match count {
          1 => redir.with_class(RedirType::HereDoc),
          2 => redir.with_class(RedirType::HereString),
          _ => redir, // Default case remains RedirType::Input
        };
      }
      '&' => {
        if chars.peek() == Some(&'-') {
          chars.next();
          src_fd.push('-');
        } else {
          while let Some(next_ch) = chars.next() {
            if next_ch.is_ascii_digit() {
              src_fd.push(next_ch)
            } else {
              break;
            }
          }
        }
        if src_fd.is_empty() {
          return Err(sherr!(
              ParseErr,
              "Invalid character '{}' in redirection operator",
              ch,
          ));
        }
      }
      _ if ch.is_ascii_digit() && tgt_fd.is_empty() => {
        tgt_fd.push(ch);
        while let Some(next_ch) = chars.peek() {
          if next_ch.is_ascii_digit() {
            let next_ch = chars.next().unwrap();
            tgt_fd.push(next_ch);
          } else {
            break;
          }
        }
      }
      _ => {
        return Err(sherr!(
            ParseErr,
            "Invalid character '{}' in redirection operator",
            ch,
        ));
      }
    });

    let tgt_fd = tgt_fd
      .parse::<i32>()
      .unwrap_or_else(|_| match redir.class.unwrap() {
        RedirType::Input | RedirType::ReadWrite | RedirType::HereDoc | RedirType::HereString => 0,
        _ => 1,
      });
    redir = redir.with_fd(tgt_fd);
    if src_fd.as_str() == "-" {
      redir = redir.with_target(RedirTarget::Close);
    } else if let Ok(src_fd) = src_fd.parse::<i32>() {
      redir = redir.with_target(RedirTarget::Fd(src_fd));
    }
    Ok(redir)
  }
}

impl TryFrom<Tk> for RedirBldr {
  type Error = ShErr;
  fn try_from(tk: Tk) -> Result<Self, Self::Error> {
    let span = tk.span.clone();
    if tk.flags.contains(TkFlags::IS_HEREDOC) {
      let flags = tk.flags;

      Ok(RedirBldr {
        fd: Some(0),
        class: Some(RedirType::HereDoc),
        target: Some(RedirTarget::HereDoc {
          body: tk.to_string(),
          flags,
        }),
        span: Some(span),
      })
    } else {
      match Self::from_str(tk.as_str()) {
        Ok(bldr) => Ok(bldr.with_span(span)),
        Err(e) => Err(e.promote(span)),
      }
    }
  }
}

#[derive(PartialEq, Clone, Copy, Debug)]
pub enum RedirType {
  Null,          // Default
  Input,         // <
  Output,        // >
  OutputForce,   // >|
  Append,        // >>
  HereDoc,       // <<
  IndentHereDoc, // <<-, strips leading tabs
  HereString,    // <<<
  ReadWrite,     // <>, fd is opened for reading and writing
}

impl RedirType {
  pub fn is_input(&self) -> bool {
    matches!(
      self,
      RedirType::Input |
      RedirType::HereDoc |
      RedirType::IndentHereDoc |
      RedirType::HereString |
      RedirType::ReadWrite
    )
  }
  pub fn is_output(&self) -> bool {
    matches!(
      self,
      RedirType::Output |
      RedirType::OutputForce |
      RedirType::Append |
      RedirType::ReadWrite
    )
  }
  pub fn is_file_op(&self) -> bool {
    matches!(
      self,
      RedirType::Output |
      RedirType::OutputForce |
      RedirType::Append |
      RedirType::Input |
      RedirType::ReadWrite
    )
  }
  pub fn is_dup_op(&self) -> bool {
    matches!(self,
      RedirType::Output |
      RedirType::Input
    )
  }
}

#[derive(Clone, Debug)]
pub enum RedirTarget {
  Path(Tk),
  Fd(RawFd),
  Close,
  HereDoc { body: String, flags: TkFlags },
}

#[derive(Debug, Clone)]
pub enum RedirSpec {
  File { fd: RawFd, path: Tk, mode: RedirType },
  Dup { from: RawFd, to: RawFd, mode: RedirType },
  Close { fd: RawFd },
  Buffer { fd: RawFd, buf: String }
}

impl RedirSpec {
  pub fn file(fd: RawFd, path: Tk, mode: RedirType) -> Self {
    Self::File { fd, path, mode }
  }
  pub fn dup(from: RawFd, to: RawFd, mode: RedirType) -> Self {
    Self::Dup { from, to, mode }
  }
  pub fn close(fd: RawFd) -> Self {
    Self::Close { fd }
  }
  pub fn buffer(fd: RawFd, buf: String) -> ShResult<Self> {
    Ok(Self::Buffer { fd, buf })
  }
  pub fn mode(&self) -> RedirType {
    match self {
      RedirSpec::File { mode, .. } => *mode,
      RedirSpec::Dup { mode, .. } => *mode,
      RedirSpec::Close { .. } => RedirType::Null,
      RedirSpec::Buffer { .. } => RedirType::HereDoc,
    }
  }
  pub fn into_redir(self) -> ShResult<Redir> {
    match self {
      RedirSpec::File { fd, path, mode } => {
        let span = path.span.clone();
        let path = path.clone().expand()
          .map(|tk| tk.get_words())
          .unwrap_or_default();

        if path.len() != 1 {
          return Err(sherr!(ExecFail @ span, "Redirection path must expand to exactly one word"));
        }

        let path = path.into_iter().next().unwrap();

        let file: OwnedFd = get_redir_file(mode, path)?.into();
        Ok(Redir::new(fd, file))
      }
      RedirSpec::Dup { from, to, mode: _ } => {
        let borrowed = unsafe { BorrowedFd::borrow_raw(from) };
        let owned = borrowed.try_clone_to_owned()
          .map_err(|e| sherr!(InternalErr, "Failed to duplicate fd {}: {}", from, e))?;
        Ok(Redir::new(to, owned))
      }
      RedirSpec::Close { fd } => {
        Ok(Redir::close(fd))
      }
      RedirSpec::Buffer { fd, buf } => {
        use nix::sys::memfd::{memfd_create, MemFdCreateFlag};
        use std::ffi::CString;
        use std::io::{Seek, SeekFrom, Write};

        let name = CString::new("shed_heredoc").unwrap();
        let owned = memfd_create(&name, MemFdCreateFlag::MFD_CLOEXEC)
          .map_err(|e| sherr!(InternalErr, "memfd_create failed: {e}"))?;

        let mut file = std::fs::File::from(owned);
        file.write_all(buf.as_bytes())
          .map_err(|e| sherr!(InternalErr, "heredoc write failed: {e}"))?;
        file.seek(SeekFrom::Start(0))
          .map_err(|e| sherr!(InternalErr, "heredoc seek failed: {e}"))?;

        Ok(Redir::new(fd, file.into()))
      }
    }
  }
}

#[derive(Default, Debug)]
pub struct RedirSet(pub Vec<RedirSpec>);

impl RedirSet {
  pub fn apply_persistent(self) -> ShResult<()> {
    for spec in self.0 {
      let mut redir = spec.into_redir()?;
      redir.apply()?;
    }
    Ok(())
  }
  pub fn apply(self) -> ShResult<Option<RedirGuard>> {
    if self.0.is_empty() {
      return Ok(None)
    }
    let guard = RedirGuard::new()?;
    for spec in self.0 {
      let mut redir = spec.into_redir()?;
      redir.apply()?;
    }
    Ok(Some(guard))
  }
  pub fn split_by_channel(self) -> (RedirSet, RedirSet) {
    let mut in_redirs = vec![];
    let mut out_redirs = vec![];
    for spec in self.0 {
      if spec.mode().is_input() {
        in_redirs.push(spec);
      } else if spec.mode().is_output() {
        out_redirs.push(spec);
      }
    }
    (RedirSet(in_redirs), RedirSet(out_redirs))
  }
}

impl From<Vec<RedirSpec>> for RedirSet {
  fn from(value: Vec<RedirSpec>) -> Self {
    Self(value)
  }
}

impl From<RedirSpec> for RedirSet {
  fn from(value: RedirSpec) -> Self {
    Self(vec![value])
  }
}

#[derive(Debug)]
pub struct RedirGuard {
  saved: Option<IoGroup>
}

impl RedirGuard {
  pub fn new() -> ShResult<Self> {
    let saved = Some(IoGroup::capture_state()?);
    Ok(Self { saved })
  }
  pub fn persist(mut self) {
    use std::mem::{take, drop};
    drop(take(&mut self.saved));
  }
}

impl Drop for RedirGuard {
  fn drop(&mut self) {
    if let Some(saved) = self.saved.as_ref() {
      saved.restore().ok();
    }
  }
}

/// A struct wrapping three fildescs representing `stdin`, `stdout`, and
/// `stderr` respectively
#[derive(Debug)]
pub struct IoGroup {
  stdin: OwnedFd,
  stdout: OwnedFd,
  stderr: OwnedFd,
}

impl IoGroup {
  pub fn capture_state() -> ShResult<Self> {
    let stdin = dup_high(stdin_fileno())?;
    let stdout = dup_high(stdout_fileno())?;
    let stderr = dup_high(stderr_fileno())?;
    Ok(Self { stdin, stdout, stderr })
  }
  pub fn restore(&self) -> ShResult<()> {
    nix::unistd::dup2(self.stdin.as_raw_fd(), 0)?;
    nix::unistd::dup2(self.stdout.as_raw_fd(), 1)?;
    nix::unistd::dup2(self.stderr.as_raw_fd(), 2)?;
    Ok(())
  }
}

/// An iterator that lazily creates a specific number of pipes.
pub struct PipeGenerator {
  num_cmds: usize,
  cursor: usize,
  last_rpipe: Option<Redir>,
}

impl PipeGenerator {
  pub fn new(num_cmds: usize) -> Self {
    Self {
      num_cmds,
      cursor: 0,
      last_rpipe: None,
    }
  }
}

impl Iterator for PipeGenerator {
  type Item = (Option<Redir>, Option<Redir>);
  fn next(&mut self) -> Option<Self::Item> {
    if self.cursor >= self.num_cmds {
      return None;
    }

    let needs_write = self.cursor + 1 < self.num_cmds; // this is not the last command

    let rpipe = self.last_rpipe.take(); // None if this is the first command
    let wpipe = needs_write.then(|| {
      let (r, w) = pipes_high().ok()?;
      let read = Redir::new(0, r);
      let write = Redir::new(1, w);
      self.last_rpipe = Some(read);
      Some(write)
    }).flatten();

    self.cursor += 1;
    Some((rpipe, wpipe))
  }
}

/// Split a list of RedirSpecs into a list of input redirs and output redirs
///
/// Returned as (input_redirs, output_redirs).
pub fn split_by_channel(specs: Vec<RedirSpec>) -> (Vec<RedirSpec>, Vec<RedirSpec>) {
  let mut out_redirs = vec![];
  let mut in_redirs = vec![];
  for spec in specs {
    if spec.mode().is_input() {
      in_redirs.push(spec);
    } else if spec.mode().is_output() {
      out_redirs.push(spec);
    }
  }
  (in_redirs, out_redirs)
}

pub fn stdin_fileno() -> BorrowedFd<'static> {
  unsafe { BorrowedFd::borrow_raw(STDIN_FILENO) }
}

pub fn stdout_fileno() -> BorrowedFd<'static> {
  unsafe { BorrowedFd::borrow_raw(STDOUT_FILENO) }
}

pub fn stderr_fileno() -> BorrowedFd<'static> {
  unsafe { BorrowedFd::borrow_raw(STDERR_FILENO) }
}

pub fn read_fd_to_string(fd: OwnedFd) -> ShResult<String> {
  use std::io::Read;
  let mut file = std::fs::File::from(fd);
  let mut buf = Vec::new();
  file.read_to_end(&mut buf)?;
  String::from_utf8(buf).map_err(|e| sherr!(InternalErr, "Failed to read fd: {}", e))
}

pub fn capture_command(cmd: &str, stdin: Option<&str>) -> ShResult<String> {
  let (rpipe, wpipe) = pipes_high()?;
  let child_stdout = Redir::new(1, wpipe);
  let mut child_stdin = None;

  let (mut stdin_pipe, stdin_write_fd) = if stdin.is_some() {
    let (r, w) = pipes_high()?;
    let write_fd = w.as_raw_fd();
    child_stdin = Some(Redir::new(0, r));
    (Some(w), Some(write_fd))
  } else {
    (None, None)
  };

  match unsafe { fork()? } {
    ForkResult::Child => {
      if let Some(fd) = stdin_write_fd {
        close(fd).ok(); // close child's copy of stdin write end
      }
      if let Err(e) = exec_nonint(cmd.to_string(), Some("command_sub".into())) {
        if let ShErrKind::CleanExit(code) = e.kind() {
          std::process::exit(*code);
        }
        e.print_error();
        unsafe { libc::_exit(1) };
      }
      let status = state::get_status();
      unsafe { libc::_exit(status) };
    }
    ForkResult::Parent { child } => {
      std::mem::drop(child_stdin); // closes parent's copy of child fds
      std::mem::drop(child_stdout); // closes parent's copy of child fds

      if let Some(pipe) = stdin_pipe.take() {
        write(pipe, stdin.unwrap().as_bytes())?;
      }

      let captured = read_fd_to_string(rpipe)?
        .trim_end()
        .to_string();

      let status = loop {
        match waitpid(child, Some(WtFlag::WSTOPPED)) {
          Ok(status) => break status,
          Err(Errno::EINTR) => continue,
          Err(e) => return Err(e.into()),
        }
      };

      match status {
        WtStat::Exited(_, code) => {
          state::set_status(code);
          Ok(captured)
        }
        _ => Err(sherr!(InternalErr, "Command sub failed")),
      }
    }
  }
}

pub fn get_redir_file<P: AsRef<Path>>(class: RedirType, path: P) -> ShResult<File> {
  let path = path.as_ref();
  let result = match class {
    RedirType::Input => OpenOptions::new().read(true).open(Path::new(&path)),
    RedirType::Output => {
      if read_shopts(|o| o.set.noclobber) && path.is_file() {
        return Err(sherr!(
          ExecFail,
          "shopt core.noclobber is set, refusing to overwrite existing file `{}`",
          path.display()
        ));
      }
      OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
    }
    RedirType::ReadWrite => OpenOptions::new()
      .write(true)
      .read(true)
      .create(true)
      .truncate(false)
      .open(path),
    RedirType::OutputForce => OpenOptions::new()
      .write(true)
      .create(true)
      .truncate(true)
      .open(path),
    RedirType::Append => OpenOptions::new().create(true).append(true).open(path),
    _ => unimplemented!("Unimplemented redir type: {:?}", class),
  };
  Ok(result?)
}


#[cfg(test)]
pub mod tests {
  use crate::tests::testutil::{TestGuard, has_cmd, has_cmds, test_input};
  use pretty_assertions::assert_eq;

  #[test]
  fn pipeline_simple() {
    if !has_cmd("sed") {
      return;
    };
    let g = TestGuard::new();

    test_input("echo foo | sed 's/foo/bar/'").unwrap();

    let out = g.read_output();
    assert_eq!(out, "bar\n");
  }

  #[test]
  fn pipeline_multi() {
    if !has_cmds(&["cut", "sed"]) {
      return;
    }
    let g = TestGuard::new();

    test_input("echo foo bar baz | cut -d ' ' -f 2 | sed 's/a/A/'").unwrap();

    let out = g.read_output();
    assert_eq!(out, "bAr\n");
  }

  #[test]
  fn rube_goldberg_pipeline() {
    if !has_cmds(&["sed", "cat"]) {
      return;
    }
    let g = TestGuard::new();

    test_input("{ echo foo; echo bar } | if cat; then :; else echo failed; fi | (read line && echo $line | sed 's/foo/baz/'; sed 's/bar/buzz/')").unwrap();

    let out = g.read_output();
    assert_eq!(out, "baz\nbuzz\n");
  }

  #[test]
  fn simple_file_redir() {
    let mut g = TestGuard::new();

    test_input("echo this is in a file > /tmp/simple_file_redir.txt").unwrap();

    g.add_cleanup(|| {
      std::fs::remove_file("/tmp/simple_file_redir.txt").ok();
    });
    let contents = std::fs::read_to_string("/tmp/simple_file_redir.txt").unwrap();

    assert_eq!(contents, "this is in a file\n");
  }

  #[test]
  fn append_file_redir() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("append.txt");
    let _g = TestGuard::new();

    test_input(format!("echo first > {}", path.display())).unwrap();
    test_input(format!("echo second >> {}", path.display())).unwrap();

    let contents = std::fs::read_to_string(&path).unwrap();
    assert_eq!(contents, "first\nsecond\n");
  }

  #[test]
  fn input_redir() {
    if !has_cmd("cat") {
      return;
    }
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("input.txt");
    std::fs::write(&path, "hello from file\n").unwrap();
    let g = TestGuard::new();

    test_input(format!("cat < {}", path.display())).unwrap();

    let out = g.read_output();
    assert_eq!(out, "hello from file\n");
  }

  #[test]
  fn stderr_redir_to_file() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("err.txt");
    let g = TestGuard::new();

    test_input(format!("echo error msg 2> {} >&2", path.display())).unwrap();

    let contents = std::fs::read_to_string(&path).unwrap();
    assert_eq!(contents, "error msg\n");
    // stdout should be empty since we redirected to stderr
    let out = g.read_output();
    assert_eq!(out, "");
  }

  #[test]
  fn pipe_and_stderr() {
    if !has_cmd("cat") {
      return;
    }
    let g = TestGuard::new();

    test_input("echo on stderr >&2 |& cat").unwrap();

    let out = g.read_output();
    assert_eq!(out, "on stderr\n");
  }

  #[test]
  fn output_redir_clobber() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("clobber.txt");
    let _g = TestGuard::new();

    test_input(format!("echo first > {}", path.display())).unwrap();
    test_input(format!("echo second > {}", path.display())).unwrap();

    let contents = std::fs::read_to_string(&path).unwrap();
    assert_eq!(contents, "second\n");
  }

  #[test]
  fn pipeline_preserves_exit_status() {
    if !has_cmd("cat") {
      return;
    }
    let _g = TestGuard::new();

    test_input("false | cat").unwrap();

    // Pipeline exit status is the last command
    let status = crate::state::get_status();
    assert_eq!(status, 0);

    test_input("cat < /dev/null | false").unwrap();

    let status = crate::state::get_status();
    assert_ne!(status, 0);
  }

  #[test]
  fn fd_duplication() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("dup.txt");
    let _g = TestGuard::new();

    test_input(format!(
      "{{ echo out; echo err >&2; }} > {} 2>&1",
      path.display()
    ))
    .unwrap();

    let contents = std::fs::read_to_string(&path).unwrap();
    assert!(contents.contains("out"));
    assert!(contents.contains("err"));
  }
}
