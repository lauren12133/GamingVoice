//! Windows 音频：8 声道 loopback 采集 + 2 声道渲染。
//!
//! 全部用 Core Audio 直连（`IAudioClient`），因为 **cpal 不支持 WASAPI loopback**。
//!
//! 设备显示名走 `IMMDevice::OpenPropertyStore` + `PKEY_Device_FriendlyName`，
//! 也就是"声音设置"里看到的那个名字（例如 `CABLE Input (VB-Audio Virtual Cable)`）。
//! 取不到时（驱动没提供属性 / 权限异常）才回落到端点 ID 的尾 8 位 —— ``#2bd6f16``
//! 那串是 **MMDevice 端点 GUID 的末 8 位十六进制**，不是串口、也不是设备序号，
//! 只是"没名字时至少能区分是哪一条"。
//!
//! ## 声道映射
//!
//! WASAPI 7.1 的默认顺序正好等于 [`Speaker::ALL`]：
//! `FL, FR, FC, LFE, BL, BR, SL, SR`。所以设备是 8 声道时**下标直接对应**，
//! 是 6 声道（5.1）时前 6 个也对得上，缺的 SL/SR 补零。不需要重排表。

use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::Arc;

use windows::Win32::Foundation::PROPERTYKEY;
use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
  eConsole, eRender, IAudioCaptureClient, IAudioClient, IAudioRenderClient, IMMDevice,
  IMMDeviceCollection, IMMDeviceEnumerator, MMDeviceEnumerator, AUDCLNT_BUFFERFLAGS_SILENT,
  AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK,
  WAVEFORMATEX,
};
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{
  CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED, STGM_READ,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;

use crate::audio::{CHANNELS, SAMPLE_RATE};

/// 一个输出端点（能作为 loopback 源，也能作为渲染目标）。
#[derive(Clone, Debug)]
#[allow(dead_code)] // rate / bytes 只进 label 字符串，不参与判断
pub struct Endpoint {
  pub id: String,
  pub channels: u16,
  pub rate: u32,
  pub bytes: u16,
  pub default: bool,
  /// 设备友好名（"声音设置"里显示的那个）。取不到时是空串。
  pub name: String,
  /// 一行摘要：`CABLE Input · 8 声道 · 48 kHz`
  pub label: String,
}

impl Endpoint {
  /// 能不能当"7.1 源"用（≥8 声道）。
  pub fn is_surround(&self) -> bool {
    self.channels >= CHANNELS as u16
  }
}

fn short_id(id: &str) -> String {
  let tail: String = id.chars().rev().take(8).collect();
  tail.chars().rev().collect()
}

fn label_of(name: &str, ch: u16, rate: u32, id: &str) -> String {
  let who = if name.is_empty() {
    format!("端点 #{}", short_id(id))
  } else {
    name.to_string()
  };
  format!("{who} · {ch} 声道 · {} kHz", rate as f32 / 1000.0)
}

/// `PKEY_Device_FriendlyName`
/// = `{a45c254e-df1c-4efd-8020-67d146a850e0}, 14`
///
/// 自己定义而不用 crate 导出的常量：这个 PKEY 十多年没变过，自己写一份
/// 能少依赖一个 feature，也避免不同 `windows` 版本间常量路径漂移。
const PKEY_DEVICE_FRIENDLY_NAME: PROPERTYKEY = PROPERTYKEY {
  fmtid: windows::core::GUID::from_u128(0xa45c254e_df1c_4efd_8020_67d146a850e0),
  pid: 14,
};

/// 读端点的友好名。失败一律返回 `None`，调用方回落到端点 ID。
unsafe fn friendly_name(dev: &IMMDevice) -> Option<String> {
  unsafe {
    let store: IPropertyStore = dev.OpenPropertyStore(STGM_READ).ok()?;
    let pv: PROPVARIANT = store.GetValue(&PKEY_DEVICE_FRIENDLY_NAME).ok()?;
    // 只认字符串型；别的类型（极少见）直接放弃
    let ptr = pv.Anonymous.Anonymous.Anonymous.pwszVal;
    if ptr.is_null() {
      return None;
    }
    let s = ptr.to_string().unwrap_or_default();
    let s = s.trim().to_string();
    if s.is_empty() {
      None
    } else {
      Some(s)
    }
  }
}

unsafe fn read_format(wfx: *const WAVEFORMATEX) -> (u32, u16, u16) {
  let f = unsafe { &*wfx };
  (f.nSamplesPerSec, f.nChannels, f.wBitsPerSample)
}

/// 枚举全部活动输出端点。
pub fn enum_endpoints() -> Result<Vec<Endpoint>, String> {
  unsafe {
    let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
  }
  let enumerator: IMMDeviceEnumerator =
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
      .map_err(|e| format!("创建设备枚举器失败：{e}"))?;

  let default_id = unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole) }
    .and_then(|d| unsafe { d.GetId() })
    .map(|p| unsafe { p.to_string() }.unwrap_or_default())
    .unwrap_or_default();

  let coll: IMMDeviceCollection = unsafe {
    enumerator.EnumAudioEndpoints(eRender, windows::Win32::Media::Audio::DEVICE_STATE_ACTIVE)
  }
  .map_err(|e| format!("枚举输出端点失败：{e}"))?;

  let count = unsafe { coll.GetCount() }.unwrap_or(0);
  let mut out = Vec::new();
  for i in 0..count {
    let Ok(dev) = (unsafe { coll.Item(i) }) else {
      continue;
    };
    let id = unsafe { dev.GetId() }
      .map(|p| unsafe { p.to_string() }.unwrap_or_default())
      .unwrap_or_default();
    if id.is_empty() {
      continue;
    }
    // 拿不到格式的端点（被独占 / 已拔出）直接跳过，不让它污染列表
    let Ok(client) = (unsafe { dev.Activate::<IAudioClient>(CLSCTX_ALL, None) }) else {
      continue;
    };
    let Ok(mix) = (unsafe { client.GetMixFormat() }) else {
      continue;
    };
    let (rate, ch, bits) = unsafe { read_format(mix) };
    unsafe { CoTaskMemFree(Some(mix as *const _)) };
    let name = unsafe { friendly_name(&dev) }.unwrap_or_default();
    out.push(Endpoint {
      label: label_of(&name, ch, rate, &id),
      id: id.clone(),
      name,
      channels: ch,
      rate,
      bytes: bits,
      default: id == default_id,
    });
  }
  // 8 声道的排前面：那才是这套功能想要的源
  out.sort_by(|a, b| b.channels.cmp(&a.channels).then(a.label.cmp(&b.label)));
  Ok(out)
}

fn resolve_device(id: Option<&str>) -> Result<IMMDevice, String> {
  let enumerator: IMMDeviceEnumerator =
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
      .map_err(|e| format!("创建设备枚举器失败：{e}"))?;
  match id.filter(|s| !s.is_empty()) {
    Some(s) => unsafe { enumerator.GetDevice(&windows::core::HSTRING::from(s)) }
      .map_err(|e| format!("找不到端点 #{}：{e}", short_id(s))),
    None => unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole) }
      .map_err(|e| format!("取默认输出端点失败：{e}")),
  }
}

/// 读一包数据里的第 `f` 帧第 `c` 声道。
#[inline]
fn sample_at(base: *const u8, bits: u16, ch: usize, f: usize, c: usize) -> f32 {
  let idx = f * ch + c;
  unsafe {
    if bits == 32 {
      *(base as *const f32).add(idx)
    } else {
      *(base as *const i16).add(idx) as f32 / 32768.0
    }
  }
}

/// 启动 8 声道采集。
///
/// 返回 `(实际声道数, 采样率)`，供界面显示"源是几声道"——
/// 这个数字如果是 2，说明端点没配成 7.1，整套东西等于没开。
pub fn start_capture(
  id: Option<String>,
  mut prod: rtrb::Producer<f32>,
  stop: Arc<AtomicBool>,
  on_open: std::sync::mpsc::Sender<Result<(u16, u32), String>>,
) -> std::thread::JoinHandle<()> {
  std::thread::Builder::new()
    .name("pv-cap".into())
    .spawn(move || {
      unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
      }
      let opened = (|| -> Result<_, String> {
        let device = resolve_device(id.as_deref())?;
        let client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None) }
          .map_err(|e| format!("激活采集 IAudioClient 失败：{e}"))?;
        let mix =
          unsafe { client.GetMixFormat() }.map_err(|e| format!("GetMixFormat 失败：{e}"))?;
        let (rate, ch, bits) = unsafe { read_format(mix) };
        if bits != 32 && bits != 16 {
          unsafe { CoTaskMemFree(Some(mix as *const _)) };
          return Err(format!("采集格式位深不支持：{bits} bit（仅 16/32）"));
        }
        if rate != SAMPLE_RATE {
          unsafe { CoTaskMemFree(Some(mix as *const _)) };
          return Err(format!(
            "源端点采样率是 {rate} Hz，本工具内部固定 {SAMPLE_RATE} Hz。\n\
             请在 声音设置 → 该设备 → 高级 → 默认格式 改成 48000 Hz 后重试。"
          ));
        }
        let init = unsafe {
          client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_LOOPBACK,
            100_000, // 10 ms
            0,
            mix,
            None,
          )
        };
        unsafe { CoTaskMemFree(Some(mix as *const _)) };
        init.map_err(|e| {
          if e.code() == windows::core::HRESULT(0x8889_0003u32 as i32) {
            "这个端点是录音设备，只能选播放（输出）端点".to_string()
          } else {
            format!("初始化 loopback 失败：{e}（该设备可能被独占）")
          }
        })?;
        let capture: IAudioCaptureClient = unsafe { client.GetService() }
          .map_err(|e| format!("获取 IAudioCaptureClient 失败：{e}"))?;
        unsafe { client.Start() }.map_err(|e| format!("启动 loopback 失败：{e}"))?;
        Ok((client, capture, ch as usize, bits))
      })();

      let (client, capture, chn, bits) = match opened {
        Ok(v) => {
          let _ = on_open.send(Ok((v.2 as u16, SAMPLE_RATE)));
          v
        }
        Err(e) => {
          let _ = on_open.send(Err(e));
          return;
        }
      };

      while !stop.load(Relaxed) {
        let mut drained = false;
        loop {
          if stop.load(Relaxed) {
            break;
          }
          let frames = match unsafe { capture.GetNextPacketSize() } {
            Ok(f) => f,
            Err(_) => break,
          };
          if frames == 0 {
            break;
          }
          let mut pdata: *mut u8 = std::ptr::null_mut();
          let mut n = 0u32;
          let mut flags = 0u32;
          if unsafe { capture.GetBuffer(&mut pdata, &mut n, &mut flags, None, None) }.is_err() {
            break;
          }
          if n > 0 {
            let silent = flags & (AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0 || pdata.is_null();
            // 统一展开成 8 声道交错：设备声道数不够就补零，多了就丢掉（7.1 用不到）
            for f in 0..n as usize {
              for c in 0..CHANNELS {
                let v = if silent || c >= chn {
                  0.0
                } else {
                  sample_at(pdata, bits, chn, f, c)
                };
                // 环满了就丢最旧（由消费端决定），这里只尽力写
                let _ = prod.push(v);
              }
            }
            let _ = unsafe { capture.ReleaseBuffer(n) };
          }
          drained = true;
        }
        if !drained {
          std::thread::sleep(std::time::Duration::from_millis(1));
        }
      }
      unsafe {
        let _ = client.Stop();
      }
    })
    .expect("spawn capture")
}

/// 启动 2 声道渲染（事件驱动，避免 `sleep(1ms)` 在 Windows 上抖成 15 ms）。
pub fn start_render(
  id: Option<String>,
  mut cons: rtrb::Consumer<f32>,
  stop: Arc<AtomicBool>,
  on_open: std::sync::mpsc::Sender<Result<(u16, u32), String>>,
) -> std::thread::JoinHandle<()> {
  std::thread::Builder::new()
    .name("pv-render".into())
    .spawn(move || {
      unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
      }
      let opened = (|| -> Result<_, String> {
        let device = resolve_device(id.as_deref())?;
        let client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None) }
          .map_err(|e| format!("激活渲染 IAudioClient 失败：{e}"))?;
        let mix =
          unsafe { client.GetMixFormat() }.map_err(|e| format!("GetMixFormat 失败：{e}"))?;
        let (rate, ch, bits) = unsafe { read_format(mix) };
        if rate != SAMPLE_RATE {
          unsafe { CoTaskMemFree(Some(mix as *const _)) };
          return Err(format!(
            "输出端点采样率是 {rate} Hz，本工具内部固定 {SAMPLE_RATE} Hz。\n\
             请在 声音设置 → 该设备 → 高级 → 默认格式 改成 48000 Hz 后重试。"
          ));
        }
        let ev = unsafe { CreateEventW(None, false, false, None) }
          .map_err(|e| format!("创建音频事件失败：{e}"))?;
        let init = unsafe {
          client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
            100_000, // 10 ms
            0,
            mix,
            None,
          )
        };
        unsafe { CoTaskMemFree(Some(mix as *const _)) };
        init.map_err(|e| format!("初始化渲染失败：{e}（该设备可能被独占）"))?;
        unsafe { client.SetEventHandle(ev) }.map_err(|e| format!("绑定音频事件失败：{e}"))?;
        let render: IAudioRenderClient = unsafe { client.GetService() }
          .map_err(|e| format!("获取 IAudioRenderClient 失败：{e}"))?;
        let buf_frames =
          unsafe { client.GetBufferSize() }.map_err(|e| format!("取缓冲长度失败：{e}"))?;
        unsafe { client.Start() }.map_err(|e| format!("启动渲染失败：{e}"))?;
        Ok((client, render, ev, ch as usize, bits, buf_frames))
      })();

      let (client, render, ev, chn, bits, buf_frames) = match opened {
        Ok(v) => {
          let _ = on_open.send(Ok((v.3 as u16, SAMPLE_RATE)));
          v
        }
        Err(e) => {
          let _ = on_open.send(Err(e));
          return;
        }
      };

      while !stop.load(Relaxed) {
        // 200 ms 超时：就算事件丢了也能醒过来检查 stop
        let w = unsafe { WaitForSingleObject(ev, 200) };
        if w != WAIT_OBJECT_0 {
          continue;
        }
        let padding = match unsafe { client.GetCurrentPadding() } {
          Ok(p) => p,
          Err(_) => break,
        };
        let avail = buf_frames.saturating_sub(padding);
        if avail == 0 {
          continue;
        }
        let buf = match unsafe { render.GetBuffer(avail) } {
          Ok(b) => b,
          Err(_) => break,
        };
        if buf.is_null() {
          continue;
        }
        for f in 0..avail as usize {
          // 环空 → 补静音（宁可静音也不要卡住渲染线程）
          let l = cons.pop().unwrap_or(0.0);
          let r = cons.pop().unwrap_or(0.0);
          for c in 0..chn {
            let v = if c == 0 {
              l
            } else if c == 1 {
              r
            } else {
              (l + r) * 0.5 // 多声道输出设备：把双耳铺满其余声道
            };
            unsafe {
              if bits == 32 {
                *(buf as *mut f32).add(f * chn + c) = v;
              } else {
                *(buf as *mut i16).add(f * chn + c) = (v.clamp(-1.0, 1.0) * 32767.0) as i16;
              }
            }
          }
        }
        let _ = unsafe { render.ReleaseBuffer(avail, 0) };
      }
      unsafe {
        let _ = client.Stop();
        let _ = CloseHandle(ev);
      }
    })
    .expect("spawn render")
}
