use std::env;
use std::path::PathBuf;

/// 编译时把仓库根目录的 VB-CABLE 安装包复制到编译输出目录（`target/<profile>/`）。
///
/// 这样 `cargo build --release` 结束后，`target/release/` 下同时有
/// `GamingVoice.exe` 和 `VBCABLE_Setup_x64.exe`，发布时直接把这个目录打包即可，
/// 用户拿到的就是「程序 + 安装包」两个文件，手动运行安装包安装驱动。
fn main() {
  // 仓库根 = Cargo.toml 所在目录（gaming-voice/）的上一级
  let Ok(manifest) = env::var("CARGO_MANIFEST_DIR") else {
    return;
  };
  let src = PathBuf::from(&manifest)
    .parent()
    .unwrap()
    .join("VBCABLE_Setup_x64.exe");
  println!("cargo:rerun-if-changed={}", src.display());

  // OUT_DIR = target/<profile>/build/<pkg>-<hash>/out，向上 3 级到 target/<profile>
  let Ok(out_dir) = env::var("OUT_DIR") else {
    return;
  };
  let out = PathBuf::from(&out_dir);
  let Some(target_dir) = out.ancestors().nth(3).map(|p| p.to_path_buf()) else {
    return;
  };

  if src.is_file() {
    match std::fs::copy(&src, target_dir.join("VBCABLE_Setup_x64.exe")) {
      Ok(_) => println!(
        "cargo:warning=已把 VBCABLE_Setup_x64.exe 复制到 {}",
        target_dir.display()
      ),
      Err(e) => println!("cargo:warning=复制 VBCABLE_Setup_x64.exe 失败：{e}"),
    }
  } else {
    println!(
      "cargo:warning=仓库根目录缺少 VBCABLE_Setup_x64.exe（{}），发布包将不包含安装器。",
      src.display()
    );
  }
}
