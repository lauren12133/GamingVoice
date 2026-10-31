//! 非 Windows 的占位实现。
//!
//! 目的是让 `cargo test` / `cargo check` 在 Linux/macOS 上能跑通
//! （DSP 与 HTTP 层与平台无关，本来就是在这里验证的）。
//! 真正调 WASAPI 的代码全在 `audio_windows.rs`，只在 Windows 编译。

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

#[derive(Clone, Debug)]
#[allow(dead_code)] // rate / bytes 只进 label 字符串，不参与判断
pub struct Endpoint {
  pub id: String,
  pub label: String,
  pub name: String,
  pub channels: u16,
  pub rate: u32,
  pub bytes: u16,
  pub default: bool,
}

impl Endpoint {
  pub fn is_surround(&self) -> bool {
    self.channels >= 8
  }
}

pub fn enum_endpoints() -> Result<Vec<Endpoint>, String> {
  // `PV_SPATIAL_MOCK=1` 时返回假端点，用于在非 Windows 上验证界面
  // （真实设备列表一定来自 WASAPI，这里只是把 UI 跑起来的脚手架）。
  if std::env::var("PV_SPATIAL_MOCK").as_deref() == Ok("1") {
    return Ok(vec![
      Endpoint {
        id: "mock-cable".into(),
        label: "CABLE Input (VB-Audio Virtual Cable) · 8 声道 · 48 kHz".into(),
        name: "CABLE Input (VB-Audio Virtual Cable)".into(),
        channels: 8,
        rate: 48_000,
        bytes: 32,
        default: true,
      },
      Endpoint {
        id: "mock-2ch".into(),
        label: "耳机 (Realtek(R) Audio) · 2 声道 · 48 kHz".into(),
        name: "耳机 (Realtek(R) Audio)".into(),
        channels: 2,
        rate: 48_000,
        bytes: 32,
        default: false,
      },
    ]);
  }
  Err("WASAPI 仅在 Windows 可用".into())
}

pub fn start_capture(
  _id: Option<String>,
  _prod: rtrb::Producer<f32>,
  _stop: Arc<AtomicBool>,
  on_open: std::sync::mpsc::Sender<Result<(u16, u32), String>>,
) -> std::thread::JoinHandle<()> {
  let _ = on_open.send(Err("采集仅 Windows 可用".into()));
  std::thread::spawn(|| {})
}

pub fn start_render(
  _id: Option<String>,
  _cons: rtrb::Consumer<f32>,
  _stop: Arc<AtomicBool>,
  on_open: std::sync::mpsc::Sender<Result<(u16, u32), String>>,
) -> std::thread::JoinHandle<()> {
  let _ = on_open.send(Err("渲染仅 Windows 可用".into()));
  std::thread::spawn(|| {})
}
