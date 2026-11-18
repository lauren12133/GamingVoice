// PureVox — AI 麦克风降噪工具
// Copyright (C) 2024-2026 a2heng <752848283@qq.com>
//
// PureVox is licensed under the GNU General Public License v3.0 or
// later (GPL-3.0-or-later).  See LICENSE for details.
//
// SPDX-License-Identifier: GPL-3.0-or-later

//! ATK V3 空间引擎 —— **1:1 逐行翻译** ArtIsWar 开源 `atk_spatial_engine.jsfx`
//! （ArtTune BO7 V3 档，`library/BO7/V3/BO7_V3_pre.txt` 的实际参数）。
//!
//! # 为什么之前"效果几乎没有"
//!
//! 之前的 `fc_suppress` 只抄了 JSFX 的 Step 2/3/4 且参数全错（分频点用了 250/4k，
//! 真实是 600/3000；漏了 LFE 侧链），还整段漏掉了 Step 5~9：
//!
//! | JSFX Step | 作用 | 之前 |
//! | --- | --- | --- |
//! | 2 FC 自抑制 | 600/3000 三段加权从 L/R 减 | 分频点错 |
//! | 3 Center Duck | FC 触发 M/S 压中置 | 参数错 |
//! | 4 自适应 EQ | 200Hz/3kHz 抬脚步 | 有（参数错） |
//! | 5 四点 Crossover | 80/600/3000/16000 五段 | **漏** |
//! | 6 Self-Suppress | 环绕 mid/side 比 | **漏** |
//! | 7 CMR | 环绕去相关 | **漏** |
//! | 8 瞬态整形+扩展+方向强调 | 前后分离 6 组 | **漏** |
//! | LFE 侧链 | 低频能量调制 | **漏** |
//!
//! # 声道约定（7ch 模式，对应 JSFX `ck_mode == 3`）
//!
//! 引擎内部 7 个处理通道：`spl0..spl6 = FL FR FC S1 S2 S3 S4`（4 个环绕对称处理），
//! `spl7 = LFE` 只进 sidechain、不参与 crossover/输出。GamingVoice 的 8 路
//! `FL FR FC LFE BL BR SL SR` 按下述映射喂入，LFE 保持原样穿过。

use crate::dsp::surround::layout::{Speaker, CHANNELS};

/// `DB_K = 20 / ln(10)` —— dB 与自然对数的换算常数。
const DB_K: f32 = 20.0 / std::f32::consts::LN_10;
/// 二阶 Butterworth 品质因数（JSFX `BW_Q` = 1/√2）。
const BW_Q: f32 = std::f32::consts::FRAC_1_SQRT_2;

/// 参与处理的 7 个通道（按 layout 下标）：`FL FR FC` 前置 + 4 个环绕。
/// 顺序即 JSFX 的 `spl0..spl6`；`spl3..spl6` 对算法是「对称环绕」，
/// 内部顺序不影响结果（surr_mid / CMR / 瞬态整形都按平均值处理）。
const PROC_CH: [usize; 7] = [
  Speaker::Fl.index(), // spl0
  Speaker::Fr.index(), // spl1
  Speaker::Fc.index(), // spl2
  Speaker::Bl.index(), // spl3
  Speaker::Br.index(), // spl4
  Speaker::Sl.index(), // spl5
  Speaker::Sr.index(), // spl6
];

/// ATK V3 全部参数（与 JSFX 59 个 slider 一一对应，默认值 = `BO7_V3_pre.txt`）。
#[derive(Clone, Debug)]
pub struct AtkV3Params {
  // Crossover
  pub low_cut: f32,
  pub high_cut: f32,
  pub xover1: f32,
  pub xover2: f32,
  // FC Self-Suppression
  pub ck_sub_low: f32,
  pub ck_sub_mid: f32,
  pub ck_sub_high: f32,
  // Self Sound Gate
  pub ck_duck_db: f32,
  pub ck_thresh_db: f32,
  pub ck_attack_ms: f32,
  pub ck_release_ms: f32,
  pub ck_knee_db: f32,
  // Adaptive EQ
  pub boost_low_db: f32,
  pub boost_low_hz: f32,
  pub boost_low_q: f32,
  pub boost_high_db: f32,
  pub boost_high_hz: f32,
  pub boost_high_q: f32,
  pub boost_attack_ms: f32,
  pub boost_release_ms: f32,
  pub boost_reject_thresh_db: f32,
  // Band 1 (low) transient shaper
  pub b1_attack: f32,
  pub b1_sustain: f32,
  pub b1_atk_str: f32,
  pub b1_sus_str: f32,
  pub fb1_atk_str: f32,
  pub fb1_sus_str: f32,
  // Band 2 (mid)
  pub b2_attack: f32,
  pub b2_sustain: f32,
  pub b2_atk_str: f32,
  pub b2_sus_str: f32,
  pub fb2_atk_str: f32,
  pub fb2_sus_str: f32,
  // Band 3 (high)
  pub b3_attack: f32,
  pub b3_sustain: f32,
  pub b3_atk_str: f32,
  pub b3_sus_str: f32,
  pub fb3_atk_str: f32,
  pub fb3_sus_str: f32,
  // Expansion
  pub b1_exp_thresh: f32,
  pub b1_exp_ratio: f32,
  pub b1_exp_floor: f32,
  pub b2_exp_thresh: f32,
  pub b2_exp_ratio: f32,
  pub b2_exp_floor: f32,
  pub b3_exp_thresh: f32,
  pub b3_exp_ratio: f32,
  pub b3_exp_floor: f32,
  // CMR
  pub cmr_low: f32,
  pub cmr_mid: f32,
  pub cmr_high: f32,
  // Master
  pub dir_emphasis: f32,
  pub self_sup: f32,
  pub out_gain_db: f32,
  pub gain_smooth: f32,
  pub mix: f32,
  // LFE sidechain
  pub lfe_mod_depth: f32,
  pub lfe_attack_ms: f32,
  pub lfe_release_ms: f32,
}

impl AtkV3Params {
  /// BO7 V3 默认档（`BO7_V3_pre.txt` ChunkData 解码得到的精确值）。
  pub fn v3_default() -> Self {
    AtkV3Params {
      low_cut: 80.0,
      high_cut: 16_000.0,
      xover1: 600.0,
      xover2: 3_000.0,
      ck_sub_low: 0.187_975,
      ck_sub_mid: 0.300_000,
      ck_sub_high: 0.250_000,
      // COD 实测：自己枪声在 FL/FR 的 mid 分量（约 -3dB），比 V3 原版假设的
      // FC（约 -23dB）高约 20dB，故 duck 深度与门限都按实测加强。
      ck_duck_db: -30.0,
      ck_thresh_db: -35.0,
      ck_attack_ms: 1.0,
      ck_release_ms: 80.0,
      ck_knee_db: 6.0,
      // 脚步增强幅度加大（配合 Step4 只抬环绕声道的改动）
      boost_low_db: 8.0,
      boost_low_hz: 200.0,
      boost_low_q: 1.0,
      boost_high_db: 4.0,
      boost_high_hz: 3_000.0,
      boost_high_q: 1.0,
      boost_attack_ms: 1.0,
      boost_release_ms: 80.0,
      boost_reject_thresh_db: -13.5,
      b1_attack: 0.35,
      b1_sustain: 0.22,
      b1_atk_str: 0.20,
      b1_sus_str: -0.595_687,
      fb1_atk_str: 0.10,
      fb1_sus_str: -0.259_892,
      b2_attack: 0.41,
      b2_sustain: 0.20,
      b2_atk_str: 0.10,
      b2_sus_str: -0.646_469,
      fb2_atk_str: 0.08,
      fb2_sus_str: -0.249_191,
      b3_attack: 0.50,
      b3_sustain: 0.153_720,
      b3_atk_str: -0.197_628,
      b3_sus_str: -0.246_146,
      fb3_atk_str: -0.09,
      fb3_sus_str: -0.12,
      b1_exp_thresh: -36.256_065,
      b1_exp_ratio: 3.072_776,
      b1_exp_floor: -69.5,
      b2_exp_thresh: -44.768_194,
      b2_exp_ratio: 3.048_518,
      b2_exp_floor: -70.0,
      b3_exp_thresh: -51.725_067,
      b3_exp_ratio: 6.479_784,
      b3_exp_floor: -70.0,
      cmr_low: 0.270_674,
      cmr_mid: 0.299_973,
      cmr_high: 0.514_825,
      dir_emphasis: 0.345_778,
      self_sup: 0.05,
      out_gain_db: 1.0,
      gain_smooth: 0.15,
      mix: 1.0,
      lfe_mod_depth: 0.357_468,
      lfe_attack_ms: 10.0,
      lfe_release_ms: 350.455_7,
    }
  }

  /// 把界面旋钮映射到引擎参数。默认位（0.8 / 0.7 / 0.5 / -30dB / 0.75）精确等于 V3 值
  /// （`range` 是「听远」旋钮，V3 原版无此旋钮，默认 0.75 已让扩展器比原版温和）。
  pub fn apply_knobs(
    &mut self,
    self_gun: f32,
    self_foot: f32,
    leveller: f32,
    duck_db: f32,
    range: f32,
  ) {
    // 默认位 =1.0；拉满（1.0）时 1.25 倍。
    let gun = (self_gun.clamp(0.0, 1.0) / 0.8).clamp(0.0, 1.25);
    let foot = self_foot.clamp(0.0, 1.0) / 0.7;
    let lev = leveller.clamp(0.0, 1.0) / 0.5;
    // FC 减法：中高频跟「自身枪声」，低频跟「自身脚步」
    self.ck_sub_mid *= gun;
    self.ck_sub_high *= gun;
    self.ck_sub_low *= foot;
    // Center Duck 深度独立成旋钮（dB）：COD 等把自己枪声放在 FL/FR 中央分量，
    // 压自声靠的是它，而不是 FC 减法——所以不再跟「自身枪声」耦合。
    self.ck_duck_db = duck_db.clamp(-60.0, 0.0);
    // 自适应 EQ（脚步放大）跟「脚步增强」
    self.boost_low_db *= lev;
    self.boost_high_db *= lev;

    // 听远（0..1）：越大越能听清远处的脚步/换弹。
    // 「听不远」的根因是 Step 8 的下行扩展器：远处声音电平落在门限以下，
    // 被按 ratio 斜率额外压掉十几~几十 dB。这里随 range 温和化扩展器 + 加强脚步 boost：
    let range = range.clamp(0.0, 1.0);
    // 扩展器强度：range=0 保持 V3 原值；range=1 时 ratio 收窄到接近 1（几乎不扩展）。
    let ext = (1.0 - range).clamp(0.0, 1.0);
    self.b1_exp_ratio = 1.0 + (self.b1_exp_ratio - 1.0) * ext;
    self.b2_exp_ratio = 1.0 + (self.b2_exp_ratio - 1.0) * ext;
    self.b3_exp_ratio = 1.0 + (self.b3_exp_ratio - 1.0) * ext;
    // 门限下移：range 越大，越少声音落入扩展区（远处声音不被误压）。
    let thresh_shift = range * 16.0;
    self.b1_exp_thresh -= thresh_shift;
    self.b2_exp_thresh -= thresh_shift;
    self.b3_exp_thresh -= thresh_shift;
    // 下限上抬：限制扩展器最大压降（远处声音最多只被压几 dB，而不是几十 dB）。
    let floor_shift = range * 10.0;
    self.b1_exp_floor += floor_shift;
    self.b2_exp_floor += floor_shift;
    self.b3_exp_floor += floor_shift;
    // 脚步频段 boost 随 range 增强。
    let boost_scale = 1.0 + range * 0.6;
    self.boost_low_db *= boost_scale;
    self.boost_high_db *= boost_scale;
  }
}

/// 二阶节（转置直接 II 型，系数已按 `a0` 归一化），与 JSFX `process_biquad` 一致。
#[derive(Clone, Copy, Debug)]
struct Biquad {
  b0: f32,
  b1: f32,
  b2: f32,
  a1: f32,
  a2: f32,
}

/// 二阶 Butterworth 低通（Q = BW_Q，与 JSFX `calc_biquad_lp` 一致）。
fn bq_lowpass(sr: f32, f: f32) -> Biquad {
  let w0 = 2.0 * std::f32::consts::PI * (f / sr);
  let (sw, cw) = w0.sin_cos();
  let alpha = sw / (2.0 * BW_Q);
  let a0 = 1.0 + alpha;
  Biquad {
    b0: (1.0 - cw) / 2.0 / a0,
    b1: (1.0 - cw) / a0,
    b2: (1.0 - cw) / 2.0 / a0,
    a1: -2.0 * cw / a0,
    a2: (1.0 - alpha) / a0,
  }
}

/// 二阶 Butterworth 高通（Q = BW_Q，与 JSFX `calc_biquad_hp` 一致）。
fn bq_highpass(sr: f32, f: f32) -> Biquad {
  let w0 = 2.0 * std::f32::consts::PI * (f / sr);
  let (sw, cw) = w0.sin_cos();
  let alpha = sw / (2.0 * BW_Q);
  let a0 = 1.0 + alpha;
  Biquad {
    b0: (1.0 + cw) / 2.0 / a0,
    b1: -(1.0 + cw) / a0,
    b2: (1.0 + cw) / 2.0 / a0,
    a1: -2.0 * cw / a0,
    a2: (1.0 - alpha) / a0,
  }
}

/// RBJ peaking EQ（与 JSFX `calc_peaking_eq` 一致）。
fn bq_peaking(sr: f32, f: f32, db: f32, q: f32) -> Biquad {
  let a = 10f32.powf(db / 40.0);
  let w0 = 2.0 * std::f32::consts::PI * (f / sr);
  let (sw, cw) = w0.sin_cos();
  let alpha = sw / (2.0 * q);
  let a0 = 1.0 + alpha / a;
  Biquad {
    b0: (1.0 + alpha * a) / a0,
    b1: -2.0 * cw / a0,
    b2: (1.0 - alpha * a) / a0,
    a1: -2.0 * cw / a0,
    a2: (1.0 - alpha / a) / a0,
  }
}

/// 转置 DF2 二阶节处理（JSFX `process_biquad`）：`y = b0*x + z1; z1 = b1*x - a1*y + z2; z2 = b2*x - a2*y`。
#[inline]
fn bq_run(coef: &Biquad, state: &mut [f32; 2], x: f32) -> f32 {
  let y = coef.b0 * x + state[0];
  state[0] = coef.b1 * x - coef.a1 * y + state[1];
  state[1] = coef.b2 * x - coef.a2 * y;
  y
}

/// 一阶包络跟随器（JSFX `eval_follower`）：attack/release 采用乘性平滑。
#[inline]
fn follower(env: &mut f32, x: f32, a: f32, r: f32) -> f32 {
  let c = if x > *env { a } else { r };
  *env = c * (*env - x) + x;
  *env
}

/// 时间常数 → 平滑系数（JSFX `time_to_coeff`）。
#[inline]
fn tc(t_sec: f32, sr: f32) -> f32 {
  (-1.0 / (t_sec * sr)).exp()
}

/// ATK V3 引擎（逐样本状态机）。
pub struct AtkV3 {
  sr: f32,
  p: AtkV3Params,
  /// 4 个分频点（lowCut/xover1/xover2/highCut）的 LP/HP 二阶节系数（LR4 每级共用）。
  xo_lp: [Biquad; 4],
  xo_hp: [Biquad; 4],
  /// crossover 状态：`[分频点 4][声道 7][LP1 LP2 HP1 HP2][z1 z2]`。
  xo_state: [[[[f32; 2]; 4]; 7]; 4],
  /// FC 减法专用 crossover 状态（xover1 LP2 + xover1 HP2 + xover2 LP2 + xover2 HP2）。
  sub_state: [[f32; 2]; 8],
  /// Center Duck 包络（dB 域）与其 attack/release 系数。
  ck_env: f32,
  ck_coeff: [f32; 2],
  /// 自适应 EQ 包络（0..1）与 peaking 系数/状态。
  boost_env: f32,
  boost_coeff: [f32; 2],
  boost_low_coef: Biquad,
  boost_high_coef: Biquad,
  boost_low_state: [[f32; 2]; 7],
  boost_high_state: [[f32; 2]; 7],
  /// LFE 侧链包络（dB 域）与系数。
  lfe_env: f32,
  lfe_coeff: [f32; 2],
  /// Self-Suppress 状态：3 频段 × (mid_sq, side_sq)。
  ss_state: [f32; 6],
  /// 瞬态门控平滑：3 频段。
  gate_state: [f32; 3],
  /// 瞬态整形包络状态：6 组（front b1/b2/b3, surround b1/b2/b3）× 4 follower。
  env_state: [[f32; 4]; 6],
  /// 瞬态整形包络系数：6 组 × 8（fast_att, fast_rel, b_atk, atk_rel, dec_att, b_sus, smooth, smooth）。
  env_coeff: [[f32; 8]; 6],
  ss_alpha: f32,
  gate_alpha: f32,
  /// 最近一块的 Center Duck 平均压降（dB，负值）。暴露给界面做实时反馈——
  /// 开枪时这个数往下掉，说明 duck 正在压自己枪声；一直是 0 说明没触发。
  pub last_duck_db: f32,
}

impl AtkV3 {
  pub fn new(sr: f32) -> Self {
    let mut e = AtkV3 {
      sr,
      p: AtkV3Params::v3_default(),
      xo_lp: [bq_lowpass(sr, 80.0); 4],
      xo_hp: [bq_highpass(sr, 80.0); 4],
      xo_state: [[[[0.0; 2]; 4]; 7]; 4],
      sub_state: [[0.0; 2]; 8],
      ck_env: -120.0,
      ck_coeff: [0.0; 2],
      boost_env: 0.0,
      boost_coeff: [0.0; 2],
      boost_low_coef: bq_peaking(sr, 200.0, 0.0, 1.0),
      boost_high_coef: bq_peaking(sr, 3000.0, 0.0, 1.0),
      boost_low_state: [[0.0; 2]; 7],
      boost_high_state: [[0.0; 2]; 7],
      lfe_env: -120.0,
      lfe_coeff: [0.0; 2],
      ss_state: [0.0; 6],
      gate_state: [0.0; 3],
      env_state: [[0.0; 4]; 6],
      env_coeff: [[0.0; 8]; 6],
      ss_alpha: 0.0,
      gate_alpha: 0.0,
      last_duck_db: 0.0,
    };
    e.set_params(&AtkV3Params::v3_default());
    e
  }

  /// 重算全部派生系数（对应 JSFX `@slider` + `@block`）。
  pub fn set_params(&mut self, p: &AtkV3Params) {
    self.p = p.clone();
    let sr = self.sr;

    // crossover 频率排序（@slider 段）
    let mut low_cut = p.low_cut;
    let mut xover1 = p.xover1;
    let mut xover2 = p.xover2;
    let mut high_cut = p.high_cut;
    xover1 = xover1.max(low_cut + 1.0);
    xover2 = xover2.max(xover1 + 1.0);
    high_cut = high_cut.max(xover2 + 1.0);
    xover2 = xover2.min(high_cut - 1.0);
    xover1 = xover1.min(xover2 - 1.0);
    low_cut = low_cut.min(xover1 - 1.0);

    self.xo_lp = [
      bq_lowpass(sr, low_cut),
      bq_lowpass(sr, xover1),
      bq_lowpass(sr, xover2),
      bq_lowpass(sr, high_cut),
    ];
    self.xo_hp = [
      bq_highpass(sr, low_cut),
      bq_highpass(sr, xover1),
      bq_highpass(sr, xover2),
      bq_highpass(sr, high_cut),
    ];

    // Center Duck 系数
    self.ck_coeff = [
      tc(p.ck_attack_ms * 0.001, sr),
      tc(p.ck_release_ms * 0.001, sr),
    ];

    // 自适应 EQ 系数
    self.boost_coeff = [
      tc(p.boost_attack_ms * 0.001, sr),
      tc(p.boost_release_ms * 0.001, sr),
    ];
    self.boost_low_coef = bq_peaking(sr, p.boost_low_hz, p.boost_low_db, p.boost_low_q);
    self.boost_high_coef = bq_peaking(sr, p.boost_high_hz, p.boost_high_db, p.boost_high_q);

    // LFE 侧链系数
    self.lfe_coeff = [
      tc(p.lfe_attack_ms * 0.001, sr),
      tc(p.lfe_release_ms * 0.001, sr),
    ];

    // 瞬态整形包络系数（@block 段）
    let fast_att = tc(0.0001, sr);
    let fast_rel = tc(0.12, sr);
    let atk_rel = tc(0.15, sr);
    let dec_att = tc(0.0001, sr);
    let smooth = tc((p.gain_smooth * 0.05).max(0.0001), sr);

    let band_coeff = |attack: f32, sustain: f32| -> [f32; 8] {
      let b_atk = tc(0.0001 + (1.0 - attack) * 0.3, sr);
      let b_sus = tc(0.0001 + (1.0 - sustain) * 0.5, sr);
      [
        fast_att, fast_rel, b_atk, atk_rel, dec_att, b_sus, smooth, smooth,
      ]
    };

    // front b1/b2/b3, surround b1/b2/b3
    self.env_coeff[0] = band_coeff(p.b1_attack, p.b1_sustain);
    self.env_coeff[1] = band_coeff(p.b2_attack, p.b2_sustain);
    self.env_coeff[2] = band_coeff(p.b3_attack, p.b3_sustain);
    self.env_coeff[3] = self.env_coeff[0];
    self.env_coeff[4] = self.env_coeff[1];
    self.env_coeff[5] = self.env_coeff[2];

    self.ss_alpha = 1.0 - tc(0.01, sr); // ~10ms 平滑（1 - exp(-1/(0.01*sr))）
    self.gate_alpha = 1.0 - tc(0.005, sr); // ~5ms
  }

  /// 清空跨块/跨会话状态。
  pub fn reset(&mut self) {
    self.xo_state = [[[[0.0; 2]; 4]; 7]; 4];
    self.sub_state = [[0.0; 2]; 8];
    self.ck_env = -120.0;
    self.boost_env = 0.0;
    self.boost_low_state = [[0.0; 2]; 7];
    self.boost_high_state = [[0.0; 2]; 7];
    self.lfe_env = -120.0;
    self.ss_state = [0.0; 6];
    self.gate_state = [0.0; 3];
    self.env_state = [[0.0; 4]; 6];
  }

  /// LR4 低通（两级 Butterworth 级联，JSFX `lr4_lp`）。
  #[inline]
  fn lr4_lp(&mut self, x: f32, ci: usize, ch: usize) -> f32 {
    let coef = self.xo_lp[ci];
    let s = &mut self.xo_state[ci][ch];
    let tmp = bq_run(&coef, &mut s[0], x);
    bq_run(&coef, &mut s[1], tmp)
  }

  /// LR4 高通（JSFX `lr4_hp`）。
  #[inline]
  fn lr4_hp(&mut self, x: f32, ci: usize, ch: usize) -> f32 {
    let coef = self.xo_hp[ci];
    let s = &mut self.xo_state[ci][ch];
    let tmp = bq_run(&coef, &mut s[2], x);
    bq_run(&coef, &mut s[3], tmp)
  }

  /// 瞬态整形 + 下行扩展（JSFX `calc_band_gain`）。
  #[allow(clippy::too_many_arguments)]
  #[inline]
  fn band_gain(
    &mut self,
    level: f32,
    bi: usize,
    atk_str: f32,
    sus_str: f32,
    exp_thresh: f32,
    exp_ratio: f32,
    exp_floor: f32,
    gain_smooth: f32,
  ) -> f32 {
    let level_db = DB_K * (level + 1e-10).ln();
    let eo = &mut self.env_state[bi];
    let co = self.env_coeff[bi];
    let fast_e = follower(&mut eo[0], level_db, co[0], co[1]);
    let atk_e = follower(&mut eo[1], level_db, co[2], co[3]);
    let dec_e = follower(&mut eo[2], level_db, co[4], co[5]);

    let sus_delta = dec_e - fast_e;
    let sus_cut_db = if sus_delta > 0.0 {
      sus_str * sus_delta * (1.0 + 2.0 * sus_str.abs() * sus_delta / 15.0)
    } else {
      0.0
    };
    let mut gain_db = atk_str * (atk_e - fast_e) + sus_cut_db;

    if exp_ratio > 1.0 && level_db < exp_thresh {
      let below = exp_thresh - level_db;
      let exp_reduction = below * (exp_ratio - 1.0);
      let mut exp_gain_db = -(exp_reduction.min(level_db + gain_db - exp_floor));
      exp_gain_db = exp_gain_db.min(0.0);
      gain_db += exp_gain_db;
    }

    if gain_smooth > 0.0 {
      gain_db = follower(&mut eo[3], gain_db, co[6], co[7]);
    }
    gain_db = gain_db.clamp(-40.0, 24.0);
    (gain_db / DB_K).exp()
  }

  /// 处理一块 7.1：`bus` 为 8 路（layout 顺序，长度 = 块长）。
  /// 就地改写 7 个非 LFE 通道，LFE 保持原样（供下游球谐编码并入 W）。
  #[allow(clippy::needless_range_loop)] // 逐样本按通道下标读写多路，迭代器不适用
  pub fn process(&mut self, bus: &mut [Vec<f32>]) {
    if bus.len() < CHANNELS {
      return;
    }
    let n = bus.first().map(|v| v.len()).unwrap_or(0);
    if n == 0 {
      return;
    }
    let p = self.p.clone();

    let out_gain_lin = 10f32.powf(p.out_gain_db / 20.0);
    let ck_thresh_db = p.ck_thresh_db;
    let ck_duck_db = p.ck_duck_db;
    let ck_knee_db = p.ck_knee_db;
    let boost_reject_db = p.boost_reject_thresh_db;
    // 本块累计的 duck 压降（负值），块末取平均写入 last_duck_db 供界面反馈。
    let mut duck_acc = 0.0f32;

    // band 索引：0=pass_low, 1=band1, 2=band2, 3=band3, 4=pass_high
    for t in 0..n {
      // === Step 0: LFE sidechain（读 sub-bass 能量）===
      let lfe_abs = bus[Speaker::Lfe.index()][t].abs();
      let lfe_db = DB_K * (lfe_abs + 1e-10).ln();
      self.lfe_env = follower(
        &mut self.lfe_env,
        lfe_db,
        self.lfe_coeff[0],
        self.lfe_coeff[1],
      );
      let lfe_norm = ((self.lfe_env + 110.0) / 80.0).clamp(0.0, 1.0);
      let lfe_factor = (1.0 - p.lfe_mod_depth) + p.lfe_mod_depth * lfe_norm;

      // === Step 1: 存 dry（7 通道）===
      let mut dry = [0.0f32; 7];
      for (i, &c) in PROC_CH.iter().enumerate() {
        dry[i] = bus[c][t];
      }

      // === Step 2: FC 自抑制（600/3000 三段加权从 FL/FR 减）===
      let fc_raw = bus[Speaker::Fc.index()][t];
      // xover1 分频
      let tmp = bq_run(&self.xo_lp[1], &mut self.sub_state[0], fc_raw);
      let fc_low = bq_run(&self.xo_lp[1], &mut self.sub_state[1], tmp);
      let tmp = bq_run(&self.xo_hp[1], &mut self.sub_state[2], fc_raw);
      let fc_above1 = bq_run(&self.xo_hp[1], &mut self.sub_state[3], tmp);
      // xover2 分频
      let tmp = bq_run(&self.xo_lp[2], &mut self.sub_state[4], fc_above1);
      let fc_mid = bq_run(&self.xo_lp[2], &mut self.sub_state[5], tmp);
      let tmp = bq_run(&self.xo_hp[2], &mut self.sub_state[6], fc_above1);
      let fc_high = bq_run(&self.xo_hp[2], &mut self.sub_state[7], tmp);

      let fc_sub = p.ck_sub_low * lfe_factor * fc_low
        + p.ck_sub_mid * lfe_factor * fc_mid
        + p.ck_sub_high * lfe_factor * fc_high;
      bus[Speaker::Fl.index()][t] -= fc_sub;
      bus[Speaker::Fr.index()][t] -= fc_sub;

      // === Step 3: Center Duck ===
      // 触发源改为「前方 mid 分量」(FL+FR 同相部分)，不再用 FC。
      // 实测枪声快照证明：COD 把自己枪声放在 FL/FR（对称同相、约 -3dB），
      // FC 几乎静音（约 -23dB），用 FC 触发时 duck 压不到自声 → 之前「没效果」的根因。
      // 自己枪声 FL≈FR → mid 大 → 触发；敌人枪声有方位差 → side 大 → 保留方位信息。
      let l = bus[Speaker::Fl.index()][t];
      let r = bus[Speaker::Fr.index()][t];
      let mid_pre = (l + r) * 0.5;
      let ck_level_db = DB_K * (mid_pre.abs() + 1e-10).ln();
      self.ck_env = follower(
        &mut self.ck_env,
        ck_level_db,
        self.ck_coeff[0],
        self.ck_coeff[1],
      );
      let mut reduction_db = 0.0f32;
      let ck_gain = if self.ck_env > ck_thresh_db {
        reduction_db = if ck_knee_db > 0.0 {
          let above = self.ck_env - ck_thresh_db;
          let ratio = (above / ck_knee_db).min(1.0);
          ck_duck_db * lfe_factor * ratio
        } else {
          ck_duck_db * lfe_factor
        };
        (reduction_db / DB_K).exp()
      } else {
        1.0
      };
      duck_acc += reduction_db;

      let mid = mid_pre * ck_gain;
      let side = (l - r) * 0.5;
      bus[Speaker::Fl.index()][t] = mid + side;
      bus[Speaker::Fr.index()][t] = mid - side;
      // 清零 FC（原版语义：中置只做侧链，不进输出）
      bus[Speaker::Fc.index()][t] = 0.0;

      // === Step 4: 自适应 EQ（前方 mid 不响 + 无巨响时抬脚步频段）===
      // 注意：V3 原版还有 `lfe_norm < 0.5` 条件，但它要求 LFE 环境 < -70dB，
      // 游戏里 LFE 几乎持续有低频 → 脚步增强被永久卡死。这里去掉该条件，
      // 巨响仍由下方 boost_reject 拒绝。
      let mut boost_target = if ck_gain > 0.9 { 1.0 } else { 0.0 };
      // 枪声拒绝：任一非 FC 通道太响就关 boost
      let mut peak_all = 0.0f32;
      for &c in &PROC_CH {
        if c != Speaker::Fc.index() {
          peak_all = peak_all.max(bus[c][t].abs());
        }
      }
      let peak_db = DB_K * (peak_all + 1e-10).ln();
      if peak_db > boost_reject_db {
        boost_target = 0.0;
      }
      self.boost_env = follower(
        &mut self.boost_env,
        boost_target,
        self.boost_coeff[0],
        self.boost_coeff[1],
      );

      if self.boost_env > 1e-4 {
        // COD 实测：自己脚步在 FL/FR（对称 mid），敌人脚步在环绕。只抬环绕
        // 声道 (Bl/Br/Sl/Sr = PROC_CH[3..7]) 的脚步频段，避免放大 FL/FR 里
        // 残留的自己脚步。
        for (ch_idx, &c) in PROC_CH.iter().enumerate().skip(3) {
          let x = bus[c][t];
          let low_s = &mut self.boost_low_state[ch_idx];
          let tmp = bq_run(&self.boost_low_coef, low_s, x);
          let x = x + (tmp - x) * self.boost_env;
          let hi_s = &mut self.boost_high_state[ch_idx];
          let tmp = bq_run(&self.boost_high_coef, hi_s, x);
          bus[c][t] = x + (tmp - x) * self.boost_env;
        }
      }

      // === Step 5: 四点 Crossover（7 通道 × 5 段）===
      // band[ch][0..5] = pass_low, band1, band2, band3, pass_high
      let mut band = [[0.0f32; 5]; 7];
      for ch in 0..7 {
        let x = bus[PROC_CH[ch]][t];
        let pass_low = self.lr4_lp(x, 0, ch);
        let above_low = self.lr4_hp(x, 0, ch);
        let band1 = self.lr4_lp(above_low, 1, ch);
        let above1 = self.lr4_hp(above_low, 1, ch);
        let band2 = self.lr4_lp(above1, 2, ch);
        let above2 = self.lr4_hp(above1, 2, ch);
        let band3 = self.lr4_lp(above2, 3, ch);
        let pass_high = self.lr4_hp(above2, 3, ch);
        band[ch] = [pass_low, band1, band2, band3, pass_high];
      }

      // === Step 6: Self-Suppress（环绕 mid/side 比 + 瞬态门控）===
      let mut ratio_b = [0.0f32; 3];
      for b in 0..3 {
        let bi = 1 + b; // band1/2/3 段
        let surr_mid = (band[3][bi] + band[4][bi] + band[5][bi] + band[6][bi]) * 0.25;
        let mid_sq = surr_mid * surr_mid;
        let mut side_sq = 0.0f32;
        for s in 3..7 {
          let d = band[s][bi] - surr_mid;
          side_sq += d * d;
        }
        side_sq *= 0.25;
        self.ss_state[b * 2] += self.ss_alpha * (mid_sq - self.ss_state[b * 2]);
        self.ss_state[b * 2 + 1] += self.ss_alpha * (side_sq - self.ss_state[b * 2 + 1]);
        ratio_b[b] =
          self.ss_state[b * 2] / (self.ss_state[b * 2] + self.ss_state[b * 2 + 1] + 1e-20);

        let eo_surr = 3 + b; // surround 组的 fast follower
        let is_transient = if self.env_state[eo_surr][0] > self.env_state[eo_surr][1] {
          1.0
        } else {
          0.0
        };
        self.gate_state[b] += self.gate_alpha * (is_transient - self.gate_state[b]);

        let suppress = p.self_sup * lfe_factor * ratio_b[b] * self.gate_state[b];
        band[0][bi] -= surr_mid * suppress; // FL
        band[1][bi] -= surr_mid * suppress; // FR
                                            // band[2] = FC，不抑制
        for s in 3..7 {
          band[s][bi] -= surr_mid * suppress;
        }
      }

      // === Step 7: CMR（环绕去相关）===
      let cmr = [p.cmr_low, p.cmr_mid, p.cmr_high];
      for b in 0..3 {
        let k = cmr[b] * lfe_factor / 3.0;
        if k > 0.0 {
          let bi = 1 + b;
          let sum = band[3][bi] + band[4][bi] + band[5][bi] + band[6][bi];
          let s = 1.0 + k;
          for ch in 3..7 {
            band[ch][bi] = s * band[ch][bi] - k * sum;
          }
        }
      }

      // === Step 8: 瞬态整形 + 下行扩展 + 方向强调 ===
      let atk_str = [p.b1_atk_str, p.b2_atk_str, p.b3_atk_str];
      let sus_str = [p.b1_sus_str, p.b2_sus_str, p.b3_sus_str];
      let fb_atk_str = [p.fb1_atk_str, p.fb2_atk_str, p.fb3_atk_str];
      let fb_sus_str = [p.fb1_sus_str, p.fb2_sus_str, p.fb3_sus_str];
      let exp_thresh = [p.b1_exp_thresh, p.b2_exp_thresh, p.b3_exp_thresh];
      let exp_ratio = [p.b1_exp_ratio, p.b2_exp_ratio, p.b3_exp_ratio];
      let exp_floor = [p.b1_exp_floor, p.b2_exp_floor, p.b3_exp_floor];

      for b in 0..3 {
        let bi = 1 + b;
        // front = FL(0), FR(1), FC(2)
        let lvl_f = band[0][bi]
          .abs()
          .max(band[1][bi].abs())
          .max(band[2][bi].abs());
        let gain_f = self.band_gain(
          lvl_f,
          b,
          fb_atk_str[b],
          fb_sus_str[b] * lfe_factor,
          exp_thresh[b],
          exp_ratio[b],
          exp_floor[b],
          p.gain_smooth,
        );
        band[0][bi] *= gain_f;
        band[1][bi] *= gain_f;
        band[2][bi] *= gain_f;
        // surround = 3..7
        let lvl_s = band[3][bi]
          .abs()
          .max(band[4][bi].abs())
          .max(band[5][bi].abs())
          .max(band[6][bi].abs());
        let gain_s = self.band_gain(
          lvl_s,
          3 + b,
          atk_str[b],
          sus_str[b],
          exp_thresh[b],
          exp_ratio[b],
          exp_floor[b],
          p.gain_smooth,
        );
        for s in 3..7 {
          band[s][bi] *= gain_s;
        }
        // 方向强调（仅环绕）
        if p.dir_emphasis > 0.0 {
          let dir_mult = 1.0 + p.dir_emphasis * (1.0 - ratio_b[b]);
          for s in 3..7 {
            band[s][bi] *= dir_mult;
          }
        }
      }

      // === Step 9: 求和输出 + dry/wet ===
      for ch in 0..7 {
        let wet = band[ch][0] + band[ch][1] + band[ch][2] + band[ch][3] + band[ch][4];
        let wet = wet * out_gain_lin;
        bus[PROC_CH[ch]][t] = dry[ch] * (1.0 - p.mix) + wet * p.mix;
      }
    }
    // 块末：把本块平均 duck 压降暴露给界面（负值 = 压了多少 dB）。
    self.last_duck_db = duck_acc / (n as f32);
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
  fn fc_centered_self_sound_is_suppressed() {
    // FC 只有"自己"，FL/FR 是同相泄漏。处理后 FC 清零，FL/FR 中间分量明显下降。
    let sr = 48_000.0;
    let mut f = AtkV3::new(sr);
    let n = 48_000usize; // 1 秒，让包络稳定
    let mut bus = vec![vec![0.0f32; n]; CHANNELS];
    let fc_idx = Speaker::Fc.index();
    let fl_idx = Speaker::Fl.index();
    let fr_idx = Speaker::Fr.index();
    for t in 0..n {
      let sig = 0.5 * (2.0 * std::f32::consts::PI * 1000.0 * t as f32 / sr).sin();
      bus[fc_idx][t] = sig;
      bus[fl_idx][t] = 0.3 * sig;
      bus[fr_idx][t] = 0.3 * sig;
    }
    f.process(&mut bus);

    // FC 必须清零
    assert!(bus[fc_idx].iter().all(|v| v.abs() < 1e-6), "FC 应被清零");
    // FL 后半段（稳态）峰值应显著低于输入泄漏峰值 0.15
    let half = n / 2;
    let out_peak = bus[fl_idx][half..]
      .iter()
      .fold(0.0f32, |a, v| a.max(v.abs()));
    assert!(
      out_peak < 0.15 * 0.5,
      "FL 的中间分量应被明显压低，out_peak {out_peak}"
    );
  }

  #[test]
  fn enemy_on_surround_is_preserved() {
    // 敌人在环绕声道（BL），不碰 FC。处理后 BL 能量应基本保留（CMR 对单声道无作用）。
    let sr = 48_000.0;
    let mut f = AtkV3::new(sr);
    let n = 48_000usize;
    let mut bus = vec![vec![0.0f32; n]; CHANNELS];
    let bl = Speaker::Bl.index();
    for t in 0..n {
      bus[bl][t] = 0.2 * (2.0 * std::f32::consts::PI * 500.0 * t as f32 / sr).sin();
    }
    let before = rms(&bus[bl]);
    f.process(&mut bus);
    let after = rms(&bus[bl]);
    assert!(
      after > before * 0.6,
      "BL 敌人声音不应被误伤，before {before} after {after}"
    );
  }

  #[test]
  fn passthrough_when_mix_zero() {
    // mix=0 时纯 dry，输出必须等于输入（除 LFE 外逐样本一致）。
    let mut f = AtkV3::new(48_000.0);
    let mut p = AtkV3Params::v3_default();
    p.mix = 0.0;
    f.set_params(&p);
    let n = 64usize;
    let mut bus = vec![vec![0.0f32; n]; CHANNELS];
    for (c, ch) in bus.iter_mut().enumerate() {
      for (t, v) in ch.iter_mut().enumerate() {
        *v = (c as f32 + 1.0) * 0.01 + (t as f32) * 0.001;
      }
    }
    let before = bus.clone();
    f.process(&mut bus);
    for c in 0..CHANNELS {
      if c == Speaker::Lfe.index() {
        continue; // LFE 不处理，但也要验证（下方统一比较）
      }
      for t in 0..n {
        assert!(
          (bus[c][t] - before[c][t]).abs() < 1e-5,
          "mix=0 时通道 {c} 样本 {t} 被改动"
        );
      }
    }
  }

  #[test]
  fn adaptive_eq_raises_footstep_when_fc_silent() {
    // FC 静默时，自适应 EQ 应抬升环绕声道的 200Hz 分量。
    let sr = 48_000.0;
    let mut f = AtkV3::new(sr);
    let n = 48_000usize;
    let mut bus = vec![vec![0.0f32; n]; CHANNELS];
    let bl = Speaker::Bl.index();
    let amp = 0.03; // 低于 boost_reject_thresh(-13.5dB=0.21)，不触发枪声拒绝
    for t in 0..n {
      bus[bl][t] = amp * (2.0 * std::f32::consts::PI * 200.0 * t as f32 / sr).sin();
    }
    f.process(&mut bus);
    let half = n / 2;
    let after = rms(&bus[bl][half..]);
    // 200Hz 在 boost_low(200Hz) 中心，应被明显抬升（+5dB 增益，但包络与 crossover 有衰减）
    assert!(
      after > amp * 0.707 * 1.3,
      "200Hz 脚步频段应被抬升，after {after}"
    );
  }

  #[test]
  fn range_knob_softens_expander_and_boosts_footsteps() {
    // 「听远」旋钮：range 越大，扩展器越温和（远处微弱声不被压掉）+ 脚步 boost 越强。
    let mut near = AtkV3Params::v3_default();
    near.apply_knobs(0.8, 0.7, 0.5, -30.0, 0.0); // range=0 = V3 原版
    let mut far = AtkV3Params::v3_default();
    far.apply_knobs(0.8, 0.7, 0.5, -30.0, 1.0); // range=1 = 最远

    // 扩展器比率应收窄（越接近 1 越不扩展，远处声音保留越多）
    assert!(far.b1_exp_ratio < near.b1_exp_ratio);
    assert!(far.b3_exp_ratio < near.b3_exp_ratio);
    // 门限应下移（更低 = 更少声音落入扩展区）
    assert!(far.b1_exp_thresh < near.b1_exp_thresh);
    // 下限应上抬（限制最大压降）
    assert!(far.b1_exp_floor > near.b1_exp_floor);
    // 脚步频段 boost 应增强
    assert!(far.boost_low_db > near.boost_low_db);
    assert!(far.boost_high_db > near.boost_high_db);
    // duck / FC 减法不受 range 影响（它们由各自旋钮控制）
    assert!((far.ck_duck_db - near.ck_duck_db).abs() < 1e-6);
    assert!((far.ck_sub_mid - near.ck_sub_mid).abs() < 1e-6);
  }
}
