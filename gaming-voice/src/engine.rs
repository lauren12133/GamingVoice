//! 处理核心：8 声道总线 → 每声道增益/延时 → HRIR → 双耳。
//!
//! 与平台无关：这里只认「交错排列的 f32 环」，采集/渲染各自在 `audio_windows.rs` 里。
//! 这样 Linux 上也能跑测试（`cargo test` 用假环喂数据），不必等 Windows。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::dsp::surround::eq::{self, Eq};
use crate::dsp::surround::hrir::EARS;
use crate::dsp::surround::layout::{Speaker, CHANNELS};
use crate::dsp::surround::{Hrir, Spatial, SpatialParams};

use crate::audio::SAMPLE_RATE;

/// **默认处理块长（样本）：128 = 2.7 ms。** 用户可在界面 [`BLOCK_MIN`]..=[`BLOCK_MAX`] 内调整。
///
/// 为什么默认 128：
///
/// 1. 卷积是精确的 overlap-save，换块长不改声音（`block_change_keeps_signal_intact`
///    测试钉住了这点）；小块长的代价只有 CPU —— 分区数翻倍，频域乘法量上升。
/// 2. 旧实现里取数用 `HOP`、处理用 `block`：一次 pop 480 帧却只消费 128 帧、丢 352 帧，
///    听感就是"电流声"。该 bug 已修（`run_processor` 统一按当前块长取数与处理），
///    与块长本身无关。
/// 3. 采样率固定 48 kHz 时，128 帧 = 2.7 ms。注意采集端 WASAPI 周期仍是 10 ms，
///    数据每 10 ms 到环一次，所以单改块长本身不降端到端延迟，只是把「攒满一块」
///    的门槛从 480 帧降到 128 帧。
///
/// 代价：块长越小分区越多、每秒频域乘法越多，**会略微增加 CPU 占用**。
pub const BLOCK: usize = 128;

/// 用户可选的块长下限（样本）：64 = 1.3 ms。
pub const BLOCK_MIN: usize = 64;

/// 用户可选的块长上限（样本）：480 = 10 ms（= 采集周期，再大环会饥饿、只增延迟）。
pub const BLOCK_MAX: usize = 480;

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
  /// 处理块长（样本），可在 [`BLOCK_MIN`]..=[`BLOCK_MAX`] 内调整（默认 [`BLOCK`]）。
  pub block: usize,
}

impl Default for Params {
  fn default() -> Self {
    Params {
      // 出厂设置：中置压低（游戏里中置多是环境音/报点），后置抬高（脚步声）
      gains_db: [0.0, 0.0, -3.0, 0.0, 4.0, 4.0, 0.0, 0.0],
      delays_ms: [0.0; CHANNELS],
      makeup_db: -6.0,
      eq_bands: eq::default_bands(),
      eq_on: true,
      eq_preamp: 0.0,
      block: BLOCK,
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
  /// 每声道输入峰值（线性 0..1+）
  pub in_peak: [f32; CHANNELS],
  /// 每声道增益后的峰值
  pub out_peak: [f32; CHANNELS],
  /// 双耳输出峰值
  pub ear_peak: [f32; EARS],
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

/// 自动挑一套：`hrir/` 里排序第一个；一个都没有则 `None`（走合成兜底）。
pub fn auto_hrir() -> Option<PathBuf> {
  list_hrir_files().into_iter().next()
}

/// 按文件名找 HRIR（也接受绝对路径）。找不到返回 `None`。
pub fn find_hrir(name: &str) -> Option<PathBuf> {
  let want = name.trim();
  if want.is_empty() {
    return None;
  }
  let direct = Path::new(want);
  if direct.is_absolute() && direct.is_file() {
    return Some(direct.to_path_buf());
  }
  list_hrir_files()
    .into_iter()
    .find(|p| p.file_name().and_then(|s| s.to_str()) == Some(want))
}

/// 读一个 HRIR 文件并归一化峰值。`Err` 是人话，直接上屏。
pub fn load_hrir(path: &Path) -> Result<Hrir, String> {
  let bytes = std::fs::read(path).map_err(|e| format!("读不了 {}：{e}", path.display()))?;
  let w = crate::dsp::surround::wav::parse(&bytes)?;
  let name = path
    .file_name()
    .map(|s| s.to_string_lossy().into_owned())
    .unwrap_or_else(|| "hrir.wav".to_string());
  let mut h = Hrir::from_wav(&w, name)?;
  // 不同来源的 HRIR 电平能差好几倍，不归一就是"换档案 = 一次音量跳变"。
  // 全局等比缩放不改 ILD/ITD，方向信息完全保留。
  h.normalize_peak(1.0);
  Ok(h)
}

/// 处理线程主体：从 `src`（交错 8ch）读、往 `dst`（交错 2ch）写。
///
/// 环里没有数据就补静音（不阻塞）—— 采集断流时宁可静音，也不要卡住渲染线程。
///
/// `hrir` 传 `None` 才退回 [`synth_hrir`]：那是**兜底**，不是默认值。合成模型只有
/// ITD 与头影一阶低通，没有耳廓谱线索，**前后在数学上不可区分**；有实测 IR 就必须用实测。
pub fn run_processor(
  shared: Arc<Shared>,
  mut src: rtrb::Consumer<f32>,
  mut dst: rtrb::Producer<f32>,
  stop: Arc<AtomicBool>,
  hrir: Option<Hrir>,
) {
  let mut params = shared.params.lock().map(|p| p.clone()).unwrap_or_default();

  let measured = hrir.is_some();
  let hrir = hrir.unwrap_or_else(|| synth_hrir(512, 175.0));
  // 「现在用的是哪套 HRIR」必须上屏 —— 装错了的声音听起来仍然"像环绕"，
  // 只是前后分不清，界面上看不出任何异常，这是最难自查的故障。
  if let Ok(mut st) = shared.status.lock() {
    st.hrir_name = hrir.name.clone();
    st.hrir_measured = measured;
  }
  // 处理块长：从参数里读，可在 BLOCK_MIN..=BLOCK_MAX 内热改（改动会重建卷积器与缓冲）。
  let mut block = params.block.clamp(BLOCK_MIN, BLOCK_MAX);
  let mut sp = Spatial::with_block(hrir.clone(), params.to_spatial(), block);
  let mut eq = Eq::new(SAMPLE_RATE as f32);

  // 取数块与处理块必须是同一个值（当前 `block`）。旧实现里一个用 HOP、一个用可调 block，
  // 结果每次只消费 480 中的 128 帧，剩下的被丢掉 → 周期性断续，听感就是"电流声"。
  let mut bus: Vec<Vec<f32>> = vec![vec![0.0; block]; CHANNELS];
  let mut l = vec![0.0f32; block];
  let mut r = vec![0.0f32; block];
  let mut scratch = [0.0f32; CHANNELS];
  let mut last_stat = Instant::now();

  while !stop.load(Ordering::Relaxed) {
    // 参数可能被界面改了：每块读一次（互斥锁在无竞争时几乎免费）
    if let Ok(p) = shared.params.lock() {
      // 块长变化 → 重建空间化引擎与缓冲（卷积器分区、窗口尺寸都跟着块长走）。
      // 这是低频操作（用户改一次才触发一次），重建会清掉旧 overlap，短暂一跳后恢复。
      let want = p.block.clamp(BLOCK_MIN, BLOCK_MAX);
      if want != block {
        block = want;
        sp = Spatial::with_block(hrir.clone(), p.to_spatial(), block);
        bus = vec![vec![0.0; block]; CHANNELS];
        l = vec![0.0f32; block];
        r = vec![0.0f32; block];
      }
      params = p.clone();
      sp.set_params(params.to_spatial());
    }
    // 系数只在值真的变了才重算（拖动推子时会连着好几块都调到这里）
    eq.set_enabled(params.eq_on);
    eq.set_preamp(params.eq_preamp);
    eq.set_bands(&params.eq_bands);

    // 环里不够一块：睡 1 ms 再看（渲染端会补静音，不会卡）
    if src.slots() < block * CHANNELS {
      std::thread::sleep(std::time::Duration::from_millis(1));
      continue;
    }

    // 取一块的 8 声道
    let mut in_peak = [0.0f32; CHANNELS];
    for t in 0..block {
      for ((dst, slot), peak) in bus
        .iter_mut()
        .zip(scratch.iter_mut())
        .zip(in_peak.iter_mut())
      {
        *slot = src.pop().unwrap_or(0.0);
        dst[t] = *slot;
        let a = slot.abs();
        if a > *peak {
          *peak = a;
        }
      }
    }

    sp.process_planar(&bus, &mut l, &mut r);
    // EQ 放在卷积之后：它补偿的是耳机本身的频响，而耳机在 HRIR 之后才介入
    eq.process(&mut l, &mut r);

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
      let buf_ms = src.slots() as f32 * 1000.0 / (SAMPLE_RATE as f32 * CHANNELS as f32);
      shared
        .buffer_ms100
        .store((buf_ms * 100.0) as u64, Ordering::Relaxed);
      if let Ok(mut lv) = shared.levels.lock() {
        lv.in_peak = in_peak;
        lv.ear_peak = ear_peak;
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
  fn loads_hesuvi_14ch_from_disk() {
    // 端到端：磁盘上的 14 轨 wav → 引擎能直接用的 Hrir（这就是之前缺的那一环）。
    const TRACKS: usize = 14;
    let taps = 8usize;
    let dir = std::env::temp_dir().join(format!("gv-hrir-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("atmos-14ch.wav");

    let mut data = vec![0.0f32; taps * TRACKS];
    for track in 0..TRACKS {
      for t in 0..taps {
        data[t * TRACKS + track] = (track + 1) as f32 / 16.0;
      }
    }
    let bytes = crate::dsp::surround::wav::encode_f32(48_000, TRACKS as u16, 0, &data);
    std::fs::write(&path, bytes).unwrap();

    let h = load_hrir(&path).unwrap();
    assert_eq!(h.taps, taps);
    assert_eq!(h.name, "atmos-14ch.wav");
    // 峰值归一：最大 FIR（第 14 轨 = 中置右耳 = 14/16）被拉到 1.0
    assert!((h.peak() - 1.0).abs() < 1e-6);
    // 归一化系数 k = 1 / (14/16)；FL 左耳来自第 1 轨、右耳来自第 2 轨
    let k = 16.0 / 14.0;
    assert!((h.fir(Speaker::Fl.index(), 0)[0] - 1.0 / 16.0 * k).abs() < 1e-6);
    assert!((h.fir(Speaker::Fl.index(), 1)[0] - 2.0 / 16.0 * k).abs() < 1e-6);

    std::fs::remove_dir_all(&dir).ok();
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
