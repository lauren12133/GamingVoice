//! GamingVoice —— 8 声道虚拟 7.1 环绕声调音台
//!
//! 链路：`游戏(7.1) → 虚拟端点 → WASAPI loopback 抓 8 声道
//!        → 每方向增益/延时 → ATK V3 空间引擎 → HRIR 卷积 → 耳机 EQ → 双耳 2 声道 → 耳机`
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
  /// 引擎 hop / 采集周期（10 ms = 480 帧）。DSP 层与延迟模型用它作基准块长；
  /// 处理块长另见 [`engine::BLOCK`]，已固定为 128。
  pub const HOP: usize = 480;
  /// 采集声道数：7.1 环绕 = 8 个离散声道。
  /// 注意：这跟 `dsp::surround::layout::CHANNELS`（=8，虚拟扬声器数）一致——
  /// 8 个 7.1 声道进环，软件内部先球谐编码成 16 分量，再解码回 8 个虚拟扬声器走 HRIR。
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
/// 12 dB 是「能明显抬升」与「不削波」之间的折中：8 路同时 +12 dB 仍会爆掉，
/// 但竞技场景只抬 1~2 个方向（脚步在后方），其余保持 0 或负值，加默认 -6 dB
/// 总补偿后峰值可控。上限压到 12 后，配上限幅器，正常抬升几乎不会削波。
pub const MAX_GAIN_DB: f32 = 12.0;

/// 正在跑的音频会话。
struct Runtime {
  stop: Arc<AtomicBool>,
  threads: Vec<std::thread::JoinHandle<()>>,
  /// 实际打开的端点 id（None = 走的系统默认设备）
  source_id: Option<String>,
  sink_id: Option<String>,
  /// 给人看的名字。存 id 的话状态栏会是一串 GUID，用户根本没法确认选对没选对。
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

/// 端点 id → 给人看的名字。
///
/// 缓存里找不到（刚插拔、或缓存还没建）就退回 id 本身 —— 显示 GUID 难看，
/// 但比显示"未知设备"更有助于排查。
fn endpoint_name(id: Option<&str>) -> String {
  let Some(id) = id.filter(|s| !s.is_empty()) else {
    return "系统默认输出".into();
  };
  endpoints_cached()
    .iter()
    .find(|e| e.id == id)
    .map(|e| {
      if e.name.is_empty() {
        e.label.clone()
      } else {
        e.name.clone()
      }
    })
    .unwrap_or_else(|| id.to_string())
}

/// 按下拉索引从**填充时的快照**里取端点 id。
///
/// 抽成独立函数是为了让这条不变量能被测试钉住：索引只对"填进去的那份列表"有效。
/// 拿实时枚举缓存反查的话，列表一旦重排（改了 7.1、插拔设备、点「刷新设备」），
/// 同一个下标就指向另一个设备 —— 症状正是"我选了输出 2，启动却开了 1"。
///
/// `sel` 为 None = 控件报"没有选中项"；`last` 是上次记住的选择。
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn pick_by_index(
  table: &[String],
  last: Option<&String>,
  sel: Option<usize>,
) -> Option<String> {
  match sel {
    Some(i) => table.get(i).cloned().or_else(|| last.cloned()),
    None => last.cloned(),
  }
}

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
  // Windows：把定时器精度提到 1 ms。采集/处理线程里的 `sleep(1 ms)` 默认会被
  // 量化到 15.6 ms（系统默认定时器 tick），提高精度后才是真正的 ~1 ms，消除采集端轮询抖动。
  #[cfg(windows)]
  unsafe {
    let _ = windows::Win32::Media::timeBeginPeriod(1);
  }

  let shared = Shared::new();

  // HRIR 双耳化：优先用户放进的 HeSuVi 模型（hrir/*.wav），否则内置 HeSuVi EAC_Default。
  println!("[提示] 方位数据使用 HeSuVi HRIR；可把自定义模型放进 hrir/ 目录后在面板切换。");

  // 数据目录：在 exe 同级，启动就建好，
  // 之后保存的预设（presets.json / eq_presets.json）都落在这里。
  for dir in engine::data_dirs() {
    if !dir.exists() && std::fs::create_dir_all(&dir).is_ok() {
      println!("已创建数据目录：{}", dir.display());
    }
  }
  // 扫一次 hrir/ 目录，让面板一打开就能看到可切换的模型列表。
  let _ = refresh_hrirs();

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

/// 内置 HRIR 在界面上显示的名字。
const BUILTIN_HRIR_LABEL: &str = "HeSuVi EAC_Default（内置）";

/// 按文件名加载 HRIR；名字为空 = 用内置 HeSuVi 默认。
///
/// 返回 `(传给处理线程的 HRIR, 显示名, 警告)`。`None` 表示让 `run_processor`
/// 自己回退到内置。外部文件会做峰值归一，避免切换 HRIR 时音量跳变。
fn load_hrir(name: Option<&str>) -> (Option<dsp::surround::Hrir>, String, Option<String>) {
  let name = name.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
  let Some(name) = name else {
    return (None, BUILTIN_HRIR_LABEL.to_string(), None);
  };
  let path = engine::list_hrir_files().into_iter().find(|p| {
    p.file_name()
      .map(|s| s.to_string_lossy().into_owned())
      .as_deref()
      == Some(name.as_str())
  });
  let Some(path) = path else {
    return (
      None,
      BUILTIN_HRIR_LABEL.to_string(),
      Some(format!("找不到 HRIR 文件「{name}」，已回退内置")),
    );
  };
  let bytes = match std::fs::read(&path) {
    Ok(b) => b,
    Err(e) => {
      return (
        None,
        BUILTIN_HRIR_LABEL.to_string(),
        Some(format!("读取 HRIR「{name}」失败：{e}，已回退内置")),
      );
    }
  };
  match dsp::surround::wav::parse(&bytes)
    .and_then(|w| dsp::surround::Hrir::from_wav(&w, name.as_str()))
  {
    Ok(mut h) => {
      h.normalize_peak(1.0);
      (Some(h), name, None)
    }
    Err(e) => (
      None,
      BUILTIN_HRIR_LABEL.to_string(),
      Some(format!("HRIR「{name}」无效：{e}，已回退内置")),
    ),
  }
}

pub fn state_json(shared: &Arc<Shared>) -> Value {
  let p = shared.params.lock().map(|p| p.clone()).unwrap_or_default();
  let lv = shared.levels.lock().map(|l| *l).unwrap_or_default();
  // source/sink 给 id（网页端拿它当 <option> 的 value 用），
  // source_label/sink_label 给名字（给人看）。
  let (running, src, snk, src_name, snk_name, err) = RT
    .lock()
    .ok()
    .and_then(|g| {
      g.as_ref().map(|r| {
        (
          true,
          r.source_id.clone(),
          r.sink_id.clone(),
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
    "source_label": src_name,
    "sink_label": snk_name,
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
      "self_gun": p.self_gun,
      "self_foot": p.self_foot,
      "leveller": p.leveller,
      "duck_db": p.duck_db,
      "range": p.range,
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
      "builtin": BUILTIN_HRIR_LABEL,
      "files": hrir_names(),
      "active": st.hrir_name,
      "measured": st.hrir_measured,
      "dir": HRIR_DIR_ACTIVE.lock().map(|d| d.clone()).unwrap_or_default(),
    },
    "levels": {
      "raw_peak": lv.raw_peak,
      "in_peak": lv.in_peak,
      "out_peak": lv.out_peak,
      "ear_peak": lv.ear_peak,
      "gun_snapshot": lv.gun_snapshot,
      "duck_db": lv.duck_db,
    },
    "status": {
      "block_ms": block_ms(engine::BLOCK),
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

/// 处理块延迟（毫秒）。块长固定为 [`engine::BLOCK`]。
fn block_ms(block: usize) -> f32 {
  block as f32 * 1000.0 / audio::SAMPLE_RATE as f32
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
  // 自身枪声压制（0..1）：越大前方中频枪声压得越狠。
  if let Some(f) = v.get("self_gun").and_then(|x| x.as_f64()) {
    p.self_gun = (f as f32).clamp(0.0, 1.0);
  }
  // 自身脚步 ducking（0..1）：越大前方低频脚步压得越狠、越久。
  if let Some(f) = v.get("self_foot").and_then(|x| x.as_f64()) {
    p.self_foot = (f as f32).clamp(0.0, 1.0);
  }
  // 整体响度均衡（0..1）：慢时间常数，缩小"自己巨响 vs 远处轻声"差距。
  if let Some(f) = v.get("leveller").and_then(|x| x.as_f64()) {
    p.leveller = (f as f32).clamp(0.0, 1.0);
  }
  // Center Duck 深度（dB）：压自己枪声的主旋钮，独立于「自身枪声」。
  if let Some(d) = v.get("duck_db").and_then(|x| x.as_f64()) {
    p.duck_db = (d as f32).clamp(-60.0, 0.0);
  }
  // 听远（0..1）：越大扩展器越温和、脚步 boost 越强（听得越远）。
  if let Some(f) = v.get("range").and_then(|x| x.as_f64()) {
    p.range = (f as f32).clamp(0.0, 1.0);
  }
  json!({
    "ok": true,
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
  // HRIR 双耳化：优先用户选的外部 HeSuVi 模型（hrir/*.wav），否则内置 EAC_Default。
  let hrir_sel = v
    .get("hrir")
    .and_then(|x| x.as_str())
    .map(|s| s.to_string());
  let (loaded_hrir, hrir_label, hrir_warn) = load_hrir(hrir_sel.as_deref());
  let hrir_measured = true;
  // 立刻上屏，让面板在设备打开前就知道「用的是哪套 HRTF」。
  if let Ok(mut st) = shared.status.lock() {
    st.hrir_name = hrir_label.clone();
    st.hrir_measured = hrir_measured;
  }

  let stop_flag = Arc::new(AtomicBool::new(false));
  // 输入环 2 s（8 声道 7.1 交错），输出环 0.5 s（2 声道交错）
  let (in_prod, in_cons) =
    rtrb::RingBuffer::<f32>::new(audio::SAMPLE_RATE as usize * 2 * audio::CHANNELS);
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
    .spawn(move || engine::run_processor(sh2, in_cons, out_prod, st2, loaded_hrir))
    .expect("spawn processor");

  let src_label = endpoint_name(source.as_deref());
  let snk_label = endpoint_name(sink.as_deref());
  if let Ok(mut g) = RT.lock() {
    *g = Some(Runtime {
      stop: stop_flag,
      threads: vec![cap, proc, ren],
      source_id: source.clone(),
      sink_id: sink.clone(),
      source_label: src_label.clone(),
      sink_label: snk_label.clone(),
    });
  }
  // 把「实际开的哪个设备」原样回给调用方：GUI 和网页都能立刻确认，
  // 而不是各自猜自己刚才选的是什么。
  json!({
    "ok": true,
    "source": source,
    "sink": sink,
    "source_label": src_label,
    "sink_label": snk_label,
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
/// 这两件事必须分开报：装了但没配成 7.1 是最常见的失败姿势 ——
/// 用户以为装完就完事，结果端点默认是立体声，游戏照样只输出 2 声道。
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

/// 启动外置 VB-CABLE 安装包（与 exe 同目录，提权运行）。
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

#[cfg(test)]
mod tests {
  use super::{endpoint_name, pick_by_index};

  fn v(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
  }

  #[test]
  fn index_is_resolved_against_fill_time_snapshot() {
    // 界面填进去的顺序
    let snapshot = v(&["A", "B", "C"]);
    // 用户点的是第 3 项
    assert_eq!(
      pick_by_index(&snapshot, None, Some(2)).as_deref(),
      Some("C")
    );

    // 之后缓存被重新枚举、顺序变了（改了 7.1 配置、插拔设备都会这样）。
    // 旧实现拿"实时缓存 + 同一个下标"反查，第 3 项就变成了 B —— 这正是那个 bug。
    let live = v(&["C", "A", "B"]);
    assert_eq!(
      pick_by_index(&live, None, Some(2)).as_deref(),
      Some("B"),
      "这条只是说明错位长什么样；真正取值的路径必须是快照"
    );
    assert_eq!(
      pick_by_index(&snapshot, None, Some(2)).as_deref(),
      Some("C"),
      "快照没变，用户选谁就是谁"
    );
  }

  #[test]
  fn endpoint_name_degrades_instead_of_going_blank() {
    // 没指定 = 走的系统默认端点
    assert_eq!(endpoint_name(None), "系统默认输出");
    assert_eq!(endpoint_name(Some("")), "系统默认输出");
    // 缓存里查不到就退回 id：难看，但比空标题有用
    assert_eq!(endpoint_name(Some("mock-unknown")), "mock-unknown");
  }

  #[test]
  fn missing_selection_falls_back_to_last_pick() {
    let last = "A".to_string();
    // 下拉为空 / 控件报 CB_ERR：不能静默变成「系统默认设备」
    assert_eq!(pick_by_index(&[], Some(&last), None).as_deref(), Some("A"));
    // 下标越界（列表被重建过）也一样退回上次的选择
    assert_eq!(
      pick_by_index(&[], Some(&last), Some(3)).as_deref(),
      Some("A")
    );
    // 什么都不知道就老实返回 None，让调用方去提示，而不是猜一个设备
    assert_eq!(pick_by_index(&[], None, None), None);
  }
}
