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

//! 7.1 声道布局：通道序、WAVE 掩码、方位角。
//!
//! **唯一事实来源**：通道下标 0..8 的语义只在这里定义，其它模块（卷积、矩阵、
//! APO 语法映射、界面雷达）一律走 [`Speaker`]，不写字面量 5/6。
//!
//! 顺序取 Windows `SPEAKER_7POINT1_SURROUND`（WAVE mask `0x63F`）：
//! `FL FR FC LFE BL BR SL SR` —— Equalizer APO 的 `Channel:` 取值与之一一对应，
//! 因此 APO 配置可以无损映射进来（见 `compat::apo`）。

/// 7.1 的通道数。
pub const CHANNELS: usize = 8;

/// Windows `SPEAKER_7POINT1_SURROUND` 掩码。
pub const MASK_7_1: u32 = 0x63F;

/// 一个虚拟音箱（= 一条通道）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Speaker {
  /// 前左
  Fl,
  /// 前右
  Fr,
  /// 中置
  Fc,
  /// 低频效果（无方向，双耳等量）
  Lfe,
  /// 后左
  Bl,
  /// 后右
  Br,
  /// 侧左
  Sl,
  /// 侧右
  Sr,
}

impl Speaker {
  /// 按通道下标排列的全部音箱（下标 = 本数组下标）。
  pub const ALL: [Speaker; CHANNELS] = [
    Speaker::Fl,
    Speaker::Fr,
    Speaker::Fc,
    Speaker::Lfe,
    Speaker::Bl,
    Speaker::Br,
    Speaker::Sl,
    Speaker::Sr,
  ];

  /// 通道下标（0..8）。`const fn` 是为了让 `hrir::HESUVI_TRACKS` 能在编译期写好映射表。
  pub const fn index(self) -> usize {
    match self {
      Speaker::Fl => 0,
      Speaker::Fr => 1,
      Speaker::Fc => 2,
      Speaker::Lfe => 3,
      Speaker::Bl => 4,
      Speaker::Br => 5,
      Speaker::Sl => 6,
      Speaker::Sr => 7,
    }
  }

  /// 稳定 id（配置、i18n、调试接口都用它，不要用下标）。
  pub fn id(self) -> &'static str {
    match self {
      Speaker::Fl => "FL",
      Speaker::Fr => "FR",
      Speaker::Fc => "FC",
      Speaker::Lfe => "LFE",
      Speaker::Bl => "BL",
      Speaker::Br => "BR",
      Speaker::Sl => "SL",
      Speaker::Sr => "SR",
    }
  }

  /// 方位角（度）：0 = 正前，正 = 右侧（俯视顺时针）。LFE 返回 0（无方向）。
  ///
  /// 数值取 ITU-R BS.775 / Dolby 7.1 的常规摆位：前置 ±30°，侧置 ±90°，后置 ±140°。
  pub fn azimuth_deg(self) -> f32 {
    match self {
      Speaker::Fl => -30.0,
      Speaker::Fr => 30.0,
      Speaker::Fc => 0.0,
      Speaker::Lfe => 0.0,
      Speaker::Bl => -140.0,
      Speaker::Br => 140.0,
      Speaker::Sl => -90.0,
      Speaker::Sr => 90.0,
    }
  }

  /// 有效值范围（Hz）：LFE 只有 0–120 Hz 分量，其余视为全频带。
  pub fn is_lfe(self) -> bool {
    self == Speaker::Lfe
  }

  /// 按 id 反查（大小写不敏感，接受 `LFE`/`lfe`）。
  pub fn from_id(s: &str) -> Option<Speaker> {
    let up = s.trim().to_ascii_uppercase();
    Speaker::ALL.into_iter().find(|sp| sp.id() == up)
  }
}
