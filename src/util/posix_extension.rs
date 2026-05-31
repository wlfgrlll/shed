use nix::errno::Errno;
use nix::unistd::execve;
use std::convert::Infallible;
use std::ffi::CStr;

use crate::Shed;

pub fn execvpe<SA: AsRef<CStr>, SE: AsRef<CStr>>(
  filename: &CStr,
  args: &[SA],
  env: &[SE],
) -> nix::Result<Infallible> {
  // for nix::unistd::execve
  let args_c: Vec<&CStr> = args.iter().map(AsRef::as_ref).collect();
  let env_c: Vec<&CStr> = env.iter().map(AsRef::as_ref).collect();
  let mut is_denied = false;

  if filename.to_bytes().contains(&b'/') {
    execve(filename, &args_c, &env_c)?;
  } else {
    let path = Shed::vars(|v| v.get_var("PATH"));
    for dir in std::env::split_paths(&path) {
      let full_path_str = dir.join(filename.to_str().unwrap());
      let c_path = std::ffi::CString::new(full_path_str.to_str().unwrap()).unwrap();
      match execve(c_path.as_c_str(), &args_c, &env_c) {
        Ok(_) => unreachable!(),
        Err(Errno::ENOENT | Errno::ENOTDIR) => (), // Try next path
        Err(Errno::EACCES) => is_denied = true,    // Permission denied
        Err(e) => return Err(e),                   // Other error
      }
    }
  }

  // Not found
  if is_denied {
    Err(Errno::EACCES)
  } else {
    Err(Errno::ENOENT)
  }
}
