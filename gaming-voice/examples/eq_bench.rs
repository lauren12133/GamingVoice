//! EQ 成本基准：给"能不能加 EQ / 最多几段"这个问题一个实测数字。
//!
//! 跑法：`cargo run --release --example eq_bench`
//!
//! 只用 Transposed Direct Form II 的 peaking biquad（RBJ cookbook），因为这是
//! Equalizer APO / 大多数图形 EQ 用的结构：每样本 5 乘 4 加，没有加法延迟。

use std::time::Instant;

/// 二阶节（Transposed Direct Form II）。
///
/// 选这个结构而不是 Direct Form I：状态量只有 2 个、数值范围小，f32 下更稳，
/// 而且每样本少一次乘法。
#[derive(Clone, Copy)]
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
  /// RBJ cookbook 的 peaking EQ。
  fn peaking(sr: f32, fc: f32, q: f32, gain_db: f32) -> Biquad {
    let a = 10f32.powf(gain_db / 40.0);
    let w0 = 2.0 * std::f32::consts::PI * fc / sr;
    let (sin, cos) = w0.sin_cos();
    let alpha = sin / (2.0 * q);
    let norm = 1.0 / (1.0 + alpha / a);
    Biquad {
      b0: (1.0 + alpha * a) * norm,
      b1: -2.0 * cos * norm,
      b2: (1.0 - alpha * a) * norm,
      a1: -2.0 * cos * norm,
      a2: (1.0 - alpha / a) * norm,
      z1: 0.0,
      z2: 0.0,
    }
  }

  #[inline]
  fn process(&mut self, x: f32) -> f32 {
    let y = self.b0 * x + self.z1;
    self.z1 = self.b1 * x - self.a1 * y + self.z2;
    self.z2 = self.b2 * x - self.a2 * y;
    y
  }
}

fn main() {
  const SR: usize = 48_000;
  const CH: usize = 8;
  const SECONDS: usize = 10;
  let frames = SR * SECONDS;

  let bands = [0usize, 5, 10, 15, 31];
  println!(
    "{:>6}  {:>8}  {:>10}  {:>10}  {:>12}  {:>10}",
    "段数", "biquad 数", "耗时(ms)", "实时率", "单核占用", "每样本运算"
  );
  println!("{}", "-".repeat(74));

  for &n in &bands {
    // 频点按对数铺开，Q 取 1.0（图形 EQ 的常见带宽）
    let mut chain: Vec<Biquad> = Vec::with_capacity(CH * n);
    for _c in 0..CH {
      for k in 0..n {
        let fc = 31.5 * 2f32.powf(k as f32 * 9.0 / n.max(1) as f32); // 31.5 Hz 起，每段 1/3 倍频程量级
        chain.push(Biquad::peaking(
          SR as f32,
          fc.clamp(20.0, 20_000.0),
          1.0,
          0.0,
        ));
      }
    }

    // 预热，避免第一次的缺页/频率爬升影响
    for b in chain.iter_mut() {
      b.process(0.1);
      b.process(-0.1);
    }

    let t0 = Instant::now();
    let mut acc = 0.0f32;
    for f in 0..frames {
      let x = ((f as f32) * 0.01).sin() * 0.2;
      for c in 0..CH {
        let mut v = x;
        for k in 0..n {
          v = chain[c * n + k].process(v);
        }
        acc += v;
      }
    }
    let dt = t0.elapsed().as_secs_f32();
    // 防止编译器把整段优化掉
    std::hint::black_box(acc);

    let audio_secs = SECONDS as f32;
    let rtf = dt / audio_secs;
    println!(
      "{:>6}  {:>8}  {:>10.1}  {:>10.4}  {:>11.2}%  {:>10}",
      n,
      CH * n,
      dt * 1000.0,
      rtf,
      rtf * 100.0,
      if n == 0 { 0 } else { CH * n * 9 }
    );
  }

  println!(
    "\n注：实时率 = 处理耗时 / 音频时长。0.01 表示 1% 的时间，即单核 1%。\n\
     真实链路上还有 HRIR 卷积（8→2，约占 0.3%~1%），两者相加才是总占用。"
  );
}
