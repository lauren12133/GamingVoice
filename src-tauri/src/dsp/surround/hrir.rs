// PureVox — AI 麦克风降噪工具
// Copyright (C) 2024-2026 a2heng <752848283@qq.com>
//
// PureVox is licensed under the GNU General Public License v3.0 or
// later (GPL-3.0-or-later).  See LICENSE for details.
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// SPDX-License-Identifier: GPL-3.0-or-later

//! HRIR（头相关冲激响应）矩阵：8 个虚拟音箱 × 2 只耳 = 16 条 FIR。
//!
//! **这是整套虚拟环绕的"方位"来源**：哪条 FIR 对应"左后方 30°"这个方向，
//! 全部由数据本身决定，代码不做任何方位假设（方位只用于界面上画雷达）。
//!
//! ## 文件格式（本项目的规范，唯一一种）
//!
//! `WAV`，48 kHz，**16 声道**（= 8 输入 × 2 耳），**in-major** 排列：
//!
//! ```text
//! ch0 = FL→左耳   ch1 = FL→右耳
//! ch2 = FR→左耳   ch3 = FR→右耳
//! …
//! ch14 = SR→左耳  ch15 = SR→右耳
//! ```
//!
//! 即 `channel = in_index * 2 + ear`（ear: 0 = 左，1 = 右）。所有声道等长，
//! 长度即 FIR 抽头数（典型 256–2048）。
//!
//! 为什么不直接吃 HeSuVi 的 `hrir/*.wav`：HeSuVi 文件里每声道的确切语义没有书面规范，
//! 且各家 HRIR 包版本间不一致；本项目只认上面这一种**可验证**的排布，
//! HeSuVi 目录由 `compat::hesuvi` 显式转换（转换顺序在那里集中定义，便于按实测修正）。
//!
//! 许可提醒：HRIR 是录音/测量数据，商业算法的 HRIR（Dolby/DTS 等）**不可随 GPL 源码分发**，
//! 只能由用户自行导入（`LICENSE-THIRD-PARTY.txt` 记一条）。

use crate::audio::SAMPLE_RATE;
use crate::dsp::surround::layout::{Speaker, CHANNELS};
use crate::dsp::surround::wav::{sample, Wav};

/// 每只耳一条（双耳化）。
pub const EARS: usize = 2;
/// 一个完整矩阵的 FIR 条数：8 音箱 × 2 耳。
pub const FIRS: usize = CHANNELS * EARS;

/// 一组 HRIR：`ir[(in_ch * 2 + ear) * taps + t]`。
#[derive(Clone, Debug)]
pub struct Hrir {
  pub sample_rate: u32,
  pub taps: usize,
  /// 长度 = [`FIRS`] × `taps`
  pub ir: Vec<f32>,
  /// 上屏用的名字（文件名 / 预设名），不参与运算。
  pub name: String,
}

impl Hrir {
  /// 从 16 声道 WAV 解析。`name` 只用于界面显示。
  pub fn from_wav(w: &Wav, name: impl Into<String>) -> Result<Hrir, String> {
    if w.channels as usize != FIRS {
      return Err(format!(
        "HRIR 需要 {FIRS} 声道（8 音箱 × 2 耳），这个文件是 {} 声道",
        w.channels
      ));
    }
    if w.sample_rate != SAMPLE_RATE {
      return Err(format!(
        "HRIR 采样率必须是 {SAMPLE_RATE} Hz（引擎内部采样率），这个文件是 {} Hz",
        w.sample_rate
      ));
    }
    if w.frames == 0 {
      return Err("HRIR 没有任何样本".to_string());
    }
    // 交错 → 按 (in, ear) 拍平
    let taps = w.frames;
    let mut ir = vec![0.0f32; FIRS * taps];
    for c in 0..FIRS {
      let base = c * taps;
      for t in 0..taps {
        ir[base + t] = sample(w, t, c);
      }
    }
    let h = Hrir {
      sample_rate: w.sample_rate,
      taps,
      ir,
      name: name.into(),
    };
    if h.peak() <= f32::EPSILON {
      return Err("HRIR 全是静音（峰值 0）".to_string());
    }
    Ok(h)
  }

  /// 一条 FIR 的切片：第 `in_ch` 个音箱 → 第 `ear` 只耳。
  #[inline]
  pub fn fir(&self, in_ch: usize, ear: usize) -> &[f32] {
    let base = (in_ch * EARS + ear) * self.taps;
    &self.ir[base..base + self.taps]
  }

  /// 全矩阵峰值（绝对值最大者）。
  pub fn peak(&self) -> f32 {
    self.ir.iter().fold(0.0f32, |m, v| m.max(v.abs()))
  }

  /// 整体缩放（导入时做电平归一会用到）。
  pub fn scale(&mut self, k: f32) {
    if k.is_finite() && k > 0.0 {
      for v in &mut self.ir {
        *v *= k;
      }
    }
  }

  /// 把峰值归一到 `target`（默认 1.0）：不同来源的 HRIR 电平差异极大，
  /// 不归一的话切换预设就是"一次音量跳变"。
  pub fn normalize_peak(&mut self, target: f32) {
    let p = self.peak();
    if p > f32::EPSILON {
      self.scale(target / p);
    }
  }

  /// 单位矩阵：第 i 个音箱只进左耳、且直通（**只用于测试与旁通诊断**）。
  pub fn identity(taps: usize) -> Hrir {
    let mut ir = vec![0.0f32; FIRS * taps];
    for in_ch in 0..CHANNELS {
      ir[(in_ch * EARS) * taps] = 1.0;
      ir[(in_ch * EARS + 1) * taps] = 1.0;
    }
    Hrir {
      sample_rate: SAMPLE_RATE,
      taps,
      ir,
      name: "identity".to_string(),
    }
  }

  /// 诊断用：某个音箱的直达声在两耳之间的电平差（dB，正 = 右耳更响）。
  ///
  /// 用来在界面/调试接口里验证"左前方的声音确实偏左"——**没有这个读数，
  /// HRIR 装错了也看不出来**（DEBUG.md 的"看不见的状态等于不存在"）。
  pub fn ear_balance_db(&self, sp: Speaker) -> f32 {
    let l = rms(self.fir(sp.index(), 0));
    let r = rms(self.fir(sp.index(), 1));
    if l <= f32::EPSILON || r <= f32::EPSILON {
      return 0.0;
    }
    20.0 * (r / l).log10()
  }
}

fn rms(x: &[f32]) -> f32 {
  if x.is_empty() {
    return 0.0;
  }
  (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt()
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::dsp::surround::wav::encode_f32;

  /// 造一个 16 声道 HRIR：每条 FIR 只有第 (in*2+ear) 个抽头为 1，便于验证映射。
  fn marker_hrir(taps: usize) -> Vec<u8> {
    let mut data = vec![0.0f32; taps * FIRS];
    for in_ch in 0..CHANNELS {
      for ear in 0..EARS {
        let c = in_ch * 2 + ear;
        // 位置编码：t = in_ch * 2 + ear，值 = 1.0
        let t = c % taps;
        data[t * FIRS + c] = 1.0;
      }
    }
    encode_f32(SAMPLE_RATE, FIRS as u16, 0, &data)
  }

  #[test]
  fn maps_channel_to_in_ear() {
    let taps = 32;
    let w = crate::dsp::surround::wav::parse(&marker_hrir(taps)).unwrap();
    let h = Hrir::from_wav(&w, "marker").unwrap();
    assert_eq!(h.taps, taps);
    for in_ch in 0..CHANNELS {
      for ear in 0..EARS {
        let fir = h.fir(in_ch, ear);
        let c = in_ch * 2 + ear;
        let expect_t = c % taps;
        for (t, v) in fir.iter().enumerate() {
          let want = if t == expect_t { 1.0 } else { 0.0 };
          assert!((v - want).abs() < 1e-7, "in={in_ch} ear={ear} t={t}");
        }
      }
    }
  }

  #[test]
  fn rejects_wrong_channel_count_and_rate() {
    let w = crate::dsp::surround::wav::parse(&encode_f32(48_000, 8, 0, &[0.0; 64])).unwrap();
    assert!(Hrir::from_wav(&w, "x").unwrap_err().contains("16 声道"));

    let w = crate::dsp::surround::wav::parse(&encode_f32(44_100, FIRS as u16, 0, &[0.1; 16 * 8]))
      .unwrap();
    assert!(Hrir::from_wav(&w, "x").unwrap_err().contains("采样率"));
  }

  #[test]
  fn rejects_silent_hrir() {
    let w = crate::dsp::surround::wav::parse(&encode_f32(48_000, FIRS as u16, 0, &[0.0; 16 * 4]))
      .unwrap();
    assert!(Hrir::from_wav(&w, "x").unwrap_err().contains("静音"));
  }

  #[test]
  fn identity_is_diagonal_and_normalizes() {
    let mut h = Hrir::identity(8);
    assert!((h.peak() - 1.0).abs() < 1e-6);
    assert!((h.fir(0, 0)[0] - 1.0).abs() < 1e-6);
    assert!((h.fir(0, 1)[0] - 1.0).abs() < 1e-6);
    assert!((h.fir(3, 0)[0] - 1.0).abs() < 1e-6);

    h.scale(4.0);
    h.normalize_peak(1.0);
    assert!((h.peak() - 1.0).abs() < 1e-6);
  }

  #[test]
  fn ear_balance_is_symmetric_for_identity() {
    let h = Hrir::identity(8);
    assert!((h.ear_balance_db(Speaker::Fl)).abs() < 1e-6);
  }
}
