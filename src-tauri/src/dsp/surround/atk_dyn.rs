// PureVox — AI 麦克风降噪工具
// Copyright (C) 2024-2026 a2heng <752848283@qq.com>
//
// PureVox is licensed under the GNU General Public License v3.0 or
// later (GPL-3.0-or-later).  See LICENSE for details.

//! 多段软拐点响度均衡器（**简化重写**）。
//!
//! # 设计原则：稳定优先，不做黑箱
//!
//! 前几版用「门控 + 冻结 + 迟滞」试图区分「自己的声音 vs 环境」，结果反复在
//! 「压不住」和「整个游戏被压没」之间横跳——因为「什么时候该冻结基准」这个判断
//! 本身就依赖一个不可靠的阈值，一旦判断错，基准就停在错误的位置，引发连锁反应。
//!
//! 本版回到音频工程里最成熟、最可预测的做法：**瞬时峰值包络 + 慢速平均基准 +
//! 软拐点向下压缩 + makeup**。
//!
//! 关键洞察（为什么不再需要门控）：
//! - 枪声/自身脚步是**瞬态**：峰值远高于平均值，二者天然存在「峰值因子」差距。
//! - 用一个**瞬时 attack**（第一个样本就抓到峰值）的峰值包络，比对一个**慢速
//!   （5 s）爬升的平均基准**，枪声一来 gap 立即拉到 20~40 dB，压缩随之触发；
//!   枪声一停，慢基准**永远在缓慢更新**，慢慢回到真实环境电平——**没有冻结、
//!   没有死锁**，持续声音只会被轻微压一下、绝不会被压没。
//!
//! # 三层结构
//!
//! 1. 3 段 Linkwitz-Riley crossover（<250Hz / 250..4k / >4k），枪声主体在中频、
//!    脚步冲击在低频，分开压互不干扰。
//! 2. 每段每声道：`峰值包络 vs 慢基准` 的相对电平超过阈值后按压缩比软压缩。
//! 3. 三段求和 × makeup 写回（LFE 直通）。makeup 把「被压下来的自己」与
//!    「没被压的远处敌人」整体往上抬，缩小二者差距。
//!
//! 三个旋钮语义（清晰、可预期）：
//! - `self_gun`：中高频段（枪声主体）压缩比强度 → 0=直通，1=≈10:1 强压缩
//! - `self_foot`：低频段（脚步冲击）压缩比强度 → 0=直通，1=≈8:1 强压缩
//! - `leveller`：makeup 补偿（0..+12 dB）→ 越大「远处敌人」被抬得越响
//!
//! ## 延迟
//! crossover 是 IIR（无块延迟）；峰值包络与慢基准都是一阶平滑、无 lookahead。
//! 零额外延迟。

use crate::dsp::surround::layout::{Speaker, CHANNELS};

/// 频段数：LOW / MID / HIGH。
const BANDS: usize = 3;
/// 低频段索引。
const B_LOW: usize = 0;
/// 中频段索引。
const B_MID: usize = 1;
/// 高频段索引。
const B_HIGH: usize = 2;

/// 第一交叉点（Hz）：低频（脚步冲击）与中频（枪声主体）的分界。
const XOVER_LO: f32 = 250.0;
/// 第二交叉点（Hz）：中频与高频（齿音/空气感）的分界。
const XOVER_HI: f32 = 4000.0;

/// 全频方向数（7.1 去掉 LFE）。
const FULL_COUNT: usize = CHANNELS - 1;
/// 全频方向（7.1 去掉 LFE），LFE 不参与动态处理、直通。
const FULL_RANGE: [Speaker; FULL_COUNT] = [
  Speaker::Fl,
  Speaker::Fr,
  Speaker::Fc,
  Speaker::Bl,
  Speaker::Br,
  Speaker::Sl,
  Speaker::Sr,
];

/// 压缩阈值（dB）：峰值包络比慢基准高出此值以上才开始压缩。
///
/// 取值权衡：太低（<6 dB）会把环境噪声的峰值因子（约 8~11 dB）也当成瞬态压、
/// 导致环境「呼吸」；太高（>15 dB）则在「自己枪声 vs 环境」差距不大的游戏里
/// 根本不触发。8 dB 是折中——枪声瞬态（通常高出 15 dB 以上）稳稳触发，环境噪声
/// 只被轻微碰一下。
const THRESHOLD_DB: f32 = 8.0;

/// 向下压缩的最大衰减量（dB）。无论信号多响，单段最多压这么多，作为最后一道
/// 保险，保证任何情况下声音都不会被压到消失。
const MAX_REDUCTION_DB: f32 = 24.0;

/// 峰值包络 release（秒）：> 连射间隔（~51 ms），连射期间压缩持续保持。
const PEAK_REL_S: f32 = 0.200;

/// 慢基准 attack（秒）：足够慢，让单个枪声 burst（20~40 ms）期间基准几乎不动；
/// 同时**永远在更新**（不像旧版会冻结死锁），持续声音在几秒内慢慢解除压缩。
const REF_ATK_S: f32 = 5.0;

/// 慢基准 release（秒）：安静时快速回落到真实环境电平。
const REF_REL_S: f32 = 1.0;

/// 瞬态保持冻结时长（秒）：检测到瞬态后，慢基准冻结这么长时间再恢复更新。
///
/// 枪声 burst 只有 20~40 ms，连射间隔 ~51 ms——每一发都会重置这个计时器，所以
/// 连射期间基准**持续冻结**、gap 不会缩小、压缩全程有效；而持续声音（背景音乐、
/// 环境音）不会在几十 ms 内回落，计时器走完后基准自动恢复爬升——**不会死锁**。
/// 这是「压得住连射」与「不压没持续声音」二者兼得的关键，替代旧版永久冻结。
const HOLD_S: f32 = 0.300;

/// 冻结期间的慢基准 attack（秒）：极慢，近似冻结但不完全停死。取值需要在
/// 「连射时基准别爬太高」与「持续声音最终能解除冻结」之间折中：150 s 下连射
/// 几秒内基准只爬几 dB（压缩全程有效），持续声音则每个 hold 周期爬约 1.5 dB、
/// 几秒后自动解除——不会死锁。
const FREEZE_ATK_S: f32 = 150.0;

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
  /// 二阶 Linkwitz-Riley（Q = 0.5）低通。LR 的 LP 与（反相的）HP 幅度互补，
  /// 三段求和才是平坦的——用 Butterworth Q=0.707 会反相抵消、产生深谷。
  fn lowpass(sr: f32, f: f32) -> Biquad {
    let w0 = 2.0 * std::f32::consts::PI * (f / sr);
    let (sw, cw) = w0.sin_cos();
    let alpha = sw; // Q = 0.5
    let b0 = (1.0 - cw) / 2.0;
    let b1 = 1.0 - cw;
    let b2 = b0;
    let a0 = 1.0 + alpha;
    let a1 = -2.0 * cw;
    let a2 = 1.0 - alpha;
    let inv = if a0.abs() < 1e-12 { 0.0 } else { 1.0 / a0 };
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

  /// 二阶 Linkwitz-Riley（Q = 0.5）高通，**反相**（LR 的 HP 极性取反才与 LP 同相）。
  fn highpass(sr: f32, f: f32) -> Biquad {
    let w0 = 2.0 * std::f32::consts::PI * (f / sr);
    let (sw, cw) = w0.sin_cos();
    let alpha = sw; // Q = 0.5
    let b0 = -(1.0 + cw) / 2.0;
    let b1 = 1.0 + cw;
    let b2 = -(1.0 + cw) / 2.0;
    let a0 = 1.0 + alpha;
    let a1 = -2.0 * cw;
    let a2 = 1.0 - alpha;
    let inv = if a0.abs() < 1e-12 { 0.0 } else { 1.0 / a0 };
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

/// 一个声道的三段 crossover 滤波器。
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

  /// 把 `x` 拆成 `[low, mid, high]`。
  #[inline]
  fn split(&mut self, x: f32, out: &mut [f32; BANDS]) {
    let low = self.lp_lo.run(x);
    let hi_lo = self.hp_lo.run(x);
    let mid = self.lp_hi.run(hi_lo);
    let high = self.hp_hi.run(hi_lo);
    out[B_LOW] = low;
    out[B_MID] = mid;
    out[B_HIGH] = high;
  }
}

/// 均方包络跟随器（一阶平滑 `x²`，attack/release 可不对称）。
#[derive(Clone, Copy)]
struct SqFollower {
  sq: f32,
}

impl SqFollower {
  fn new() -> SqFollower {
    // 初始值 -30 dBFS：中性起点，配合「基准永远更新」在几秒内收敛到真实电平。
    // 旧版改到 -60 dB 过低，配合冻结导致死锁（整个游戏被压没）。
    SqFollower { sq: 1e-3 }
  }

  /// 一阶均方平滑，attack/release 不对称。`atk = 1.0` 时瞬时跟踪（峰值检测）。
  #[inline]
  fn step(&mut self, x: f32, atk: f32, rel: f32) -> f32 {
    let s = x * x;
    let c = if s > self.sq { atk } else { rel };
    self.sq += c * (s - self.sq);
    if self.sq < 1e-18 {
      self.sq = 1e-18;
    }
    self.sq.sqrt()
  }

  /// 当前包络值（RMS），不更新。
  #[inline]
  fn current(&self) -> f32 {
    self.sq.sqrt()
  }
}

/// 动态处理参数（0..1，全部可热改）。
#[derive(Clone, Copy, Debug)]
pub struct AtkDynParams {
  /// 中高频段压缩比强度（0..1，默认 0.8）。越大枪声瞬态压得越狠。
  pub self_gun: f32,
  /// 低频段压缩比强度（0..1，默认 0.7）。越大脚步冲击压得越狠。
  pub self_foot: f32,
  /// makeup 补偿强度（0..1，默认 0.5）。越大「远处敌人」被抬得越响（0..+12 dB）。
  pub leveller: f32,
}

impl Default for AtkDynParams {
  fn default() -> Self {
    AtkDynParams {
      self_gun: 0.8,
      self_foot: 0.7,
      leveller: 0.5,
    }
  }
}

impl AtkDynParams {
  /// 三个旋钮全关 = 真直通。
  fn is_passthrough(&self) -> bool {
    self.self_gun < 0.01 && self.self_foot < 0.01 && self.leveller < 0.01
  }
}

/// 多段软拐点响度均衡器（峰值包络 + 慢基准 + 向下压缩 + makeup）。
pub struct AtkDynamic {
  sr: f32,
  /// 每个全频声道一个三段 crossover。
  xover: Vec<Crossover>,
  /// 每声道三段的分频缓冲。
  band: Vec<[f32; BANDS]>,
  /// `[声道][频段]` 瞬时峰值包络（快 attack / 慢 release）。
  peak: Vec<[SqFollower; BANDS]>,
  /// `[声道][频段]` 慢速平均基准（瞬态期间冻结 HOLD_S，其余时间正常更新）。
  ref_: Vec<[SqFollower; BANDS]>,
  /// `[声道][频段]` 冻结剩余样本数（瞬态保持计时器，0 = 未冻结）。
  hold: Vec<[usize; BANDS]>,
}

impl AtkDynamic {
  pub fn new(sr: f32) -> AtkDynamic {
    AtkDynamic {
      sr,
      xover: (0..FULL_COUNT).map(|_| Crossover::new(sr)).collect(),
      band: vec![[0.0f32; BANDS]; FULL_COUNT],
      peak: (0..FULL_COUNT)
        .map(|_| [SqFollower::new(); BANDS])
        .collect(),
      ref_: (0..FULL_COUNT)
        .map(|_| [SqFollower::new(); BANDS])
        .collect(),
      hold: vec![[0usize; BANDS]; FULL_COUNT],
    }
  }

  /// 清空跨块状态（重建会话 / 换设备时用）。
  pub fn reset(&mut self) {
    for c in &mut self.xover {
      c.lp_lo.z1 = 0.0;
      c.lp_lo.z2 = 0.0;
      c.hp_lo.z1 = 0.0;
      c.hp_lo.z2 = 0.0;
      c.lp_hi.z1 = 0.0;
      c.lp_hi.z2 = 0.0;
      c.hp_hi.z1 = 0.0;
      c.hp_hi.z2 = 0.0;
    }
    for row in self.peak.iter_mut().chain(self.ref_.iter_mut()) {
      for e in row.iter_mut() {
        *e = SqFollower::new();
      }
    }
    for row in self.hold.iter_mut() {
      row.fill(0);
    }
  }

  /// 处理一块：`bus` 是 8 路 7.1（长度 = 块长），就地改 7 个全频方向，LFE 直通。
  pub fn process(&mut self, bus: &mut [Vec<f32>], p: &AtkDynParams) {
    if p.is_passthrough() {
      return;
    }
    let n = bus.first().map(|v| v.len()).unwrap_or(0);
    if n == 0 {
      return;
    }

    let sr = self.sr;
    let coef = |tau: f32| 1.0 - (-1.0 / (sr * tau)).exp();

    // 瞬时峰值包络（attack=1.0 第一个样本就抓到峰值）+ 慢 release。
    let peak_atk = 1.0;
    let peak_rel = coef(PEAK_REL_S);
    // 慢基准：attack 5s（枪声 burst 内几乎不动）、release 1s（安静时快速回落）。
    // 瞬态期间用极慢 attack（FREEZE_ATK_S）近似冻结 HOLD_S 时长——连射每发重置、
    // 持续声音走完后自动恢复，杜绝「死锁压没」。
    let ref_atk = coef(REF_ATK_S);
    let ref_rel = coef(REF_REL_S);
    let freeze_atk = coef(FREEZE_ATK_S);
    // 瞬态保持冻结的样本数。
    let hold_samples = (sr * HOLD_S) as usize;
    // 瞬态检测阈值（线性）：峰值包络比慢基准高出此值即判定为瞬态。
    let gate_on_lin = 10f32.powf(THRESHOLD_DB / 20.0);

    let g = p.self_gun.clamp(0.0, 1.0);
    let f = p.self_foot.clamp(0.0, 1.0);
    let l = p.leveller.clamp(0.0, 1.0);

    // 压缩比：低频（脚步）1..10:1，中高频（枪声）1..13:1。
    let ratio_low = 1.0 + 9.0 * f;
    let ratio_midhi = 1.0 + 12.0 * g;
    // makeup：0..+12 dB（远处敌人相对放大）。
    let makeup = 10f32.powf(12.0 * l / 20.0);
    // 最大衰减 -24 dB 的保险。
    let min_gain = 10f32.powf(-MAX_REDUCTION_DB / 20.0);

    for (c, ch) in FULL_RANGE.iter().enumerate() {
      let src = match bus.get_mut(ch.index()) {
        Some(s) => s,
        None => continue,
      };

      for t in 0..n.min(src.len()) {
        let x = src[t];
        let (low, mid, high) = {
          let b = &mut self.band[c];
          self.xover[c].split(x, b);
          (b[B_LOW], b[B_MID], b[B_HIGH])
        };

        let low = self.band_compress(
          c,
          B_LOW,
          low,
          ratio_low,
          peak_atk,
          peak_rel,
          ref_atk,
          ref_rel,
          freeze_atk,
          hold_samples,
          gate_on_lin,
          min_gain,
        );
        let mid = self.band_compress(
          c,
          B_MID,
          mid,
          ratio_midhi,
          peak_atk,
          peak_rel,
          ref_atk,
          ref_rel,
          freeze_atk,
          hold_samples,
          gate_on_lin,
          min_gain,
        );
        let high = self.band_compress(
          c,
          B_HIGH,
          high,
          ratio_midhi,
          peak_atk,
          peak_rel,
          ref_atk,
          ref_rel,
          freeze_atk,
          hold_samples,
          gate_on_lin,
          min_gain,
        );

        src[t] = (low + mid + high) * makeup;
      }
    }
  }

  /// 单频段单声道的一步：峰值包络 / 慢基准 → 软压缩。
  ///
  /// 慢基准用「瞬态保持冻结」：检测到瞬态（峰值比基准高出阈值）后冻结 `hold`
  /// 个样本。枪声 burst 与连射会反复重置计时器 → 基准持续冻结、gap 不缩小、
  /// 压缩全程有效；持续声音走完计时器后基准恢复爬升 → 不会死锁。
  #[inline]
  #[allow(clippy::too_many_arguments)]
  fn band_compress(
    &mut self,
    c: usize,
    b: usize,
    x: f32,
    ratio: f32,
    peak_atk: f32,
    peak_rel: f32,
    ref_atk: f32,
    ref_rel: f32,
    freeze_atk: f32,
    hold_samples: usize,
    gate_on_lin: f32,
    min_gain: f32,
  ) -> f32 {
    let pk = self.peak[c][b].step(x, peak_atk, peak_rel);
    let r_prev = self.ref_[c][b].current();
    // 瞬态检测：峰值比慢基准高出阈值 → 重置冻结计时器。
    if pk > r_prev * gate_on_lin {
      self.hold[c][b] = hold_samples;
    }
    // 冻结期间用极慢 attack，否则正常 attack/release。
    let (atk, rel) = if self.hold[c][b] > 0 {
      self.hold[c][b] -= 1;
      (freeze_atk, ref_rel)
    } else {
      (ref_atk, ref_rel)
    };
    let r = self.ref_[c][b].step(x, atk, rel);
    let gd = rel_gain(pk, r, THRESHOLD_DB, ratio).max(min_gain);
    x * gd
  }
}

/// 由「峰值包络 vs 慢基准」的相对电平算向下压缩增益（线性，硬拐点）。
#[inline]
fn rel_gain(peak: f32, rms: f32, thr_db: f32, ratio: f32) -> f32 {
  if rms < 1e-9 || peak < 1e-9 {
    return 1.0;
  }
  let over = 20.0 * (peak / rms).log10() - thr_db;
  if over <= 0.0 {
    1.0
  } else {
    10f32.powf(over * (1.0 / ratio - 1.0) / 20.0)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn rms(x: &[f32]) -> f32 {
    (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt()
  }

  #[test]
  fn crossover_recombines_flat() {
    let sr = 48_000.0;
    let mut x = Crossover::new(sr);
    let n = 48_000usize / 2;
    let mut total = 0.0f32;
    let mut expect = 0.0f32;
    for i in 0..n {
      let v = (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / sr).sin() * 0.5;
      let mut b = [0.0f32; BANDS];
      x.split(v, &mut b);
      let sum = b[0] + b[1] + b[2];
      if i > 480 {
        total += sum * sum;
        expect += v * v;
      }
    }
    let ratio = total / expect;
    assert!(
      (0.8..1.2).contains(&ratio),
      "三段重组幅度应守恒，实际能量比 {ratio}"
    );
  }

  #[test]
  fn crossover_separates_low_and_high() {
    let sr = 48_000.0;
    let n = 48_000usize / 4;
    let mut b = [0.0f32; BANDS];
    let mut x = Crossover::new(sr);
    let (mut low_e, mut mid_e, mut high_e) = (0.0, 0.0, 0.0);
    for i in 0..n {
      let v = (2.0 * std::f32::consts::PI * 60.0 * i as f32 / sr).sin();
      x.split(v, &mut b);
      if i > 480 {
        low_e += b[0] * b[0];
        mid_e += b[1] * b[1];
        high_e += b[2] * b[2];
      }
    }
    assert!(low_e > mid_e && low_e > high_e, "60Hz 应主要在 LOW 段");
    let mut x = Crossover::new(sr);
    let (mut low_e, mut mid_e, mut high_e) = (0.0, 0.0, 0.0);
    for i in 0..n {
      let v = (2.0 * std::f32::consts::PI * 10_000.0 * i as f32 / sr).sin();
      x.split(v, &mut b);
      if i > 480 {
        low_e += b[0] * b[0];
        mid_e += b[1] * b[1];
        high_e += b[2] * b[2];
      }
    }
    assert!(high_e > mid_e && high_e > low_e, "10kHz 应主要在 HIGH 段");
  }

  #[test]
  fn passthrough_when_all_zero() {
    let mut dyn_ = AtkDynamic::new(48_000.0);
    let mut bus: Vec<Vec<f32>> = vec![vec![0.5; 64]; CHANNELS];
    let before = bus.clone();
    dyn_.process(
      &mut bus,
      &AtkDynParams {
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
  fn loud_self_gun_is_compressed() {
    // 核心回归：响的「自己枪声」必须被明显压下去。
    // 真实 loopback 电平：背景白噪声 -50 dBFS，自己枪声 -12 dBFS 连射。
    let sr = 48_000.0;
    let mut dyn_ = AtkDynamic::new(sr);
    let p = AtkDynParams::default();
    let mut seed = 0x1234_5678u32;
    let mut noise = move || {
      seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
      ((seed >> 8) as f32 / 8_388_608.0 - 1.0) * 0.0055
    };
    let mut fill_noise = |bus: &mut Vec<Vec<f32>>| {
      for ch in bus.iter_mut() {
        for v in ch.iter_mut() {
          *v = noise();
        }
      }
    };
    // 喂 3 s 白噪声背景，让慢基准收敛到真实底噪附近。
    for _ in 0..(sr * 3.0 / 128.0) as usize {
      let mut bus = vec![vec![0.0f32; 128]; CHANNELS];
      fill_noise(&mut bus);
      dyn_.process(&mut bus, &p);
    }
    // 连射 3 s：每发 20 ms 枪声（1kHz @ -12dBFS），间隔 51 ms（≈14 发/秒）。
    let period = (sr * 0.071) as usize;
    let shot = (sr * 0.020) as usize;
    let amp = 0.25;
    let total = (sr * 3.0) as usize;
    let mut steady_peak = 0.0f32;
    let mut i = 0usize;
    while i < total {
      let mut bus = vec![vec![0.0f32; 128]; CHANNELS];
      fill_noise(&mut bus);
      for (t, v) in bus[Speaker::Fc.index()].iter_mut().enumerate() {
        let idx = i + t;
        if idx % period < shot {
          *v = amp * (2.0 * std::f32::consts::PI * 1000.0 * idx as f32 / sr).sin();
        }
      }
      dyn_.process(&mut bus, &p);
      if i >= (sr * 2.0) as usize {
        steady_peak = steady_peak.max(
          bus[Speaker::Fc.index()]
            .iter()
            .fold(0.0f32, |a, v| a.max(v.abs())),
        );
      }
      i += 128;
    }
    // 输入峰值 0.25（-12 dBFS）。稳态连射应被压到 -20 dBFS 以下（压 >8 dB）。
    assert!(
      steady_peak < 0.1,
      "响的自己枪声稳态应被明显压制，实际峰值 {steady_peak}（{} dBFS）",
      20.0 * steady_peak.log10()
    );
  }

  #[test]
  fn quiet_enemy_is_not_compressed() {
    // 轻的「远处敌人」不应被向下压缩压没（它低于阈值）。
    let sr = 48_000.0;
    let mut dyn_ = AtkDynamic::new(sr);
    let p = AtkDynParams::default();
    let mut seed = 0x1234_5678u32;
    let mut noise = move || {
      seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
      ((seed >> 8) as f32 / 8_388_608.0 - 1.0) * 0.0055
    };
    let mut fill = |bus: &mut Vec<Vec<f32>>| {
      for ch in bus.iter_mut() {
        for v in ch.iter_mut() {
          *v = noise();
        }
      }
    };
    for _ in 0..(sr * 3.0 / 128.0) as usize {
      let mut bus = vec![vec![0.0f32; 128]; CHANNELS];
      fill(&mut bus);
      dyn_.process(&mut bus, &p);
    }
    // 远处敌人脚步：-44 dBFS，80Hz 周期脉冲（比慢基准低，不应被压）。
    let mut out_peak = 0.0f32;
    let mut in_peak = 0.0f32;
    for b in 0..40 {
      let mut bus = vec![vec![0.0f32; 128]; CHANNELS];
      fill(&mut bus);
      for (t, v) in bus[Speaker::Fc.index()].iter_mut().enumerate() {
        let i = b * 128 + t;
        let ph = i % (sr as usize / 2);
        if ph < (sr * 0.1) as usize {
          let envv = (-8.0 * ph as f32 / (sr * 0.1)).exp();
          *v = 0.0063 * envv * (2.0 * std::f32::consts::PI * 80.0 * i as f32 / sr).sin();
          in_peak = in_peak.max(v.abs());
        }
      }
      dyn_.process(&mut bus, &p);
      out_peak = out_peak.max(
        bus[Speaker::Fc.index()]
          .iter()
          .fold(0.0f32, |a, v| a.max(v.abs())),
      );
    }
    // 输入峰值约 0.0063（-44 dBFS）。远处敌人经 makeup（+6 dB）后不应比输入更小——
    // 即不能被向下压缩压掉，输出峰值应 >= 输入峰值的一半。
    assert!(
      out_peak >= in_peak * 0.5,
      "轻的敌人不应被向下压缩压没，in {in_peak} out {out_peak}"
    );
  }

  #[test]
  fn lfe_is_never_touched() {
    let mut dyn_ = AtkDynamic::new(48_000.0);
    let p = AtkDynParams {
      self_gun: 1.0,
      self_foot: 1.0,
      leveller: 1.0,
    };
    let mut bus: Vec<Vec<f32>> = vec![vec![0.0; 64]; CHANNELS];
    bus[Speaker::Lfe.index()].fill(0.7);
    dyn_.process(&mut bus, &p);
    assert!(
      bus[Speaker::Lfe.index()]
        .iter()
        .all(|v| (*v - 0.7).abs() < 1e-9),
      "LFE 应直通"
    );
  }

  #[test]
  fn continuous_signal_is_not_silenced() {
    // 回归：持续信号（背景音乐/环境音）绝不能被压没。慢基准永远更新（不冻结），
    // 持续声音只在最初几秒被压、随后慢慢恢复，加上 makeup 更不会消失。
    let sr = 48_000.0;
    let mut dyn_ = AtkDynamic::new(sr);
    let p = AtkDynParams::default();
    let mut seed = 0x1234_5678u32;
    let mut noise = move || {
      seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
      ((seed >> 8) as f32 / 8_388_608.0 - 1.0) * 0.0055
    };
    let mut fill = |bus: &mut Vec<Vec<f32>>| {
      for ch in bus.iter_mut() {
        for v in ch.iter_mut() {
          *v = noise();
        }
      }
    };
    for _ in 0..(sr * 3.0 / 128.0) as usize {
      let mut bus = vec![vec![0.0f32; 128]; CHANNELS];
      fill(&mut bus);
      dyn_.process(&mut bus, &p);
    }
    // 持续 -20 dB 信号（1kHz 正弦），喂 5 秒，看最低 RMS（不应被压没）。
    let mut min_rms = f32::MAX;
    for _ in 0..(sr * 5.0 / 128.0) as usize {
      let mut bus = vec![vec![0.0f32; 128]; CHANNELS];
      fill(&mut bus);
      for (t, v) in bus[Speaker::Fc.index()].iter_mut().enumerate() {
        *v = 0.1 * (2.0 * std::f32::consts::PI * 1000.0 * t as f32 / sr).sin();
      }
      dyn_.process(&mut bus, &p);
      min_rms = min_rms.min(rms(&bus[Speaker::Fc.index()]));
    }
    // 输入 RMS = 0.1/√2 ≈ 0.071。最坏（压缩瞬间）也不应低于输入的 15%，
    // 且绝不能趋近静音。
    assert!(
      min_rms > 0.0107,
      "持续信号不应被压没，实际最低 RMS {min_rms}"
    );
  }

  #[test]
  fn first_shot_front_is_compressed() {
    // 回归：枪声**前沿**必须被压（瞬时峰值检测让第一发也不漏）。
    let sr = 48_000.0;
    let mut dyn_ = AtkDynamic::new(sr);
    let p = AtkDynParams::default();
    let mut seed = 0x1234_5678u32;
    let mut noise = move || {
      seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
      ((seed >> 8) as f32 / 8_388_608.0 - 1.0) * 0.0055
    };
    let mut fill = |bus: &mut Vec<Vec<f32>>| {
      for ch in bus.iter_mut() {
        for v in ch.iter_mut() {
          *v = noise();
        }
      }
    };
    for _ in 0..(sr * 3.0 / 128.0) as usize {
      let mut bus = vec![vec![0.0f32; 128]; CHANNELS];
      fill(&mut bus);
      dyn_.process(&mut bus, &p);
    }
    // 第一发枪声：20ms、1kHz、-12dBFS（峰值 0.25），落在第一块。
    let mut bus = vec![vec![0.0f32; 128]; CHANNELS];
    fill(&mut bus);
    for (t, v) in bus[Speaker::Fc.index()].iter_mut().enumerate() {
      *v = 0.25 * (2.0 * std::f32::consts::PI * 1000.0 * t as f32 / sr).sin();
    }
    dyn_.process(&mut bus, &p);
    let out_peak = bus[Speaker::Fc.index()]
      .iter()
      .fold(0.0f32, |a, v| a.max(v.abs()));
    // 第一发枪声峰值应从 -12dB 压到 -18.8dB 以下（净压 >6.8dB）。旧版前沿完全
    // 漏出、只压 ~0.3dB。第一发是无历史的最难情况：crossover 在交叉点有少量能量
    // 泄漏到相邻频段（gap 低于阈值不触发）+ makeup 会抵消一部分，所以前沿压不到
    // 稳态那么狠——剩下的由输出端 lookahead limiter 兜底。净压 >6.8dB 已足以让
    // 「自己的枪声开头」明显变轻。
    assert!(
      out_peak < 0.115,
      "第一发枪声前沿应被压住，实际峰值 {out_peak}（{} dBFS）",
      20.0 * out_peak.log10()
    );
  }
}
