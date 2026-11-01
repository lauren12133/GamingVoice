//! 双耳输出上的 10 段图形 EQ。
//!
//! 位置在**卷积之后**（双耳域），和 HeSuVi 的做法一致：这一层是给耳机做校正的，
//! 补偿的是"耳机本身的频响"，而耳机在 HRIR 之后才介入 —— 所以必须放最后。
//!
//! 代价要说清楚：EQ 会改变 HRIR 已经塑造好的频谱，抬得越狠、方位线索被冲淡得越多。
//! 所以增益范围限在 ±12 dB，且默认全平（`0 dB` 时是真正的直通，不是"接近直通"）。

/// 10 段中心频率（Hz），按倍频程排。
pub const BANDS: [f32; 10] = [
  31.5, 63.0, 125.0, 250.0, 500.0, 1000.0, 2000.0, 4000.0, 8000.0, 16000.0,
];

/// 每段的 Q。1.41 ≈ 一个倍频程带宽，正好和上面的频率间隔对齐。
const Q: f32 = 1.41;

/// 增益上下限（dB）。
pub const MAX_DB: f32 = 12.0;

/// 单个二阶节（RBJ peaking EQ，转置直接 II 型）。
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

impl Default for Biquad {
  fn default() -> Self {
    // 直通：y = x
    Biquad {
      b0: 1.0,
      b1: 0.0,
      b2: 0.0,
      a1: 0.0,
      a2: 0.0,
      z1: 0.0,
      z2: 0.0,
    }
  }
}

impl Biquad {
  fn peaking(sr: f32, f0: f32, q: f32, db: f32) -> Biquad {
    if db.abs() < 1e-4 {
      return Biquad::default();
    }
    let a = 10f32.powf(db / 40.0);
    let w0 = 2.0 * std::f32::consts::PI * (f0 / sr).min(0.49);
    let (sn, cs) = w0.sin_cos();
    let alpha = sn / (2.0 * q);

    let b0 = 1.0 + alpha * a;
    let b1 = -2.0 * cs;
    let b2 = 1.0 - alpha * a;
    let a0 = 1.0 + alpha / a;
    let a1 = -2.0 * cs;
    let a2 = 1.0 - alpha / a;

    Biquad {
      b0: b0 / a0,
      b1: b1 / a0,
      b2: b2 / a0,
      a1: a1 / a0,
      a2: a2 / a0,
      z1: 0.0,
      z2: 0.0,
    }
  }

  /// 只换系数，保留滤波器状态 —— 拖动推子时不清零，才不会听到咔哒声。
  fn retune(&mut self, sr: f32, f0: f32, q: f32, db: f32) {
    let fresh = Biquad::peaking(sr, f0, q, db);
    self.b0 = fresh.b0;
    self.b1 = fresh.b1;
    self.b2 = fresh.b2;
    self.a1 = fresh.a1;
    self.a2 = fresh.a2;
  }

  #[inline]
  fn run(&mut self, x: f32) -> f32 {
    let y = self.b0 * x + self.z1;
    self.z1 = self.b1 * x - self.a1 * y + self.z2;
    self.z2 = self.b2 * x - self.a2 * y;
    y
  }
}

/// 左右耳各一串 10 段。
pub struct Eq {
  sr: f32,
  /// 当前生效的增益（dB），用来判断"要不要重建系数"
  gains: [f32; BANDS.len()],
  /// `[耳][段]`
  chain: [[Biquad; BANDS.len()]; 2],
  /// 总开关：关掉时直接跳过，一帧都不算
  on: bool,
}

impl Eq {
  pub fn new(sr: f32) -> Eq {
    Eq {
      sr,
      gains: [0.0; BANDS.len()],
      chain: [[Biquad::default(); BANDS.len()]; 2],
      on: true,
    }
  }

  pub fn set_enabled(&mut self, on: bool) {
    self.on = on;
  }

  pub fn enabled(&self) -> bool {
    self.on
  }

  /// 设置 10 段增益（dB）。值没变就什么都不做 —— 每块都会调一次，
  /// 不能每次都重算 20 组 sin/cos。
  pub fn set_gains(&mut self, db: &[f32; BANDS.len()]) {
    for (i, want) in db.iter().enumerate() {
      let want = want.clamp(-MAX_DB, MAX_DB);
      if (want - self.gains[i]).abs() < 1e-3 {
        continue;
      }
      self.gains[i] = want;
      for ear in 0..2 {
        self.chain[ear][i].retune(self.sr, BANDS[i], Q, want);
      }
    }
  }

  /// 全平（真正直通，不是"接近"）。
  pub fn is_flat(&self) -> bool {
    self.gains.iter().all(|g| g.abs() < 1e-4)
  }

  /// 就地处理一个块的双耳信号。
  pub fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
    if !self.on || self.is_flat() {
      return;
    }
    for i in 0..l.len().min(r.len()) {
      let mut a = l[i];
      let mut b = r[i];
      for band in 0..BANDS.len() {
        a = self.chain[0][band].run(a);
        b = self.chain[1][band].run(b);
      }
      l[i] = a;
      r[i] = b;
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn rms(x: &[f32]) -> f32 {
    (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt()
  }

  /// 走一段 1 kHz 正弦，返回后半段的 RMS（避开起始瞬态）。
  fn run_tone(eq: &mut Eq, sr: f32, hz: f32) -> f32 {
    let n = sr as usize / 2;
    let mut l = vec![0.0f32; n];
    let mut r = vec![0.0f32; n];
    for i in 0..n {
      let v = (2.0 * std::f32::consts::PI * hz * i as f32 / sr).sin() * 0.5;
      l[i] = v;
      r[i] = v;
    }
    eq.process(&mut l, &mut r);
    rms(&l[n / 2..])
  }

  #[test]
  fn flat_is_bitwise_passthrough() {
    let sr = 48_000.0;
    let mut eq = Eq::new(sr);
    eq.set_gains(&[0.0; BANDS.len()]);
    let mut l: Vec<f32> = (0..480).map(|i| (i as f32 * 0.01).sin()).collect();
    let before = l.clone();
    let mut r = l.clone();
    eq.process(&mut l, &mut r);
    assert_eq!(l, before, "全平状态必须一个样本都不改");
  }

  #[test]
  fn boost_at_1k_raises_1k_by_about_6db() {
    let sr = 48_000.0;
    let mut flat = Eq::new(sr);
    flat.set_gains(&[0.0; BANDS.len()]);
    let base = run_tone(&mut flat, sr, 1000.0);

    let mut eq = Eq::new(sr);
    let mut g = [0.0f32; BANDS.len()];
    g[5] = 6.0; // 1 kHz 那一段
    eq.set_gains(&g);
    let boosted = run_tone(&mut eq, sr, 1000.0);

    let db = 20.0 * (boosted / base).log10();
    assert!((db - 6.0).abs() < 0.6, "1 kHz 增益 = {db:.2} dB，期望 ~6");
  }

  #[test]
  fn boost_at_1k_does_not_move_100hz_much() {
    let sr = 48_000.0;
    let mut flat = Eq::new(sr);
    flat.set_gains(&[0.0; BANDS.len()]);
    let base = run_tone(&mut flat, sr, 100.0);

    let mut eq = Eq::new(sr);
    let mut g = [0.0f32; BANDS.len()];
    g[5] = 6.0;
    eq.set_gains(&g);
    let out = run_tone(&mut eq, sr, 100.0);

    let db = 20.0 * (out / base).log10();
    assert!(
      db.abs() < 1.5,
      "100 Hz 被 1 kHz 那段带动了 {db:.2} dB，太多"
    );
  }

  #[test]
  fn disabled_eq_is_skipped() {
    let sr = 48_000.0;
    let mut eq = Eq::new(sr);
    let mut g = [0.0f32; BANDS.len()];
    g[5] = 12.0;
    eq.set_gains(&g);
    eq.set_enabled(false);
    let mut l = vec![0.3f32; 64];
    let mut r = l.clone();
    eq.process(&mut l, &mut r);
    assert!(l.iter().all(|v| (*v - 0.3).abs() < 1e-9));
  }
}
