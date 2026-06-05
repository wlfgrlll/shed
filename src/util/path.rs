use std::{os::unix::fs::PermissionsExt, path::PathBuf};

pub fn split_path_list(path_list: &str) -> impl Iterator<Item = PathBuf> {
  // `split_all_with` calls `build(start, end)` — both byte indices into
  // `path_list`, not (start, length). Naming the second arg `end` keeps the
  // slice expression honest; the previous `start + len` form treated it as
  // a length and ran past the buffer once `cursor` grew large.
  let paths = super::strops::split_all_with(
    path_list,
    |paths| super::split_at_unescaped(paths, ":"),
    |start, end| path_list[start..end].to_string(),
  );

  paths.into_iter().map(PathBuf::from)
}

pub fn path_list_entries(path_list: &str) -> impl Iterator<Item = std::fs::DirEntry> {
  let paths = split_path_list(path_list);

  paths.flat_map(|p| {
    p.read_dir()
      .ok()
      .into_iter()
      .flatten()
      .filter_map(Result::ok)
  })
}

pub fn is_executable_file(entry: &std::fs::DirEntry) -> bool {
  let ft = entry.file_type().ok();
  let is_symlink = ft.is_some_and(|t| t.is_symlink());
  let meta = if is_symlink {
    std::fs::metadata(entry.path())
  } else {
    entry.metadata()
  };
  let Ok(meta) = meta else {
    return false;
  };
  meta.is_file() && meta.permissions().mode() & 0o111 != 0
}
