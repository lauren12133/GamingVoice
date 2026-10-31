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
//! ## 文件格式：接受两种排布，按声道数自动识别
//!
//! **A. 16 声道 · in-major**（本项目原生格式，推荐）：
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
//! **B. 14 轨 · HeSuVi 原始排布**（`hrir/*.wav`，可直接丢进来，无需外部转换）：
//!
//! 顺序不是 in-major —— 中置夹在中间、右声道左右耳还倒了一次，出处是
//! `jaakkopasanen/Impulcifer` 的 `constants.py`，由 HeSuVi 作者 Jaakko Pasanen 本人确认。
//! 见 [`HESUVI_TRACKS`]。LFE 在 HeSuVi 里没有独立轨（低频并进中置），
//! 所以第 3 号音箱复用中置的双耳 IR。
//!
//! 两种都由 [`Hrir::from_wav`] 自动判别；采样率必须是 48 kHz（引擎内部率）。
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

/// HeSuVi 的 14 轨排布 → `(音箱下标, 耳)`；耳 0 = 左，1 = 右。
///
/// 顺序出处：`jaakkopasanen/Impulcifer` 的 `constants.py`，HeSuVi 作者
/// Jaakko Pasanen 在 SourceForge 论坛帖中逐轨确认。**三处反直觉的地方都是真的**：
/// 中置在第 7 和第 14 轨（不是连续的最后一对）、FR/SR/BR 的右耳排在左耳之前。
///
/// LFE 不在表里 —— HeSuVi 把低频并进中置，没有第 15 轨。
pub const HESUVI_TRACKS: [(usize, usize); 14] = [
  (Speaker::Fl.index(), 0), // 0  FL-left
  (Speaker::Fl.index(), 1), // 1  FL-right
  (Speaker::Sl.index(), 0), // 2  SL-left
  (Speaker::Sl.index(), 1), // 3  SL-right
  (Speaker::Bl.index(), 0), // 4  BL-left
  (Speaker::Bl.index(), 1), // 5  BL-right
  (Speaker::Fc.index(), 0), // 6  FC-left
  (Speaker::Fr.index(), 1), // 7  FR-right
  (Speaker::Fr.index(), 0), // 8  FR-left
  (Speaker::Sr.index(), 1), // 9  SR-right
  (Speaker::Sr.index(), 0), // 10 SR-left
  (Speaker::Br.index(), 1), // 11 BR-right
  (Speaker::Br.index(), 0), // 12 BR-left
  (Speaker::Fc.index(), 1), // 13 FC-right
];

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
  /// 从 WAV 解析，**按声道数自动识别排布**：16 声道走 in-major，14 声道走 HeSuVi。
  /// `name` 只用于界面显示。
  pub fn from_wav(w: &Wav, name: impl Into<String>) -> Result<Hrir, String> {
    let name = name.into();
    if w.sample_rate != SAMPLE_RATE {
      return Err(format!(
        "HRIR 采样率必须是 {SAMPLE_RATE} Hz（引擎内部采样率），这个文件是 {} Hz",
        w.sample_rate
      ));
    }
    if w.frames == 0 {
      return Err("HRIR 没有任何样本".to_string());
    }
    let ch = w.channels as usize;
    let h = if ch == FIRS {
      Hrir::from_in_major(w, name)?
    } else if ch == HESUVI_TRACKS.len() {
      Hrir::from_hesuvi(w, name)?
    } else {
      return Err(format!(
        "HRIR 需要 16 声道（8 音箱 × 2 耳，in-major）或 14 轨（HeSuVi 原始格式，\
         直接从 HeSuVi 的 hrir 目录拷过来即可），这个文件是 {ch} 声道"
      ));
    };
    if h.peak() <= f32::EPSILON {
      return Err("HRIR 全是静音（峰值 0）".to_string());
    }
    Ok(h)
  }

  /// 16 声道 in-major：`channel = in_index * 2 + ear`，原样拍平。
  fn from_in_major(w: &Wav, name: String) -> Result<Hrir, String> {
    let taps = w.frames;
    let mut ir = vec![0.0f32; FIRS * taps];
    for c in 0..FIRS {
      let base = c * taps;
      for t in 0..taps {
        ir[base + t] = sample(w, t, c);
      }
    }
    Ok(Hrir {
      sample_rate: w.sample_rate,
      taps,
      ir,
      name,
    })
  }

  /// 14 轨 HeSuVi：按 [`HESUVI_TRACKS`] 重排成 in-major。
  ///
  /// LFE 没有独立轨（HeSuVi 把低频并进中置），这里直接复用中置的双耳 IR ——
  /// 与 HeSuVi 的实际行为一致，而不是留空（留空会让低频整段消失）。
  fn from_hesuvi(w: &Wav, name: String) -> Result<Hrir, String> {
    let taps = w.frames;
    let mut ir = vec![0.0f32; FIRS * taps];
    for (track, &(in_ch, ear)) in HESUVI_TRACKS.iter().enumerate() {
      let base = (in_ch * EARS + ear) * taps;
      for t in 0..taps {
        ir[base + t] = sample(w, t, track);
      }
    }
    for ear in 0..EARS {
      let from = (Speaker::Fc.index() * EARS + ear) * taps;
      let to = (Speaker::Lfe.index() * EARS + ear) * taps;
      ir.copy_within(from..from + taps, to);
    }
    Ok(Hrir {
      sample_rate: w.sample_rate,
      taps,
      ir,
      name,
    })
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

  /// 造一个 14 轨 HeSuVi 文件：第 n 轨的全部样本 = (n+1)/16，
  /// 这样每条 FIR 的值就能反查出它来自第几轨。
  fn hesuvi_14(taps: usize) -> Vec<u8> {
    const TRACKS: usize = 14;
    let mut data = vec![0.0f32; taps * TRACKS];
    for track in 0..TRACKS {
      let v = (track + 1) as f32 / 16.0;
      for t in 0..taps {
        data[t * TRACKS + track] = v;
      }
    }
    encode_f32(SAMPLE_RATE, TRACKS as u16, 0, &data)
  }

  #[test]
  fn maps_hesuvi_14_track_layout() {
    let taps = 16;
    let w = crate::dsp::surround::wav::parse(&hesuvi_14(taps)).unwrap();
    let h = Hrir::from_wav(&w, "hesuvi").unwrap();
    assert_eq!(h.taps, taps);

    // 写死最容易搞反的三条，避免"用表验证表"的自证：
    // FR 在 HeSuVi 里右耳排在左耳前面（第 8 轨=右耳，第 9 轨=左耳）。
    assert!((h.fir(Speaker::Fr.index(), 0)[0] - 9.0 / 16.0).abs() < 1e-7);
    assert!((h.fir(Speaker::Fr.index(), 1)[0] - 8.0 / 16.0).abs() < 1e-7);
    // 中置被劈到文件头尾两轨（第 7 轨=左耳，第 14 轨=右耳），不是连续一对。
    assert!((h.fir(Speaker::Fc.index(), 0)[0] - 7.0 / 16.0).abs() < 1e-7);
    assert!((h.fir(Speaker::Fc.index(), 1)[0] - 14.0 / 16.0).abs() < 1e-7);

    // 整表都对上
    for (track, &(in_ch, ear)) in HESUVI_TRACKS.iter().enumerate() {
      let want = (track + 1) as f32 / 16.0;
      for (t, v) in h.fir(in_ch, ear).iter().enumerate() {
        assert!(
          (v - want).abs() < 1e-7,
          "轨 {track} → in={in_ch} ear={ear} t={t}"
        );
      }
    }
    // LFE 复用中置（HeSuVi 没有第 15 轨）
    for ear in 0..EARS {
      assert_eq!(
        h.fir(Speaker::Lfe.index(), ear),
        h.fir(Speaker::Fc.index(), ear)
      );
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
