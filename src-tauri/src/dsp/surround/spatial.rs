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

//! 空间化：单声道（或将来的 8 声道）→ 7.1 总线 → 每声道增益/延时 → HRIR → 双耳。
//!
//! ```text
//! 输入 ──► 上混 ──► [g1..g8] ──► [d1..d8] ──► 卷积(8×2) ──► L / R
//!            │          ▲            ▲             ▲
//!            │          └── 增益对角阵 G（"单独调各方向音量"就在这里）
//!            │                        └── 每声道延时（对齐 / ITD 微调）
//!            └── 单声道源没有方位信息，必须由这里决定"放到哪"
//! ```
//!
//! **顺序不可交换**：增益与延时都在卷积**之前**（G·x 先于 H），道理见
//! `ARCHITECTURE.md` 的说明——卷积之后 8 路已求和成 2 路，再乘任何系数都只是整体音量。

use std::f32::consts::FRAC_1_SQRT_2;

use crate::audio::{HOP, SAMPLE_RATE};
use crate::dsp::surround::conv::MatrixConv;
use crate::dsp::surround::hrir::{Hrir, EARS};
use crate::dsp::surround::layout::{Speaker, CHANNELS};

/// 单声道源往上混的方式。源本身没有方位信息，这一项决定"听起来从哪来"。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Upmix {
  /// 全部送到中置（最中性：像"人在正前方说话"）。
  Center,
  /// 只送前左/前右（等功率），适合音乐与系统声音。
  Front,
  /// 前 + 侧 + 后都分一点：把单声道铺成"房间感"，宽度由 `spread` 控制。
  Wide,
}

impl Upmix {
  pub fn id(self) -> &'static str {
    match self {
      Upmix::Center => "center",
      Upmix::Front => "front",
      Upmix::Wide => "wide",
    }
  }

  pub fn from_id(s: &str) -> Upmix {
    match s.trim().to_ascii_lowercase().as_str() {
      "center" => Upmix::Center,
      "wide" => Upmix::Wide,
      _ => Upmix::Front,
    }
  }
}

/// 空间化参数（**全部可热改**：改增益/延时不需要重建会话）。
#[derive(Clone, Debug)]
pub struct SpatialParams {
  /// 每个虚拟音箱的增益（dB）。-60 dB 视为静音。
  pub gains_db: [f32; CHANNELS],
  /// 每个虚拟音箱的延时（ms，0..25）。
  pub delays_ms: [f32; CHANNELS],
  pub upmix: Upmix,
  /// [`Upmix::Wide`] 的扩散量（0..1）。
  pub spread: f32,
  /// 总输出增益（dB），补偿卷积的电平损失。
  pub makeup_db: f32,
}

impl Default for SpatialParams {
  fn default() -> Self {
    SpatialParams {
      gains_db: [0.0; CHANNELS],
      delays_ms: [0.0; CHANNELS],
      upmix: Upmix::Front,
      spread: 0.5,
      makeup_db: 0.0,
    }
  }
}

/// 每声道最大延时（ms）。超过这个值就不是"对齐"而是"回声"了。
pub const MAX_DELAY_MS: f32 = 25.0;

/// -60 dB 以下视为静音（避免 `10^(x/20)` 得到 denormal）。
const SILENT_DB: f32 = -60.0;

#[inline]
fn db_to_gain(db: f32) -> f32 {
  if !db.is_finite() || db <= SILENT_DB {
    0.0
  } else {
    10f32.powf(db / 20.0)
  }
}

/// 单声道延时线（线性插值，支持小数样本）。
struct DelayLine {
  buf: Vec<f32>,
  w: usize,
}

impl DelayLine {
  fn new(max_samples: usize) -> DelayLine {
    DelayLine {
      buf: vec![0.0; max_samples.max(2) + 2],
      w: 0,
    }
  }

  #[inline]
  fn push(&mut self, x: f32) {
    self.buf[self.w] = x;
    self.w += 1;
    if self.w == self.buf.len() {
      self.w = 0;
    }
  }

  /// 读 `d` 个样本之前的值（`d` 可为小数）。
  #[inline]
  fn read(&self, d: f32) -> f32 {
    let n = self.buf.len() as f32;
    let d = d.clamp(0.0, n - 3.0);
    let i = d.floor();
    let frac = d - i;
    let idx0 = (self.w as f32 - 1.0 - i).rem_euclid(n) as usize;
    let idx1 = (idx0 + self.buf.len() - 1) % self.buf.len();
    self.buf[idx0] * (1.0 - frac) + self.buf[idx1] * frac
  }
}

/// 空间化引擎（每个输出行一份，状态互不共享）。
pub struct Spatial {
  params: SpatialParams,
  conv: MatrixConv,
  hrir: Hrir,
  /// 处理块长（样本）。**这是唯一的延迟来源**：卷积本身零算法延迟，
  /// 但必须攒满一块才能出结果，所以端到端至少多一块。
  block: usize,
  delay: Vec<DelayLine>,
  /// [8][block] 上混 + 增益 + 延时之后的分通道缓冲
  bus: Vec<Vec<f32>>,
  /// [2][block] 卷积输出
  ears: Vec<Vec<f32>>,
}

impl Spatial {
  /// 默认块长 = 引擎 hop（10 ms），与 `session.rs` 的节拍一致。
  pub fn new(hrir: Hrir, params: SpatialParams) -> Spatial {
    Spatial::with_block(hrir, params, HOP)
  }

  /// 指定块长（样本）。
  ///
  /// **竞技模式**：把块长压到 64 / 128，端到端少掉 7~9 ms，代价是 FFT 调用更频繁
  /// （见 `README.md` 的实测表格）。卷积器对任意块长都是零算法延迟的——
  /// 输出第 n 块只用到第 n 块及更早的输入，不会等未来的样本。
  ///
  /// 块长不必是 2 的幂（引擎 hop 480 就不是），但 **2 的幂走 rustfft 的专用内核、
  /// 最快**；480 这类混合基会慢一些，却是"不改引擎节拍"的唯一选择。
  pub fn with_block(hrir: Hrir, params: SpatialParams, block: usize) -> Spatial {
    assert!(
      (16..=8192).contains(&block),
      "块长 {block} 超出范围（16..=8192 个样本）"
    );
    let conv = MatrixConv::new(CHANNELS, block, &hrir);
    let max_delay = (MAX_DELAY_MS * SAMPLE_RATE as f32 / 1000.0).ceil() as usize + 2;
    Spatial {
      params,
      conv,
      hrir,
      block,
      delay: (0..CHANNELS).map(|_| DelayLine::new(max_delay)).collect(),
      bus: vec![vec![0.0; block]; CHANNELS],
      ears: vec![vec![0.0; block]; EARS],
    }
  }

  /// 当前块长（样本）。
  pub fn block(&self) -> usize {
    self.block
  }

  /// 块长对应的毫秒数（这就是"结构性延迟"，不含设备缓冲）。
  pub fn block_ms(&self) -> f32 {
    self.block as f32 * 1000.0 / SAMPLE_RATE as f32
  }

  pub fn hrir_name(&self) -> &str {
    &self.hrir.name
  }

  pub fn hrir_taps(&self) -> usize {
    self.hrir.taps
  }

  /// 换 HRIR：重建卷积器并清掉旧尾巴（否则切预设的瞬间会听到上一套的残响）。
  pub fn set_hrir(&mut self, hrir: Hrir) {
    self.conv = MatrixConv::new(CHANNELS, self.block, &hrir);
    self.hrir = hrir;
  }

  pub fn params(&self) -> &SpatialParams {
    &self.params
  }

  /// 改一组参数（热路径：下一 hop 生效，不重建会话）。
  pub fn set_params(&mut self, params: SpatialParams) {
    self.params = params;
  }

  pub fn set_gain_db(&mut self, sp: Speaker, db: f32) {
    self.params.gains_db[sp.index()] = db;
  }

  pub fn gain_db(&self, sp: Speaker) -> f32 {
    self.params.gains_db[sp.index()]
  }

  /// 每 hop 的复数乘加量（进调试接口，用来解释 CPU）。
  pub fn macs_per_hop(&self) -> u64 {
    self.conv.macs_per_hop()
  }

  /// 处理一块：`mono` 长度 = [`Spatial::block`]；`out_l` / `out_r` 长度 = block。
  pub fn process(&mut self, mono: &[f32], out_l: &mut [f32], out_r: &mut [f32]) {
    self.upmix_into_bus(mono);
    self.render(out_l, out_r);
  }

  /// 处理一块，但**已经有 8 声道**（真 7.1 源：游戏 / 系统声音的 8 声道回环）。
  ///
  /// 这是"真方位"与"假方位"的分界：走 [`Spatial::process`] 时单声道必须先上混，
  /// 方位是编出来的；走本函数时方位来自源本身，HRIR 只负责把它折进双耳。
  ///
  /// `input` 不足 8 路时缺的通道按静音处理（实时路径不 panic）。
  pub fn process_planar(&mut self, input: &[Vec<f32>], out_l: &mut [f32], out_r: &mut [f32]) {
    self.load_bus(input);
    self.render(out_l, out_r);
  }

  /// 把 8 路平面输入装进总线（不足补零）。**不做上混**：方位来自源本身。
  fn load_bus(&mut self, input: &[Vec<f32>]) {
    for (i, dst) in self.bus.iter_mut().enumerate() {
      let n = input.get(i).map(|s| s.len()).unwrap_or(0).min(self.block);
      if n > 0 {
        dst[..n].copy_from_slice(&input[i][..n]);
      }
      for v in dst[n..].iter_mut() {
        *v = 0.0;
      }
    }
  }

  /// **旁路双耳化**：只做每声道增益 / 延时，把 8 声道原样交出去。
  ///
  /// 这条路径上**没有卷积**，因此没有块延迟——块只是个搬运单位。
  /// 用途：把双耳化交给 Windows 的 spatializer（Dolby Atmos / Windows Sonic / DTS），
  /// PureVox 只负责"单独调各方向音量"这件事。这也是唯一能和 Dolby 叠加而不打架的分工。
  ///
  /// `out` 长度不足 8 路时只写给出的那几路（实时路径不 panic）。
  pub fn process_bed(&mut self, input: &[Vec<f32>], out: &mut [Vec<f32>]) {
    self.load_bus(input);
    self.apply_gain_and_delay();
    for (i, dst) in out.iter_mut().enumerate() {
      let src = match self.bus.get(i) {
        Some(s) => s,
        None => break,
      };
      let n = dst.len().min(self.block);
      dst[..n].copy_from_slice(&src[..n]);
    }
  }

  /// 增益 + 延时 + 卷积 + 补偿，双耳写回（两条入口共用，保证只有一条实现路径）。
  fn render(&mut self, out_l: &mut [f32], out_r: &mut [f32]) {
    self.apply_gain_and_delay();
    self.conv.process(&self.bus, &mut self.ears);
    let make = db_to_gain(self.params.makeup_db);
    let n = self.block.min(out_l.len()).min(out_r.len());
    for t in 0..n {
      out_l[t] = self.ears[0][t] * make;
      out_r[t] = self.ears[1][t] * make;
    }
  }

  /// 上混：把单声道按 [`Upmix`] 铺到 8 条通道。
  fn upmix_into_bus(&mut self, mono: &[f32]) {
    let n = self.block.min(mono.len());
    let spread = self.params.spread.clamp(0.0, 1.0);
    // 先整块清零，再按模式赋值：单声道源每次都是"重新铺"，不是累加
    for ch in self.bus.iter_mut() {
      for v in ch.iter_mut() {
        *v = 0.0;
      }
    }
    match self.params.upmix {
      Upmix::Center => {
        let dst = &mut self.bus[Speaker::Fc.index()];
        dst[..n].copy_from_slice(&mono[..n]);
      }
      Upmix::Front => {
        // 等功率：两路各 -3 dB，相加后能量与单路相同
        let k = FRAC_1_SQRT_2;
        for sp in [Speaker::Fl, Speaker::Fr] {
          let dst = &mut self.bus[sp.index()];
          for t in 0..n {
            dst[t] = mono[t] * k;
          }
        }
      }
      Upmix::Wide => {
        let kf = FRAC_1_SQRT_2;
        let ks = 0.5 * spread;
        let kb = 0.5 * spread;
        let kc = 0.5 * (1.0 - spread);
        let puts: [(Speaker, f32); 7] = [
          (Speaker::Fl, kf),
          (Speaker::Fr, kf),
          (Speaker::Fc, kc),
          (Speaker::Sl, ks),
          (Speaker::Sr, ks),
          (Speaker::Bl, kb),
          (Speaker::Br, kb),
        ];
        for (sp, k) in puts {
          let dst = &mut self.bus[sp.index()];
          for t in 0..n {
            dst[t] = mono[t] * k;
          }
        }
      }
    }
  }

  /// G（对角增益）+ 每声道延时。**顺序：先进延时线，再读延时后的值乘增益**，
  /// 这样增益变化不会被延时线"记"住（否则拖推子会听到一段延迟的旧音量）。
  fn apply_gain_and_delay(&mut self) {
    let max_delay_samples = MAX_DELAY_MS * SAMPLE_RATE as f32 / 1000.0;
    for sp in Speaker::ALL {
      let i = sp.index();
      let g = db_to_gain(self.params.gains_db[i]);
      let d_ms = self.params.delays_ms[i].clamp(0.0, MAX_DELAY_MS);
      let d = (d_ms * SAMPLE_RATE as f32 / 1000.0).clamp(0.0, max_delay_samples);
      let line = &mut self.delay[i];
      let src = &self.bus[i];
      // 先整体入线（保持时间轴连续），再逐样本读延时值
      for v in src.iter() {
        line.push(*v);
      }
      if d <= f32::EPSILON {
        continue; // 无延时：bus 里已是当前值，无需重写
      }
      let dst = &mut self.bus[i];
      for (t, cell) in dst.iter_mut().enumerate() {
        // 读的是"当前样本往前 d 个"，但 line 已推进了一个 hop，
        // 因此偏移要加上 (block - 1 - t) 把窗口退回去
        let back = d + (self.block - 1 - t) as f32;
        *cell = line.read(back) * g;
      }
    }
    // 无延时通道乘增益
    for sp in Speaker::ALL {
      let i = sp.index();
      let d_ms = self.params.delays_ms[i].clamp(0.0, MAX_DELAY_MS);
      if d_ms > f32::EPSILON {
        continue;
      }
      let g = db_to_gain(self.params.gains_db[i]);
      for cell in self.bus[i].iter_mut() {
        *cell *= g;
      }
    }
  }

  /// 清空流状态（重建会话 / 换设备时用）。
  pub fn reset(&mut self) {
    self.conv.reset();
    for line in &mut self.delay {
      for v in line.buf.iter_mut() {
        *v = 0.0;
      }
      line.w = 0;
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn impulse(n: usize) -> Vec<f32> {
    let mut v = vec![0.0; n];
    v[0] = 1.0;
    v
  }

  /// 每 `block` 个样本跑一次，返回拼接后的双耳信号。
  fn run_block(
    sp: &mut Spatial,
    blocks: usize,
    mut feed: impl FnMut(usize) -> Vec<f32>,
  ) -> (Vec<f32>, Vec<f32>) {
    let block = sp.block();
    let mut l = Vec::new();
    let mut r = Vec::new();
    let mut ol = vec![0.0; block];
    let mut or = vec![0.0; block];
    for b in 0..blocks {
      let x = feed(b);
      sp.process(&x, &mut ol, &mut or);
      l.extend_from_slice(&ol);
      r.extend_from_slice(&or);
    }
    (l, r)
  }

  /// 造一组"像样"的 HRIR：每个方向有不同的衰减常数与首达延迟，
  /// 这样块边界一旦算错，拼接处就会出现肉眼可见的台阶（单位冲激测不出来）。
  fn synthetic_hrir(taps: usize) -> Hrir {
    let mut ir = vec![0.0f32; CHANNELS * EARS * taps];
    for ch in 0..CHANNELS {
      for ear in 0..EARS {
        let base = (ch * EARS + ear) * taps;
        let onset = 3 + (ch * 2 + ear) % 7;
        let decay = 0.90 + 0.01 * ((ch + ear) % 5) as f32;
        let sign = if (ch + ear) % 2 == 0 { 1.0 } else { -1.0 };
        for t in onset..taps {
          let k = (t - onset) as f32;
          ir[base + t] = sign * 0.6 * decay.powf(k) * (k * 0.31).cos();
        }
      }
    }
    Hrir {
      sample_rate: SAMPLE_RATE,
      taps,
      ir,
      name: "synthetic".into(),
    }
  }

  /// 每 hop 跑一次，返回拼接后的双耳信号。
  fn run(
    sp: &mut Spatial,
    hops: usize,
    mut feed: impl FnMut(usize) -> Vec<f32>,
  ) -> (Vec<f32>, Vec<f32>) {
    let mut l = Vec::new();
    let mut r = Vec::new();
    let mut ol = vec![0.0; HOP];
    let mut or = vec![0.0; HOP];
    for h in 0..hops {
      let x = feed(h);
      sp.process(&x, &mut ol, &mut or);
      l.extend_from_slice(&ol);
      r.extend_from_slice(&or);
    }
    (l, r)
  }

  #[test]
  fn upmix_front_is_equal_power() {
    let mut sp = Spatial::new(Hrir::identity(8), SpatialParams::default());
    let x = vec![0.25f32; HOP];
    sp.upmix_into_bus(&x);
    let fl = &sp.bus[Speaker::Fl.index()];
    let fr = &sp.bus[Speaker::Fr.index()];
    assert!((fl[0] - 0.25 * FRAC_1_SQRT_2).abs() < 1e-6);
    assert!((fr[0] - 0.25 * FRAC_1_SQRT_2).abs() < 1e-6);
    for sp_ in [Speaker::Fc, Speaker::Bl, Speaker::Sr] {
      assert!(
        sp.bus[sp_.index()].iter().all(|v| *v == 0.0),
        "{} 应为静音",
        sp_.id()
      );
    }
  }

  #[test]
  fn upmix_wide_respects_spread() {
    let mut p = SpatialParams {
      upmix: Upmix::Wide,
      spread: 0.0,
      ..Default::default()
    };
    let mut sp = Spatial::new(Hrir::identity(8), p.clone());
    sp.upmix_into_bus(&vec![1.0; HOP]);
    assert!(
      sp.bus[Speaker::Bl.index()].iter().all(|v| *v == 0.0),
      "spread=0 时后方应静音"
    );
    assert!((sp.bus[Speaker::Fc.index()][0] - 0.5).abs() < 1e-6);

    p.spread = 1.0;
    sp.set_params(p);
    sp.upmix_into_bus(&vec![1.0; HOP]);
    assert!((sp.bus[Speaker::Bl.index()][0] - 0.5).abs() < 1e-6);
  }

  #[test]
  fn per_channel_gain_only_affects_that_direction() {
    // 单位 HRIR + Front 上混：只有 FL/FR 有信号，把 FL 拉低 6 dB → 双耳总量减半
    let mut p = SpatialParams::default();
    let mut sp = Spatial::new(Hrir::identity(8), p.clone());
    let mono = vec![0.5f32; HOP];
    let (l0, _r0) = run(&mut sp, 1, |_| mono.clone());

    p.gains_db[Speaker::Fl.index()] = -6.020_6; // 恰好 0.5
    sp.set_params(p);
    sp.reset();
    let (l1, r1) = run(&mut sp, 1, |_| mono.clone());

    let expect = (0.5 * FRAC_1_SQRT_2) * 1.5; // FR 1.0 + FL 0.5
    assert!((l1[HOP - 1] - expect).abs() < 1e-4, "左 {}", l1[HOP - 1]);
    assert!((r1[HOP - 1] - expect).abs() < 1e-4);
    assert!(l1[HOP - 1] < l0[HOP - 1], "降低前左增益后总输出应变小");
  }

  #[test]
  fn muting_a_channel_removes_its_contribution() {
    let mut p = SpatialParams {
      upmix: Upmix::Wide,
      spread: 1.0,
      ..Default::default()
    };
    let mut sp = Spatial::new(Hrir::identity(8), p.clone());
    let mono = vec![0.4f32; HOP];
    let (l0, _) = run(&mut sp, 1, |_| mono.clone());

    p.gains_db[Speaker::Bl.index()] = SILENT_DB;
    p.gains_db[Speaker::Br.index()] = SILENT_DB;
    sp.set_params(p);
    sp.reset();
    let (l1, _) = run(&mut sp, 1, |_| mono.clone());

    // 关掉后方 = 少了 2 × (0.4 × 0.5)，双耳同时减小
    let drop = 0.4 * 0.5 * 2.0;
    assert!((l0[HOP - 1] - l1[HOP - 1] - drop).abs() < 1e-4);
  }

  #[test]
  fn delay_shifts_signal_in_time() {
    let mut p = SpatialParams {
      upmix: Upmix::Center,
      ..Default::default()
    };
    // 一格 (HOP) 延时
    p.delays_ms[Speaker::Fc.index()] = HOP as f32 * 1000.0 / SAMPLE_RATE as f32;
    let mut sp = Spatial::new(Hrir::identity(8), p);
    let (l, _) = run(&mut sp, 3, |h| {
      if h == 0 {
        impulse(HOP)
      } else {
        vec![0.0; HOP]
      }
    });
    // 第 0 hop 的输出应为空，第 1 hop 的第 0 个样本才是冲激
    assert!(l[0].abs() < 1e-6, "第 0 hop 不该有输出");
    assert!(
      (l[HOP] - 1.0).abs() < 1e-4,
      "冲激应推迟一个 hop，实际 {}",
      l[HOP]
    );
  }

  #[test]
  fn planar_input_keeps_real_directions() {
    // 真 7.1：冲激只出现在 BL 上。单位 HRIR 下双耳都能听到，
    // 但把 BL 增益拉到 -60 dB 后必须彻底消失 —— 证明"每方向音量"对真 7.1 同样成立。
    let mut sp = Spatial::new(Hrir::identity(8), SpatialParams::default());
    let mut input: Vec<Vec<f32>> = vec![vec![0.0; HOP]; CHANNELS];
    input[Speaker::Bl.index()] = impulse(HOP);

    let mut l = vec![0.0f32; HOP];
    let mut r = vec![0.0f32; HOP];
    sp.process_planar(&input, &mut l, &mut r);
    assert!((l[0] - 1.0).abs() < 1e-5);
    assert!((r[0] - 1.0).abs() < 1e-5);

    let p = SpatialParams {
      gains_db: {
        let mut g = [0.0f32; CHANNELS];
        g[Speaker::Bl.index()] = -80.0;
        g
      },
      ..Default::default()
    };
    sp.set_params(p);
    sp.reset();
    sp.process_planar(&input, &mut l, &mut r);
    assert!(
      l[0].abs() < 1e-6 && r[0].abs() < 1e-6,
      "BL 静音后不应有任何输出"
    );
  }

  #[test]
  fn planar_short_input_is_zero_padded() {
    let mut sp = Spatial::new(Hrir::identity(8), SpatialParams::default());
    let input: Vec<Vec<f32>> = vec![vec![1.0; HOP]]; // 只给 1 路
    let mut l = vec![0.0f32; HOP];
    let mut r = vec![0.0f32; HOP];
    sp.process_planar(&input, &mut l, &mut r);
    assert!((l[HOP - 1] - 1.0).abs() < 1e-5);
    assert!((r[HOP - 1] - 1.0).abs() < 1e-5);
  }

  #[test]
  fn makeup_gain_is_linear() {
    let p = SpatialParams {
      makeup_db: -6.020_6,
      ..Default::default()
    };
    let mut sp = Spatial::new(Hrir::identity(8), p);
    let (l, _) = run(&mut sp, 1, |_| vec![0.5; HOP]);
    // FL 与 FR 各 0.5×0.7071，单位 HRIR 双耳都收 → 再乘 -6 dB makeup
    assert!((l[HOP - 1] - 2.0 * 0.5 * FRAC_1_SQRT_2 * 0.5).abs() < 1e-4);
  }
  #[test]
  fn block_size_does_not_change_output() {
    // 同一段信号按 64 / 128 / 256 分块，拼接结果必须逐样本一致。
    // 这条是"把块长压到 64 换低延迟"的前提：换了块长不能换声音。
    let hrir = synthetic_hrir(256);
    let params = SpatialParams {
      upmix: Upmix::Wide,
      makeup_db: -3.0,
      ..Default::default()
    };
    let n = 1536usize;
    let signal: Vec<f32> = (0..n)
      .map(|i| {
        let t = i as f32;
        0.3 * (t * 0.017).sin() + 0.2 * (t * 0.211).cos() + 0.1 * (t * 0.0007).sin()
      })
      .collect();

    let mut reference: Option<(Vec<f32>, Vec<f32>)> = None;
    for block in [64usize, 128, 256] {
      let mut sp = Spatial::with_block(hrir.clone(), params.clone(), block);
      let (l, r) = run_block(&mut sp, n / block, |b| {
        signal[b * block..(b + 1) * block].to_vec()
      });
      match &reference {
        None => reference = Some((l, r)),
        Some((rl, rr)) => {
          let dl = l
            .iter()
            .zip(rl)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0, f32::max);
          let dr = r
            .iter()
            .zip(rr)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0, f32::max);
          assert!(dl < 1e-5, "块长 {block} 与基准左声道不一致，最大差 {dl}");
          assert!(dr < 1e-5, "块长 {block} 与基准右声道不一致，最大差 {dr}");
        }
      }
    }
  }

  #[test]
  fn small_block_adds_no_algorithmic_latency() {
    // 单位 HRIR + 首样本冲激 → 第一块第 0 个样本就该出结果。
    // 如果卷积器偷偷补了一个块的延迟，这里会变成 0（而不是 1）。
    let params = SpatialParams {
      upmix: Upmix::Center,
      ..Default::default()
    };
    for block in [16usize, 64, 128, 480] {
      let mut sp = Spatial::with_block(Hrir::identity(4), params.clone(), block);
      let mut x = vec![0.0f32; block];
      x[0] = 1.0;
      let mut l = vec![0.0f32; block];
      let mut r = vec![0.0f32; block];
      sp.process(&x, &mut l, &mut r);
      assert!((l[0] - 1.0).abs() < 1e-5, "block={block} 左耳 t=0");
      assert!((r[0] - 1.0).abs() < 1e-5, "block={block} 右耳 t=0");
      assert_eq!(sp.block(), block);
      assert!((sp.block_ms() - block as f32 / 48.0).abs() < 1e-4);
    }
  }

  #[test]
  #[should_panic(expected = "超出范围")]
  fn tiny_block_is_rejected() {
    let _ = Spatial::with_block(Hrir::identity(4), SpatialParams::default(), 8);
  }

  #[test]
  fn engine_hop_480_is_accepted() {
    // 480 = 2^5 × 3 × 5，不是 2 的幂但必须能用（引擎节拍就是它）。
    let sp = Spatial::with_block(Hrir::identity(4), SpatialParams::default(), 480);
    assert_eq!(sp.block(), 480);
    assert!((sp.block_ms() - 10.0).abs() < 1e-4);
  }

  #[test]
  fn delay_works_at_custom_block() {
    // 5 ms 延时在 block=64 下仍精确（跨块读延时线，最容易写错的地方）。
    let block = 64usize;
    let mut p = SpatialParams {
      upmix: Upmix::Center,
      ..Default::default()
    };
    let d_samples = 5.0 * SAMPLE_RATE as f32 / 1000.0;
    p.delays_ms[Speaker::Fc.index()] = 5.0;
    let mut sp = Spatial::with_block(Hrir::identity(1), p, block);

    let total = 4 * block;
    let mut out = Vec::new();
    for b in 0..(total / block) {
      let mut x = vec![0.0f32; block];
      if b == 0 {
        x[0] = 1.0;
      }
      let mut l = vec![0.0f32; block];
      let mut r = vec![0.0f32; block];
      sp.process(&x, &mut l, &mut r);
      out.extend_from_slice(&l);
    }
    let at = d_samples as usize;
    assert!(
      out[at] > 0.99,
      "延时应出现在第 {at} 个样本，实际峰值在 {:?}",
      out.iter().position(|v| v.abs() > 0.5)
    );
    assert!(out[at - 2].abs() < 1e-4);
  }

  #[test]
  fn bed_path_is_sample_aligned_and_gain_only() {
    // 旁路双耳化：输入第 0 个样本必须对应输出第 0 个样本（零延迟），
    // 且结果与 HRIR 无关（这条路径根本不卷积）。
    let block = 64usize;
    let mut sp = Spatial::with_block(synthetic_hrir(256), SpatialParams::default(), block);
    let mut input: Vec<Vec<f32>> = vec![vec![0.0; block]; CHANNELS];
    for (i, ch) in input.iter_mut().enumerate() {
      ch[0] = 0.1 * (i as f32 + 1.0);
    }
    let mut out: Vec<Vec<f32>> = vec![vec![0.0; block]; CHANNELS];
    sp.process_bed(&input, &mut out);
    for i in 0..CHANNELS {
      assert!(
        (out[i][0] - input[i][0]).abs() < 1e-6,
        "声道 {i} 未被原样透传"
      );
      assert!(
        out[i][1].abs() < 1e-9,
        "出现了本不该有的拖尾（说明卷积没被跳过）"
      );
    }

    // 换一套完全不同的 HRIR，输出必须一模一样
    sp.set_hrir(Hrir::identity(8));
    let mut out2: Vec<Vec<f32>> = vec![vec![0.0; block]; CHANNELS];
    sp.process_bed(&input, &mut out2);
    for i in 0..CHANNELS {
      assert!(
        (out2[i][0] - out[i][0]).abs() < 1e-9,
        "bed 路径受了 HRIR 影响"
      );
    }
  }

  #[test]
  fn bed_path_applies_per_channel_gain() {
    let block = 64usize;
    let mut p = SpatialParams::default();
    p.gains_db[Speaker::Bl.index()] = -80.0;
    p.gains_db[Speaker::Fl.index()] = 6.0;
    let mut sp = Spatial::with_block(Hrir::identity(4), p, block);
    let input: Vec<Vec<f32>> = vec![vec![1.0; block]; CHANNELS];
    let mut out: Vec<Vec<f32>> = vec![vec![0.0; block]; CHANNELS];
    sp.process_bed(&input, &mut out);

    assert!(out[Speaker::Bl.index()][0].abs() < 1e-6, "BL 应被静音");
    assert!(
      (out[Speaker::Fl.index()][0] - 1.995_262).abs() < 1e-4,
      "FL +6 dB"
    );
    assert!(
      (out[Speaker::Br.index()][0] - 1.0).abs() < 1e-6,
      "其余通道不该动"
    );
  }

  #[test]
  fn bed_path_ignores_delay_block_boundary() {
    // 延时在 bed 路径上同样按样本精确（跨块读延时线）。
    let block = 64usize;
    let mut p = SpatialParams::default();
    p.delays_ms[Speaker::Fc.index()] = 3.0;
    let mut sp = Spatial::with_block(Hrir::identity(1), p, block);
    let at = (3.0 * SAMPLE_RATE as f32 / 1000.0) as usize;

    let mut out: Vec<Vec<f32>> = vec![Vec::new(); CHANNELS];
    for b in 0..4 {
      let mut input: Vec<Vec<f32>> = vec![vec![0.0; block]; CHANNELS];
      if b == 0 {
        input[Speaker::Fc.index()][0] = 1.0;
      }
      let mut blk: Vec<Vec<f32>> = vec![vec![0.0; block]; CHANNELS];
      sp.process_bed(&input, &mut blk);
      for i in 0..CHANNELS {
        out[i].extend_from_slice(&blk[i]);
      }
    }
    assert!(out[Speaker::Fc.index()][at] > 0.99, "3 ms 延时位置不对");
    assert!(out[Speaker::Fc.index()][0].abs() < 1e-6, "零时刻不该有信号");
  }
}
