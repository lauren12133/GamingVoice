//! GamingVoice —— 8 声道虚拟 7.1 调音台
//!
//! 链路：`游戏(7.1) → 虚拟端点 → WASAPI loopback 抓 8 声道 → 每声道增益/延时
//!        → HRIR 卷积 → 双耳 2 声道 → 耳机`
//!
//! 为什么不做进 PureVox：它的引擎是 **F32 单声道**（`ARCHITECTURE.md` §2），
//! 采集回调第一行就把 N 声道求平均成 1 声道（`audio/capture.rs:221-233`）。
//! 8 个推子在那条链路上只能合成一个标量，`Σ(w_c·g_c)·x`，物理上不可能分方向。
//!
//! 这个工具走的是**旁路**：自己开 loopback 和自己的渲染流，完全不碰引擎的单声道通路。

// 双击启动时不要弹黑框：release 版直接以 GUI 程序身份运行。
// debug 版保留控制台 —— 调试时那些枚举端点/HRIR 的日志就是靠它看的。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

mod audio {
  /// 全链路统一 48 kHz：I48k 是 Windows 混音的默认值，也是 HRIR 的常见采样率。
  pub const SAMPLE_RATE: u32 = 48_000;
  /// 引擎 hop（10 ms）。处理块长可以更小，但取数据的单位是它。
  pub const HOP: usize = 480;
  /// 7.1 环绕的 8 个声道
  #[allow(dead_code)] // 只有 Windows 的采集/渲染用到（Linux 走 stub）
  pub const CHANNELS: usize = 8;
}

mod dsp;

mod engine;

mod presets;

mod eq_presets;

mod eq_import;

mod autostart;

mod vbcable;

/// 内嵌的 GV 图标（窗口 + 托盘共用）。
#[cfg(windows)]
mod icon;

/// 原生窗口 + 任务栏托盘（Windows）。非 Windows 用空实现，只为让 `cargo test` 跑通。
#[cfg(windows)]
#[path = "gui_windows.rs"]
mod gui;

#[cfg(not(windows))]
#[path = "gui_stub.rs"]
mod gui;

use engine::Shared;

#[cfg(windows)]
#[path = "audio_windows.rs"]
mod audio_platform;

#[cfg(not(windows))]
#[path = "audio_stub.rs"]
mod audio_platform;

const PORT: u16 = 8760;

/// 单个方向的增益上限。
///
/// 20 dB 是"能明显听出来"和"不会一开就削波"的折中：8 路同时 +20 dB 会立刻爆掉，
/// 但竞技场景只抬 1~2 个方向（脚步在后方），其余保持 0 或负值，
/// 加上默认 -6 dB 的总补偿，实际峰值仍在可接受范围。UI 上超过 +12 dB 时会提示削波。
pub const MAX_GAIN_DB: f32 = 20.0;

/// 正在跑的音频会话。
struct Runtime {
  stop: Arc<AtomicBool>,
  threads: Vec<std::thread::JoinHandle<()>>,
  source_label: String,
  sink_label: String,
}

static RT: Mutex<Option<Runtime>> = Mutex::new(None);

/// 端点枚举缓存。
///
/// 界面 100 ms 轮询一次，而每次 `enum_endpoints` 都要走 COM：创建枚举器、逐个
/// `Activate` + `GetMixFormat`，几十毫秒起步。不缓存的话光是轮询就能烧掉半核。
/// 插拔设备后点界面上的"刷新设备"清空缓存。
static EPS: Mutex<Vec<audio_platform::Endpoint>> = Mutex::new(Vec::new());

pub fn endpoints_cached() -> Vec<audio_platform::Endpoint> {
  let mut g = match EPS.lock() {
    Ok(g) => g,
    Err(_) => return Vec::new(),
  };
  if g.is_empty() {
    if let Ok(v) = audio_platform::enum_endpoints() {
      *g = v;
    }
  }
  g.clone()
}

fn main() {
  let shared = Shared::new();

  // HRIR 目录：不存在就建一个空目录，用户才知道 wav 该往哪放
  // （「看不见的状态等于不存在」——目录都没有，就别指望用户猜到路径）。
  for dir in engine::hrir_dirs() {
    if !dir.exists() && std::fs::create_dir_all(&dir).is_ok() {
      println!("已创建 HRIR 目录：{}", dir.display());
    }
  }
  let found = refresh_hrirs();
  if found.is_empty() {
    println!(
      "[提示] HRIR 目录里没有 .wav —— 将使用合成模型（只有方位感，前后分不清）。\n\
       把 HeSuVi 的 hrir/*.wav 直接拷进 -> {}",
      engine::hrir_dirs()
        .first()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "hrir/".into())
    );
  } else {
    println!("发现 {} 个 HRIR：{}", found.len(), found.join(", "));
  }

  // 先枚举一次端点，让界面一打开就有内容
  let endpoints = endpoints_cached();
  if endpoints.is_empty() {
    eprintln!("[警告] 没有枚举到任何输出端点。");
  } else {
    println!("枚举到 {} 个输出端点：", endpoints.len());
    for e in &endpoints {
      println!(
        "  {}{}",
        e.label,
        if e.is_surround() {
          "   ← 8 声道，可作 7.1 源"
        } else {
          ""
        }
      );
    }
  }

  // 单实例：端口已被占，八成是已经开着一个 GamingVoice。
  // 这时不要弹第二个窗口（会有两个托盘图标、两套状态），直接把面板打开就完事。
  if TcpStream::connect(("127.0.0.1", PORT)).is_ok() {
    println!("GamingVoice 已在运行，直接打开面板。");
    open_browser(&format!("http://127.0.0.1:{PORT}/"));
    std::process::exit(0);
  }

  let listener = TcpListener::bind(("127.0.0.1", PORT)).unwrap_or_else(|e| {
    eprintln!("端口 {PORT} 被占用（但不是 GamingVoice）：{e}\n换个端口要改 PORT 常量并重新编译。");
    std::process::exit(1);
  });
  let url = format!("http://127.0.0.1:{PORT}/");
  println!("\n面板地址：{url}");

  // 网页面板在后台线程：GUI 窗口是前台（要跑消息循环），两者共用同一份 Shared。
  let sh = shared.clone();
  std::thread::spawn(move || {
    for stream in listener.incoming().flatten() {
      let sh = sh.clone();
      std::thread::spawn(move || {
        let _ = handle(stream, sh);
      });
    }
  });

  #[cfg(windows)]
  gui::run(shared);

  // 非 Windows（只用于开发机上跑测试 / 看界面）：直接阻塞，不退出
  #[cfg(not(windows))]
  {
    let _ = &url;
    loop {
      std::thread::sleep(Duration::from_secs(3600));
    }
  }
}

pub fn open_browser(url: &str) {
  #[cfg(windows)]
  let _ = std::process::Command::new("cmd")
    .args(["/C", "start", "", url])
    .spawn();
  #[cfg(target_os = "macos")]
  let _ = std::process::Command::new("open").arg(url).spawn();
  #[cfg(all(unix, not(target_os = "macos")))]
  let _ = std::process::Command::new("xdg-open").arg(url).spawn();
}

// ---------------------------------------------------------------- HTTP

fn handle(mut s: TcpStream, shared: Arc<Shared>) -> std::io::Result<()> {
  let mut reader = BufReader::new(s.try_clone()?);
  let mut line = String::new();
  if reader.read_line(&mut line)? == 0 {
    return Ok(());
  }
  let mut parts = line.split_whitespace();
  let method = parts.next().unwrap_or("").to_string();
  let path = parts.next().unwrap_or("/").to_string();

  // 读头部，主要为了 Content-Length
  let mut len = 0usize;
  loop {
    let mut h = String::new();
    if reader.read_line(&mut h)? == 0 {
      break;
    }
    let t = h.trim();
    if t.is_empty() {
      break;
    }
    if let Some(v) = t.to_ascii_lowercase().strip_prefix("content-length:") {
      len = v.trim().parse().unwrap_or(0);
    }
  }
  let mut body = vec![0u8; len];
  if len > 0 {
    reader.read_exact(&mut body)?;
  }

  let (ctype, payload): (&str, Vec<u8>) = match (method.as_str(), path.as_str()) {
    ("GET", "/") | ("GET", "/index.html") => {
      ("text/html; charset=utf-8", INDEX.as_bytes().to_vec())
    }
    ("GET", "/app.js") => ("text/javascript; charset=utf-8", APP_JS.as_bytes().to_vec()),
    ("GET", "/style.css") => ("text/css; charset=utf-8", STYLE.as_bytes().to_vec()),
    ("GET", "/api/state") => (
      "application/json",
      state_json(&shared).to_string().into_bytes(),
    ),
    ("POST", "/api/params") => ("application/json", set_params(&shared, &body).into_bytes()),
    ("POST", "/api/start") => ("application/json", start(&shared, &body).into_bytes()),
    ("POST", "/api/stop") => ("application/json", stop().into_bytes()),
    ("POST", "/api/refresh") => ("application/json", refresh().into_bytes()),
    ("POST", "/api/vbcable/install") => (
      "application/json",
      vbcable_install().to_string().into_bytes(),
    ),
    ("POST", "/api/setup-guide") => ("application/json", setup_guide().to_string().into_bytes()),
    ("POST", "/api/autostart") => (
      "application/json",
      set_autostart(&body).to_string().into_bytes(),
    ),
    ("GET", "/api/presets") => (
      "application/json",
      presets_json(&shared).to_string().into_bytes(),
    ),
    ("POST", "/api/presets") => (
      "application/json",
      presets_post(&shared, &body).into_bytes(),
    ),
    ("POST", "/api/eq/presets") => (
      "application/json",
      eq_presets_post(&shared, &body).into_bytes(),
    ),
    ("POST", "/api/eq/import") => ("application/json", eq_import_post(&body).into_bytes()),
    _ => ("text/plain; charset=utf-8", b"404".to_vec()),
  };

  let head = format!(
    "HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
    payload.len()
  );
  s.write_all(head.as_bytes())?;
  s.write_all(&payload)?;
  s.flush()
}

/// 可用 HRIR 文件名缓存。
///
/// 目录扫描不该出现在每帧的 `state` 里（面板轮询 10–20 Hz），所以只在
/// 启动和「刷新」时重扫。
static HRIRS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// 当前 HRIR 实际来自哪个目录（可能不是首选目录，所以单独缓存）。
static HRIR_DIR_ACTIVE: Mutex<String> = Mutex::new(String::new());

/// 重扫 `hrir/` 目录，返回文件名列表（启动流程与「刷新」都会调）。
pub fn refresh_hrirs() -> Vec<String> {
  let paths = engine::list_hrir_files();
  let names: Vec<String> = paths
    .iter()
    .filter_map(|p| p.file_name().map(|s| s.to_string_lossy().into_owned()))
    .collect();
  if let Ok(mut g) = HRIRS.lock() {
    *g = names.clone();
  }
  if let Ok(mut g) = HRIR_DIR_ACTIVE.lock() {
    *g = paths
      .first()
      .and_then(|p| p.parent().map(|d| d.display().to_string()))
      .or_else(|| engine::hrir_dirs().first().map(|p| p.display().to_string()))
      .unwrap_or_default();
  }
  names
}

fn hrir_names() -> Vec<String> {
  HRIRS.lock().map(|g| g.clone()).unwrap_or_default()
}

pub fn state_json(shared: &Arc<Shared>) -> Value {
  let p = shared.params.lock().map(|p| p.clone()).unwrap_or_default();
  let lv = shared.levels.lock().map(|l| *l).unwrap_or_default();
  let (running, src, snk, err) = RT
    .lock()
    .ok()
    .and_then(|g| {
      g.as_ref().map(|r| {
        (
          true,
          r.source_label.clone(),
          r.sink_label.clone(),
          None::<String>,
        )
      })
    })
    .unwrap_or_default();
  let st = shared.status.lock().map(|s| s.clone()).unwrap_or_default();
  let vb = vb_cable_state();
  let endpoints: Vec<Value> = endpoints_cached()
    .iter()
    .map(|e| {
      json!({
        "id": e.id, "label": e.label, "name": e.name, "channels": e.channels,
        "default": e.default, "surround": e.is_surround(),
      })
    })
    .collect();

  json!({
    "running": running,
    "source": src,
    "sink": snk,
    "error": err.or(st.error.clone()),
    "endpoints": endpoints,
    "params": {
      "gains_db": p.gains_db,
      "delays_ms": p.delays_ms,
      "makeup_db": p.makeup_db,
      "eq_bands": eq_bands_json(&p.eq_bands),
      "eq_on": p.eq_on,
      "eq_preamp": p.eq_preamp,
      "eq_preamp_min": dsp::surround::eq::MIN_PREAMP_DB,
      "eq_preamp_max": dsp::surround::eq::MAX_PREAMP_DB,
      "eq_max": dsp::surround::eq::MAX_BANDS,
      "block": engine::BLOCK,
    },
    "preset": presets::active_name(),
    // 预设列表跟着 state 走：面板每帧都要用它画下拉，单独发一次请求反而更容易不同步
    "presets": presets::list().iter().map(|p| p.to_json()).collect::<Vec<_>>(),
    // EQ 预设同理跟着 state 走：面板和托盘都要用它画下拉/菜单
    "eq_preset": eq_presets::active_name(),
    "eq_presets": eq_presets::list().iter().map(|p| p.to_json()).collect::<Vec<_>>(),
    "autostart": autostart::enabled(),
    "vbcable": {
      "installed": vb.0,
      "surround": vb.1,
    },
    "hrir": {
      "files": hrir_names(),
      "active": st.hrir_name,
      "measured": st.hrir_measured,
      "dir": HRIR_DIR_ACTIVE.lock().map(|d| d.clone()).unwrap_or_default(),
    },
    "levels": {
      "in_peak": lv.in_peak,
      "out_peak": lv.out_peak,
      "ear_peak": lv.ear_peak,
    },
    "status": {
      "block_ms": params_block_ms(),
      "buffer_ms": shared.buffer_ms100.load(Ordering::Relaxed) as f32 / 100.0,
      "underruns": shared.underruns.load(Ordering::Relaxed),
      "frames": shared.frames.load(Ordering::Relaxed),
      "source_channels": st.source_channels,
      "source_rate": st.source_rate,
      "sink_channels": st.sink_channels,
      "sink_rate": st.sink_rate,
    }
  })
}

/// 处理块延迟（毫秒）。块长锁定为 [`engine::BLOCK`]，所以这是个常量。
fn params_block_ms() -> f32 {
  engine::BLOCK as f32 * 1000.0 / audio::SAMPLE_RATE as f32
}

/// 频点列表 → JSON。前端拿它画表格和曲线，字段名要和 `set_params` 的解析对齐。
fn eq_bands_json(bands: &[dsp::surround::eq::Band]) -> Vec<Value> {
  bands
    .iter()
    .map(|b| {
      json!({
        "type": b.kind.as_str(),
        "freq": b.freq,
        "q": b.q,
        "gain": b.gain_db,
      })
    })
    .collect()
}

/// JSON 数组 → 频点链。
///
/// 「整条替换」而不是逐项合并：频点可以被删、被重排，合并会留下幽灵段。
/// 越界值一律走 `sanitized`，别让界面的笔误把滤波器推到不稳定区。
fn bands_from_json(arr: &[Value]) -> Vec<dsp::surround::eq::Band> {
  let mut bands = Vec::with_capacity(arr.len().min(dsp::surround::eq::MAX_BANDS));
  for item in arr.iter().take(dsp::surround::eq::MAX_BANDS) {
    let kind = item
      .get("type")
      .and_then(|x| x.as_str())
      .and_then(dsp::surround::eq::FilterKind::parse)
      .unwrap_or(dsp::surround::eq::FilterKind::Peak);
    let num = |k: &str, d: f32| {
      item
        .get(k)
        .and_then(|x| x.as_f64())
        .map(|x| x as f32)
        .filter(|x| x.is_finite())
        .unwrap_or(d)
    };
    bands.push(
      dsp::surround::eq::Band {
        kind,
        freq: num("freq", 1000.0),
        q: num("q", 1.41),
        gain_db: num("gain", 0.0),
      }
      .sanitized(),
    );
  }
  bands
}

fn set_params(shared: &Arc<Shared>, body: &[u8]) -> String {
  let Ok(v) = serde_json::from_slice::<Value>(body) else {
    return json!({"ok": false, "error": "body 不是合法 JSON"}).to_string();
  };
  let Ok(mut p) = shared.params.lock() else {
    return json!({"ok": false, "error": "内部锁失败"}).to_string();
  };
  if let Some(arr) = v.get("gains_db").and_then(|x| x.as_array()) {
    for (i, item) in arr.iter().take(8).enumerate() {
      if let Some(d) = item.as_f64() {
        p.gains_db[i] = (d as f32).clamp(-60.0, MAX_GAIN_DB);
      }
    }
  }
  if let Some(arr) = v.get("delays_ms").and_then(|x| x.as_array()) {
    for (i, item) in arr.iter().take(8).enumerate() {
      if let Some(d) = item.as_f64() {
        p.delays_ms[i] = (d as f32).clamp(0.0, 25.0);
      }
    }
  }
  if let Some(d) = v.get("makeup_db").and_then(|x| x.as_f64()) {
    p.makeup_db = (d as f32).clamp(-30.0, 12.0);
  }
  if let Some(arr) = v.get("eq_bands").and_then(|x| x.as_array()) {
    // 整条链替换而不是逐项覆盖：频点可以被删、被重排，逐项合并会留下幽灵段
    p.eq_bands = bands_from_json(arr);
  }
  if let Some(d) = v.get("eq_preamp").and_then(|x| x.as_f64()) {
    let d = d as f32;
    if d.is_finite() {
      p.eq_preamp = d.clamp(
        dsp::surround::eq::MIN_PREAMP_DB,
        dsp::surround::eq::MAX_PREAMP_DB,
      );
    }
  }
  if let Some(b) = v.get("eq_on").and_then(|x| x.as_bool()) {
    p.eq_on = b;
  }
  // 改完之后再看一眼：曲线还是不是那个预设？
  // 是 → 名字保留（界面上"改回原值"不该莫名其妙变成「未保存」）；
  // 不是 → 名字作废，否则下拉框会拿一个不再成立的标签骗人。
  if !eq_presets::still_matches(&p.eq_bands, p.eq_preamp) {
    eq_presets::clear_active();
  }
  // 块长入口已按需求锁死为 480（10 ms），这里显式忽略旧客户端的 block 字段，
  // 而不是静默接受 —— 免得界面以为改成功了。
  // 将来要重新开放：把上面 `Params` 的 `block` 字段加回来，并让 `run_processor` 用它。
  json!({
    "ok": true,
    "note": format!("块长固定 {} 样本（10 ms）", engine::BLOCK),
  })
  .to_string()
}

pub fn start(shared: &Arc<Shared>, body: &[u8]) -> String {
  if RT.lock().map(|g| g.is_some()).unwrap_or(false) {
    let _ = stop();
  }
  let v: Value = serde_json::from_slice(body).unwrap_or(json!({}));
  let source = v
    .get("source")
    .and_then(|x| x.as_str())
    .map(|s| s.to_string());
  let sink = v
    .get("sink")
    .and_then(|x| x.as_str())
    .map(|s| s.to_string());
  // 显式指定 HRIR（文件名或绝对路径）；不指定则自动挑 hrir/ 里第一个。
  let hrir_want = v
    .get("hrir")
    .and_then(|x| x.as_str())
    .map(|s| s.to_string());

  // ── 先把 HRIR 解决掉：纯文件 IO，和设备能不能打开无关，所以放最前面 ─────────
  // 口径：**加载失败不阻断启动**（音频照样能跑），但必须把原因带上屏 —— 否则
  // 用户听到的是一套"有环绕感、但前后分不清"的合成模型，还以为装成功了。
  let mut hrir_warn: Option<String> = None;
  let picked = match hrir_want.as_deref() {
    Some(n) => engine::find_hrir(n).ok_or_else(|| format!("找不到 HRIR「{n}」")),
    None => engine::auto_hrir().ok_or_else(|| {
      format!(
        "{} 里没有 .wav 文件（HeSuVi 的 hrir/*.wav 直接拷进去即可）",
        engine::hrir_dirs()
          .first()
          .map(|p| p.display().to_string())
          .unwrap_or_else(|| "hrir/".into())
      )
    }),
  };
  let hrir = match picked {
    Ok(p) => match engine::load_hrir(&p) {
      Ok(h) => Some(h),
      Err(e) => {
        hrir_warn = Some(format!("{} 解析失败，已退回合成模型：{e}", p.display()));
        None
      }
    },
    Err(e) => {
      hrir_warn = Some(format!("{e}，已退回合成模型（只有方位感，前后分不清）"));
      None
    }
  };
  let hrir_label = hrir.as_ref().map(|h| h.name.clone());
  let hrir_measured = hrir.is_some();
  // 立刻上屏：处理线程要等设备打开才跑得起来，但「用的是哪套 HRIR」现在就该知道。
  if let Ok(mut st) = shared.status.lock() {
    st.hrir_name = hrir_label.clone().unwrap_or_default();
    st.hrir_measured = hrir_measured;
  }

  let stop_flag = Arc::new(AtomicBool::new(false));
  // 输入环 2 s（8 声道交错），输出环 0.5 s（2 声道交错）
  let (in_prod, in_cons) = rtrb::RingBuffer::<f32>::new(audio::SAMPLE_RATE as usize * 2 * 8);
  let (out_prod, out_cons) = rtrb::RingBuffer::<f32>::new(audio::SAMPLE_RATE as usize / 2 * 2);

  let (tx_cap, rx_cap) = std::sync::mpsc::channel();
  let (tx_ren, rx_ren) = std::sync::mpsc::channel();

  // 先起渲染：它是"主时钟"，先跑起来处理线程才有地方写
  let ren = audio_platform::start_render(sink.clone(), out_cons, stop_flag.clone(), tx_ren);
  match rx_ren.recv_timeout(Duration::from_secs(5)) {
    Ok(Ok((ch, _rate))) => {
      if let Ok(mut st) = shared.status.lock() {
        st.sink_channels = ch;
      }
    }
    Ok(Err(e)) => {
      stop_flag.store(true, Ordering::Relaxed);
      return json!({"ok": false, "error": format!("输出设备打不开：{e}")}).to_string();
    }
    Err(_) => {
      stop_flag.store(true, Ordering::Relaxed);
      return json!({"ok": false, "error": "输出设备 5 秒无响应"}).to_string();
    }
  }

  let cap = audio_platform::start_capture(source.clone(), in_prod, stop_flag.clone(), tx_cap);
  match rx_cap.recv_timeout(Duration::from_secs(5)) {
    Ok(Ok((ch, _rate))) => {
      if let Ok(mut st) = shared.status.lock() {
        st.source_channels = ch;
        if ch < 8 {
          st.error = Some(format!(
            "源端点是 {ch} 声道，不是 8 声道 —— 方向信息不存在，推子只会改总音量。\n\
             把该端点（VB-CABLE 之类）在 声音设置 → 配置扬声器 里改成 7.1 环绕。"
          ));
        } else {
          st.error = None;
        }
      }
    }
    Ok(Err(e)) => {
      stop_flag.store(true, Ordering::Relaxed);
      let _ = std::thread::spawn(move || {
        let _ = ren.join();
      });
      return json!({"ok": false, "error": format!("源设备打不开：{e}")}).to_string();
    }
    Err(_) => {
      stop_flag.store(true, Ordering::Relaxed);
      return json!({"ok": false, "error": "源设备 5 秒无响应"}).to_string();
    }
  }

  let sh2 = shared.clone();
  let st2 = stop_flag.clone();
  let proc = std::thread::Builder::new()
    .name("pv-proc".into())
    .spawn(move || engine::run_processor(sh2, in_cons, out_prod, st2, hrir))
    .expect("spawn processor");

  let src_label = source.clone().unwrap_or_else(|| "系统默认输出".into());
  let snk_label = sink.clone().unwrap_or_else(|| "系统默认输出".into());
  if let Ok(mut g) = RT.lock() {
    *g = Some(Runtime {
      stop: stop_flag,
      threads: vec![cap, proc, ren],
      source_label: src_label,
      sink_label: snk_label,
    });
  }
  json!({
    "ok": true,
    "hrir": hrir_label,
    "hrir_measured": hrir_measured,
    "warning": hrir_warn,
  })
  .to_string()
}

/// 开/关开机启动。
pub fn set_autostart(body: &[u8]) -> Value {
  let v: Value = serde_json::from_slice(body).unwrap_or_else(|_| json!({}));
  let on = v.get("on").and_then(|x| x.as_bool()).unwrap_or(false);
  match autostart::set(on) {
    Ok(()) => json!({ "ok": true, "on": on }),
    Err(e) => json!({ "ok": false, "error": e }),
  }
}

/// 预设列表（含当前生效名与当前推子值）。
pub fn presets_json(shared: &Arc<Shared>) -> Value {
  let list: Vec<Value> = presets::list().iter().map(|p| p.to_json()).collect();
  json!({
    "presets": list,
    "active": presets::active_name(),
    "gains_db": presets::current_gains(shared),
  })
}

/// 预设的写操作：`apply` / `save` / `delete`。
pub fn presets_post(shared: &Arc<Shared>, body: &[u8]) -> String {
  let v: Value = serde_json::from_slice(body).unwrap_or_else(|_| json!({}));
  let action = v.get("action").and_then(|x| x.as_str()).unwrap_or("");
  let name = v.get("name").and_then(|x| x.as_str()).unwrap_or("");

  let out = match action {
    "apply" => presets::apply(name, shared).map(|()| json!({ "ok": true })),
    "save" => {
      // 允许直接传 gains_db（界面保存当前推子值），不传就用实时的
      let mut gains = presets::current_gains(shared);
      if let Some(arr) = v.get("gains_db").and_then(|x| x.as_array()) {
        for (i, s) in arr.iter().take(8).enumerate() {
          gains[i] = (s.as_f64().unwrap_or(0.0) as f32).clamp(-60.0, MAX_GAIN_DB);
        }
      }
      presets::save(name, gains).map(|()| json!({ "ok": true }))
    }
    "delete" => presets::delete(name).map(|()| json!({ "ok": true })),
    _ => Err("action 只能是 apply / save / delete".to_string()),
  };

  match out {
    Ok(mut j) => {
      j["presets"] = json!(presets::list()
        .iter()
        .map(|p| p.to_json())
        .collect::<Vec<_>>());
      j.to_string()
    }
    Err(e) => json!({ "ok": false, "error": e }).to_string(),
  }
}

/// EQ 预设的写操作：`apply` / `save` / `delete`。
///
/// `save` 允许直接带 `bands` + `preamp`（界面刚导入完、还没等参数请求回来就存），
/// 不带就从实时参数里取 —— 和方向预设同一套口味。
pub fn eq_presets_post(shared: &Arc<Shared>, body: &[u8]) -> String {
  let v: Value = serde_json::from_slice(body).unwrap_or_else(|_| json!({}));
  let action = v.get("action").and_then(|x| x.as_str()).unwrap_or("");
  let name = v.get("name").and_then(|x| x.as_str()).unwrap_or("");

  let out = match action {
    "apply" => eq_presets::apply(name, shared).map(|()| json!({ "ok": true })),
    "save" => {
      let (live_preamp, live_bands) = eq_presets::current(shared);
      let preamp = v
        .get("preamp")
        .and_then(|x| x.as_f64())
        .map(|x| x as f32)
        .filter(|x| x.is_finite())
        .unwrap_or(live_preamp);
      let bands = match v.get("bands").and_then(|x| x.as_array()) {
        Some(arr) => bands_from_json(arr),
        None => live_bands,
      };
      eq_presets::save(name, preamp, &bands).map(|()| json!({ "ok": true }))
    }
    "delete" => eq_presets::delete(name).map(|()| json!({ "ok": true })),
    _ => Err("action 只能是 apply / save / delete".to_string()),
  };

  match out {
    Ok(mut j) => {
      j["eq_presets"] = json!(eq_presets::list()
        .iter()
        .map(|p| p.to_json())
        .collect::<Vec<_>>());
      j["active"] = json!(eq_presets::active_name());
      j.to_string()
    }
    Err(e) => json!({ "ok": false, "error": e }).to_string(),
  }
}

/// 导入 EQ 文件：**只解析，不动任何状态**。
///
/// 落地由界面决定 —— 它还要顺手把这份曲线存成预设、把警告念给用户听。
/// 放在这儿做（而不是前端拿正则拆），是因为这些格式的坑得能用 `cargo test` 钉住。
pub fn eq_import_post(body: &[u8]) -> String {
  let v: Value = serde_json::from_slice(body).unwrap_or_else(|_| json!({}));
  let text = v.get("text").and_then(|x| x.as_str()).unwrap_or("");
  if text.len() > 512 * 1024 {
    return json!({ "ok": false, "error": "文件太大（上限 512 KB），这不是一份 EQ 配置" })
      .to_string();
  }
  match eq_import::parse(text) {
    Ok(imp) => json!({
      "ok": true,
      "format": imp.format,
      "bands": eq_bands_json(&imp.bands),
      "preamp": imp.preamp_db,
      "notes": imp.notes,
    })
    .to_string(),
    Err(e) => json!({ "ok": false, "error": e }).to_string(),
  }
}

/// VB-CABLE 的两个状态位：`(装了没, 配成 8 声道没)`。
///
/// 这两件事必须分开报：装了但没配成 7.1 是最常见的失败姿势 —— 用户以为装完就完事，
/// 结果端点默认是立体声，游戏照样只输出 2 声道。
pub fn vb_cable_state() -> (bool, bool) {
  let eps = endpoints_cached();
  let mut installed = false;
  let mut surround = false;
  for e in eps.iter().filter(|e| vbcable::looks_like_cable(&e.name)) {
    installed = true;
    if e.channels >= 8 {
      surround = true;
    }
  }
  (installed, surround)
}

/// 一键装 VB-CABLE（内嵌安装包 → 临时目录 → 提权运行）。
pub fn vbcable_install() -> Value {
  match vbcable::install() {
    Ok(msg) => json!({ "ok": true, "message": msg }),
    Err(e) => json!({ "ok": false, "error": e }),
  }
}

/// 打开系统声音设置（横幅右侧那个「设置引导」）。
pub fn setup_guide() -> Value {
  match vbcable::open_sound_settings() {
    Ok(()) => json!({ "ok": true }),
    Err(e) => json!({ "ok": false, "error": e }),
  }
}

pub fn refresh() -> String {
  if let Ok(mut g) = EPS.lock() {
    g.clear();
  }
  let n = endpoints_cached().len();
  let h = refresh_hrirs().len();
  json!({"ok": true, "count": n, "hrir_count": h}).to_string()
}

/// GUI 用：现在是否在跑。
pub fn is_running() -> bool {
  RT.lock().map(|g| g.is_some()).unwrap_or(false)
}

/// GUI 用：当前源/输出端点的显示名（没在跑就是空）。
pub fn running_labels() -> Option<(String, String)> {
  RT.lock().ok().and_then(|g| {
    g.as_ref()
      .map(|r| (r.source_label.clone(), r.sink_label.clone()))
  })
}

pub fn stop() -> String {
  let rt = RT.lock().ok().and_then(|mut g| g.take());
  match rt {
    Some(rt) => {
      rt.stop.store(true, Ordering::Relaxed);
      for h in rt.threads {
        let _ = h.join();
      }
      json!({"ok": true}).to_string()
    }
    None => json!({"ok": true, "note": "本来就没在跑"}).to_string(),
  }
}

const INDEX: &str = include_str!("web/index.html");
const APP_JS: &str = include_str!("web/app.js");
const STYLE: &str = include_str!("web/style.css");
