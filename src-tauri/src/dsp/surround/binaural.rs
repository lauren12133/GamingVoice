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

//! 球谐域直接双耳解码：16 球谐分量 → 双耳。
//!
//! 这是 ArtIsWar V5（ATK `Binaural` 解码器 + CIPIC/Listen HRTF）的等价形态：
//! 不经过「8 虚拟扬声器 → 逐方向 HRIR」的中间层，而是把 3 阶球谐的 16 个分量
//! 直接卷积到双耳：
//!
//! ```text
//! hoa[16] ──[Focus 16×16]──► hoa'[16] ──[SH-HRTF 16×2, 256 taps]──► L / R
//! ```
//!
//! SH-HRTF（球谐域 HRTF）由 [`sh_hrif`] 模块内置，数据来自 CIPIC HRTF 数据库
//! `subject_021`（KEMAR 大耳廓）经 3 阶球谐变换得到，48 kHz、256 抽头。
//! 渲染公式：`ear_out = Σ_hoa (hoa[hoa] ⊛ SH_HRIF[hoa][ear])`。
//!
//! 卷积复用 [`MatrixConv`]（16 进 2 出的 uniformly-partitioned overlap-save），
//! 因此延迟特性与原来的 8×2 HRIR 卷积完全一致：零算法延迟，块长由引擎决定。

use crate::dsp::surround::ambisonics::Focus;
use crate::dsp::surround::conv::MatrixConv;
use crate::dsp::surround::hrir::EARS;
use crate::dsp::surround::sh_hrif::{SH_HRIF, SH_HRIF_EARS, SH_HRIF_HOA, SH_HRIF_TAPS};
use crate::dsp::surround::HOA_CHANNELS;

/// 球谐域直接双耳解码器（每块独立、无跨块状态，可被多块连续调用）。
pub struct AmbisonicsBinaural {
  /// 16 进 2 出分块卷积（SH-HRTF）。
  conv: MatrixConv,
  /// 声场聚焦矩阵（热改，零延迟）。
  focus: Focus,
  /// 处理块长（样本）。
  block: usize,
  /// 聚焦后的球谐缓冲 [16][block]。
  hoa_focused: Vec<Vec<f32>>,
  /// 双耳输出 [2][block]。
  ears: Vec<Vec<f32>>,
}

impl AmbisonicsBinaural {
  /// 用内置 SH-HRTF 建解码器，聚焦度默认 1.0（全开）。
  pub fn new(block: usize) -> AmbisonicsBinaural {
    let mut firs: Vec<Vec<f32>> = Vec::with_capacity(HOA_CHANNELS * EARS);
    for per_hoa in SH_HRIF.iter() {
      for per_ear in per_hoa.iter() {
        firs.push(per_ear.to_vec());
      }
    }
    AmbisonicsBinaural {
      conv: MatrixConv::from_firs(HOA_CHANNELS, block, &firs),
      focus: Focus::default(),
      block,
      hoa_focused: vec![vec![0.0; block]; HOA_CHANNELS],
      ears: vec![vec![0.0; block]; EARS],
    }
  }

  /// 改聚焦度（0..1，热路径：下一块生效，无需重建）。
  pub fn set_focus(&mut self, f: f32) {
    self.focus = Focus::new(f);
  }

  /// SH-HRTF 抽头数（诊断用）。
  pub fn taps(&self) -> usize {
    SH_HRIF_TAPS
  }

  /// 每块复数乘加量（调试面板：解释 CPU 占用）。
  pub fn macs_per_hop(&self) -> u64 {
    self.conv.macs_per_hop()
  }

  /// 清空流状态（重建会话 / 换设备时用）。
  pub fn reset(&mut self) {
    self.conv.reset();
  }

  /// 处理一块：`hoa` 是 16 路球谐分量（每路长度 = 块长），
  /// `l` / `r` 是双耳输出（长度 = 块长）。
  ///
  /// 输入不足 16 路时缺的分量按静音处理，**不 panic**（实时路径）。
  pub fn process(&mut self, hoa: &[Vec<f32>], l: &mut [f32], r: &mut [f32]) {
    // 1. 拷贝进内部缓冲（不足补零），再施加聚焦权重。
    for n in 0..HOA_CHANNELS {
      let dst = &mut self.hoa_focused[n];
      if let Some(src) = hoa.get(n) {
        let m = src.len().min(self.block);
        dst[..m].copy_from_slice(&src[..m]);
        for v in dst[m..].iter_mut() {
          *v = 0.0;
        }
      } else {
        for v in dst.iter_mut() {
          *v = 0.0;
        }
      }
    }
    self.focus.apply(&mut self.hoa_focused);

    // 2. 16 路球谐分量 → 双耳卷积。
    self.conv.process(&self.hoa_focused, &mut self.ears);

    // 3. 写回（长度不足只写可用部分）。
    let n = self.block.min(l.len()).min(r.len());
    l[..n].copy_from_slice(&self.ears[0][..n]);
    r[..n].copy_from_slice(&self.ears[1][..n]);
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn sh_hrif_dimensions_match_hoa3() {
    assert_eq!(HOA_CHANNELS, 16);
    assert_eq!(SH_HRIF_HOA, 16);
    assert_eq!(SH_HRIF_EARS, 2);
    assert_eq!(SH_HRIF_TAPS, 256);
    // 数据必须非静音（否则解码器是哑的）
    let energy: f32 = SH_HRIF
      .iter()
      .flat_map(|e| e.iter())
      .flat_map(|f| f.iter())
      .map(|v| v * v)
      .sum();
    assert!(energy > 1.0, "SH-HRTF 能量异常低：{energy}");
  }

  #[test]
  fn w_component_renders_binaurally() {
    // 只有 W 分量有信号 → 双耳应收到"全向"响应（左右接近、非静音）。
    let block = 64usize;
    let mut dec = AmbisonicsBinaural::new(block);
    let mut hoa: Vec<Vec<f32>> = vec![vec![0.0; block]; HOA_CHANNELS];
    hoa[0].fill(0.1);
    let mut l = vec![0.0f32; block];
    let mut r = vec![0.0f32; block];
    dec.process(&hoa, &mut l, &mut r);

    let le: f32 = l.iter().map(|v| v * v).sum();
    let re: f32 = r.iter().map(|v| v * v).sum();
    assert!(le > 1e-4, "左耳应有能量");
    assert!(re > 1e-4, "右耳应有能量");
    // 全向 W → 左右能量应接近（KEMAR 略有不对称，留余量）
    let ratio = (le / re).sqrt();
    assert!((0.3..3.0).contains(&ratio), "W 分量左右能量比异常：{ratio}");
  }

  #[test]
  fn focus_zero_leaves_only_w_audible() {
    // focus=0 时只保留 W：把高阶灌满，输出应和"只有 W"的输出一致。
    let block = 64usize;
    let mut dec_full = AmbisonicsBinaural::new(block);
    dec_full.set_focus(0.0);
    let mut hoa: Vec<Vec<f32>> = vec![vec![0.0; block]; HOA_CHANNELS];
    for ch in hoa.iter_mut().skip(1) {
      ch.fill(0.5);
    }
    let mut l = vec![0.0f32; block];
    let mut r = vec![0.0f32; block];
    dec_full.process(&hoa, &mut l, &mut r);
    // 高阶全被聚焦清零 → 双耳应为静音
    for v in l.iter().chain(r.iter()) {
      assert!(v.abs() < 1e-6, "focus=0 时高阶不应有输出");
    }
  }

  #[test]
  fn short_input_is_zero_padded() {
    // 只给 1 路（W）也要能跑，不 panic。
    let block = 32usize;
    let mut dec = AmbisonicsBinaural::new(block);
    let hoa: Vec<Vec<f32>> = vec![vec![0.1; block]];
    let mut l = vec![0.0f32; block];
    let mut r = vec![0.0f32; block];
    dec.process(&hoa, &mut l, &mut r);
    assert!(l.iter().any(|v| v.abs() > 0.0));
  }
}
