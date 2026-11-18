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

//! 3 阶 Ambisonics（HOA3）编解码：7.1（8 方向）↔ 16 球谐分量。
//!
//! ArtIsWar V5 的「16 声道」本质是 3 阶球谐声场（`(3+1)² = 16` 个分量），
//! 不是 16 个方向。本模块提供两端：
//!
//! - [`AmbisonicsEncoder`]：7 个全频方向（7.1 去掉 LFE）→ 16 球谐分量（伪逆编码，无损往返）。
//! - [`AmbisonicsDecoder`]：16 球谐分量 → 8 个 7.1 虚拟扬声器（basic decoding）。
//!
//! ```text
//! 7.1(8ch) ──► 编码矩阵 [16×7] ──► 16 球谐分量 ──► 解码矩阵 [8×16] ──► 8 虚拟扬声器 ──► HRIR(8×2) ──► 双耳
//! ```
//!
//! ## 约定
//!
//! - **声道顺序**：ACN（Ambisonic Channel Number），`n = l² + l + m`。
//! - **归一化**：SN3D（semi-normalized，m≠0 项乘 √2）。
//! - **解码方式**：basic decoding（模式匹配）——`speaker_i = Σₙ Yₙ(dirᵢ)·Bₙ`。
//!   这是最朴素的解码，电平靠下游 `makeup_db` 补偿；进阶可换 max-rE 加权。
//! - **方位角**：本模块用「逆时针为正」的声学约定；`layout` 里的方位角是
//!   「俯视顺时针为正」（ITU 摆位），所以这里取负号转换。
//!
//! ## 为什么这是零延迟
//!
//! 解码是逐样本矩阵乘（每样本 `8×16` 次乘加），没有块、没有状态、没有卷积，
//! 因此**不引入任何额外延迟**——和上游采集周期、下游 HRIR 卷积完全解耦。

use crate::dsp::surround::layout::{Speaker, CHANNELS};

/// Ambisonics 阶数（3 阶 = 16 声道）。
pub const ORDER: usize = 3;

/// 球谐分量数：`(ORDER + 1)² = 16`。
pub const HOA_CHANNELS: usize = (ORDER + 1) * (ORDER + 1);

const SQRT2: f32 = std::f32::consts::SQRT_2;
const PI: f32 = std::f32::consts::PI;

/// 3 阶 16 个分量的 (l, m)，ACN 顺序。写死避免浮点反推阶数。
pub const ACN_LM: [(usize, i32); HOA_CHANNELS] = [
  (0, 0),
  (1, -1),
  (1, 0),
  (1, 1),
  (2, -2),
  (2, -1),
  (2, 0),
  (2, 1),
  (2, 2),
  (3, -3),
  (3, -2),
  (3, -1),
  (3, 0),
  (3, 1),
  (3, 2),
  (3, 3),
];

/// 阶乘（0! = 1! = 1）。
fn fact(n: usize) -> f32 {
  (2..=n).fold(1.0, |acc, i| acc * i as f32)
}

/// SN3D 归一化因子：`sqrt((2l+1)/(4π) · (l−m)!/(l+m)!)`。
fn sn3d_norm(l: usize, m: usize) -> f32 {
  let v = (2.0 * l as f32 + 1.0) / (4.0 * PI) * (fact(l - m) / fact(l + m));
  v.sqrt()
}

/// 带勒让德函数 `P_l^m(x)`，l ≤ 3 显式展开（不含 Condon-Shortley 相位）。
fn legendre(l: usize, m: usize, x: f32) -> f32 {
  let s = (1.0 - x * x).max(0.0).sqrt();
  match (l, m) {
    (0, 0) => 1.0,
    (1, 0) => x,
    (1, 1) => s,
    (2, 0) => 0.5 * (3.0 * x * x - 1.0),
    (2, 1) => 3.0 * x * s,
    (2, 2) => 3.0 * s * s,
    (3, 0) => 0.5 * x * (5.0 * x * x - 3.0),
    (3, 1) => 1.5 * (5.0 * x * x - 1.0) * s,
    (3, 2) => 15.0 * x * s * s,
    (3, 3) => 15.0 * s * s * s,
    _ => 0.0,
  }
}

/// 实值球谐函数（SN3D）。`elev` 仰角、`azim` 方位角（弧度，逆时针为正）。
fn sh(l: usize, m: i32, elev: f32, azim: f32) -> f32 {
  let am = m.unsigned_abs() as usize;
  let base = sn3d_norm(l, am) * legendre(l, am, elev.sin());
  match m.cmp(&0) {
    std::cmp::Ordering::Greater => base * SQRT2 * (m as f32 * azim).cos(),
    std::cmp::Ordering::Less => base * SQRT2 * (am as f32 * azim).sin(),
    std::cmp::Ordering::Equal => base,
  }
}

/// 全频带方向数：7.1 去掉 LFE = 7。
pub const FULL_COUNT: usize = CHANNELS - 1;

/// 7 个全频带方向（7.1 去掉 LFE）。LFE 无方向、只走 W 分量，编码/解码里单独处理。
pub const FULL_RANGE: [Speaker; FULL_COUNT] = [
  Speaker::Fl,
  Speaker::Fr,
  Speaker::Fc,
  Speaker::Bl,
  Speaker::Br,
  Speaker::Sl,
  Speaker::Sr,
];

/// W（全向）分量按等功率摊到 7 个全频方向的权重：`1/√7`。
fn w_scale() -> f32 {
  1.0 / ((CHANNELS - 1) as f32).sqrt()
}

/// 单个扬声器的 16 维权值（basic decoding 的一行）。
///
/// - LFE：无方向，只从 W 分量取一份（`w_scale()`），其余为 0。
/// - 全频方向：W 分量取 `w_scale()`，高阶分量取球谐值。
fn decode_weights(sp: Speaker) -> [f32; HOA_CHANNELS] {
  let mut w = [0.0f32; HOA_CHANNELS];
  if sp.is_lfe() {
    w[0] = w_scale();
    return w;
  }
  // layout 方位角是「俯视顺时针为正」，声学球谐用逆时针为正 → 取负。
  let azim = -sp.azimuth_deg().to_radians();
  for (n, &(l, m)) in ACN_LM.iter().enumerate() {
    w[n] = if n == 0 {
      w_scale()
    } else {
      sh(l, m, 0.0, azim)
    };
  }
  w
}

/// N×N 方阵求逆（Gauss-Jordan，带部分主元）。7.1 布局不会奇异，防御性处理。
fn invert_square<const N: usize>(a: &mut [[f32; N]; N]) -> [[f32; N]; N] {
  let mut inv = [[0.0f32; N]; N];
  for (i, row) in inv.iter_mut().enumerate() {
    row[i] = 1.0;
  }
  for col in 0..N {
    let mut pivot = col;
    for r in (col + 1)..N {
      if a[r][col].abs() > a[pivot][col].abs() {
        pivot = r;
      }
    }
    a.swap(col, pivot);
    inv.swap(col, pivot);
    let pv = a[col][col];
    if pv.abs() < 1e-9 {
      continue;
    }
    for j in 0..N {
      a[col][j] /= pv;
      inv[col][j] /= pv;
    }
    for r in 0..N {
      if r == col {
        continue;
      }
      let f = a[r][col];
      if f == 0.0 {
        continue;
      }
      for j in 0..N {
        a[r][j] -= f * a[col][j];
        inv[r][j] -= f * inv[col][j];
      }
    }
  }
  inv
}

/// 解码器：16 球谐分量 → 8 虚拟扬声器。
///
/// 解码矩阵 `[spk][hoa]` 在构造时算好（一次性），运行时只做矩阵乘。
/// 构造后不可变，`decode` 无内部状态，可被多块连续调用。
#[derive(Clone, Debug)]
pub struct AmbisonicsDecoder {
  /// `matrix[spk][hoa]`：第 `spk` 个虚拟扬声器对第 `hoa` 个球谐分量的权重。
  matrix: [[f32; HOA_CHANNELS]; CHANNELS],
}

impl AmbisonicsDecoder {
  /// 用 7.1 布局（`layout::Speaker` 的方位）生成 basic decoding 矩阵。
  pub fn new() -> Self {
    let mut matrix = [[0.0f32; HOA_CHANNELS]; CHANNELS];
    for sp in Speaker::ALL {
      matrix[sp.index()] = decode_weights(sp);
    }
    AmbisonicsDecoder { matrix }
  }

  /// 解码一块：`input` 是 16 路球谐系数（每路长度 = 块长），
  /// `output` 是 8 路虚拟扬声器信号。
  ///
  /// 长度不足按可用长度处理、缺的补零，**不 panic**（实时路径）。
  pub fn decode(&self, input: &[Vec<f32>], output: &mut [Vec<f32>]) {
    let n = input.first().map(|v| v.len()).unwrap_or(0);
    for (i, dst) in output.iter_mut().take(CHANNELS).enumerate() {
      let m = n.min(dst.len());
      for t in 0..m {
        let mut acc = 0.0f32;
        for (j, x) in input.iter().take(HOA_CHANNELS).enumerate() {
          let k = self.matrix[i][j];
          if k != 0.0 {
            acc += k * x[t];
          }
        }
        dst[t] = acc;
      }
      for v in dst[m..].iter_mut() {
        *v = 0.0;
      }
    }
  }

  /// 解码矩阵（测试/诊断用）。
  pub fn matrix(&self) -> &[[f32; HOA_CHANNELS]; CHANNELS] {
    &self.matrix
  }
}

impl Default for AmbisonicsDecoder {
  fn default() -> Self {
    AmbisonicsDecoder::new()
  }
}

/// 编码器：7 个全频方向（7.1 去掉 LFE）→ 16 个球谐分量。
///
/// 用**最小范数伪逆**构造编码矩阵：`E = Dᵀ (D·Dᵀ)⁻¹`，其中 `D` 是解码器对 7 个
/// 全频方向的子矩阵。这样 `D·E = I`——全频方向信号**无损往返**，不引入串扰、不
/// 劣化方向分离度；LFE 完全直通、不进球谐域。
///
/// 因此 `7.1 → 球谐 → 7.1` 是一条**恒等**变换：它不改变声音，只建立球谐中间表示，
/// 为将来在球谐域插入声场操作（旋转 / 聚焦 / 增强）或换成真正的球谐 HRTF 解码留接口。
#[derive(Clone, Debug)]
pub struct AmbisonicsEncoder {
  /// `matrix[hoa][full]`：第 `hoa` 个球谐分量对第 `full` 个全频方向的权重。
  matrix: [[f32; FULL_COUNT]; HOA_CHANNELS],
}

impl AmbisonicsEncoder {
  /// 用解码矩阵的伪逆构造编码矩阵（保证编码→解码恒等）。
  pub fn new() -> Self {
    // D[full][hoa]：7 个全频方向的解码权重。
    let mut d = [[0.0f32; HOA_CHANNELS]; FULL_COUNT];
    for (i, sp) in FULL_RANGE.iter().enumerate() {
      d[i] = decode_weights(*sp);
    }
    // G = D·Dᵀ [full][full]
    let mut g = [[0.0f32; FULL_COUNT]; FULL_COUNT];
    for i in 0..FULL_COUNT {
      for j in 0..FULL_COUNT {
        g[i][j] = d[i].iter().zip(d[j].iter()).map(|(a, b)| a * b).sum();
      }
    }
    let gi = invert_square(&mut g);
    // E = Dᵀ·G⁻¹ [hoa][full]
    let mut matrix = [[0.0f32; FULL_COUNT]; HOA_CHANNELS];
    for hoa in 0..HOA_CHANNELS {
      for i in 0..FULL_COUNT {
        let mut s = 0.0f32;
        for k in 0..FULL_COUNT {
          s += d[k][hoa] * gi[k][i];
        }
        matrix[hoa][i] = s;
      }
    }
    AmbisonicsEncoder { matrix }
  }

  /// 编码一块：`input` 是 8 路 7.1（只取 7 个全频方向，LFE 直通），
  /// `output` 是 16 路球谐系数（每路长度 = 块长）。
  ///
  /// 长度不足按可用长度处理、缺的补零，**不 panic**（实时路径）。
  pub fn encode(&self, input: &[Vec<f32>], output: &mut [Vec<f32>]) {
    let n = input.first().map(|v| v.len()).unwrap_or(0);
    for (hoa, dst) in output.iter_mut().take(HOA_CHANNELS).enumerate() {
      let m = n.min(dst.len());
      for t in 0..m {
        let mut acc = 0.0f32;
        for (i, sp) in FULL_RANGE.iter().enumerate() {
          let k = self.matrix[hoa][i];
          if k != 0.0 && sp.index() < input.len() {
            acc += k * input[sp.index()][t];
          }
        }
        dst[t] = acc;
      }
      for v in dst[m..].iter_mut() {
        *v = 0.0;
      }
    }
  }

  /// 编码矩阵（测试/诊断用）。
  pub fn matrix(&self) -> &[[f32; FULL_COUNT]; HOA_CHANNELS] {
    &self.matrix
  }
}

impl Default for AmbisonicsEncoder {
  fn default() -> Self {
    AmbisonicsEncoder::new()
  }
}

/// 声场聚焦矩阵：球谐域 16×16 **对角**矩阵，按阶 `l` 施加权重。
///
/// 这是 Ambisonics 里「声像锐利度 / 空间展开度」的标准控制（对应 ATK 的聚焦手感）：
///
/// - `focus = 1.0`（默认）：各阶全开，定位最锐利，等价于不做聚焦。
/// - `focus = 0.0`：只保留 W（全向）分量，无方向信息，最「扩散 / 模糊」。
/// - 中间值：高阶分量（`l ≥ 1`，共 15 个）按 `focus` 线性衰减，W 恒定。
///
/// W 恒定是为了让整体能量（含并入 W 的 LFE 低频）不随聚焦度起伏——旋钮只改
/// 「锐利度」，不改「响度」。逐样本对角缩放，零块、零状态、零延迟。
#[derive(Clone, Debug)]
pub struct Focus {
  /// 对角权重 `matrix[n][n] = w_l`（第 n 个分量的阶为 `ACN_LM[n].0`）。
  matrix: [[f32; HOA_CHANNELS]; HOA_CHANNELS],
}

impl Focus {
  /// 按 `focus ∈ [0,1]` 构造阶权重矩阵。
  pub fn new(focus: f32) -> Focus {
    let f = focus.clamp(0.0, 1.0);
    let mut matrix = [[0.0f32; HOA_CHANNELS]; HOA_CHANNELS];
    for (n, &(l, _)) in ACN_LM.iter().enumerate() {
      matrix[n][n] = if l == 0 { 1.0 } else { f };
    }
    Focus { matrix }
  }

  /// 把聚焦权重就地作用到 16 路球谐分量上。
  ///
  /// 长度不足按可用长度处理、缺的补零，**不 panic**（实时路径）。
  pub fn apply(&self, hoa: &mut [Vec<f32>]) {
    for (n, ch) in hoa.iter_mut().take(HOA_CHANNELS).enumerate() {
      let w = self.matrix[n][n];
      if w == 1.0 {
        continue; // W 分量（以及 focus=1 时的全部分量）原样保留
      }
      for v in ch.iter_mut() {
        *v *= w;
      }
    }
  }
}

impl Default for Focus {
  fn default() -> Self {
    Focus::new(1.0)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn focus_attenuates_high_orders_but_keeps_w() {
    // focus=1 全开（恒等）；focus=0 只剩 W；focus=0.5 高阶减半。
    let mut hoa = vec![vec![1.0f32; 4]; HOA_CHANNELS];

    Focus::new(1.0).apply(&mut hoa);
    for ch in &hoa {
      assert!(ch.iter().all(|v| (*v - 1.0).abs() < 1e-6));
    }

    let mut hoa = vec![vec![1.0f32; 4]; HOA_CHANNELS];
    Focus::new(0.0).apply(&mut hoa);
    assert!(hoa[0].iter().all(|v| (*v - 1.0).abs() < 1e-6), "W 应保留");
    for (n, ch) in hoa.iter().enumerate().skip(1) {
      assert!(ch.iter().all(|v| v.abs() < 1e-6), "高阶分量 {n} 应被清零");
    }

    let mut hoa = vec![vec![1.0f32; 4]; HOA_CHANNELS];
    Focus::new(0.5).apply(&mut hoa);
    assert!(hoa[0].iter().all(|v| (*v - 1.0).abs() < 1e-6));
    assert!(hoa[3].iter().all(|v| (*v - 0.5).abs() < 1e-6));
  }

  #[test]
  fn focus_clamps_to_unit_interval() {
    let mut hoa = vec![vec![1.0f32; 2]; HOA_CHANNELS];
    Focus::new(2.0).apply(&mut hoa); // 越界 → 夹到 1.0，全开
    assert!(hoa[3].iter().all(|v| (*v - 1.0).abs() < 1e-6));

    let mut hoa = vec![vec![1.0f32; 2]; HOA_CHANNELS];
    Focus::new(-1.0).apply(&mut hoa); // 越界 → 夹到 0.0，只剩 W
    assert!(hoa[3].iter().all(|v| v.abs() < 1e-6));
  }

  #[test]
  fn hoa_channels_is_16_for_third_order() {
    assert_eq!(ORDER, 3);
    assert_eq!(HOA_CHANNELS, 16);
    assert_eq!(ACN_LM.len(), 16);
  }

  #[test]
  fn acn_indexing_is_standard() {
    // 头几个分量必须符合 ACN：0=W, 1=Y, 2=Z, 3=X, 4=V
    assert_eq!(ACN_LM[0], (0, 0));
    assert_eq!(ACN_LM[1], (1, -1));
    assert_eq!(ACN_LM[2], (1, 0));
    assert_eq!(ACN_LM[3], (1, 1));
    assert_eq!(ACN_LM[4], (2, -2));
    assert_eq!(ACN_LM[15], (3, 3));
  }

  #[test]
  fn w_component_reaches_all_speakers_equally() {
    // 全向 W 分量：所有方向扬声器（含 LFE）应收到相同能量。
    let dec = AmbisonicsDecoder::new();
    let mut input: Vec<Vec<f32>> = vec![vec![0.0; 8]; HOA_CHANNELS];
    input[0] = vec![1.0; 8];
    let mut out: Vec<Vec<f32>> = vec![vec![0.0; 8]; CHANNELS];
    dec.decode(&input, &mut out);

    for sp in Speaker::ALL {
      let v = out[sp.index()][0];
      assert!(
        (v - 1.0 / 7.0f32.sqrt()).abs() < 1e-5,
        "{} 收到 {v}",
        sp.id()
      );
    }
  }

  #[test]
  fn x_component_is_front_back_opposed() {
    // X 分量（第 3 号，前）：前方扬声器应为正，后方为负（反相）。
    let dec = AmbisonicsDecoder::new();
    let mut input: Vec<Vec<f32>> = vec![vec![0.0; 8]; HOA_CHANNELS];
    input[3] = vec![1.0; 8];
    let mut out: Vec<Vec<f32>> = vec![vec![0.0; 8]; CHANNELS];
    dec.decode(&input, &mut out);

    let fc = out[Speaker::Fc.index()][0];
    let bl = out[Speaker::Bl.index()][0];
    let br = out[Speaker::Br.index()][0];
    assert!(fc > 0.1, "正前方应为正，实际 {fc}");
    assert!(bl < -0.1, "正后方应为负，实际 {bl}");
    assert!(br < -0.1, "正后方应为负，实际 {br}");
  }

  #[test]
  fn y_component_is_left_right_opposed() {
    // Y 分量（第 1 号，左）：左方为正、右方为负。
    let dec = AmbisonicsDecoder::new();
    let mut input: Vec<Vec<f32>> = vec![vec![0.0; 8]; HOA_CHANNELS];
    input[1] = vec![1.0; 8];
    let mut out: Vec<Vec<f32>> = vec![vec![0.0; 8]; CHANNELS];
    dec.decode(&input, &mut out);

    let sl = out[Speaker::Sl.index()][0];
    let sr = out[Speaker::Sr.index()][0];
    assert!(sl > 0.1, "正左应为正，实际 {sl}");
    assert!(sr < -0.1, "正右应为负，实际 {sr}");
  }

  #[test]
  fn lfe_only_taps_w_component() {
    // LFE 只从 W 取，其余分量不得泄漏进去。
    let dec = AmbisonicsDecoder::new();
    for n in 1..HOA_CHANNELS {
      assert_eq!(
        dec.matrix()[Speaker::Lfe.index()][n],
        0.0,
        "分量 {n} 漏进 LFE"
      );
    }
    assert!(dec.matrix()[Speaker::Lfe.index()][0] > 0.0);
  }

  #[test]
  fn short_input_is_zero_padded() {
    // 输入只有 1 路时，其余分量按 0 处理，不 panic。
    let dec = AmbisonicsDecoder::new();
    let input: Vec<Vec<f32>> = vec![vec![1.0; 4]];
    let mut out: Vec<Vec<f32>> = vec![vec![0.0; 4]; CHANNELS];
    dec.decode(&input, &mut out);
    assert!((out[Speaker::Fc.index()][0] - 1.0 / 7.0f32.sqrt()).abs() < 1e-5);
  }

  #[test]
  fn encoder_inverts_decoder_for_full_range() {
    // 编码矩阵 E 必须是解码矩阵 D（7 全频方向子块）的伪逆：D·E = I。
    let enc = AmbisonicsEncoder::new();
    let dec = AmbisonicsDecoder::new();
    let dm = dec.matrix();
    let em = enc.matrix();
    for (i, sp_i) in FULL_RANGE.iter().enumerate() {
      for (j, _) in FULL_RANGE.iter().enumerate() {
        let s: f32 = dm[sp_i.index()]
          .iter()
          .zip(em.iter())
          .map(|(a, col)| a * col[j])
          .sum();
        let want = if i == j { 1.0 } else { 0.0 };
        assert!(
          (s - want).abs() < 1e-4,
          "D·E[{i}][{j}] = {s}（期望 {want}）"
        );
      }
    }
  }

  #[test]
  fn full_range_roundtrip_is_identity() {
    // 单方向信号走「7.1 → 球谐编码 → 解码」后必须无损恢复，其它方向不串扰。
    let enc = AmbisonicsEncoder::new();
    let dec = AmbisonicsDecoder::new();
    // 输入 8 路 7.1：只有 FL 有信号，其余（含 LFE）为 0。
    let mut input: Vec<Vec<f32>> = vec![vec![0.0f32; 4]; CHANNELS];
    input[Speaker::Fl.index()] = vec![1.0, 2.0, -1.0, 0.5];
    let mut hoa = vec![vec![0.0f32; 4]; HOA_CHANNELS];
    enc.encode(&input, &mut hoa);
    let mut out = vec![vec![0.0f32; 4]; CHANNELS];
    dec.decode(&hoa, &mut out);
    // LFE 直通（与引擎一致）：不进球谐域，避免被 W 分量摊薄。
    out[Speaker::Lfe.index()].copy_from_slice(&input[Speaker::Lfe.index()]);

    for sp in Speaker::ALL {
      let want = if sp == Speaker::Fl {
        vec![1.0, 2.0, -1.0, 0.5]
      } else {
        vec![0.0; 4]
      };
      let got = &out[sp.index()];
      for t in 0..4 {
        assert!(
          (got[t] - want[t]).abs() < 1e-3,
          "{} t={t}: 得 {} 期望 {}",
          sp.id(),
          got[t],
          want[t]
        );
      }
    }
  }
}
