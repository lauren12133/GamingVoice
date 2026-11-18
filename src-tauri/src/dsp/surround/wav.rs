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

//! 极简 RIFF/WAVE 读取：只服务于 HRIR 冲激响应文件，**不是通用解码器**。
//!
//! 为什么要自己写（而不是走 `symphonia`）：HRIR 需要的东西解码器普遍不友好 ——
//! 16 声道、`WAVE_FORMAT_EXTENSIBLE` + 通道掩码、24 bit PCM、32 bit float，
//! 而我们只需要「交错 f32 + 元数据」这一件事，150 行没有依赖，也没有重采样/声道折叠风险。
//!
//! 支持：PCM 16/24/32 bit、IEEE float 32/64 bit、`WAVE_FORMAT_EXTENSIBLE`（取其子格式）。

/// 一次解析出来的波形：交错 f32，样本按 `[frame][channel]` 排列。
#[derive(Clone, Debug)]
pub struct Wav {
  pub sample_rate: u32,
  pub channels: u16,
  /// `WAVE_FORMAT_EXTENSIBLE` 的通道掩码；普通 PCM 为 0（未知）。
  pub mask: u32,
  pub frames: usize,
  pub data: Vec<f32>,
}

/// 第 `frame` 帧、第 `ch` 声道的样本。
#[inline]
pub fn sample(w: &Wav, frame: usize, ch: usize) -> f32 {
  w.data[frame * w.channels as usize + ch]
}

fn u16_at(b: &[u8], o: usize) -> Result<u16, String> {
  b.get(o..o + 2)
    .map(|s| u16::from_le_bytes([s[0], s[1]]))
    .ok_or_else(|| "WAV 结构被截断（u16）".to_string())
}

fn u32_at(b: &[u8], o: usize) -> Result<u32, String> {
  b.get(o..o + 4)
    .map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    .ok_or_else(|| "WAV 结构被截断（u32）".to_string())
}

/// 解析 WAV 字节。`Err` 一律是「文件不可用」的人话原因（界面直接上屏）。
pub fn parse(bytes: &[u8]) -> Result<Wav, String> {
  if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
    return Err("不是 RIFF/WAVE 文件".to_string());
  }

  let mut fmt: Option<Fmt> = None;
  let mut data: Option<&[u8]> = None;
  let mut pos = 12usize;
  while pos + 8 <= bytes.len() {
    let id = &bytes[pos..pos + 4];
    let size = u32_at(bytes, pos + 4)? as usize;
    let body_start = pos + 8;
    let body_end = body_start.saturating_add(size).min(bytes.len());
    let body = &bytes[body_start..body_end];
    match id {
      b"fmt " => fmt = Some(Fmt::parse(body)?),
      b"data" => data = Some(body),
      _ => {} // LIST / fact / bext 等一概跳过
    }
    // chunk 按偶数字节对齐
    pos = body_start + size + (size & 1);
  }

  let fmt = fmt.ok_or_else(|| "WAV 缺少 fmt 块".to_string())?;
  let data = data.ok_or_else(|| "WAV 缺少 data 块".to_string())?;

  let ch = fmt.channels as usize;
  if ch == 0 {
    return Err("WAV 声道数为 0".to_string());
  }
  let bytes_per_sample = (fmt.bits as usize).div_ceil(8);
  if bytes_per_sample == 0 {
    return Err("WAV 位深为 0".to_string());
  }
  let frame_bytes = bytes_per_sample * ch;
  let frames = data.len() / frame_bytes;
  let mut out = Vec::with_capacity(frames * ch);

  for f in 0..frames {
    let base = f * frame_bytes;
    for c in 0..ch {
      let o = base + c * bytes_per_sample;
      let v = match (fmt.tag, bytes_per_sample) {
        (1, 1) => (data[o] as f32 - 128.0) / 128.0, // 8 bit 无符号
        (1, 2) => i16::from_le_bytes([data[o], data[o + 1]]) as f32 / 32768.0,
        (1, 3) => {
          let v =
            ((data[o + 2] as i32) << 24 | (data[o + 1] as i32) << 16 | (data[o] as i32) << 8) >> 8;
          v as f32 / 8_388_608.0
        }
        (1, 4) => {
          i32::from_le_bytes([data[o], data[o + 1], data[o + 2], data[o + 3]]) as f32
            / 2_147_483_648.0
        }
        (3, 4) => f32::from_le_bytes([data[o], data[o + 1], data[o + 2], data[o + 3]]),
        (3, 8) => f64::from_le_bytes([
          data[o],
          data[o + 1],
          data[o + 2],
          data[o + 3],
          data[o + 4],
          data[o + 5],
          data[o + 6],
          data[o + 7],
        ]) as f32,
        (tag, n) => return Err(format!("不支持的 WAV 编码（format={tag}，{n} 字节/样本）")),
      };
      out.push(v);
    }
  }

  Ok(Wav {
    sample_rate: fmt.sample_rate,
    channels: fmt.channels,
    mask: fmt.mask,
    frames,
    data: out,
  })
}

struct Fmt {
  tag: u16,
  channels: u16,
  sample_rate: u32,
  bits: u16,
  mask: u32,
}

impl Fmt {
  fn parse(b: &[u8]) -> Result<Fmt, String> {
    let mut tag = u16_at(b, 0)?;
    let channels = u16_at(b, 2)?;
    let sample_rate = u32_at(b, 4)?;
    let bits = u16_at(b, 14)?;
    let mut mask = 0u32;
    // 0xFFFE = EXTENSIBLE：真正的格式在子格式 GUID 的前 2 字节，掩码在 +20
    if tag == 0xFFFE && b.len() >= 40 {
      mask = u32_at(b, 20)?;
      let sub = u16_at(b, 24)?;
      if sub != 1 && sub != 3 {
        return Err(format!("不支持的 WAVE_FORMAT_EXTENSIBLE 子格式 {sub}"));
      }
      tag = sub;
    }
    Ok(Fmt {
      tag,
      channels,
      sample_rate,
      bits,
      mask,
    })
  }
}

/// 生成一段 f32 交错 WAV（**只用于测试与自检工具**，不参与运行时）。
#[cfg(test)]
pub(crate) fn encode_f32(sample_rate: u32, channels: u16, mask: u32, data: &[f32]) -> Vec<u8> {
  let bits = 32u16;
  let frame = channels as usize * 4;
  let data_bytes = data.len() * 4;
  let mut out = Vec::with_capacity(44 + data_bytes);
  out.extend_from_slice(b"RIFF");
  out.extend_from_slice(&((36 + data_bytes) as u32).to_le_bytes());
  out.extend_from_slice(b"WAVE");
  out.extend_from_slice(b"fmt ");
  out.extend_from_slice(&40u32.to_le_bytes());
  out.extend_from_slice(&0xFFFEu16.to_le_bytes()); // EXTENSIBLE
  out.extend_from_slice(&channels.to_le_bytes());
  out.extend_from_slice(&sample_rate.to_le_bytes());
  out.extend_from_slice(&(sample_rate * frame as u32).to_le_bytes());
  out.extend_from_slice(&(frame as u16).to_le_bytes());
  out.extend_from_slice(&bits.to_le_bytes());
  out.extend_from_slice(&22u16.to_le_bytes()); // cbSize
  out.extend_from_slice(&bits.to_le_bytes()); // valid bits
  out.extend_from_slice(&mask.to_le_bytes());
  // KSDATAFORMAT_SUBTYPE_IEEE_FLOAT
  out.extend_from_slice(&3u16.to_le_bytes());
  out.extend_from_slice(&[0, 0, 0, 0, 0x10, 0, 0x80, 0, 0, 0xaa, 0, 0x38, 0x9b, 0x71]);
  out.extend_from_slice(b"data");
  out.extend_from_slice(&(data_bytes as u32).to_le_bytes());
  for v in data {
    out.extend_from_slice(&v.to_le_bytes());
  }
  out
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn roundtrip_16ch_float() {
    let mut data = Vec::new();
    for f in 0..4 {
      for c in 0..16 {
        data.push(f as f32 * 0.1 + c as f32 * 0.01);
      }
    }
    let bytes = encode_f32(48_000, 16, 0x63F, &data);
    let w = parse(&bytes).expect("解析失败");
    assert_eq!(w.channels, 16);
    assert_eq!(w.frames, 4);
    assert_eq!(w.sample_rate, 48_000);
    assert_eq!(w.mask, 0x63F);
    for (i, v) in data.iter().enumerate() {
      assert!((w.data[i] - v).abs() < 1e-7, "第 {i} 个样本不一致");
    }
  }

  #[test]
  fn rejects_non_wav() {
    assert!(parse(b"not a wav at all").is_err());
    assert!(parse(&[]).is_err());
  }

  #[test]
  fn truncated_data_is_clamped_not_panicking() {
    let mut bytes = encode_f32(48_000, 2, 0, &[0.0; 32]);
    bytes.truncate(bytes.len() - 3); // 砍掉半个样本
    let w = parse(&bytes).expect("应当能容忍截断");
    // 32 个样本 = 16 帧（每帧 2 声道 × 4 字节），砍 3 字节后剩 125 字节 → 15 整帧
    assert_eq!(w.frames, 15, "整帧数取整");
  }
}
