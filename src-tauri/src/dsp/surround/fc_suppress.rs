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

//! FC 中置自抑制（ArtTune 核心算法，按「声道」区分自己/敌人）。
//!
//! # 为什么之前按「响度」做会失败
//!
//! 「响=近=自己、轻=远=敌人」这个假设不成立：远处敌人脚步声与自己的脚步
//! 在响度上会重叠，阈值一错就「压不住 / 全压没」。ArtTune 的正确依据是
//! **声道结构**：FPS 游戏把玩家自己的枪声/脚步主要混在中置声道 `FC`，
//! 且对 FC 不做距离衰减；敌人的声音按方位散布在其它声道。
//!
//! # 算法（逐条对齐 `atk_spatial_engine.jsfx` 的 Step 2/3/4）
//!
//! 1. **FC 自抑制**：把 FC 按 250Hz / 4kHz 分三段，各乘泄漏权重后从
//!    FL/FR 减掉（游戏把 FC 内容按比例漏进了左右）。
//! 2. **Center Duck**：FC 电平越过门限时，把 L/R 的「中间分量」（M/S 的 M）
//!    动态压低，最后把 FC 通道**清零**。
//! 3. **自适应脚步 EQ**：FC 静默且无枪声时，抬升 200Hz / 3kHz 脚步频段。
//!
//! 全程 IIR / 一阶包络，**零额外延迟**（无 lookahead）。

use crate::dsp::surround::layout::{Speaker, CHANNELS};

/// FC 三频段泄漏权重（低/中/高），ArtTune 默认值。
const SUB_LOW: f32 = 0.19;
const SUB_MID: f32 = 0.30;
const SUB_HIGH: f32 = 0.25;

/// Center Duck 门限（dBFS）。FC 电平超过它才开始 duck。
const CK_THRESH_DB: f32 = -23.0;
/// Center Duck 最大压制深度（dB）。
const CK_DUCK_DB: f32 = -12.0;
/// Duck 包络 attack / release（秒）。
const CK_ATK_S: f32 = 0.005;
const CK_REL_S: f32 = 0.080;

/// 脚步增强：低频 peaking。
const BOOST_LOW_HZ: f32 = 200.0;
const BOOST_LOW_DB: f32 = 5.0;
/// 脚步增强：高频 peaking（脚步的"咔哒"声）。
const BOOST_HIGH_HZ: f32 = 3000.0;
const BOOST_HIGH_DB: f32 = 2.5;
/// 脚步增强包络 attack / release（秒），慢起慢收，避免呼吸感。
const BOOST_ATK_S: f32 = 0.080;
const BOOST_REL_S: f32 = 0.250;

/// 三频段交叉点（与 ArtTune 一致）。
const XOVER_LO: f32 = 250.0;
const XOVER_HI: f32 = 4000.0;

/// 参与脚步增强的全频方向（跳过 FC 与 LFE）。
const BOOST_CHANNELS: [Speaker; 6] = [
  Speaker::Fl,
  Speaker::Fr,
  Speaker::Bl,
  Speaker::Br,
  Speaker::Sl,
  Speaker::Sr,
];

/// 一个二阶节（转置直接 II 型，系数已按 `a0` 归一化）。
#[derive(Clone, Copy, Debug)]
struct Biquad {
  b0: f32,
  b1: f32,
  b2: f32,
  a1: f32,
  a2: f32,
  z1: f32,
  z2: f32,
}

impl Biquad {
  /// 二阶 Linkwitz-Riley（Q = 0.5）低通。
  fn lowpass(sr: f32, f: f32) -> Biquad {
    let w0 = 2.0 * std::f32::consts::PI * (f / sr);
    let (sw, cw) = w0.sin_cos();
    let alpha = sw;
    let b0 = (1.0 - cw) / 2.0;
    let b1 = 1.0 - cw;
    let b2 = b0;
    let a0 = 1.0 + alpha;
    let a1 = -2.0 * cw;
    let a2 = 1.0 - alpha;
    let inv = 1.0 / a0;
    Biquad {
      b0: b0 * inv,
      b1: b1 * inv,
      b2: b2 * inv,
      a1: a1 * inv,
      a2: a2 * inv,
      z1: 0.0,
      z2: 0.0,
    }
  }

  /// 二阶 Linkwitz-Riley（Q = 0.5）高通，**反相**（与 LP 幅度互补且同相）。
  fn highpass(sr: f32, f: f32) -> Biquad {
    let w0 = 2.0 * std::f32::consts::PI * (f / sr);
    let (sw, cw) = w0.sin_cos();
    let alpha = sw;
    let b0 = -(1.0 + cw) / 2.0;
    let b1 = 1.0 + cw;
    let b2 = -(1.0 + cw) / 2.0;
    let a0 = 1.0 + alpha;
    let a1 = -2.0 * cw;
    let a2 = 1.0 - alpha;
    let inv = 1.0 / a0;
    Biquad {
      b0: b0 * inv,
      b1: b1 * inv,
      b2: b2 * inv,
      a1: a1 * inv,
      a2: a2 * inv,
      z1: 0.0,
      z2: 0.0,
    }
  }

  /// RBJ peaking EQ（`db` 增益，`q` 品质因数）。
  fn peaking(sr: f32, f: f32, db: f32, q: f32) -> Biquad {
    let w0 = 2.0 * std::f32::consts::PI * (f / sr);
    let (sw, cw) = w0.sin_cos();
    let alpha = sw / (2.0 * q);
    let a = 10f32.powf(db / 40.0);
    let b0 = 1.0 + alpha * a;
    let b1 = -2.0 * cw;
    let b2 = 1.0 - alpha * a;
    let a0 = 1.0 + alpha / a;
    let a1 = -2.0 * cw;
    let a2 = 1.0 - alpha / a;
    let inv = 1.0 / a0;
    Biquad {
      b0: b0 * inv,
      b1: b1 * inv,
      b2: b2 * inv,
      a1: a1 * inv,
      a2: a2 * inv,
      z1: 0.0,
      z2: 0.0,
    }
  }

  #[inline]
  fn run(&mut self, x: f32) -> f32 {
    let y = self.b0 * x + self.z1;
    self.z1 = self.b1 * x - self.a1 * y + self.z2;
    self.z2 = self.b2 * x - self.a2 * y;
    y
  }
}

/// 三频段 crossover（250Hz / 4kHz）。
struct Crossover {
  lp_lo: Biquad,
  hp_lo: Biquad,
  lp_hi: Biquad,
  hp_hi: Biquad,
}

impl Crossover {
  fn new(sr: f32) -> Crossover {
    Crossover {
      lp_lo: Biquad::lowpass(sr, XOVER_LO),
      hp_lo: Biquad::highpass(sr, XOVER_LO),
      lp_hi: Biquad::lowpass(sr, XOVER_HI),
      hp_hi: Biquad::highpass(sr, XOVER_HI),
    }
  }

  #[inline]
  fn split(&mut self, x: f32, out: &mut [f32; 3]) {
    let low = self.lp_lo.run(x);
    let hi_lo = self.hp_lo.run(x);
    let mid = self.lp_hi.run(hi_lo);
    let high = self.hp_hi.run(hi_lo);
    out[0] = low;
    out[1] = mid;
    out[2] = high;
  }
}

/// 一阶绝对值包络跟随器（attack/release 可不对称）。
struct Envelope {
  level: f32,
}

impl Envelope {
  fn new() -> Envelope {
    Envelope { level: 1e-4 }
  }

  #[inline]
  fn step(&mut self, x_abs: f32, atk: f32, rel: f32) -> f32 {
    let c = if x_abs > self.level { atk } else { rel };
    self.level += c * (x_abs - self.level);
    self.level
  }
}

/// FC 自抑制参数（0..1，全部可热改，语义与界面三个旋钮对齐）。
#[derive(Clone, Copy, Debug)]
pub struct FcParams {
  /// 中高频减法强度（0..1，默认 0.8）。枪声主体在中高频。
  pub self_gun: f32,
  /// 低频减法强度（0..1，默认 0.7）。脚步冲击在低频。
  pub self_foot: f32,
  /// 脚步增强强度（0..1，默认 0.5）。FC 静默时抬升 200Hz/3kHz。
  pub leveller: f32,
}

impl Default for FcParams {
  fn default() -> Self {
    FcParams {
      self_gun: 0.8,
      self_foot: 0.7,
      leveller: 0.5,
    }
  }
}

impl FcParams {
  fn is_passthrough(&self) -> bool {
    self.self_gun < 0.01 && self.self_foot < 0.01 && self.leveller < 0.01
  }
}

/// FC 中置自抑制引擎。
pub struct FcSuppress {
  sr: f32,
  /// FC 三频段分割。
  xover: Crossover,
  /// Center Duck 的 FC 电平包络。
  ck_env: Envelope,
  /// 脚步增强包络（0..1，慢起慢收）。
  boost_env: Envelope,
  /// 脚步增强 peaking（低频 + 高频），每方向独立状态。
  boost_low: Vec<Biquad>,
  boost_high: Vec<Biquad>,
}

impl FcSuppress {
  pub fn new(sr: f32) -> FcSuppress {
    let boost_low = (0..BOOST_CHANNELS.len())
      .map(|_| Biquad::peaking(sr, BOOST_LOW_HZ, BOOST_LOW_DB, 0.7))
      .collect();
    let boost_high = (0..BOOST_CHANNELS.len())
      .map(|_| Biquad::peaking(sr, BOOST_HIGH_HZ, BOOST_HIGH_DB, 0.7))
      .collect();
    FcSuppress {
      sr,
      xover: Crossover::new(sr),
      ck_env: Envelope::new(),
      boost_env: Envelope::new(),
      boost_low,
      boost_high,
    }
  }

  /// 清空跨块状态（重建会话 / 换设备时用）。
  pub fn reset(&mut self) {
    self.xover.lp_lo.z1 = 0.0;
    self.xover.lp_lo.z2 = 0.0;
    self.xover.hp_lo.z1 = 0.0;
    self.xover.hp_lo.z2 = 0.0;
    self.xover.lp_hi.z1 = 0.0;
    self.xover.lp_hi.z2 = 0.0;
    self.xover.hp_hi.z1 = 0.0;
    self.xover.hp_hi.z2 = 0.0;
    self.ck_env = Envelope::new();
    self.boost_env = Envelope::new();
    for b in self.boost_low.iter_mut().chain(self.boost_high.iter_mut()) {
      b.z1 = 0.0;
      b.z2 = 0.0;
    }
  }

  /// 处理一块：`bus` 是 8 路 7.1（长度 = 块长），就地改 FL/FR/FC 及脚步增强方向。
  #[allow(clippy::needless_range_loop)] // 需同时按样本下标读写 FL/FR/FC 等多路，迭代器不适用
  pub fn process(&mut self, bus: &mut [Vec<f32>], p: &FcParams) {
    if p.is_passthrough() || bus.len() < CHANNELS {
      return;
    }
    let n = bus.first().map(|v| v.len()).unwrap_or(0);
    if n == 0 {
      return;
    }

    let sr = self.sr;
    let coef = |tau: f32| 1.0 - (-1.0 / (sr * tau)).exp();
    let ck_atk = coef(CK_ATK_S);
    let ck_rel = coef(CK_REL_S);
    let boost_atk = coef(BOOST_ATK_S);
    let boost_rel = coef(BOOST_REL_S);

    // FC 减法权重：基础泄漏 × 强度旋钮。
    let gun = p.self_gun.clamp(0.0, 1.0);
    let foot = p.self_foot.clamp(0.0, 1.0);
    let w_low = SUB_LOW * foot;
    let w_mid = SUB_MID * gun;
    let w_high = SUB_HIGH * gun;

    // Center Duck：门限（线性）与最大 duck 增益。
    let thresh_lin = 10f32.powf(CK_THRESH_DB / 20.0);
    let duck_gain = 10f32.powf(CK_DUCK_DB / 20.0);
    // 软拐点宽度（超过门限一倍即达到最大 duck）。
    let knee = thresh_lin * 2.0;

    // 脚步增强强度（leveller 同时控制 peaking 提升量）。
    let boost_amount = p.leveller.clamp(0.0, 1.0);

    let fl = Speaker::Fl.index();
    let fr = Speaker::Fr.index();
    let fc = Speaker::Fc.index();

    let mut band = [0.0f32; 3];

    for t in 0..n {
      // ---- Step 2：FC 自抑制（三频段加权从 FL/FR 减掉）----
      // 注意取反：本工程 LR crossover 的「LP + 反相 HP」三段重组后与输入**反相**
      // （诊断实测 corr_sum ≈ -0.48），取反后才与 FL/FR 里的 FC 泄漏同相，
      // 减法才成立（否则减法变加法，自己的声音反而变大）。
      let fc_raw = bus[fc][t];
      self.xover.split(fc_raw, &mut band);
      let fc_sub = -(w_low * band[0] + w_mid * band[1] + w_high * band[2]);
      bus[fl][t] -= fc_sub;
      bus[fr][t] -= fc_sub;

      // ---- Step 3：Center Duck（FC 触发，M/S 压中间分量，再清零 FC）----
      let ck_level = self.ck_env.step(fc_raw.abs(), ck_atk, ck_rel);
      let ck_gain = if ck_level > thresh_lin {
        let k = ((ck_level - thresh_lin) / (knee - thresh_lin)).min(1.0);
        1.0 + (duck_gain - 1.0) * k
      } else {
        1.0
      };
      let l = bus[fl][t];
      let r = bus[fr][t];
      let mid = (l + r) * 0.5 * ck_gain;
      let side = (l - r) * 0.5;
      bus[fl][t] = mid + side;
      bus[fr][t] = mid - side;
      // 清零 FC（sidechain 用的是上面保存的 fc_raw，不受影响）。
      bus[fc][t] = 0.0;

      // ---- Step 4：自适应脚步增强（FC 静默时抬 200Hz/3kHz）----
      let boost_target = if ck_gain > 0.9 { 1.0 } else { 0.0 };
      let boost_env = self.boost_env.step(boost_target, boost_atk, boost_rel);
      if boost_env > 1e-4 && boost_amount > 1e-3 {
        for (i, sp) in BOOST_CHANNELS.iter().enumerate() {
          let idx = sp.index();
          let x = bus[idx][t];
          let wet = self.boost_high[i].run(self.boost_low[i].run(x));
          bus[idx][t] = x + (wet - x) * boost_env * boost_amount;
        }
      }
    }
  }
}

#[cfg(test)]
mod tests {
  #![allow(clippy::needless_range_loop)] // 测试按样本下标同时写多声道，迭代器反而绕
  use super::*;

  fn rms(x: &[f32]) -> f32 {
    (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt()
  }

  #[test]
  fn fc_centered_self_sound_is_removed() {
    // 核心回归：FC 里只有"自己"，FL/FR 里是 FC 的泄漏（同相信号）。
    // 处理后 FC 必须清零，且 FL/FR 的中间分量被明显压掉。
    let sr = 48_000.0;
    let mut f = FcSuppress::new(sr);
    let p = FcParams {
      self_gun: 1.0,
      self_foot: 1.0,
      leveller: 0.0,
    };
    let n = 480usize;
    let mut bus = vec![vec![0.0f32; n]; CHANNELS];
    for t in 0..n {
      let sig = 0.5 * (2.0 * std::f32::consts::PI * 1000.0 * t as f32 / sr).sin();
      bus[Speaker::Fc.index()][t] = sig;
      // 左右是同相泄漏（约 0.3 倍），其余方向为 0
      bus[Speaker::Fl.index()][t] = 0.3 * sig;
      bus[Speaker::Fr.index()][t] = 0.3 * sig;
    }
    f.process(&mut bus, &p);

    // FC 必须清零
    assert!(
      bus[Speaker::Fc.index()].iter().all(|v| v.abs() < 1e-6),
      "FC 应被清零"
    );
    // FL/FR 处理后的峰值应显著低于输入泄漏（被减 + 被 duck）
    let in_peak = 0.3 * 0.5;
    let out_peak = bus[Speaker::Fl.index()]
      .iter()
      .fold(0.0f32, |a, v| a.max(v.abs()));
    assert!(
      out_peak < in_peak * 0.5,
      "FL 的中间分量应被明显压低，in_peak {in_peak} out_peak {out_peak}"
    );
  }

  #[test]
  fn enemy_on_side_channel_is_preserved() {
    // 敌人声音在 SL（侧声道），不碰 FC。处理后 SL 必须保留能量（不被误伤）。
    let sr = 48_000.0;
    let mut f = FcSuppress::new(sr);
    let p = FcParams::default();
    let n = 480usize;
    let mut bus = vec![vec![0.0f32; n]; CHANNELS];
    for t in 0..n {
      bus[Speaker::Sl.index()][t] =
        0.2 * (2.0 * std::f32::consts::PI * 500.0 * t as f32 / sr).sin();
    }
    let before = rms(&bus[Speaker::Sl.index()]);
    f.process(&mut bus, &p);
    let after = rms(&bus[Speaker::Sl.index()]);
    assert!(
      after > before * 0.9,
      "SL 敌人声音不应被误伤，before {before} after {after}"
    );
  }

  #[test]
  fn passthrough_when_all_zero() {
    let mut f = FcSuppress::new(48_000.0);
    let mut bus = vec![vec![0.5f32; 64]; CHANNELS];
    let before = bus.clone();
    f.process(
      &mut bus,
      &FcParams {
        self_gun: 0.0,
        self_foot: 0.0,
        leveller: 0.0,
      },
    );
    for (c, ch) in bus.iter().enumerate() {
      for (t, v) in ch.iter().enumerate() {
        assert_eq!(*v, before[c][t], "全关时不得改动样本");
      }
    }
  }

  #[test]
  fn boost_raises_footstep_band_when_fc_silent() {
    // FC 静默时，脚步增强应抬升其它声道的 200Hz 分量。
    let sr = 48_000.0;
    let mut f = FcSuppress::new(sr);
    let p = FcParams {
      self_gun: 0.0,
      self_foot: 0.0,
      leveller: 1.0,
    };
    let n = 48_000usize; // 1 秒，让 boost 包络爬满
    let mut bus = vec![vec![0.0f32; n]; CHANNELS];
    for t in 0..n {
      bus[Speaker::Sl.index()][t] =
        0.05 * (2.0 * std::f32::consts::PI * 200.0 * t as f32 / sr).sin();
    }
    f.process(&mut bus, &p);
    // 取后半段（包络稳定后）的 RMS 与输入比较
    let half = n / 2;
    let before = rms(&bus[Speaker::Sl.index()][half..]);
    // 用一个未处理副本比较
    let mut clean = vec![vec![0.0f32; n]; CHANNELS];
    for t in 0..n {
      clean[Speaker::Sl.index()][t] =
        0.05 * (2.0 * std::f32::consts::PI * 200.0 * t as f32 / sr).sin();
    }
    let clean_rms = rms(&clean[Speaker::Sl.index()][half..]);
    assert!(
      before > clean_rms * 1.2,
      "200Hz 脚步频段应被抬升，before {before} clean {clean_rms}"
    );
  }
}
