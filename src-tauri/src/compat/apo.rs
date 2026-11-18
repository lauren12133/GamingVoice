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

//! 与 **Equalizer APO** 的配置互操作：读 `config.txt`（以及 HeSuVi 写进去的
//! `hesuvi.txt` 里那部分语法），把它翻译成本引擎的空间域参数。
//!
//! 这是"把 APO 集合进来"的实际含义：**不是去调用 APO 的 DLL**（那要装驱动、要管理员、
//! 且 Windows 专属），而是把它的**配置语义**变成我们自己的 DSP 参数 —— 用户已有的
//! ArtIsWar / ArtTune 那套 `config.txt` 可以原样导入，链路却跑在 PureVox 内部。
//!
//! ## 支持的子集（其余一律进 `warnings`，绝不静默）
//!
//! | 指令 | 处理方式 |
//! | --- | --- |
//! | `Preamp: X dB` | 无 `Channel:` 时为全局前级；有则叠加到该通道 |
//! | `Channel: L/R/C/SUB/BL/BR/SL/SR/ALL` | 设定后续指令的作用通道 |
//! | `Filter: ON PK/LS/HS/LP/HP/NO/AP/BP Fc … Gain … Q …` | 收集为 [`ApoFilter`]，交给 31 段 EQ 之外的参量段落地 |
//! | `Copy: L = 1.0*L + 0.8*BL` | 逐行求值成 8×8 混音矩阵（APO 的赋值语义） |
//! | `Include: 文件` | 记录下来由调用方按顺序递归（本模块不碰文件系统） |
//! | `Convolution: 文件` | 记为 HRIR 来源路径 |
//! | `GraphicEQ: 20 -3; …` | 收成 (Hz, dB) 断点，供 EQ 面板画线 |
//!
//! **不做**：`Device:` / `Stage:` / `Channel: ALL` 之外的组合、`Copy` 的延迟语法、
//! VST 插件行 —— 这些在 APO 里本身就是特例，导入时明确报出来比"以为导入了"安全。

use std::f32::consts::FRAC_1_SQRT_2;

use crate::dsp::surround::layout::{Speaker, CHANNELS};
use crate::dsp::surround::MixMatrix;

/// APO 的滤波器类型（`PK` / `LS` / `HS` / `LP` / `HP` / `NO` / `AP` / `BP`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FilterKind {
  Peaking,
  LowShelf,
  HighShelf,
  LowPass,
  HighPass,
  Notch,
  AllPass,
  BandPass,
}

impl FilterKind {
  fn parse(tok: &str) -> Option<FilterKind> {
    Some(match tok.to_ascii_uppercase().as_str() {
      "PK" => FilterKind::Peaking,
      "LS" | "LSC" => FilterKind::LowShelf,
      "HS" | "HSC" => FilterKind::HighShelf,
      "LP" | "LPQ" => FilterKind::LowPass,
      "HP" | "HPQ" => FilterKind::HighPass,
      "NO" => FilterKind::Notch,
      "AP" => FilterKind::AllPass,
      "BP" => FilterKind::BandPass,
      _ => return None,
    })
  }

  /// 回写 config.txt 时用的记号。
  pub fn id(self) -> &'static str {
    match self {
      FilterKind::Peaking => "PK",
      FilterKind::LowShelf => "LS",
      FilterKind::HighShelf => "HS",
      FilterKind::LowPass => "LP",
      FilterKind::HighPass => "HP",
      FilterKind::Notch => "NO",
      FilterKind::AllPass => "AP",
      FilterKind::BandPass => "BP",
    }
  }
}

/// 一条参量滤波器。
#[derive(Clone, Debug, PartialEq)]
pub struct ApoFilter {
  pub kind: FilterKind,
  /// `None` = 作用于整条总线（APO 未指定 `Channel:` 时就是这个意思）。
  pub channel: Option<Speaker>,
  pub fc_hz: f32,
  pub gain_db: f32,
  pub q: f32,
}

/// 一次导入的结果。**永远返回结构体**（不是 `Result`）：APO 配置里几乎必然有我们
/// 不支持的行，直接失败等于"整份配置白导"；把问题收进 `warnings` 由界面展示才是可用行为。
#[derive(Clone, Debug, Default)]
pub struct ApoImport {
  /// 全局前级（dB）
  pub preamp_db: f32,
  /// 每通道前级（dB）—— `Channel: X` + `Preamp:` 的结果
  pub channel_gain_db: [f32; CHANNELS],
  /// `Copy:` 行求值出的混音矩阵（默认单位阵）
  pub mix: MixMatrix,
  pub filters: Vec<ApoFilter>,
  /// `Convolution:` 指向的文件（HeSuVi 的 hesuvi.txt 就是这样指定 HRIR 的）
  pub convolution: Option<String>,
  /// `Include:` 列表（保持出现顺序；递归展开由调用方负责）
  pub includes: Vec<String>,
  /// `GraphicEQ:` 的断点
  pub graphic_eq: Vec<(f32, f32)>,
  /// 人话问题清单（不致命）
  pub warnings: Vec<String>,
}

impl ApoImport {
  /// 把 `channel_gain_db` 与矩阵对角线合成为"每方向音量"（dB）。
  ///
  /// 只有纯对角配置（最常见的用法：`Channel: BL` + `Preamp`）才严格等价；
  /// 有非对角项时这一步是近似，界面应提示"该配置含混音，建议整体导入"。
  pub fn diagonal_gain_db(&self) -> [f32; CHANNELS] {
    let mut out = [0.0f32; CHANNELS];
    for (i, cell) in out.iter_mut().enumerate() {
      let d = self.mix.rows[i][i];
      let db = if d.abs() > 1e-6 {
        20.0 * d.abs().log10()
      } else {
        -120.0
      };
      *cell = self.preamp_db + self.channel_gain_db[i] + db;
    }
    out
  }

  /// 配置里是否出现了非对角混音（决定界面要不要提示"这是混音矩阵，不是纯增益"）。
  pub fn has_cross_mix(&self) -> bool {
    (0..CHANNELS).any(|i| (0..CHANNELS).any(|j| i != j && self.mix.rows[i][j].abs() > 1e-9))
  }
}

/// 通道名 → [`Speaker`]。同时接受 APO 的 `RL/RR`（旧称）与 `SUB`。
fn channel_of(name: &str) -> Option<Speaker> {
  match name.trim().to_ascii_uppercase().as_str() {
    "L" | "FL" => Some(Speaker::Fl),
    "R" | "FR" => Some(Speaker::Fr),
    "C" | "FC" => Some(Speaker::Fc),
    "SUB" | "LFE" => Some(Speaker::Lfe),
    "BL" | "RL" => Some(Speaker::Bl),
    "BR" | "RR" => Some(Speaker::Br),
    "SL" => Some(Speaker::Sl),
    "SR" => Some(Speaker::Sr),
    _ => None,
  }
}

fn parse_db(s: &str) -> Option<f32> {
  let t = s.trim();
  let t = t
    .strip_suffix("dB")
    .or_else(|| t.strip_suffix("db"))
    .unwrap_or(t)
    .trim();
  t.trim_start_matches('+').parse::<f32>().ok()
}

/// 解析一份 APO 配置文本。
pub fn parse(text: &str) -> ApoImport {
  let mut out = ApoImport::default();
  let mut current: Option<Speaker> = None;
  // `Channel: ALL` 之后 current 回到 None（= 全部通道）

  for (lineno, raw) in text.lines().enumerate() {
    let no = lineno + 1;
    let line = match raw.find('#') {
      Some(i) => &raw[..i],
      None => raw,
    }
    .trim();
    if line.is_empty() {
      continue;
    }
    let Some((key, rest)) = line.split_once(':') else {
      out.warnings.push(format!(
        "第 {no} 行：不是 `指令: 参数` 形式，已跳过（{line}）"
      ));
      continue;
    };
    let key = key.trim().to_ascii_lowercase();
    let rest = rest.trim();

    match key.as_str() {
      "preamp" => match parse_db(rest) {
        Some(db) => match current {
          Some(sp) => out.channel_gain_db[sp.index()] += db,
          None => out.preamp_db += db,
        },
        None => out
          .warnings
          .push(format!("第 {no} 行：Preamp 数值无法解析（{rest}）")),
      },
      "channel" => {
        let up = rest.to_ascii_uppercase();
        if up == "ALL" {
          current = None;
        } else {
          match channel_of(rest) {
            Some(sp) => current = Some(sp),
            None => out.warnings.push(format!(
              "第 {no} 行：未知通道 `{rest}`（支持 L R C SUB LFE BL BR SL SR RL RR ALL）"
            )),
          }
        }
      }
      "filter" => match parse_filter(rest, current) {
        Ok(Some(f)) => out.filters.push(f),
        Ok(None) => {}
        Err(e) => out.warnings.push(format!("第 {no} 行：{e}")),
      },
      "copy" => match parse_copy(rest, &mut out.mix) {
        Ok(()) => {}
        Err(e) => out.warnings.push(format!("第 {no} 行：{e}")),
      },
      "include" => {
        if rest.is_empty() {
          out.warnings.push(format!("第 {no} 行：Include 没有文件名"));
        } else {
          out.includes.push(rest.to_string());
        }
      }
      "convolution" => {
        if rest.is_empty() {
          out
            .warnings
            .push(format!("第 {no} 行：Convolution 没有文件名"));
        } else {
          out.convolution = Some(rest.to_string());
        }
      }
      "graphiceq" => {
        for part in rest.split(';') {
          let mut it = part.split_whitespace();
          if let (Some(f), Some(g)) = (it.next(), it.next()) {
            if let (Ok(f), Ok(g)) = (f.parse::<f32>(), g.parse::<f32>()) {
              out.graphic_eq.push((f, g));
            }
          }
        }
      }
      other => out
        .warnings
        .push(format!("第 {no} 行：不支持的指令 `{other}`")),
    }
  }

  if out.preamp_db < -60.0 {
    out.warnings.push(format!(
      "全局前级 {} dB 会让输出基本静音，确认是否写错",
      out.preamp_db
    ));
  }
  out
}

/// `ON PK Fc 1000 Hz Gain -3 dB Q 1.4` / `OFF PK …`
fn parse_filter(rest: &str, channel: Option<Speaker>) -> Result<Option<ApoFilter>, String> {
  let toks: Vec<&str> = rest.split_whitespace().collect();
  if toks.is_empty() {
    return Err("Filter 没有内容".to_string());
  }
  let enabled = match toks[0].to_ascii_uppercase().as_str() {
    "ON" => true,
    "OFF" => false,
    other => return Err(format!("Filter 第一项应为 ON/OFF，实际 `{other}`")),
  };
  let kind = toks
    .get(1)
    .and_then(|t| FilterKind::parse(t))
    .ok_or_else(|| format!("不支持的滤波器类型 `{}`", toks.get(1).unwrap_or(&"")))?;

  let (mut fc, mut gain, mut q) = (1_000.0f32, 0.0f32, FRAC_1_SQRT_2);
  let mut i = 2;
  while i < toks.len() {
    let key = toks[i].to_ascii_lowercase();
    let val = toks.get(i + 1).and_then(|s| s.parse::<f32>().ok());
    match (key.as_str(), val) {
      ("fc", Some(v)) => {
        fc = v;
        i += 2;
        // 吃掉可选的单位
        if let Some(u) = toks.get(i) {
          if u.eq_ignore_ascii_case("hz") || u.eq_ignore_ascii_case("khz") {
            if u.eq_ignore_ascii_case("khz") {
              fc *= 1000.0;
            }
            i += 1;
          }
        }
      }
      ("gain", Some(v)) => {
        gain = v;
        i += 2;
        if let Some(u) = toks.get(i) {
          if u.eq_ignore_ascii_case("db") {
            i += 1;
          }
        }
      }
      ("q", Some(v)) => {
        q = v;
        i += 2;
      }
      // `BW Oct 1.0` 之类：跳过两个 token
      ("bw", Some(_)) => i += 4,
      _ => i += 1,
    }
  }

  if !enabled {
    return Ok(None);
  }
  Ok(Some(ApoFilter {
    kind,
    channel,
    fc_hz: fc.clamp(1.0, 24_000.0),
    gain_db: gain.clamp(-60.0, 60.0),
    q: q.clamp(0.05, 40.0),
  }))
}

/// `L = 1.0*L + 0.8*BL`（APO 的赋值语义：先算右侧，再整行覆盖）
fn parse_copy(rest: &str, mix: &mut MixMatrix) -> Result<(), String> {
  let (target, expr) = rest
    .split_once('=')
    .ok_or_else(|| format!("Copy 缺少 `=`（{rest}）"))?;
  let ti = channel_of(target)
    .ok_or_else(|| format!("Copy 目标通道未知 `{}`", target.trim()))?
    .index();

  let mut row = [0.0f32; CHANNELS];
  for term in expr.split('+') {
    let term = term.trim();
    if term.is_empty() {
      continue;
    }
    let (coef, name) = match term.split_once('*') {
      Some((c, n)) => (
        c.trim()
          .parse::<f32>()
          .map_err(|_| format!("Copy 系数无法解析 `{}`", c.trim()))?,
        n.trim(),
      ),
      None => (1.0, term),
    };
    let si = channel_of(name).ok_or_else(|| format!("Copy 源通道未知 `{name}`"))?;
    row[si.index()] += coef;
  }
  mix.rows[ti] = row;
  Ok(())
}

/// 反向导出：生成一份**只含本引擎能表达的内容**的 config.txt。
///
/// 用途：用户想回到 APO/HeSuVi 继续折腾时，我们给的东西必须是自洽的，
/// 而不是"导入就丢一半"。
pub fn export(cfg: &ApoImport) -> String {
  let mut s = String::new();
  s.push_str("# PureVox 导出：可由 Equalizer APO 直接读取\n");
  if cfg.preamp_db.abs() > 1e-6 {
    s.push_str(&format!("Preamp: {:+.1} dB\n", cfg.preamp_db));
  }
  for sp in Speaker::ALL {
    let g = cfg.channel_gain_db[sp.index()];
    if g.abs() > 1e-6 {
      let name = match sp {
        Speaker::Fl => "L",
        Speaker::Fr => "R",
        Speaker::Fc => "C",
        Speaker::Lfe => "SUB",
        Speaker::Bl => "BL",
        Speaker::Br => "BR",
        Speaker::Sl => "SL",
        Speaker::Sr => "SR",
      };
      s.push_str(&format!("Channel: {name}\nPreamp: {g:+.1} dB\n"));
    }
  }
  s.push_str("Channel: ALL\n");

  if !cfg.mix.is_identity() {
    for sp in Speaker::ALL {
      let i = sp.index();
      let row = &cfg.mix.rows[i];
      let same = (0..CHANNELS).all(|j| {
        let want = if i == j { 1.0 } else { 0.0 };
        (row[j] - want).abs() < 1e-9
      });
      if same {
        continue;
      }
      let dst = match sp {
        Speaker::Fl => "L",
        Speaker::Fr => "R",
        Speaker::Fc => "C",
        Speaker::Lfe => "SUB",
        Speaker::Bl => "BL",
        Speaker::Br => "BR",
        Speaker::Sl => "SL",
        Speaker::Sr => "SR",
      };
      let mut terms = Vec::new();
      for (j, k) in row.iter().enumerate() {
        if k.abs() < 1e-9 {
          continue;
        }
        let src = match Speaker::ALL[j] {
          Speaker::Fl => "L",
          Speaker::Fr => "R",
          Speaker::Fc => "C",
          Speaker::Lfe => "SUB",
          Speaker::Bl => "BL",
          Speaker::Br => "BR",
          Speaker::Sl => "SL",
          Speaker::Sr => "SR",
        };
        terms.push(format!("{k:.3}*{src}"));
      }
      if !terms.is_empty() {
        s.push_str(&format!("Copy: {dst} = {}\n", terms.join(" + ")));
      }
    }
  }

  for f in &cfg.filters {
    if let Some(sp) = f.channel {
      let name = match sp {
        Speaker::Fl => "L",
        Speaker::Fr => "R",
        Speaker::Fc => "C",
        Speaker::Lfe => "SUB",
        Speaker::Bl => "BL",
        Speaker::Br => "BR",
        Speaker::Sl => "SL",
        Speaker::Sr => "SR",
      };
      s.push_str(&format!("Channel: {name}\n"));
    }
    s.push_str(&format!(
      "Filter: ON {} Fc {:.0} Hz Gain {:+.2} dB Q {:.3}\n",
      f.kind.id(),
      f.fc_hz,
      f.gain_db,
      f.q
    ));
    if f.channel.is_some() {
      s.push_str("Channel: ALL\n");
    }
  }

  if let Some(c) = &cfg.convolution {
    s.push_str(&format!("Convolution: {c}\n"));
  }
  for inc in &cfg.includes {
    s.push_str(&format!("Include: {inc}\n"));
  }
  s
}

#[cfg(test)]
mod tests {
  use super::*;

  const SAMPLE: &str = "\
# ArtIsWar 风格的配置片段
Preamp: -6 dB
Channel: BL
Preamp: +3 dB
Channel: ALL
Filter: ON PK Fc 1000 Hz Gain -3 dB Q 1.4
Copy: L = 1.0*L + 0.8*BL
Copy: R = 1.0*R + 0.8*BR
Include: ArtTuneDBboost.txt
Convolution: hrir/atmos.wav
";

  #[test]
  fn parses_the_usual_suspects() {
    let a = parse(SAMPLE);
    assert!((a.preamp_db + 6.0).abs() < 1e-3);
    assert!((a.channel_gain_db[Speaker::Bl.index()] - 3.0).abs() < 1e-3);
    assert_eq!(a.filters.len(), 1);
    let f = &a.filters[0];
    assert_eq!(f.kind, FilterKind::Peaking);
    assert!((f.fc_hz - 1000.0).abs() < 1e-3);
    assert!((f.gain_db + 3.0).abs() < 1e-3);
    assert!((f.q - 1.4).abs() < 1e-4);
    assert!(f.channel.is_none(), "Channel: ALL 之后不应带通道");
    assert_eq!(a.includes, vec!["ArtTuneDBboost.txt".to_string()]);
    assert_eq!(a.convolution.as_deref(), Some("hrir/atmos.wav"));
    assert!(a.warnings.is_empty(), "{:?}", a.warnings);
  }

  #[test]
  fn copy_builds_the_mix_matrix() {
    let a = parse(SAMPLE);
    let fl = Speaker::Fl.index();
    let bl = Speaker::Bl.index();
    assert!((a.mix.rows[fl][fl] - 1.0).abs() < 1e-6);
    assert!((a.mix.rows[fl][bl] - 0.8).abs() < 1e-6);
    assert!((a.mix.rows[Speaker::Fr.index()][Speaker::Br.index()] - 0.8).abs() < 1e-6);
    assert!(a.has_cross_mix());
  }

  #[test]
  fn copy_is_sequential_assignment() {
    // 第二行引用第一行改过的 L：APO 是逐行赋值，故 L 此时已含 0.8*BL
    let a = parse("Copy: L = 1.0*L + 0.8*BL\nCopy: R = 0.5*L\n");
    let fl = Speaker::Fl.index();
    let fr = Speaker::Fr.index();
    assert!((a.mix.rows[fr][fl] - 0.5).abs() < 1e-6);
    assert!((a.mix.rows[fl][Speaker::Bl.index()] - 0.8).abs() < 1e-6);
  }

  #[test]
  fn unknown_lines_become_warnings_not_failures() {
    let a = parse("Stage: pre-mix\nDevice: CABLE Input\nPreamp: -1 dB\n");
    assert!((a.preamp_db + 1.0).abs() < 1e-6);
    assert_eq!(a.warnings.len(), 2);
    assert!(a.warnings[0].contains("stage"));
  }

  #[test]
  fn channel_all_resets_selection() {
    let a = parse("Channel: SL\nPreamp: 2 dB\nChannel: ALL\nPreamp: 1 dB\n");
    assert!((a.channel_gain_db[Speaker::Sl.index()] - 2.0).abs() < 1e-6);
    assert!((a.preamp_db - 1.0).abs() < 1e-6);
  }

  #[test]
  fn diagonal_gain_combines_preamp_and_matrix() {
    let a = parse("Preamp: -6 dB\nChannel: BL\nPreamp: +6 dB\nCopy: BL = 0.5*BL\n");
    let g = a.diagonal_gain_db();
    // BL: -6 (preamp) + 6 (channel) + 20log10(0.5) = -6.02
    assert!((g[Speaker::Bl.index()] + 6.020_6).abs() < 1e-3, "{:?}", g);
    // 其它通道只有全局前级
    assert!((g[Speaker::Fl.index()] + 6.0).abs() < 1e-6);
  }

  #[test]
  fn export_roundtrips() {
    let a = parse(SAMPLE);
    let text = export(&a);
    let b = parse(&text);
    assert!((b.preamp_db - a.preamp_db).abs() < 0.1);
    for sp in Speaker::ALL {
      let i = sp.index();
      assert!(
        (b.channel_gain_db[i] - a.channel_gain_db[i]).abs() < 0.1,
        "{} 通道增益不一致",
        sp.id()
      );
      for j in 0..CHANNELS {
        assert!(
          (b.mix.rows[i][j] - a.mix.rows[i][j]).abs() < 2e-3,
          "矩阵 {i},{j} 不一致"
        );
      }
    }
    assert_eq!(b.filters.len(), a.filters.len());
    assert_eq!(b.convolution, a.convolution);
  }

  #[test]
  fn invalid_values_are_clamped_with_reason() {
    let a = parse("Filter: ON PK Fc 0 Hz Gain 200 dB Q 0\n");
    let f = &a.filters[0];
    assert!(f.fc_hz >= 1.0);
    assert!(f.gain_db <= 60.0);
    assert!(f.q >= 0.05);
  }

  #[test]
  fn disabled_filter_is_skipped() {
    let a = parse("Filter: OFF PK Fc 100 Hz Gain 10 dB Q 1\n");
    assert!(a.filters.is_empty());
    assert!(a.warnings.is_empty());
  }
}
