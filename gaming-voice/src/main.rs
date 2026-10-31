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
      "block": engine::BLOCK,
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
    .spawn(move || engine::run_processor(sh2, in_cons, out_prod, st2))
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
  json!({"ok": true}).to_string()
}

pub fn refresh() -> String {
  if let Ok(mut g) = EPS.lock() {
    g.clear();
  }
  let n = endpoints_cached().len();
  json!({"ok": true, "count": n}).to_string()
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
