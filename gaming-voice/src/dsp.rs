//! 直接引用 PureVox 补丁里的同一份 DSP 文件（不是复制）。
//!
//! 放在独立文件里而不是 inline 模块，是因为 `#[path]` 的解析基准不同：
//! 非 mod-rs 文件的相对路径以 `src/` 为基准，inline 模块则以 `src/<模块名>/` 为基准。
//! 写错一层就会变成 `src/dsp/../../src-tauri/...` 找不到文件。

#[allow(unused_imports, dead_code)]
#[path = "../../src-tauri/src/dsp/surround/mod.rs"]
pub mod surround;
