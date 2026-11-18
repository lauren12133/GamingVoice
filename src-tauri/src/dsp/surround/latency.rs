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

//! 端到端延迟预算：**把"感觉慢"拆成可加减的几段**。
//!
//! 竞技场景的判断标准（社区共识的量级，非精确阈值）：
//! 视觉→听觉总延迟 20 ms 以内基本无感，40 ms 以上开始有人察觉"声画不同步"，
//! 60 ms 以上明显。**音效本身不需要这么低，是"枪口火光 vs 枪声"在打架。**
//!
//! 这条链路里，唯一不可压缩的是 Windows 音频引擎的周期（48 kHz 下通常 480 帧 = 10 ms）。
//! Equalizer APO 之所以"只有 10 ms"，不是因为算法快，而是因为：
//!
//! 1. 它**住在引擎里**（audiodg 进程内的 APO），没有"采集→回传"这一趟；
//! 2. 它的滤波是**逐样本 IIR**，不攒块，块延迟 = 0。
//!
//! 反过来，任何用户态方案都要付两笔钱：**一笔采集周期**（把声音从端点捞回来）
//! 和**一笔块延迟**（卷积必须攒满一块才能算）。本模块就是把这两笔钱算清楚，
//! 据此给出三档预设，而不是拍脑袋说"优化一下就快了"。
//!
//! ```text
//! 游戏 ──► 端点/声卡 ──► 采集 ──► 块处理 ──► 渲染 ──► 耳机
//!              │            │        │          │
//!         cable_us    capture_us  block_us  render_us
//! ```
//!
//! 参考实现见 `README.md` 的实测表格与 `LATENCY.md` 的取舍分析。

use crate::audio::{HOP, SAMPLE_RATE};

/// 共享模式默认周期：480 帧 @ 48 kHz。
pub const SHARED_PERIOD_US: u32 = 10_000;
/// `IAudioClient3` 最小周期：128 帧 @ 48 kHz（设备支持时才拿得到）。
///
/// 注意：这是**共享模式**下的最小值，不是所有设备都支持 128；拿不到时会自动退到
/// `GetSharedModeEnginePeriod` 报的实际值，所以模型里一律当"最好情况"。
pub const MIN_PERIOD_US: u32 = 2_667;
/// VB-Cable 内部缓冲的典型值。
///
/// 它的 Control Panel 里 `Internal buffer` 可调（默认 7168 samples ≈ 149 ms 是**上限**，
/// 实际按流格式协商；实测 10–25 ms 区间，调小会爆音）。取 15 ms 作为默认估计。
pub const VB_CABLE_TYPICAL_US: u32 = 15_000;

/// 链路形态。延迟差别几乎全在这一项上，而不是卷积。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Chain {
  /// 游戏 → 虚拟声卡（8 声道端点）→ 回环采集 → PureVox → 耳机。
  ///
  /// 多付一笔虚拟声卡的内部缓冲（P1 方案就是这条）。
  VirtualCable,
  /// 游戏 → 物理端点 → 回环采集（同端点）→ PureVox → 耳机。
  ///
  /// 省掉虚拟声卡，但要接受"系统声音也会被处理"。
  Loopback,
  /// 游戏 → PureVox（APO / spatial bed 客户端，进程内）→ 端点 → 耳机。
  ///
  /// 没有采集，没有跨进程回传；只有渲染周期（+ 卷积块，若启用）。
  InProcess,
}

impl Chain {
  pub fn id(self) -> &'static str {
    match self {
      Chain::VirtualCable => "virtual-cable",
      Chain::Loopback => "loopback",
      Chain::InProcess => "in-process",
    }
  }
}

/// 一条链路的延迟构成（单位：微秒，整数避免浮点漂移）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LatencyModel {
  /// 虚拟声卡内部缓冲（无虚拟声卡时 0）。
  pub cable_us: u32,
  /// 采集周期（`InProcess` 时 0）。
  pub capture_us: u32,
  /// 处理块（卷积块长；纯 IIR/EQ 时为 0）。
  ///
  /// **这是唯一可以靠"换块长"直接砍掉的一项**，下限是一块。
  pub block_us: u32,
  /// 渲染周期。
  pub render_us: u32,
  /// 时钟漂移与驱动安全余量（重采样、双缓冲取整）。
  pub safety_us: u32,
}

impl LatencyModel {
  /// 现状：虚拟声卡 + 10 ms 引擎块 + 共享模式渲染。
  ///
  /// 这套组合的实测区间就是 40–50 ms，模型取中值 45 ms。
  pub fn virtual_cable(cable_us: u32, block: usize) -> LatencyModel {
    LatencyModel {
      cable_us,
      capture_us: SHARED_PERIOD_US,
      block_us: block_us(block),
      render_us: SHARED_PERIOD_US,
      safety_us: 2_000,
    }
  }

  /// 无虚拟声卡：省掉 `cable_us`，其余不变。
  pub fn loopback(block: usize) -> LatencyModel {
    LatencyModel {
      cable_us: 0,
      ..LatencyModel::virtual_cable(0, block)
    }
  }

  /// 进程内（APO / spatial bed）：没有采集、没有跨进程回传。
  ///
  /// `block = 0` 表示关键路径上**没有卷积**（只做每声道增益与 biquad EQ），
  /// 这正是"和 Equalizer APO 同级"的形态。
  pub fn in_process(block: usize) -> LatencyModel {
    LatencyModel {
      cable_us: 0,
      capture_us: 0,
      block_us: block_us(block),
      render_us: SHARED_PERIOD_US,
      safety_us: 1_000,
    }
  }

  /// 逐项拆解（上屏 / 打日志用，顺序即信号流经顺序）。
  pub fn stages(&self) -> [(&'static str, u32); 5] {
    [
      ("虚拟声卡缓冲", self.cable_us),
      ("采集周期", self.capture_us),
      ("处理块", self.block_us),
      ("渲染周期", self.render_us),
      ("安全余量", self.safety_us),
    ]
  }

  pub fn total_us(&self) -> u32 {
    self.cable_us + self.capture_us + self.block_us + self.render_us + self.safety_us
  }

  pub fn total_ms(&self) -> f32 {
    self.total_us() as f32 / 1000.0
  }

  /// 竞技可用性判断（阈值取 20 / 40 ms，见模块文档）。
  pub fn verdict(&self) -> Verdict {
    match self.total_us() {
      t if t <= 20_000 => Verdict::Competitive,
      t if t <= 40_000 => Verdict::Playable,
      _ => Verdict::Sluggish,
    }
  }
}

/// 竞技可用性。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
  /// ≤ 20 ms：声画基本同步，可用于 FPS。
  Competitive,
  /// 20–40 ms：能玩，但对枪时能感觉到一点点滞后。
  Playable,
  /// > 40 ms：明显不同步，不适合对抗性场景。
  Sluggish,
}

impl Verdict {
  pub fn id(self) -> &'static str {
    match self {
      Verdict::Competitive => "competitive",
      Verdict::Playable => "playable",
      Verdict::Sluggish => "sluggish",
    }
  }
}

/// 三档预设。UI 上就是一个下拉框，背后对应完全不同的链路形态。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Preset {
  /// 兼容优先：VB-Cable + 引擎块。功能最全（真 8 声道、可换 HRIR），延迟最高。
  Compatibility,
  /// 均衡：去掉虚拟声卡，块长压到 128。
  Balanced,
  /// 竞技：进程内、关键路径无卷积（双耳化交给 spatializer）。
  Competitive,
}

impl Preset {
  pub fn id(self) -> &'static str {
    match self {
      Preset::Compatibility => "compatibility",
      Preset::Balanced => "balanced",
      Preset::Competitive => "competitive",
    }
  }

  /// 预设对应的模型（块长写死为各档最优值）。
  pub fn model(self) -> LatencyModel {
    match self {
      // 480 = 引擎 hop，不改节拍，最稳
      Preset::Compatibility => LatencyModel::virtual_cable(VB_CABLE_TYPICAL_US, HOP),
      // 128 = 2.67 ms 块，独占一个 cpu 核的余量足够
      Preset::Balanced => LatencyModel::loopback(128),
      // 0 = 关键路径不做卷积
      Preset::Competitive => LatencyModel::in_process(0),
    }
  }

  pub fn all() -> [Preset; 3] {
    [Preset::Compatibility, Preset::Balanced, Preset::Competitive]
  }
}

/// 块长 → 微秒。`block = 0` 表示该链路不做块处理。
pub fn block_us(block: usize) -> u32 {
  if block == 0 {
    return 0;
  }
  (block as u64 * 1_000_000 / SAMPLE_RATE as u64) as u32
}

/// 给定块长，求"结构上可能的最小总延迟"（用于界面上那句"理论下限"）。
///
/// 唯一变量是 `render_us`：拿到 `IAudioClient3` 最小周期时才能用 [`MIN_PERIOD_US`]。
pub fn floor_us(block: usize, min_period: bool) -> u32 {
  let render = if min_period {
    MIN_PERIOD_US
  } else {
    SHARED_PERIOD_US
  };
  block_us(block) + render
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn current_chain_matches_observed_40_to_50ms() {
    // 这条用例是"锚"：它把"用户实测 40–50 ms"钉进模型，
    // 以后谁改常数导致模型不再解释实测，测试立刻红。
    let m = LatencyModel::virtual_cable(VB_CABLE_TYPICAL_US, HOP);
    assert!(
      (40.0..=50.0).contains(&m.total_ms()),
      "现状模型 {} ms 落在 40–50 之外",
      m.total_ms()
    );
  }

  #[test]
  fn stages_sum_to_total() {
    for p in Preset::all() {
      let m = p.model();
      let sum: u32 = m.stages().iter().map(|(_, us)| *us).sum();
      assert_eq!(sum, m.total_us(), "{} 分项之和与总数不一致", p.id());
    }
  }

  #[test]
  fn presets_are_ordered_by_latency() {
    let c = Preset::Compatibility.model().total_us();
    let b = Preset::Balanced.model().total_us();
    let k = Preset::Competitive.model().total_us();
    assert!(c > b && b > k, "预设必须严格递减：{c} / {b} / {k}");
  }

  #[test]
  fn competitive_preset_is_competitive() {
    let m = Preset::Competitive.model();
    assert_eq!(m.verdict(), Verdict::Competitive);
    // 进程内 + 无卷积 = 一个渲染周期 + 安全余量，不该超过 12 ms
    assert!(m.total_ms() <= 12.0, "竞技档 {} ms", m.total_ms());
    // 与 Equalizer APO 同档（差一个安全余量）
    assert!(
      m.total_ms() < LatencyModel::virtual_cable(VB_CABLE_TYPICAL_US, HOP).total_ms() / 3.0,
      "竞技档太慢，比现状没有明显改善"
    );
  }

  #[test]
  fn convolution_block_is_the_only_block_cost() {
    // 关键路径不做卷积（block=0）时，块延迟必须是 0 —— 不是"很小"，是"没有"。
    assert_eq!(block_us(0), 0);
    let a = LatencyModel::in_process(0).total_us();
    let b = LatencyModel::in_process(128).total_us();
    assert_eq!(b - a, block_us(128));
  }

  #[test]
  fn dropping_virtual_cable_saves_exactly_the_cable_buffer() {
    let with = LatencyModel::virtual_cable(VB_CABLE_TYPICAL_US, HOP).total_us();
    let without = LatencyModel::loopback(HOP).total_us();
    assert_eq!(with - without, VB_CABLE_TYPICAL_US);
  }

  #[test]
  fn smaller_block_buys_time_linearly() {
    // 480 → 128 省 7.33 ms；128 → 64 再省 1.33 ms。收益递减，但都不是零。
    let d1 = block_us(HOP) - block_us(128);
    let d2 = block_us(128) - block_us(64);
    assert_eq!(d1, 7_334); // 480→128 是 10.0 − 2.666 ms
    assert_eq!(d2, 1_333);
    assert!(d2 < d1);
  }

  #[test]
  fn floor_is_render_period_plus_block() {
    assert_eq!(floor_us(0, false), SHARED_PERIOD_US);
    assert_eq!(floor_us(0, true), MIN_PERIOD_US);
    assert_eq!(floor_us(64, true), MIN_PERIOD_US + 1_333);
  }
}
