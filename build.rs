fn main() {
  println!("cargo:rustc-check-cfg=cfg(linux_like)");
  let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
  if matches!(target_os.as_str(), "linux" | "android") {
    println!("cargo:rustc-cfg=linux_like")
  }
}
