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

//! 虚拟 7.1 空间域（L1 DSP）：把这套"VB-Cable + Equalizer APO + HeSuVi"的外挂链路
//! 收进引擎内部，成为一条**普通输出行**能选的处理链。
//!
//! | 外挂方案里的东西 | 这里的对应物 |
//! | --- | --- |
//! | HeSuVi 的 HRIR 卷积（7.1 → 双耳） | [`hrir`] + [`conv`] |
//! | APO 的 `Channel: BL` + `Preamp` | [`spatial::Spatial::set_gain_db`]（G 对角阵） |
//! | APO 的 `Copy:` 混音矩阵 | [`compat::apo`] 解析后写进 [`MixMatrix`] |
//! | APO 的 `Filter: ON PK …` | 交给 `engine::components::eq31`（EQ 只有一个实现） |
//! | VB-Cable 端点 | L0 的设备层（本模块不管设备） |
//!
//! 分层纪律（ARCHITECTURE.md §1）：**本模块只做 DSP，不碰设备、不做重采样、不切 hop**。
//! 进出一律 480 样本；采样率恒为 `audio::SAMPLE_RATE`。

pub mod conv;
pub mod hrir;
pub mod latency;
pub mod layout;
pub mod spatial;
pub mod wav;

pub use conv::MatrixConv;
pub use hrir::Hrir;
pub use latency::{Chain, LatencyModel, Preset, Verdict};
pub use layout::{Speaker, CHANNELS};
pub use spatial::{Spatial, SpatialParams, Upmix};

use crate::audio::{HOP, SAMPLE_RATE};

/// 8×8 混音矩阵：`out[i] = Σ_j m[i][j] * in[j]`（APO `Copy:` 行的等价物）。
///
/// 默认是单位阵；对角线上放增益就是"每方向音量"，非对角项是"往别的方向漏一点"。
#[derive(Clone, Debug)]
pub struct MixMatrix {
  pub rows: [[f32; CHANNELS]; CHANNELS],
}

impl Default for MixMatrix {
  fn default() -> Self {
    let mut rows = [[0.0f32; CHANNELS]; CHANNELS];
    for (i, row) in rows.iter_mut().enumerate() {
      row[i] = 1.0;
    }
    MixMatrix { rows }
  }
}

impl MixMatrix {
  pub fn is_identity(&self) -> bool {
    (0..CHANNELS).all(|i| {
      (0..CHANNELS).all(|j| {
        let want = if i == j { 1.0 } else { 0.0 };
        (self.rows[i][j] - want).abs() < 1e-9
      })
    })
  }

  /// 把矩阵作用在一块 8×HOP 的 planar 缓冲上（就地）。
  pub fn apply(&self, bus: &mut [Vec<f32>]) {
    if self.is_identity() || bus.len() < CHANNELS {
      return;
    }
    let mut tmp = vec![vec![0.0f32; HOP]; CHANNELS];
    for (row, dst) in self.rows.iter().zip(tmp.iter_mut()) {
      for (t, cell) in dst.iter_mut().enumerate() {
        let mut acc = 0.0f32;
        for (j, k) in row.iter().enumerate() {
          if *k != 0.0 {
            acc += k * bus[j][t];
          }
        }
        *cell = acc;
      }
    }
    for (dst, src) in bus.iter_mut().zip(tmp.iter()) {
      dst.copy_from_slice(src);
    }
  }
}

/// 一次空间化所需的全部静态条件（改这些要重建输出行；改增益/延时不用）。
#[derive(Clone, Debug, Default)]
pub struct SurroundSetup {
  pub params: SpatialParams,
  pub mix: MixMatrix,
}

impl SurroundSetup {
  /// 规格自检：返回人话问题列表（空 = 可用）。用于界面预检与调试接口。
  pub fn check(&self) -> Vec<String> {
    let mut out = Vec::new();
    if SAMPLE_RATE != 48_000 {
      out.push(format!(
        "引擎采样率 {SAMPLE_RATE} Hz 与 HRIR 规范（48000 Hz）不一致"
      ));
    }
    if self.params.spread < 0.0 || self.params.spread > 1.0 {
      out.push("spread 必须在 0..1".to_string());
    }
    for sp in Speaker::ALL {
      let d = self.params.delays_ms[sp.index()];
      if !(0.0..=spatial::MAX_DELAY_MS).contains(&d) {
        out.push(format!(
          "{} 延时 {d} ms 越界（0..{}）",
          sp.id(),
          spatial::MAX_DELAY_MS
        ));
      }
    }
    let _ = HOP;
    out
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn default_matrix_is_identity() {
    assert!(MixMatrix::default().is_identity());
  }

  #[test]
  fn matrix_routes_back_left_into_front_left() {
    let mut m = MixMatrix::default();
    m.rows[Speaker::Fl.index()][Speaker::Bl.index()] = 0.8;
    let mut bus = vec![vec![0.0f32; HOP]; CHANNELS];
    bus[Speaker::Bl.index()] = vec![1.0; HOP];
    m.apply(&mut bus);
    assert!((bus[Speaker::Fl.index()][0] - 0.8).abs() < 1e-6);
    assert!((bus[Speaker::Bl.index()][0] - 1.0).abs() < 1e-6);
  }

  #[test]
  fn setup_check_flags_bad_delay() {
    let mut s = SurroundSetup::default();
    s.params.delays_ms[2] = 99.0;
    assert!(s.check().iter().any(|m| m.contains("延时")));
  }
}
