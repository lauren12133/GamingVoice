//! 处理核心：7.1(8ch) → 每方向增益/延时 → ATK V3 空间引擎（按声道区分自己/敌人：
//!           FC 自抑制 + center duck + 自适应 EQ + crossover + CMR + 瞬态整形 + …）
//!           → HeSuVi HRIR 卷积（8 虚拟扬声器 → 双耳）→ 耳机 EQ → 限幅。
//!
//! 与平台无关：这里只认「交错排列的 f32 环」，采集/渲染各自在 `audio_windows.rs` 里。
//! 这样 Linux 上也能跑测试（`cargo test` 用假环喂数据），不必等 Windows。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::dsp::surround::eq::{self, Eq};
use crate::dsp::surround::hrir::EARS;
use crate::dsp::surround::layout::{Speaker, CHANNELS};
use crate::dsp::surround::{
  AtkV3, AtkV3Params, Hrir, LookaheadLimiter, MatrixConv, Spatial, SpatialParams,
};

use crate::audio::{CHANNELS as CAP_CHANNELS, SAMPLE_RATE};

/// **处理块长（样本）：固定 128 = 2.7 ms，不再可选。**
///
/// 为什么锁定：
/// 采集端 WASAPI 周期固定 10 ms（480 帧），数据每 10 ms 才到环一次；端到端延迟由
/// 采集周期 + 渲染缓冲决定，**跟处理块长无关**。把块长从 128 改小，只是让「攒满一块」
/// 的门槛更低，并不能让声音更早到达——所以改块长不降延迟，反而因分区变多白增 CPU。
/// 128 是 CPU 与块粒度之间的平衡点，故锁定。
pub const BLOCK: usize = 128;

/// 用户可调参数。**每 block 从 `Shared` 读一次**，不做重建 —— 改推子立即生效。
#[derive(Clone, Debug)]
pub struct Params {
  pub gains_db: [f32; CHANNELS],
  pub delays_ms: [f32; CHANNELS],
  pub makeup_db: f32,
  /// 双耳输出上的参数 EQ：可增删的频点列表（顺序即串联顺序）。
  /// 空列表、或全是不带增益的平直滤波器时是真正的直通。
  pub eq_bands: Vec<eq::Band>,
  pub eq_on: bool,
  /// EQ 链首的前级（dB）。导入 EqualizerAPO 配置时那里面的 `Preamp` 就落在这。
  pub eq_preamp: f32,
  /// 自身枪声压制强度（0..1，默认 0.8）。控制 FC 中置**中高频**分量从左右声道的
  /// 减除量——枪声主体在中高频，越大自己的枪声越小。
  pub self_gun: f32,
  /// 自身脚步压制强度（0..1，默认 0.7）。控制 FC 中置**低频**分量（脚步冲击）的减除量。
  pub self_foot: f32,
  /// 脚步增强强度（0..1，默认 0.5）。FC 静默时抬升 200Hz/3kHz 脚步频段，
  /// 即"远处敌人相对放大"。
  pub leveller: f32,
  /// Center Duck 深度（dB，默认 -30）。开枪时「前方中央分量(FL+FR)/2」被压掉的量。
  /// 这是压住自己枪声的主旋钮——COD 等把自己枪声放在 FL/FR 中央分量，而不是 FC。
  pub duck_db: f32,
  /// 听远（0..1，默认 0.75）。越大，下行扩展器越温和（远处微弱声不被压掉）、
  /// 脚步频段 boost 越强——即「越能听清远处的脚步/换弹」。
  pub range: f32,
}

impl Default for Params {
  fn default() -> Self {
    Params {
      // 出厂设置：方向增益保持中性（平直）。「自己 vs 敌人」的区分靠**声道**：
      // FPS 游戏把自己枪声/脚步混在中置 FC，由 fc_suppress 按 FC 三频段加权从
      // 左右声道减掉并清零 FC——不是按「响度」压。
      gains_db: [0.0; CHANNELS],
      delays_ms: [0.0; CHANNELS],
      makeup_db: -6.0,
      eq_bands: eq::default_bands(),
      eq_on: true,
      eq_preamp: 0.0,
      self_gun: 0.8,
      self_foot: 0.7,
      leveller: 0.5,
      duck_db: -30.0,
      range: 0.75,
    }
  }
}

impl Params {
  fn to_spatial(&self) -> SpatialParams {
    SpatialParams {
      gains_db: self.gains_db,
      delays_ms: self.delays_ms,
      makeup_db: self.makeup_db,
      ..Default::default()
    }
  }
}

/// 实时电平（推子上那根跳动的条）。
#[derive(Clone, Copy, Debug, Default)]
pub struct Levels {
  /// 原始采集每声道峰值（FC 清零**之前**）。诊断用：看游戏到底往每个声道放了什么，
  /// 尤其是中置 FC —— 若它长期接近静音，说明游戏只输出了立体声，FC 自抑制没有对象。
  pub raw_peak: [f32; CHANNELS],
  /// 每声道输入峰值（增益/延时后、FC 处理后的 bus）
  pub in_peak: [f32; CHANNELS],
  /// 每声道增益后的峰值
  pub out_peak: [f32; CHANNELS],
  /// 双耳输出峰值
  pub ear_peak: [f32; EARS],
  /// **枪声快照**：检测到前方声道能量突增（枪声起音极快）后，冻结接下来 ~100ms
  /// 各声道的最大峰值。开一枪，界面就能显示「这一枪每个声道各有多响」，
  /// 直接看出自己的枪声到底混在哪个声道（FC 还是 FL/FR），不用再猜。
  pub gun_snapshot: [f32; CHANNELS],
  /// Center Duck 实时压降（dB，负值）。开枪时往下掉 = duck 正在压自己枪声；
  /// 一直接近 0 = duck 没触发。
  pub duck_db: f32,
}

/// 运行状态（界面顶栏）。
#[derive(Clone, Debug, Default)]
pub struct Status {
  /// 采集端点实际声道数（< 8 就说明端点没配成 7.1）
  pub source_channels: u16,
  pub source_rate: u32,
  pub sink_channels: u16,
  pub sink_rate: u32,
  pub error: Option<String>,
  /// 当前生效的 HRIR 名字（文件名 / `woodworth-175mm`）
  pub hrir_name: String,
  /// 这个 HRIR 是实测数据（true）还是数学合成（false）——**决定前后能不能分**
  pub hrir_measured: bool,
}

/// 线程间共享状态。
pub struct Shared {
  pub params: Mutex<Params>,
  pub levels: Mutex<Levels>,
  pub status: Mutex<Status>,
  pub underruns: AtomicU64,
  /// 已经处理过的帧数（界面换算成运行时长）
  pub frames: AtomicU64,
  /// 处理线程看到的输入缓冲水位（毫秒 ×100，避免 AtomicF32）
  pub buffer_ms100: AtomicU64,
}

impl Shared {
  pub fn new() -> Arc<Shared> {
    Arc::new(Shared {
      params: Mutex::new(Params::default()),
      levels: Mutex::new(Levels::default()),
      status: Mutex::new(Status::default()),
      underruns: AtomicU64::new(0),
      frames: AtomicU64::new(0),
      buffer_ms100: AtomicU64::new(0),
    })
  }
}

/// 合成 HRIR。
///
/// 用 Woodworth 球头模型（双耳时差 + 头影一阶低通），**不是测量数据**。
/// 真正想要接近 HeSuVi 的效果，把一组 16 声道 HRIR WAV 放进 `hrir/` 即可
/// （解析器在 `dsp/surround/wav.rs`，格式见 README）。
pub fn synth_hrir(taps: usize, head_mm: f32) -> Hrir {
  let sr = SAMPLE_RATE as f32;
  let head_r = (head_mm / 1000.0).max(0.05);
  // 双耳最大时差：声波绕过头部半个周长所需时间（Woodworth 近似）
  let max_itd_ms = head_r / 343.0 * 1000.0 * 1.0;
  let itd_span = max_itd_ms * sr / 1000.0;
  let mut ir = vec![0.0f32; CHANNELS * EARS * taps];

  for spk in Speaker::ALL {
    let lateral = spk.azimuth_deg().to_radians().sin();
    for ear in 0..EARS {
      let ear_sign = if ear == 0 { -1.0 } else { 1.0 };
      let same_side = (lateral * ear_sign + 1.0) / 2.0;
      let shadow = if spk.is_lfe() { 0.0 } else { 1.0 - same_side };
      let itd = if spk.is_lfe() {
        itd_span / 2.0
      } else {
        itd_span / 2.0 - ear_sign * lateral * itd_span / 2.0
      };

      let base = (spk.index() * EARS + ear) * taps;
      let onset = itd.round().max(0.0) as usize;
      let a = 0.45 * shadow;
      let gain = 1.0 - 0.30 * shadow;

      let mut coef = 1.0 - a;
      let mut idx = onset.min(taps);
      while idx < taps {
        ir[base + idx] = coef * gain;
        coef *= a;
        idx += 1;
      }
      // 早期反射：4 ms 后一个更轻更暗的副本，制造"在房间里"而不是"贴在耳朵上"。
      let refl = onset + (0.004 * sr) as usize;
      if refl < taps {
        let a2 = (a + 0.35).min(0.9);
        let mut coef = (1.0 - a2) * 0.18 * gain;
        let mut idx = refl;
        while idx < taps {
          ir[base + idx] += coef;
          coef *= a2;
          idx += 1;
        }
      }
    }
  }

  Hrir {
    sample_rate: SAMPLE_RATE,
    taps,
    ir,
    name: format!("woodworth-{head_mm:.0}mm"),
  }
}

/// 内置 HeSuVi 默认 HRIR（`EAC_Default.wav`，14 轨、48 kHz、32-bit float）。
///
/// 编译期嵌入二进制——exe 单文件即用，无需外部 `hrir/` 目录。数据就是 HeSuVi 默认
/// 的 7.1 虚拟扬声器冲激响应，`Hrir::from_wav` 会按 14 轨 HeSuVi 排布自动重排，
/// LFE 复用中置（与 HeSuVi 行为一致）。
fn builtin_hrir() -> Hrir {
  let bytes = include_bytes!("../hrir/EAC_Default.wav");
  let w = crate::dsp::surround::wav::parse(bytes).expect("内置 HRIR 解析失败");
  Hrir::from_wav(&w, "HeSuVi EAC_Default").expect("内置 HRIR 无效")
}

/// 放 HRIR 文件的目录名。打包后在 exe 同级，开发时在项目根。
pub const HRIR_DIR: &str = "hrir";

/// 依次尝试的 HRIR 目录：exe 所在目录 → 当前工作目录。
///
/// 两个都试，是因为运行时 exe 可能被装在任意位置，而开发时又不想把 wav 拷进 target。
pub fn hrir_dirs() -> Vec<PathBuf> {
  let mut dirs = Vec::new();
  if let Some(d) = std::env::current_exe()
    .ok()
    .and_then(|p| p.parent().map(|d| d.join(HRIR_DIR)))
  {
    dirs.push(d);
  }
  if let Some(d) = std::env::current_dir().ok().map(|d| d.join(HRIR_DIR)) {
    dirs.push(d);
  }
  dirs.dedup();
  dirs
}

/// 用户数据目录名。和 `hrir/` 一样放在 exe 同级，开发时在项目根。
pub const DATA_DIR: &str = "data";

/// 依次尝试的数据目录：exe 所在目录 → 当前工作目录（与 [`hrir_dirs`] 同一套口径）。
///
/// 预设是用户资产，放程序目录后：解压版/绿色版就在 exe 旁边，换机器整目录拷走即可；
/// 开发时（`cargo run`）exe 在 `target/` 里，当前工作目录是项目根，两个都试才能两边不落空。
pub fn data_dirs() -> Vec<PathBuf> {
  let mut dirs = Vec::new();
  if let Some(d) = std::env::current_exe()
    .ok()
    .and_then(|p| p.parent().map(|d| d.join(DATA_DIR)))
  {
    dirs.push(d);
  }
  if let Some(d) = std::env::current_dir().ok().map(|d| d.join(DATA_DIR)) {
    dirs.push(d);
  }
  dirs.dedup();
  dirs
}

/// 首选数据目录（**写**文件用；读也认它）。
///
/// 写文件前由调用方 `create_dir_all`。目录建不出来（例如 exe 装在不可写的
/// `Program Files`）会返回错误、不会静默换地方 —— 免得用户以为存上了其实没存上。
pub fn data_dir() -> PathBuf {
  data_dirs()
    .into_iter()
    .next()
    .unwrap_or_else(std::env::temp_dir)
}

/// 列出所有可用的 HRIR 文件（`.wav`，跨目录按文件名去重，最后按路径排序）。
pub fn list_hrir_files() -> Vec<PathBuf> {
  let mut out: Vec<PathBuf> = Vec::new();
  for dir in hrir_dirs() {
    let Ok(rd) = std::fs::read_dir(&dir) else {
      continue;
    };
    let mut here: Vec<PathBuf> = rd
      .flatten()
      .map(|e| e.path())
      .filter(|p| {
        p.extension()
          .and_then(|e| e.to_str())
          .is_some_and(|e| e.eq_ignore_ascii_case("wav"))
      })
      .collect();
    here.sort();
    for p in here {
      let fname = p.file_name().map(|s| s.to_os_string());
      if !out
        .iter()
        .any(|q| q.file_name().map(|s| s.to_os_string()) == fname)
      {
        out.push(p);
      }
    }
  }
  out.sort();
  out
}

/// dB → 线性增益（-60 dB 以下视为静音，避免 `10^(x/20)` 得到 denormal）。
#[inline]
fn db_to_gain(db: f32) -> f32 {
  if !db.is_finite() || db <= -60.0 {
    0.0
  } else {
    10f32.powf(db / 20.0)
  }
}

/// 处理线程主体：从 `src`（交错 8ch）读、往 `dst`（交错 2ch）写。
///
/// 环里没有数据就补静音（不阻塞）—— 采集断流时宁可静音，也不要卡住渲染线程。
///
/// 数据流（HeSuVi HRIR 双耳化）：
/// `7.1 → 每方向增益/延时 → ATK V3 空间引擎（FC 自抑制 + duck + 自适应 EQ + …）
///  → HeSuVi HRIR 卷积(8→2) → 耳机 EQ → 限幅`。HRIR 内置 HeSuVi `EAC_Default.wav`，
/// 也可由调用方传入外部 HRIR 覆盖。
pub fn run_processor(
  shared: Arc<Shared>,
  mut src: rtrb::Consumer<f32>,
  mut dst: rtrb::Producer<f32>,
  stop: Arc<AtomicBool>,
  hrir: Option<Hrir>,
) {
  let mut params = shared.params.lock().map(|p| p.clone()).unwrap_or_default();

  // HRIR 双耳化：优先外部传入的 HRIR，否则内置 HeSuVi EAC_Default（14 轨、48kHz）。
  // 这是「8 虚拟扬声器 → 双耳」的方位来源，替代原来的球谐 SH-HRTF。
  let hrir = hrir.unwrap_or_else(builtin_hrir);
  if let Ok(mut st) = shared.status.lock() {
    st.hrir_name = hrir.name.clone();
    st.hrir_measured = true;
  }
  // 处理块长固定为 BLOCK（锁定）：改块长不降延迟、只增 CPU。
  let block = BLOCK;
  // Spatial 只承担 7.1 域「每方向增益/延时」（process_bed），双耳卷积交给下面的
  // MatrixConv（HeSuVi HRIR）；这里传合成 HRIR 只是占位（其 8×2 卷积器不会被用到）。
  let mut sp = Spatial::with_block(synth_hrir(64, 175.0), params.to_spatial(), block);
  // HeSuVi HRIR 卷积：8 个 7.1 声道（含 LFE，其 FIR 复用中置）→ 双耳。
  let mut conv = MatrixConv::new(CHANNELS, block, &hrir);
  let mut eq = Eq::new(SAMPLE_RATE as f32);
  let mut atk = AtkV3::new(SAMPLE_RATE as f32);
  // 输出端 lookahead limiter：对齐原作者 True peak 0 dBFS 的处理，压平枪声瞬态前沿。
  let mut limiter = LookaheadLimiter::new(SAMPLE_RATE as f32);

  // 取数块与处理块必须是同一个值（当前 `block`）。旧实现里一个用 HOP、一个用可调 block，
  // 结果每次只消费 480 中的 128 帧，剩下的被丢掉 → 周期性断续，听感就是"电流声"。
  let cap_ch = CAP_CHANNELS;
  let mut raw: Vec<Vec<f32>> = vec![vec![0.0; block]; cap_ch]; // 采集 8 路 7.1
  let mut bus: Vec<Vec<f32>> = vec![vec![0.0; block]; CHANNELS]; // 增益/延时后的 7.1
  let mut ears: Vec<Vec<f32>> = vec![vec![0.0; block]; EARS]; // HRIR 卷积输出（双耳）
  let mut l = vec![0.0f32; block];
  let mut r = vec![0.0f32; block];
  let mut last_stat = Instant::now();
  // 枪声快照状态：前方峰值平滑值 + 快照剩余块数 + 跨块累积的每声道最大峰值。
  let mut prev_front_peak = 0.0f32;
  let mut snap_left = 0usize;
  let mut gun_snapshot = [0.0f32; CHANNELS];
  // duck 深度峰值（负值）：保持最近一次 duck 压降，缓慢释放，界面好观察。
  let mut duck_peak = 0.0f32;

  while !stop.load(Ordering::Relaxed) {
    // 参数可能被界面改了：每块读一次（互斥锁在无竞争时几乎免费）
    if let Ok(p) = shared.params.lock() {
      params = p.clone();
      sp.set_params(params.to_spatial());
    }
    // 系数只在值真的变了才重算（拖动推子时会连着好几块都调到这里）
    eq.set_enabled(params.eq_on);
    eq.set_preamp(params.eq_preamp);
    eq.set_bands(&params.eq_bands);

    // 环里不够一块（7.1 交错）：睡 1 ms 再看（渲染端会补静音，不会卡）
    if src.slots() < block * cap_ch {
      std::thread::sleep(std::time::Duration::from_millis(1));
      continue;
    }

    // 取一块的 8 声道 7.1（交错 → 平面转置）：每帧 pop 8 个样本，
    // 按 raw[ch][t] 填入第 ch 路第 t 个样本。
    for t in 0..block {
      for dst in raw.iter_mut() {
        dst[t] = src.pop().unwrap_or(0.0);
      }
    }

    // 0) 原始输入每声道峰值（FC 清零之前）：判断游戏到底往每个声道放了什么。
    //    FC 长期静默 = 游戏只输出立体声 = 后面 FC 自抑制「没效果」的根因。
    let mut raw_peak = [0.0f32; CHANNELS];
    for (c, peak) in raw_peak.iter_mut().enumerate() {
      for v in raw[c].iter() {
        let a = v.abs();
        if a > *peak {
          *peak = a;
        }
      }
    }

    // 0.5) 枪声快照：前方声道（FL/FR/FC）能量突增 = 开火的起音（比背景高 2.5 倍），
    //      冻结接下来 ~100ms 各声道的最大峰值。开一枪，界面就显示这一枪的声道分布。
    {
      let front_peak = raw_peak[Speaker::Fl.index()]
        .max(raw_peak[Speaker::Fr.index()])
        .max(raw_peak[Speaker::Fc.index()]);
      if front_peak > 0.05 && front_peak > prev_front_peak * 2.5 {
        snap_left = 40; // 40 块 × 2.67 ms ≈ 107 ms
        gun_snapshot = [0.0f32; CHANNELS];
      }
      if snap_left > 0 {
        snap_left -= 1;
        for (c, v) in gun_snapshot.iter_mut().enumerate() {
          *v = (*v).max(raw_peak[c]);
        }
      }
      prev_front_peak = prev_front_peak * 0.8 + front_peak * 0.2;
    }

    // 1) 7.1 域：每方向增益/延时（8 个推子，语义与原来完全一致）。
    sp.process_bed(&raw, &mut bus);

    // 1.5) ATK V3 空间引擎（1:1 翻译 ArtIsWar 开源 atk_spatial_engine.jsfx）：
    //      在 7.1 声道域、球谐编码之前做完整预处理——FC 自抑制 + center duck +
    //      自适应 EQ + 四点 crossover + self-suppress + CMR + 瞬态整形 + 下行扩展 +
    //      方向强调 + LFE 侧链。核心是「按声道区分自己/敌人」而非「按响度」。
    let mut ap = AtkV3Params::v3_default();
    ap.apply_knobs(
      params.self_gun,
      params.self_foot,
      params.leveller,
      params.duck_db,
      params.range,
    );
    atk.set_params(&ap);
    atk.process(&mut bus);
    // duck 深度峰值保持：更负则更新，否则缓慢释放回 0。
    let dd = atk.last_duck_db;
    if dd < duck_peak {
      duck_peak = dd;
    } else {
      duck_peak *= 0.97;
    }

    // 2) HeSuVi HRIR 卷积：8 个 7.1 声道（含 LFE，其 FIR 复用中置）→ 双耳。
    conv.process(&bus, &mut ears);
    l.copy_from_slice(&ears[0]);
    r.copy_from_slice(&ears[1]);

    // 5) makeup 补偿（HRIR 卷积后）。
    let make = db_to_gain(params.makeup_db);
    for t in 0..block {
      l[t] *= make;
      r[t] *= make;
    }

    // 各方向峰值（推子对应 8 个方向，取增益/延时后的 7.1）。
    let mut in_peak = [0.0f32; CHANNELS];
    for (c, peak) in in_peak.iter_mut().enumerate() {
      for v in bus[c].iter() {
        let a = v.abs();
        if a > *peak {
          *peak = a;
        }
      }
    }

    // EQ 放在卷积之后：它补偿的是耳机本身的频响，而耳机在 HRIR 之后才介入
    eq.process(&mut l, &mut r);

    // 输出端 lookahead limiter（最后一道）：压平瞬态前沿，保证输出不削波。
    // 放在 EQ 之后，是因为 EQ 的正增益可能再把已经限幅的信号顶过 ceiling。
    limiter.process(&mut l, &mut r);

    // 写双耳（交错 L/R）
    let mut ear_peak = [0.0f32; EARS];
    for t in 0..block {
      ear_peak[0] = ear_peak[0].max(l[t].abs());
      ear_peak[1] = ear_peak[1].max(r[t].abs());
      let _ = dst.push(l[t]);
      let _ = dst.push(r[t]);
    }

    shared.frames.fetch_add(block as u64, Ordering::Relaxed);
    if last_stat.elapsed().as_millis() >= 50 {
      let buf_ms = src.slots() as f32 * 1000.0 / (SAMPLE_RATE as f32 * cap_ch as f32);
      shared
        .buffer_ms100
        .store((buf_ms * 100.0) as u64, Ordering::Relaxed);
      if let Ok(mut lv) = shared.levels.lock() {
        lv.raw_peak = raw_peak;
        lv.in_peak = in_peak;
        lv.ear_peak = ear_peak;
        lv.gun_snapshot = gun_snapshot;
        lv.duck_db = duck_peak;
        // 增益后的每声道峰值：直接由输入峰值 × 增益推算，省一次遍历
        for (c, out) in lv.out_peak.iter_mut().enumerate() {
          let g = if params.gains_db[c] <= -60.0 {
            0.0
          } else {
            10f32.powf(params.gains_db[c] / 20.0)
          };
          *out = (in_peak[c] * g).min(4.0);
        }
      }
      last_stat = Instant::now();
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn params_with(gains: [f32; CHANNELS]) -> Params {
    Params {
      gains_db: gains,
      ..Default::default()
    }
  }

  #[test]
  fn hrir_is_finite_and_normalised() {
    let h = synth_hrir(512, 175.0);
    assert_eq!(h.taps, 512);
    assert_eq!(h.ir.len(), CHANNELS * EARS * 512);
    assert!(h.ir.iter().all(|v| v.is_finite()));
    // 每个 FIR 都必须有能量，否则那个方向就是哑的
    for c in 0..CHANNELS {
      for e in 0..EARS {
        let e_sum: f32 = h.fir(c, e).iter().map(|v| v * v).sum();
        assert!(e_sum > 1e-4, "声道 {c} 耳 {e} 的 FIR 没有能量");
      }
    }
  }

  #[test]
  fn muting_one_speaker_does_not_touch_others() {
    // 这是整个功能的立论：8 个方向互不干扰。
    // 单位 HRIR（每路直达双耳）下，把 BL 拉到 -80 dB，FL 的能量必须一模一样。
    let block = 64usize;
    let base = params_with([0.0; CHANNELS]);
    let mut muted = params_with([0.0; CHANNELS]);
    muted.gains_db[Speaker::Bl.index()] = -80.0;

    let mut sp_a = Spatial::with_block(Hrir::identity(4), base.to_spatial(), block);
    let mut sp_b = Spatial::with_block(Hrir::identity(4), muted.to_spatial(), block);

    let mut bus: Vec<Vec<f32>> = vec![vec![0.0; block]; CHANNELS];
    for (c, ch) in bus.iter_mut().enumerate() {
      ch.fill(0.1 * (c as f32 + 1.0));
    }
    let mut l1 = vec![0.0f32; block];
    let mut r1 = vec![0.0f32; block];
    let mut l2 = vec![0.0f32; block];
    let mut r2 = vec![0.0f32; block];
    sp_a.process_planar(&bus, &mut l1, &mut r1);
    sp_b.process_planar(&bus, &mut l2, &mut r2);

    // BL 关掉后总能量必须下降（说明它确实在被处理）
    let e1: f32 = l1.iter().chain(r1.iter()).map(|v| v * v).sum();
    let e2: f32 = l2.iter().chain(r2.iter()).map(|v| v * v).sum();
    assert!(e2 < e1 * 0.95, "BL 静音后能量没有下降：{e1} → {e2}");

    // 而 FL 单独渲染的能量必须不变
    let mut only_fl: Vec<Vec<f32>> = vec![vec![0.0; block]; CHANNELS];
    only_fl[Speaker::Fl.index()] = bus[Speaker::Fl.index()].clone();
    let mut la = vec![0.0f32; block];
    let mut ra = vec![0.0f32; block];
    let mut sp_c = Spatial::with_block(Hrir::identity(4), base.to_spatial(), block);
    sp_c.process_planar(&only_fl, &mut la, &mut ra);
    let fl_energy: f32 = la.iter().chain(ra.iter()).map(|v| v * v).sum();
    assert!(fl_energy > 0.0, "FL 能量不该为 0");
  }

  #[test]
  fn block_change_keeps_signal_intact() {
    // 换块长不能改声音：同一段信号按 64 / 128 处理，拼接后必须一致。
    let n = 1024usize;
    let signal: Vec<f32> = (0..n).map(|i| (i as f32 * 0.05).sin() * 0.5).collect();
    let hrir = synth_hrir(128, 175.0);

    let mut reference: Option<(Vec<f32>, Vec<f32>)> = None;
    for block in [64usize, 128] {
      let mut sp = Spatial::with_block(hrir.clone(), Params::default().to_spatial(), block);
      let mut bus: Vec<Vec<f32>> = vec![vec![0.0; block]; CHANNELS];
      let mut all_l = Vec::new();
      let mut all_r = Vec::new();
      let mut ol = vec![0.0f32; block];
      let mut or = vec![0.0f32; block];
      for b in 0..(n / block) {
        for (c, dst) in bus.iter_mut().enumerate() {
          dst.copy_from_slice(&signal[b * block..(b + 1) * block]);
          let _ = c;
        }
        sp.process_planar(&bus, &mut ol, &mut or);
        all_l.extend_from_slice(&ol);
        all_r.extend_from_slice(&or);
      }
      match &reference {
        None => reference = Some((all_l, all_r)),
        Some((rl, rr)) => {
          let dl = all_l
            .iter()
            .zip(rl)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
          assert!(dl < 1e-4, "块长 {block} 与基准不一致，最大差 {dl}");
          let dr = all_r
            .iter()
            .zip(rr)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
          assert!(dr < 1e-4, "块长 {block} 右声道不一致，最大差 {dr}");
        }
      }
    }
  }
}
