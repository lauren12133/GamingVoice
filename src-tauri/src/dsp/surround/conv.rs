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

//! 8 进 2 出的分块卷积（uniformly-partitioned overlap-save）。
//!
//! 这是 HeSuVi 那一步的等价物：把 8 条虚拟音箱通道**同时**卷积到双耳。
//! 直接卷积的代价是 `8 × 2 × taps` 次乘法/样本（1024 抽头 = 每样本 16k 次，
//! 48 kHz 下 786 MFLOP/s），跑不动；分块 FFT 把它压到每 hop 约 20k 次复数乘加。
//!
//! ## 为什么块长是 480、FFT 是 960
//!
//! 引擎的时间粒度是 10 ms hop（480 @ 48 kHz，ARCHITECTURE.md §2）。卷积块长**必须**
//! 与之相同，否则这一层会把信号错位半个块，后面所有时钟伺服都在跟一个错位的信号较劲。
//! 于是 FFT 长度 = 2 × 480 = 960（overlap-save 需要 2 倍），与 `dsp::meter` 的 `NFFT` 同源。
//!
//! ## 对齐
//!
//! 输出块 `n` 对应 `y[n*480 .. n*480+480]`，**没有额外延迟**（窗口取「上一块 + 当前块」）。

use std::sync::Arc;

use rustfft::num_complex::Complex;
use rustfft::{Fft, FftPlanner};

use crate::dsp::surround::hrir::{Hrir, EARS};

/// 分块卷积器：`in_ch` 路进、2 路出，块长固定 `block`。
pub struct MatrixConv {
  in_ch: usize,
  ears: usize,
  block: usize,
  nfft: usize,
  parts: usize,
  fwd: Arc<dyn Fft<f32>>,
  inv: Arc<dyn Fft<f32>>,
  /// [in][ear][part] → 频谱（长度 nfft）
  h: Vec<Vec<Vec<Vec<Complex<f32>>>>>,
  /// [in][part] → 输入频谱的频域延迟线（0 = 最新）
  xfifo: Vec<Vec<Vec<Complex<f32>>>>,
  /// [in] → 2*block 的 overlap-save 窗口
  hist: Vec<Vec<f32>>,
  /// [ear] → 累加频谱
  acc: Vec<Vec<Complex<f32>>>,
  scratch: Vec<Complex<f32>>,
  fft_scratch: Vec<Complex<f32>>,
}

impl MatrixConv {
  /// 用一组 HRIR 建卷积器。`in_ch` 必须等于 [`crate::dsp::surround::layout::CHANNELS`]（8）。
  pub fn new(in_ch: usize, block: usize, hrir: &Hrir) -> MatrixConv {
    assert!(block > 0, "块长必须为正");
    let nfft = block * 2;
    let parts = hrir.taps.div_ceil(block).max(1);

    let mut planner = FftPlanner::<f32>::new();
    let fwd = planner.plan_fft_forward(nfft);
    let inv = planner.plan_fft_inverse(nfft);

    let mut h = vec![vec![vec![vec![Complex::new(0.0, 0.0); nfft]; parts]; EARS]; in_ch];
    for (i, per_in) in h.iter_mut().enumerate().take(in_ch) {
      for (e, per_ear) in per_in.iter_mut().enumerate().take(EARS) {
        let fir = hrir.fir(i, e);
        for (p, spec) in per_ear.iter_mut().enumerate() {
          let start = p * block;
          for (t, cell) in spec.iter_mut().take(block).enumerate() {
            *cell = Complex::new(fir.get(start + t).copied().unwrap_or(0.0), 0.0);
          }
          fwd.process_with_scratch(
            spec,
            &mut vec![Complex::new(0.0, 0.0); fwd.get_inplace_scratch_len()],
          );
        }
      }
    }

    let xfifo = vec![vec![vec![Complex::new(0.0, 0.0); nfft]; parts]; in_ch];
    let hist = vec![vec![0.0f32; nfft]; in_ch];
    let acc = vec![vec![Complex::new(0.0, 0.0); nfft]; EARS];
    let scratch = vec![Complex::new(0.0, 0.0); nfft];
    let fft_scratch = vec![Complex::new(0.0, 0.0); fwd.get_inplace_scratch_len()];

    MatrixConv {
      in_ch,
      ears: EARS,
      block,
      nfft,
      parts,
      fwd,
      inv,
      h,
      xfifo,
      hist,
      acc,
      scratch,
      fft_scratch,
    }
  }

  /// FIR 抽头数（含分区补齐）。
  pub fn taps(&self) -> usize {
    self.parts * self.block
  }

  /// 每 hop 的复数乘加量（调试面板用：这是 CPU 占用的主因）。
  pub fn macs_per_hop(&self) -> u64 {
    (self.in_ch * self.ears * self.parts * self.nfft) as u64
  }

  /// 处理一个 hop：`input[i]` 是第 i 路（长度必须 = block），`out[e]` 是第 e 只耳。
  ///
  /// 长度不匹配时按可用长度处理并补零，**不 panic**（实时线程里 panic = 整个会话炸）。
  pub fn process(&mut self, input: &[Vec<f32>], out: &mut [Vec<f32>]) {
    let block = self.block;
    let nfft = self.nfft;
    let in_ch = self.in_ch.min(input.len());

    for (i, src) in input.iter().enumerate().take(in_ch) {
      // 窗口 = [上一块 | 当前块]
      {
        let hist = &mut self.hist[i];
        hist.copy_within(block..nfft, 0);
        let n = src.len().min(block);
        hist[block..block + n].copy_from_slice(&src[..n]);
        for v in hist[block + n..nfft].iter_mut() {
          *v = 0.0;
        }
      }
      for (cell, v) in self.scratch.iter_mut().zip(self.hist[i].iter()) {
        *cell = Complex::new(*v, 0.0);
      }
      self
        .fwd
        .process_with_scratch(&mut self.scratch, &mut self.fft_scratch);
      self.xfifo[i].rotate_right(1);
      self.xfifo[i][0].copy_from_slice(&self.scratch);
    }

    for e in 0..self.ears {
      for v in self.acc[e].iter_mut() {
        *v = Complex::new(0.0, 0.0);
      }
    }

    for i in 0..in_ch {
      for p in 0..self.parts {
        for e in 0..self.ears {
          let xs = &self.xfifo[i][p];
          let hs = &self.h[i][e][p];
          let acc = &mut self.acc[e];
          for ((a, x), hv) in acc.iter_mut().zip(xs.iter()).zip(hs.iter()) {
            a.re += x.re * hv.re - x.im * hv.im;
            a.im += x.re * hv.im + x.im * hv.re;
          }
        }
      }
    }

    let scale = 1.0 / nfft as f32;
    for e in 0..self.ears.min(out.len()) {
      self
        .inv
        .process_with_scratch(&mut self.acc[e], &mut self.fft_scratch);
      let dst = &mut out[e];
      let n = dst.len().min(block);
      for (d, src) in dst
        .iter_mut()
        .take(n)
        .zip(self.acc[e][block..block + n].iter())
      {
        *d = src.re * scale;
      }
      for v in dst[n..].iter_mut() {
        *v = 0.0;
      }
    }
  }

  /// 清空所有流状态（切 HRIR / 重建会话时调用，避免旧尾巴漏进新预设）。
  pub fn reset(&mut self) {
    for x in &mut self.xfifo {
      for s in x.iter_mut() {
        for v in s.iter_mut() {
          *v = Complex::new(0.0, 0.0);
        }
      }
    }
    for h in &mut self.hist {
      for v in h.iter_mut() {
        *v = 0.0;
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::dsp::surround::layout::CHANNELS;

  fn rand_signal(n: usize, seed: u32) -> Vec<f32> {
    let mut s = seed;
    (0..n)
      .map(|_| {
        s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        ((s >> 9) as f32 / 4_194_304.0) - 1.0
      })
      .collect()
  }

  /// 直接卷积（O(n·taps)），作为分块 FFT 的参照实现。
  fn direct(x: &[f32], h: &[f32]) -> Vec<f32> {
    let mut y = vec![0.0f32; x.len()];
    for (n, out) in y.iter_mut().enumerate() {
      let mut acc = 0.0f32;
      for (k, hk) in h.iter().enumerate() {
        if n >= k {
          acc += hk * x[n - k];
        }
      }
      *out = acc;
    }
    y
  }

  #[test]
  fn partitioned_matches_direct_convolution() {
    let block = 480;
    let taps = 1_000; // 故意不是 block 的整数倍：验证分区补齐
    let mut hrir = Hrir::identity(taps);
    // 给每条 FIR 一点随机尾巴，避免退化成单位冲激
    for (i, v) in hrir.ir.iter_mut().enumerate() {
      let s = (i as u32).wrapping_mul(2_654_435_761);
      *v += ((s >> 11) as f32 / 8_388_608.0 - 0.5) * 0.05;
    }
    let mut conv = MatrixConv::new(CHANNELS, block, &hrir);

    let total = block * 6;
    let mut xs: Vec<Vec<f32>> = (0..CHANNELS)
      .map(|c| rand_signal(total, c as u32 + 7))
      .collect();
    // 让非目标通道静音，单独比对一条，便于定位
    for x in xs.iter_mut().skip(1) {
      for v in x.iter_mut() {
        *v = 0.0;
      }
    }

    let mut out = vec![vec![0.0f32; block]; EARS];
    let mut got = vec![vec![0.0f32; total]; EARS];
    for b in 0..(total / block) {
      let inp: Vec<Vec<f32>> = xs
        .iter()
        .map(|x| x[b * block..(b + 1) * block].to_vec())
        .collect();
      conv.process(&inp, &mut out);
      for (slot, e_out) in got.iter_mut().zip(out.iter()) {
        slot[b * block..(b + 1) * block].copy_from_slice(e_out);
      }
    }

    for (e, got_ear) in got.iter().enumerate() {
      // 分区补齐后的 FIR 与实际参与运算的一致
      let mut h = vec![0.0f32; conv.taps()];
      h[..taps].copy_from_slice(hrir.fir(0, e));
      let want = direct(&xs[0], &h);
      let mut max_err = 0.0f32;
      for (a, b) in got_ear.iter().zip(want.iter()) {
        max_err = max_err.max((a - b).abs());
      }
      assert!(max_err < 2e-5, "耳 {e} 最大误差 {max_err}");
    }
  }

  #[test]
  fn identity_hrir_passes_signal_through() {
    let block = 480;
    let hrir = Hrir::identity(64);
    let mut conv = MatrixConv::new(CHANNELS, block, &hrir);
    let x = rand_signal(block, 99);
    let mut inp: Vec<Vec<f32>> = vec![vec![0.0; block]; CHANNELS];
    inp[0] = x.clone();
    let mut out = vec![vec![0.0f32; block]; EARS];
    conv.process(&inp, &mut out);
    for t in 0..block {
      assert!((out[0][t] - x[t]).abs() < 1e-6, "左耳 t={t}");
      assert!((out[1][t] - x[t]).abs() < 1e-6, "右耳 t={t}");
    }
  }

  #[test]
  fn reset_clears_tail() {
    let block = 480;
    let hrir = Hrir::identity(4096); // 长 FIR：尾巴会留在分区里
    let mut conv = MatrixConv::new(CHANNELS, block, &hrir);
    let mut inp: Vec<Vec<f32>> = vec![vec![1.0; block]; CHANNELS];
    let mut out = vec![vec![0.0f32; block]; EARS];
    conv.process(&inp, &mut out);
    conv.reset();
    for v in inp.iter_mut() {
      for x in v.iter_mut() {
        *x = 0.0;
      }
    }
    conv.process(&inp, &mut out);
    for (e, ear) in out.iter().enumerate() {
      for (t, v) in ear.iter().enumerate() {
        assert!(v.abs() < 1e-9, "reset 后仍有残留（耳 {e} t={t}）");
      }
    }
  }

  #[test]
  fn mac_count_is_sane() {
    let hrir = Hrir::identity(1024);
    let conv = MatrixConv::new(CHANNELS, 480, &hrir);
    // 1024 抽头要占 3 个 480 分区（末尾补零到 1440）——补零不影响结果，只多算一点
    assert_eq!(conv.taps(), 1440);
    assert_eq!(conv.macs_per_hop(), 8 * 2 * 3 * 960);
  }
}
