//! 非 Windows 的 GUI 占位。
//!
//! 原生窗口与托盘都是 Win32 API，别的平台不需要（也没法用）。这个文件存在的唯一
//! 目的是让 `mod gui;` 在两个平台上的模块树一致，`cargo test` 才不用到处加 `cfg`。

#![allow(dead_code)]

use std::sync::Arc;

use crate::engine::Shared;

pub fn run(_shared: Arc<Shared>) {}
