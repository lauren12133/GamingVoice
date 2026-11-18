//! 双耳输出上的参数均衡器（parametric EQ）。
//!
//! 位置在**卷积之后**（双耳域），和 HeSuVi 的做法一致：这一层是给耳机做校正的，
//! 补偿的是"耳机本身的频响"，而耳机在 HRIR 之后才介入 —— 所以必须放最后。
//!
//! 代价要说清楚：EQ 会改变 HRIR 已经塑造好的频谱，抬得越狠、方位线索被冲淡得越多。
//! 所以增益范围限在 ±12 dB，且默认全平（`0 dB` 时是真正的直通，不是"接近直通"）。
//!
//! 结构是**滤波器列表**而不是固定段数：每个频点可以自己选类型（峰/架/高通/低通）、
//! 频率、Q、增益，顺序即串联顺序。左右耳各一条同样的链（共用参数，各自保留状态）。

/// 默认模板的频率（Hz），按倍频程排。
///
/// 只是"新建时给一串顺手的位置"，不再是硬约束 —— 用户可以随意增删改。
pub const DEFAULT_FREQS: [f32; 10] = [
  31.5, 63.0, 125.0, 250.0, 500.0, 1000.0, 2000.0, 4000.0, 8000.0, 16000.0,
];

/// 最多几个频点。
///
/// 成本是每耳每频点一个二阶节：16 个 = 32 个 biquad，48 kHz 下约占单核 1%。
/// 再往上加算得动（31 段实测 2%），但曲线会互相重叠到没法调 —— 上限卡在"人还能看懂"。
pub const MAX_BANDS: usize = 16;

/// 频率范围（Hz）。上限压到 20 kHz：48 kHz 采样时 Nyquist 在 24 kHz，
/// 贴着边缘做 shelf 会把系数推到不稳定区。
pub const MIN_FREQ: f32 = 20.0;
/// 见 [`MIN_FREQ`]。
pub const MAX_FREQ: f32 = 20_000.0;

/// Q 值范围。
///
/// 下限到 0.1（很宽的坡），上限 18（接近单频点陷波）。上不封顶会让 f32 下的
/// 极点逼近单位圆，长跑到直流分量上就是自激。
pub const MIN_Q: f32 = 0.1;
/// 见 [`MIN_Q`]。
pub const MAX_Q: f32 = 18.0;

/// 增益上下限（dB）。
pub const MAX_DB: f32 = 12.0;

/// 前级增益下限（dB）。
///
/// 前级不是为了"调音量"而存在的（那是总补偿的活），它的唯一职责是**给提升留出余量**：
/// 一个 +7 dB 的峰叠在满幅信号上必爆，所以上游先压下去。EqualizerAPO 配置里的
/// `Preamp: -6.3 dB` 就是同一个东西，导入时必须原样接住，否则导入完就削波。
pub const MIN_PREAMP_DB: f32 = -30.0;
/// 见 [`MIN_PREAMP_DB`]。
pub const MAX_PREAMP_DB: f32 = 12.0;

/// 滤波器类型。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FilterKind {
  /// 峰值（peaking）：提升/衰减一个频段，最常用
  Peak,
  /// 低架（low shelf）：把某个频率以下的整体抬/压
  LowShelf,
  /// 高架（high shelf）：把某个频率以上的整体抬/压
  HighShelf,
  /// 高通：截掉低频（不用增益）
  HighPass,
  /// 低通：截掉高频（不用增益）
  LowPass,
}

impl FilterKind {
  /// 序列化用的短名（和前端 `<option value>` 一致）。
  pub fn as_str(self) -> &'static str {
    match self {
      FilterKind::Peak => "peak",
      FilterKind::LowShelf => "lowshelf",
      FilterKind::HighShelf => "highshelf",
      FilterKind::HighPass => "highpass",
      FilterKind::LowPass => "lowpass",
    }
  }

  /// 界面上的中文名。
  pub fn name_cn(self) -> &'static str {
    match self {
      FilterKind::Peak => "峰值",
      FilterKind::LowShelf => "低架",
      FilterKind::HighShelf => "高架",
      FilterKind::HighPass => "高通",
      FilterKind::LowPass => "低通",
    }
  }

  pub fn parse(s: &str) -> Option<FilterKind> {
    Some(match s {
      "peak" => FilterKind::Peak,
      "lowshelf" => FilterKind::LowShelf,
      "highshelf" => FilterKind::HighShelf,
      "highpass" => FilterKind::HighPass,
      "lowpass" => FilterKind::LowPass,
      _ => return None,
    })
  }

  /// 这一类是否用得上增益（高通/低通没有）。
  pub fn uses_gain(self) -> bool {
    matches!(
      self,
      FilterKind::Peak | FilterKind::LowShelf | FilterKind::HighShelf
    )
  }
}

/// 一个频点。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Band {
  pub kind: FilterKind,
  pub freq: f32,
  pub q: f32,
  pub gain_db: f32,
}

impl Default for Band {
  fn default() -> Self {
    Band {
      kind: FilterKind::Peak,
      freq: 1000.0,
      q: 1.41,
      gain_db: 0.0,
    }
  }
}

impl Band {
  /// 把越界的字段拉回合法范围。外部（HTTP）来的值一律过一遍这里。
  pub fn sanitized(mut self) -> Band {
    self.freq = self.freq.clamp(MIN_FREQ, MAX_FREQ);
    self.q = self.q.clamp(MIN_Q, MAX_Q);
    self.gain_db = self.gain_db.clamp(-MAX_DB, MAX_DB);
    self
  }

  /// 参数是否"等价"（用来判断要不要重建系数）。
  ///
  /// 阈值比 clamp 的精度粗：拖动推子时 0.5 dB 一档，0.001 的差没意义，
  /// 但每块都重算 32 组 sin/cos 是纯浪费。
  fn same_as(&self, o: &Band) -> bool {
    self.kind == o.kind
      && (self.freq - o.freq).abs() < 0.01
      && (self.q - o.q).abs() < 1e-3
      && (self.gain_db - o.gain_db).abs() < 1e-3
  }
}

/// 默认模板：10 段全平的峰值滤波器。
pub fn default_bands() -> Vec<Band> {
  DEFAULT_FREQS
    .iter()
    .map(|&f| Band {
      freq: f,
      ..Band::default()
    })
    .collect()
}

/// 单个二阶节（转置直接 II 型）。
///
/// 系数已按 `a0` 归一化，`run` 里省掉一次除法。
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
  /// 按 RBJ Audio EQ Cookbook 算系数。
  ///
  /// shelf 用 `alpha = sin(w0)/(2Q)` 这条 Q 参数化的写法（Equalizer APO 也是这套），
  /// 好处是 Q 和峰值的 Q 含义一致：越大拐点越硬。但 shelf 的 Q 过大时会在肩部
  /// 拱出一个谐振峰，所以内部再夹一道 `0.3..=4.0`——曲线看得出问题，但不会炸。
  fn compute(sr: f32, band: &Band) -> Biquad {
    // 平直的峰值/架式滤波器是恒等变换，直接给直通，省一条链上的乘加
    if band.kind.uses_gain() && band.gain_db.abs() < 1e-4 {
      return Biquad::default();
    }
    let f0 = band.freq.clamp(MIN_FREQ, MAX_FREQ.min(sr * 0.49));
    let w0 = 2.0 * std::f32::consts::PI * (f0 / sr);
    let (sn, cs) = w0.sin_cos();
    let q = band.q.clamp(MIN_Q, MAX_Q);
    let alpha = sn / (2.0 * q);

    let (b0, b1, b2, a0, a1, a2) = match band.kind {
      FilterKind::Peak => {
        let a = 10f32.powf(band.gain_db / 40.0);
        (
          1.0 + alpha * a,
          -2.0 * cs,
          1.0 - alpha * a,
          1.0 + alpha / a,
          -2.0 * cs,
          1.0 - alpha / a,
        )
      }
      FilterKind::LowShelf => {
        let a = 10f32.powf(band.gain_db / 40.0);
        let k = 2.0 * a.sqrt() * sn / (2.0 * q.clamp(0.3, 4.0));
        (
          a * ((a + 1.0) - (a - 1.0) * cs + k),
          2.0 * a * ((a - 1.0) - (a + 1.0) * cs),
          a * ((a + 1.0) - (a - 1.0) * cs - k),
          (a + 1.0) + (a - 1.0) * cs + k,
          -2.0 * ((a - 1.0) + (a + 1.0) * cs),
          (a + 1.0) + (a - 1.0) * cs - k,
        )
      }
      FilterKind::HighShelf => {
        let a = 10f32.powf(band.gain_db / 40.0);
        let k = 2.0 * a.sqrt() * sn / (2.0 * q.clamp(0.3, 4.0));
        (
          a * ((a + 1.0) + (a - 1.0) * cs + k),
          -2.0 * a * ((a - 1.0) + (a + 1.0) * cs),
          a * ((a + 1.0) + (a - 1.0) * cs - k),
          (a + 1.0) - (a - 1.0) * cs + k,
          2.0 * ((a - 1.0) - (a + 1.0) * cs),
          (a + 1.0) - (a - 1.0) * cs - k,
        )
      }
      FilterKind::LowPass => (
        (1.0 - cs) / 2.0,
        1.0 - cs,
        (1.0 - cs) / 2.0,
        1.0 + alpha,
        -2.0 * cs,
        1.0 - alpha,
      ),
      FilterKind::HighPass => (
        (1.0 + cs) / 2.0,
        -(1.0 + cs),
        (1.0 + cs) / 2.0,
        1.0 + alpha,
        -2.0 * cs,
        1.0 - alpha,
      ),
    };

    // a0 理论上恒正；夹一道只是防 NaN 从上流漏下来把整条链带死
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

  /// 只换系数，保留滤波器状态 —— 拖动推子时不清零，才不会听到咔哒声。
  fn retune(&mut self, fresh: Biquad) {
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

/// 左右耳各一条同样参数的链。
pub struct Eq {
  sr: f32,
  /// 当前生效的频点（用来判断"要不要重建"）
  bands: Vec<Band>,
  /// `[耳][频点]`
  chain: [Vec<Biquad>; 2],
  /// 总开关：关掉时直接跳过，一帧都不算
  on: bool,
  /// 链首的线性增益（dB）。0 时乘 1.0，不影响直通判定。
  preamp_db: f32,
}

impl Eq {
  pub fn new(sr: f32) -> Eq {
    Eq {
      sr,
      bands: Vec::new(),
      chain: [Vec::new(), Vec::new()],
      on: true,
      preamp_db: 0.0,
    }
  }

  pub fn set_enabled(&mut self, on: bool) {
    self.on = on;
  }

  pub fn enabled(&self) -> bool {
    self.on
  }

  /// 设置前级（dB）。越界值夹回 [`MIN_PREAMP_DB`]..=[`MAX_PREAMP_DB`]。
  pub fn set_preamp(&mut self, db: f32) {
    self.preamp_db = if db.is_finite() {
      db.clamp(MIN_PREAMP_DB, MAX_PREAMP_DB)
    } else {
      0.0
    };
  }

  pub fn preamp_db(&self) -> f32 {
    self.preamp_db
  }

  pub fn bands(&self) -> &[Band] {
    &self.bands
  }

  /// 整条链换成 `want`。值没变就什么都不做 —— 每块都会调一次，
  /// 不能每次都重算 32 组 sin/cos。
  pub fn set_bands(&mut self, want: &[Band]) {
    let want = &want[..want.len().min(MAX_BANDS)];
    if want.len() == self.bands.len()
      && want
        .iter()
        .zip(self.bands.iter())
        .all(|(a, b)| a.same_as(b))
    {
      return;
    }
    for ear in 0..2 {
      // 按位置复用旧节的状态：只改了增益/频率时，z1/z2 保留，听不到咔哒
      let old = std::mem::take(&mut self.chain[ear]);
      let mut next: Vec<Biquad> = Vec::with_capacity(want.len());
      for (i, b) in want.iter().enumerate() {
        let fresh = Biquad::compute(self.sr, b);
        let mut bq = old.get(i).copied().unwrap_or_default();
        bq.retune(fresh);
        next.push(bq);
      }
      self.chain[ear] = next;
    }
    self.bands.clear();
    self.bands.extend_from_slice(want);
  }

  /// 全平（真正直通，不是"接近"）。
  ///
  /// 高通/低通永远不是直通 —— 它们没有增益，只要在链上就一定会削频段。
  pub fn is_passthrough(&self) -> bool {
    self
      .bands
      .iter()
      .all(|b| b.kind.uses_gain() && b.gain_db.abs() < 1e-4)
  }

  /// 就地处理一个块的双耳信号。
  pub fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
    if !self.on {
      return;
    }
    // 前级是恒定标量：0 dB 时乘 1.0 在 IEEE754 下是精确的，不会破坏"直通一个样本都不改"
    let g = if self.preamp_db.abs() < 1e-4 {
      1.0
    } else {
      10f32.powf(self.preamp_db / 20.0)
    };
    if g == 1.0 && self.is_passthrough() {
      return;
    }
    let n = l.len().min(r.len());
    for x in l.iter_mut().take(n) {
      let mut v = if g == 1.0 { *x } else { *x * g };
      for bq in self.chain[0].iter_mut() {
        v = bq.run(v);
      }
      *x = v;
    }
    for x in r.iter_mut().take(n) {
      let mut v = if g == 1.0 { *x } else { *x * g };
      for bq in self.chain[1].iter_mut() {
        v = bq.run(v);
      }
      *x = v;
    }
  }

  /// 单个频点的幅频响应（dB），给测试和自检用。
  ///
  /// 前端画曲线时是自己算的（同一套公式），不需要每帧问后端要 200 个点。
  pub fn response_db(&self, hz: f32) -> f32 {
    let mut sum = 0.0;
    for b in &self.bands {
      sum += mag_db(&Biquad::compute(self.sr, b), hz, self.sr);
    }
    sum
  }
}

/// 二阶节在 `hz` 处的幅度响应（dB）。
///
/// `H(e^{jw}) = (b0 + b1·e^{-jw} + b2·e^{-2jw}) / (a0 + a1·e^{-jw} + a2·e^{-2jw})`，
/// 系数已归一化（a0 = 1）。
fn mag_db(c: &Biquad, hz: f32, sr: f32) -> f32 {
  let w = 2.0 * std::f32::consts::PI * hz / sr;
  let (sw, cw) = w.sin_cos();
  let (s2, c2) = (2.0 * w).sin_cos();
  let nr = c.b0 + c.b1 * cw + c.b2 * c2;
  let ni = -(c.b1 * sw + c.b2 * s2);
  let dr = 1.0 + c.a1 * cw + c.a2 * c2;
  let di = -(c.a1 * sw + c.a2 * s2);
  let num = nr * nr + ni * ni;
  let den = dr * dr + di * di;
  if den < 1e-20 {
    return 0.0;
  }
  10.0 * (num / den).log10()
}

#[cfg(test)]
mod tests {
  use super::*;

  fn rms(x: &[f32]) -> f32 {
    (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt()
  }

  /// 走一段正弦，返回后半段的 RMS（避开起始瞬态）。
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

  /// 单频点，走一遍正弦，返回相对平直的 dB。
  fn gain_at(band: Band, sr: f32, hz: f32) -> f32 {
    let mut flat = Eq::new(sr);
    flat.set_bands(&default_bands());
    let base = run_tone(&mut flat, sr, hz);

    let mut eq = Eq::new(sr);
    eq.set_bands(&[band.sanitized()]);
    let out = run_tone(&mut eq, sr, hz);
    20.0 * (out / base).log10()
  }

  fn peak(freq: f32, q: f32, db: f32) -> Band {
    Band {
      kind: FilterKind::Peak,
      freq,
      q,
      gain_db: db,
    }
  }

  #[test]
  fn flat_is_bitwise_passthrough() {
    let sr = 48_000.0;
    let mut eq = Eq::new(sr);
    eq.set_bands(&default_bands());
    assert!(eq.is_passthrough(), "全平的 10 段必须判定为直通");
    let mut l: Vec<f32> = (0..480).map(|i| (i as f32 * 0.01).sin()).collect();
    let before = l.clone();
    let mut r = l.clone();
    eq.process(&mut l, &mut r);
    assert_eq!(l, before, "全平状态必须一个样本都不改");
  }

  #[test]
  fn empty_chain_is_passthrough() {
    let mut eq = Eq::new(48_000.0);
    eq.set_bands(&[]);
    assert!(eq.is_passthrough());
  }

  #[test]
  fn boost_at_1k_raises_1k_by_about_6db() {
    let db = gain_at(peak(1000.0, 1.41, 6.0), 48_000.0, 1000.0);
    assert!((db - 6.0).abs() < 0.6, "1 kHz 增益 = {db:.2} dB，期望 ~6");
  }

  #[test]
  fn boost_at_1k_does_not_move_100hz_much() {
    let db = gain_at(peak(1000.0, 1.41, 6.0), 48_000.0, 100.0);
    assert!(db.abs() < 1.5, "100 Hz 被 1 kHz 频点带动了 {db:.2} dB");
  }

  #[test]
  fn lowpass_kills_highs_keeps_lows() {
    let b = Band {
      kind: FilterKind::LowPass,
      freq: 500.0,
      q: 0.707,
      gain_db: 0.0,
    };
    let low = gain_at(b, 48_000.0, 50.0);
    let high = gain_at(b, 48_000.0, 8000.0);
    assert!(low.abs() < 1.0, "50 Hz 应该基本不动，实际 {low:.2}");
    assert!(high < -30.0, "8 kHz 应该被砍掉，实际 {high:.2}");
    // 低通不是直通 —— 没有增益也要参与运算
    let mut eq = Eq::new(48_000.0);
    eq.set_bands(&[b]);
    assert!(!eq.is_passthrough(), "低通不能被当成直通跳过");
  }

  #[test]
  fn highpass_kills_lows_keeps_highs() {
    let b = Band {
      kind: FilterKind::HighPass,
      freq: 500.0,
      q: 0.707,
      gain_db: 0.0,
    };
    let low = gain_at(b, 48_000.0, 40.0);
    let high = gain_at(b, 48_000.0, 6000.0);
    assert!(low < -30.0, "40 Hz 应该被砍掉，实际 {low:.2}");
    assert!(high.abs() < 1.0, "6 kHz 应该基本不动，实际 {high:.2}");
  }

  #[test]
  fn lowshelf_boosts_bottom_only() {
    let b = Band {
      kind: FilterKind::LowShelf,
      freq: 200.0,
      q: 0.707,
      gain_db: 6.0,
    };
    let low = gain_at(b, 48_000.0, 40.0);
    let high = gain_at(b, 48_000.0, 8000.0);
    assert!((low - 6.0).abs() < 1.2, "40 Hz 应抬 ~6 dB，实际 {low:.2}");
    assert!(high.abs() < 1.0, "8 kHz 应基本不动，实际 {high:.2}");
  }

  #[test]
  fn highshelf_boosts_top_only() {
    let b = Band {
      kind: FilterKind::HighShelf,
      freq: 4000.0,
      q: 0.707,
      gain_db: -6.0,
    };
    let high = gain_at(b, 48_000.0, 12000.0);
    let low = gain_at(b, 48_000.0, 500.0);
    assert!(
      (high + 6.0).abs() < 1.2,
      "12 kHz 应减 ~6 dB，实际 {high:.2}"
    );
    assert!(low.abs() < 1.0, "500 Hz 应基本不动，实际 {low:.2}");
  }

  #[test]
  fn disabled_eq_is_skipped() {
    let sr = 48_000.0;
    let mut eq = Eq::new(sr);
    eq.set_bands(&[peak(1000.0, 1.41, 12.0)]);
    eq.set_enabled(false);
    let mut l = vec![0.3f32; 64];
    let mut r = l.clone();
    eq.process(&mut l, &mut r);
    assert!(l.iter().all(|v| (*v - 0.3).abs() < 1e-9));
  }

  #[test]
  fn preamp_scales_the_whole_block() {
    // 导入 EqualizerAPO 配置时，`Preamp: -6.3 dB` 必须真的生效 —— 否则那些 +7 dB 的
    // 峰叠在满幅信号上会立刻削波，用户听到的是"导入完更难听"。
    let mut eq = Eq::new(48_000.0);
    eq.set_bands(&default_bands());
    eq.set_preamp(-6.0);
    let mut l = vec![0.5f32; 64];
    let mut r = l.clone();
    eq.process(&mut l, &mut r);
    let want = 0.5 * 10f32.powf(-6.0 / 20.0);
    assert!(
      (l[0] - want).abs() < 1e-6,
      "前级 -6 dB 应乘 0.5012，实际得到 {}",
      l[0]
    );
    assert!(l[0] < 0.5, "负前级必须让电平变小");
  }

  #[test]
  fn zero_preamp_keeps_bitwise_passthrough() {
    let mut eq = Eq::new(48_000.0);
    eq.set_bands(&default_bands());
    eq.set_preamp(0.0);
    let mut l: Vec<f32> = (0..480).map(|i| (i as f32 * 0.01).sin()).collect();
    let before = l.clone();
    let mut r = l.clone();
    eq.process(&mut l, &mut r);
    assert_eq!(l, before, "前级 0 dB 不能破坏直通");
  }

  #[test]
  fn preamp_is_clamped() {
    let mut eq = Eq::new(48_000.0);
    eq.set_preamp(-999.0);
    assert_eq!(eq.preamp_db(), MIN_PREAMP_DB);
    eq.set_preamp(999.0);
    assert_eq!(eq.preamp_db(), MAX_PREAMP_DB);
    // NaN 会一路污染到输出（整块变静音），必须在入口挡掉
    eq.set_preamp(f32::NAN);
    assert_eq!(eq.preamp_db(), 0.0);
  }

  #[test]
  fn retuning_keeps_filter_state() {
    // 拖动推子时会连着调很多次 set_bands。如果每次都把 z1/z2 清零，
    // 输出会在这一块里出现台阶 —— 听感就是"咔哒"。
    let sr = 48_000.0;
    let mut eq = Eq::new(sr);
    eq.set_bands(&[peak(1000.0, 1.41, 6.0)]);
    let mut l = vec![0.25f32; 480];
    let mut r = l.clone();
    eq.process(&mut l, &mut r);
    let z = eq.chain[0][0].z1;
    assert!(z.abs() > 1e-6, "跑过一块后状态应该非零");

    eq.set_bands(&[peak(1000.0, 1.41, 7.0)]);
    assert_eq!(eq.chain[0][0].z1, z, "改增益不该清空滤波器状态");
  }

  #[test]
  fn band_count_is_capped() {
    let mut eq = Eq::new(48_000.0);
    let many: Vec<Band> = (0..64)
      .map(|i| peak(100.0 + i as f32 * 100.0, 1.0, 1.0))
      .collect();
    eq.set_bands(&many);
    assert_eq!(eq.bands().len(), MAX_BANDS);
    assert_eq!(eq.chain[0].len(), MAX_BANDS);
  }

  #[test]
  fn out_of_range_values_are_clamped() {
    let b = peak(1.0, 900.0, 99.0).sanitized();
    assert_eq!(b.freq, MIN_FREQ);
    assert_eq!(b.q, MAX_Q);
    assert_eq!(b.gain_db, MAX_DB);

    let b = peak(90_000.0, 0.0, -99.0).sanitized();
    assert_eq!(b.freq, MAX_FREQ);
    assert_eq!(b.q, MIN_Q);
    assert_eq!(b.gain_db, -MAX_DB);
  }

  #[test]
  fn response_curve_matches_measured_gain() {
    // 频响函数和实际卷积结果必须一致，否则界面画的曲线是假的
    let sr = 48_000.0;
    let band = peak(1000.0, 3.0, 8.0);
    let mut eq = Eq::new(sr);
    eq.set_bands(&[band]);
    let predicted = eq.response_db(1000.0);
    let measured = gain_at(band, sr, 1000.0);
    assert!(
      (predicted - measured).abs() < 0.5,
      "曲线算出 {predicted:.2} dB，实测 {measured:.2} dB"
    );
  }

  #[test]
  fn kind_round_trips_through_string() {
    for k in [
      FilterKind::Peak,
      FilterKind::LowShelf,
      FilterKind::HighShelf,
      FilterKind::HighPass,
      FilterKind::LowPass,
    ] {
      assert_eq!(FilterKind::parse(k.as_str()), Some(k));
    }
    assert_eq!(FilterKind::parse("butterworth"), None);
  }
}
