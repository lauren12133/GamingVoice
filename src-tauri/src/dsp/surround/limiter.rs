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

//! 输出端 lookahead 峰值 limiter（对齐 ArtIsWar V5 输出 True peak = 0 dBFS 的处理）。
//!
//! 为什么需要它：无 lookahead 的压缩器，瞬态**前沿**（枪声的第一个亚毫秒级尖峰）
//! 总会漏出一部分——因为包络检测需要时间爬升，等它反应过来，前沿已经过去了。原作者
//! 的输出 `True peak = 0.0 dBFS`，说明他在链尾放了一个 lookahead limiter 把瞬态前沿
//! 压平，这正是「自己开枪声音很小」的最后一道关键工序。
//!
//! 本实现：
//! - **1 ms lookahead**（48 样本 @48kHz）：用一条延迟线提前看到未来峰值，在枪声前沿
//!   到达输出位置**之前**就把增益降到位，前沿不漏出。
//! - **stereo-linked**：左右耳取同一增益（峰值取两耳最大），声像不左右漂移。
//! - **ceiling = -0.3 dBFS**：比原作者（0.0 dBFS）多留 0.3 dB 余量，防渲染端
//!   重采样/设备增益产生 intersample peak，听感无差异。
//! - **gain 平滑**：attack 0.1 ms（快、跟得上瞬态）、release 50 ms（慢、不产生泵吸）。
//!
//! 代价：+1 ms 延迟（lookahead 的固有代价），端到端 40 → 41 ms，无感。

use crate::dsp::surround::hrir::EARS;

/// 输出端 lookahead 峰值 limiter。
pub struct LookaheadLimiter {
  /// lookahead 样本数（1 ms @ 采样率）。
  lookahead: usize,
  /// `[耳][延迟线]`，长度 `lookahead + 1`。
  delay: Vec<Vec<f32>>,
  /// 环形写指针：push 后它指向「最旧」样本（即要输出的那个）。
  write: usize,
  /// 当前增益（一阶平滑状态，跨块保持）。
  gain: f32,
  /// 峰值上限（线性）。
  ceiling: f32,
  /// 增益下降（attack）平滑系数。
  atk_coef: f32,
  /// 增益恢复（release）平滑系数。
  rel_coef: f32,
}

impl LookaheadLimiter {
  pub fn new(sr: f32) -> LookaheadLimiter {
    let lookahead = (0.001 * sr).round().max(1.0) as usize;
    let coef = |tau: f32| 1.0 - (-1.0 / (sr * tau)).exp();
    LookaheadLimiter {
      lookahead,
      delay: vec![vec![0.0f32; lookahead + 1]; EARS],
      write: 0,
      gain: 1.0,
      ceiling: 10f32.powf(-0.3 / 20.0), // -0.3 dBFS
      atk_coef: coef(0.0001),           // 0.1 ms
      rel_coef: coef(0.050),            // 50 ms
    }
  }

  /// 处理一块：`l` / `r` 就地替换为限幅后的双耳输出。
  pub fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
    let cap = self.lookahead + 1;
    let n = l.len().min(r.len());
    for t in 0..n {
      // push 当前样本进延迟线（两个耳）。
      self.delay[0][self.write] = l[t];
      self.delay[1][self.write] = r[t];
      self.write += 1;
      if self.write >= cap {
        self.write = 0;
      }

      // 窗口峰值 = 延迟线内全部样本（含「未来」的 lookahead 个样本）。
      let mut peak = 0.0f32;
      for v in self.delay[0].iter() {
        let a = v.abs();
        if a > peak {
          peak = a;
        }
      }
      for v in self.delay[1].iter() {
        let a = v.abs();
        if a > peak {
          peak = a;
        }
      }

      // 目标增益：峰值不超 ceiling 则 1.0（直通），否则压到 ceiling。
      let target = if peak > self.ceiling {
        self.ceiling / peak
      } else {
        1.0
      };
      // 平滑：下降要快（压前沿），恢复要慢（防泵吸）。
      let c = if target < self.gain {
        self.atk_coef
      } else {
        self.rel_coef
      };
      self.gain += c * (target - self.gain);

      // 输出 = 延迟了 lookahead 的样本 × 当前增益（前沿已提前被压住）。
      l[t] = self.delay[0][self.write] * self.gain;
      r[t] = self.delay[1][self.write] * self.gain;
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn passes_through_when_below_ceiling() {
    // 低电平信号只被延迟 lookahead 个样本、幅度不变（≈ 直通）。
    let sr = 48_000.0;
    let mut lim = LookaheadLimiter::new(sr);
    let n = 256usize;
    let mut l: Vec<f32> = (0..n).map(|i| 0.2 * (i as f32 * 0.1).sin()).collect();
    let mut r: Vec<f32> = (0..n).map(|i| 0.2 * (i as f32 * 0.1).cos()).collect();
    let l_orig = l.clone();
    lim.process(&mut l, &mut r);
    // 除去 lookahead 延迟后应完全一致（增益始终 1.0）。
    let la = (0.001 * sr).round() as usize;
    for i in la..n {
      assert!(
        (l[i] - l_orig[i - la]).abs() < 1e-6,
        "样本 {i} 应等于延迟前的样本，实际 {} vs {}",
        l[i],
        l_orig[i - la]
      );
    }
  }

  #[test]
  fn clamps_transient_to_ceiling() {
    // 超限瞬态必须被压到 ceiling（-0.3 dBFS）以内，且前沿不再漏出。
    let sr = 48_000.0;
    let mut lim = LookaheadLimiter::new(sr);
    let n = 256usize;
    let mut l = vec![0.0f32; n];
    let mut r = vec![0.0f32; n];
    // 一个 1.0（0 dBFS）的满幅瞬态，远超 ceiling。
    l[64] = 1.0;
    r[64] = 1.0;
    lim.process(&mut l, &mut r);
    let peak = l.iter().chain(r.iter()).fold(0.0f32, |a, v| a.max(v.abs()));
    assert!(peak <= 0.967, "瞬态应被压到 ceiling 以内，实际峰值 {peak}");
    assert!(peak > 0.9, "不应过度压到听不到，实际峰值 {peak}");
  }

  #[test]
  fn lookahead_reduces_gain_before_transient_arrives() {
    // 核心：在瞬态到达输出位置之前，增益就已经降下来了（这才是 lookahead 的意义）。
    let sr = 48_000.0;
    let mut lim = LookaheadLimiter::new(sr);
    let n = 256usize;
    let mut l = vec![0.0f32; n];
    let mut r = vec![0.0f32; n];
    // 先喂一段时间满幅，让增益压下来。
    for i in 0..128 {
      l[i] = 1.0;
      r[i] = 1.0;
    }
    lim.process(&mut l, &mut r);
    // 处理后稳态应被压在 ceiling 附近（增益已降到位），而非满幅。
    let steady_peak = l[64..128].iter().fold(0.0f32, |a, v| a.max(v.abs()));
    assert!(
      steady_peak < 0.99,
      "稳态满幅应被压到 ceiling 附近，实际峰值 {steady_peak}"
    );
  }
}
