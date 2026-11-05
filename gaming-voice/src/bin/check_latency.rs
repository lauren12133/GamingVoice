//! 独立检测工具：逐个检测输出设备的 WASAPI 共享模式引擎周期，
//! 并实测「128 帧（2.67ms）渲染流」能否初始化成功。
//!
//! 用途：回答「VB-Cable 到底支不支持 128 帧渲染周期」。
//! 运行：`cargo run --bin check_latency --release`
//!
//! 仅在 Windows 上能跑（依赖 WASAPI）。它会列出每个输出设备：
//!   - 默认混音格式（采样率 / 声道 / 位深）
//!   - 引擎周期范围（default / fundamental / min / max，帧 + 毫秒）
//!   - 用 IAudioClient3::InitializeSharedAudioStream 实测能否以 128 帧初始化
//! 并单独高亮 VB-Cable 的结论。

#[cfg(windows)]
mod win {
  use windows::core::Interface;
  use windows::Win32::Foundation::PROPERTYKEY;
  use windows::Win32::Media::Audio::{
    eRender, IAudioClient, IAudioClient3, IMMDevice, IMMDeviceCollection, IMMDeviceEnumerator,
    MMDeviceEnumerator, AUDCLNT_STREAMFLAGS_EVENTCALLBACK, DEVICE_STATE_ACTIVE, WAVEFORMATEX,
  };
  use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
  use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED, STGM_READ,
  };
  use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;

  /// 目标周期（帧）。128 帧 @48kHz = 2.67ms。
  const TARGET_FRAMES: u32 = 128;

  fn ms(frames: u32, rate: u32) -> f32 {
    frames as f32 * 1000.0 / rate as f32
  }

  const PKEY_DEVICE_FRIENDLY_NAME: PROPERTYKEY = PROPERTYKEY {
    fmtid: windows::core::GUID::from_u128(0xa45c254e_df1c_4efd_8020_67d146a850e0),
    pid: 14,
  };

  unsafe fn friendly_name(dev: &IMMDevice) -> Option<String> {
    unsafe {
      let store: IPropertyStore = dev.OpenPropertyStore(STGM_READ).ok()?;
      let pv: PROPVARIANT = store.GetValue(&PKEY_DEVICE_FRIENDLY_NAME).ok()?;
      let ptr = pv.Anonymous.Anonymous.Anonymous.pwszVal;
      if ptr.is_null() {
        return None;
      }
      let s = ptr.to_string().unwrap_or_default().trim().to_string();
      if s.is_empty() {
        None
      } else {
        Some(s)
      }
    }
  }

  fn looks_like_cable(name: &str) -> bool {
    let n = name.to_lowercase();
    n.contains("cable input") || n.contains("vb-audio")
  }

  unsafe fn read_format(wfx: *const WAVEFORMATEX) -> (u32, u16, u16) {
    let f = unsafe { &*wfx };
    (f.nSamplesPerSec, f.nChannels, f.wBitsPerSample)
  }

  /// 把目标 128 帧按 fundamental 对齐、夹到 min..=max，得到实际可请求的周期。
  fn align_target(fund: u32, mn: u32, mx: u32) -> u32 {
    let mut want = TARGET_FRAMES.clamp(mn, mx);
    want = (want / fund) * fund;
    if want < mn {
      want = mn;
    }
    if want > mx {
      want = mx;
    }
    want
  }

  pub fn run() {
    unsafe {
      let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }

    let enumerator: IMMDeviceEnumerator =
      match unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) } {
        Ok(e) => e,
        Err(e) => {
          println!("创建设备枚举器失败：{e}");
          return;
        }
      };

    let coll: IMMDeviceCollection = match unsafe {
      enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)
    } {
      Ok(c) => c,
      Err(e) => {
        println!("枚举输出端点失败：{e}");
        return;
      }
    };

    let count = unsafe { coll.GetCount() }.unwrap_or(0);
    println!("共找到 {count} 个输出设备");
    println!("检测目标：能否以 128 帧（2.67ms）初始化渲染流\n");
    println!("{}", "=".repeat(78));

    let mut cable_verdict: Vec<String> = Vec::new();

    for i in 0..count {
      let dev = match unsafe { coll.Item(i) } {
        Ok(d) => d,
        Err(_) => continue,
      };
      let name = unsafe { friendly_name(&dev) }.unwrap_or_else(|| "(无名端点)".to_string());
      let is_cable = looks_like_cable(&name);
      let tag = if is_cable { "  ★ VB-Cable" } else { "" };

      // —— 客户端 A：查询格式 + 引擎周期 ——
      let client = match unsafe { dev.Activate::<IAudioClient>(CLSCTX_ALL, None) } {
        Ok(c) => c,
        Err(e) => {
          println!("【{name}】{tag}\n  激活失败：{e}\n");
          continue;
        }
      };
      let mix = match unsafe { client.GetMixFormat() } {
        Ok(m) => m,
        Err(e) => {
          println!("【{name}】{tag}\n  GetMixFormat 失败：{e}\n");
          continue;
        }
      };
      let (rate, ch, bits) = unsafe { read_format(mix) };

      let mut has_c3 = false;
      let period_info = if let Ok(c3) = client.cast::<IAudioClient3>() {
        has_c3 = true;
        let mut def = 0u32;
        let mut fund = 0u32;
        let mut mn = 0u32;
        let mut mx = 0u32;
        if unsafe { c3.GetSharedModeEnginePeriod(mix, &mut def, &mut fund, &mut mn, &mut mx) }
          .is_ok()
        {
          format!(
            "default {}帧({:.1}ms) · fundamental {}帧 · min {}帧({:.1}ms) · max {}帧({:.1}ms)",
            def,
            ms(def, rate),
            fund,
            mn,
            ms(mn, rate),
            mx,
            ms(mx, rate)
          )
        } else {
          "GetSharedModeEnginePeriod 查询失败".to_string()
        }
      } else {
        "不支持 IAudioClient3（无法查询引擎周期）".to_string()
      };
      unsafe { CoTaskMemFree(Some(mix as *const _)) };

      // —— 客户端 B：实测 InitializeSharedAudioStream ——
      let mut supported_128 = false;
      let init_note = if has_c3 {
        match unsafe { dev.Activate::<IAudioClient>(CLSCTX_ALL, None) } {
          Ok(cb) => {
            if let Ok(mix_b) = unsafe { cb.GetMixFormat() } {
              let r = if let Ok(c3b) = cb.cast::<IAudioClient3>() {
                let mut def = 0u32;
                let mut fund = 0u32;
                let mut mn = 0u32;
                let mut mx = 0u32;
                let q = unsafe {
                  c3b.GetSharedModeEnginePeriod(mix_b, &mut def, &mut fund, &mut mn, &mut mx)
                };
                if q.is_ok() && fund > 0 && mn > 0 && mx >= mn {
                  let want = align_target(fund, mn, mx);
                  match unsafe {
                    c3b.InitializeSharedAudioStream(
                      AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                      want,
                      mix_b,
                      None,
                    )
                  } {
                    Ok(()) => {
                      supported_128 = want == TARGET_FRAMES;
                      format!("InitializeSharedAudioStream({want}帧) 成功")
                    }
                    Err(e) => {
                      format!("InitializeSharedAudioStream({want}帧) 失败：{e}")
                    }
                  }
                } else {
                  "查询周期失败，跳过初始化实测".to_string()
                }
              } else {
                "不支持 IAudioClient3".to_string()
              };
              unsafe { CoTaskMemFree(Some(mix_b as *const _)) };
              r
            } else {
              "GetMixFormat 失败".to_string()
            }
          }
          Err(e) => format!("二次激活失败：{e}"),
        }
      } else {
        "（设备不支持 IAudioClient3，跳过）".to_string()
      };

      let verdict = if supported_128 {
        "✅ 支持 128 帧（2.67ms）"
      } else {
        "❌ 不支持 128 帧，需回退 10ms"
      };

      println!("【{name}】{tag}");
      println!("  混音格式：{rate} Hz · {ch} 声道 · {bits} bit");
      println!("  引擎周期：{period_info}");
      println!("  128帧实测：{init_note}");
      println!("  结论：{verdict}");
      println!();

      if is_cable {
        cable_verdict.push(format!("  VB-Cable【{name}】→ {verdict}"));
      }
    }

    println!("{}", "=".repeat(78));
    if cable_verdict.is_empty() {
      println!("\n未识别到 VB-Cable（播放端点名应含 \"CABLE Input\" 或 \"VB-Audio\"）。");
      println!("请确认 VB-Cable 已安装、且其播放端（CABLE Input）处于启用状态。");
    } else {
      println!("\n★ VB-Cable 检测结论：");
      for line in cable_verdict {
        println!("{line}");
      }
    }
    println!("\n说明：");
    println!("  · 128 帧 = 2.67ms（48kHz 下）。只有 min 周期 ≤128、且 InitializeSharedAudioStream(128)");
    println!("    成功，才叫「支持 128 帧渲染」。");
    println!("  · 若 VB-Cable 的 min 周期是 480 帧（10ms），说明它只支持 10ms 起步，");
    println!("    强锁 128 无效，渲染端必须回退 10ms —— 这与主程序的自动回退逻辑一致。");
  }
}

fn main() {
  #[cfg(windows)]
  {
    win::run();
  }
  #[cfg(not(windows))]
  {
    eprintln!("本工具仅在 Windows 上运行（依赖 WASAPI）。");
    eprintln!("请在 Windows 下执行：cargo run --bin check_latency --release");
    std::process::exit(1);
  }
}
