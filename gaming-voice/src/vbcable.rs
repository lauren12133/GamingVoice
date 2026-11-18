//! VB-CABLE 检测 + 一键安装。
//!
//! 为什么这是硬性前置：GamingVoice 的输入端必须是**一条 8 声道的虚拟端点**。
//! 没有 VB-CABLE，用户能选的只有立体声设备 —— loopback 抓到的是已经下混的 2 声道，
//! 8 个推子就退化成一个总音量。听感上只是"声音变小了"，看不出哪里错，
//! 所以必须在界面上把它变成一个**显式的、可安装的前置条件**。

/// 安装包本体（VB-CABLE Driver Pack 45，VB-Audio 的 Donationware，
/// 允许原样再分发；这里内嵌一份，用户不必再去官网找）。
#[cfg(windows)]
const SETUP: &[u8] = include_bytes!("../assets/VBCABLE_Setup_x64.exe");

/// 释放后的文件名。放临时目录，装完即弃 —— 不做静默安装，驱动必须用户点确认。
#[cfg(windows)]
const SETUP_NAME: &str = "VBCABLE_Setup_x64.exe";

/// 播放端点名字里出现这些片段就认定是 VB-CABLE。
///
/// VB-CABLE 的**播放**端叫 `CABLE Input`（名字反直觉：对播放端来说是"输入到线缆"），
/// 录音端叫 `CABLE Output`。我们只枚举播放端点，所以匹配 `CABLE Input`。
const HINTS: [&str; 2] = ["cable input", "vb-audio"];

/// 端点名是否像 VB-CABLE（大小写不敏感）。
pub fn looks_like_cable(name: &str) -> bool {
  if name.is_empty() {
    return false;
  }
  let n = name.to_lowercase();
  HINTS.iter().any(|h| n.contains(h))
}

/// 把安装包释放到临时目录并**以管理员身份**启动它。
///
/// 不做静默安装：驱动安装需要 UAC 确认，绕过它既不合法也不稳。
/// 用户点完 VB-CABLE 自己的 Install 按钮、重启之后，再来点一次「刷新设备」。
#[cfg(windows)]
pub fn install() -> Result<String, String> {
  use windows::core::{w, HSTRING};
  use windows::Win32::UI::Shell::ShellExecuteW;
  use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

  let dir = std::env::temp_dir().join("GamingVoice");
  std::fs::create_dir_all(&dir).map_err(|e| format!("建临时目录失败：{e}"))?;
  let exe = dir.join(SETUP_NAME);
  std::fs::write(&exe, SETUP).map_err(|e| format!("写出安装包失败：{e}"))?;

  let path = HSTRING::from(exe.to_string_lossy().to_string());
  // runas = 提权；装驱动的标准做法
  let ret = unsafe { ShellExecuteW(None, w!("runas"), &path, None, None, SW_SHOWNORMAL) };
  // ShellExecuteW 的返回值 >32 才算成功，≤32 是错误码
  if ret.0 as isize > 32 {
    Ok(format!("已启动安装程序：{}", exe.display()))
  } else {
    Err(format!(
      "启动安装程序失败（代码 {}）。可能是取消了 UAC 授权，或系统策略禁止提权。",
      ret.0 as isize
    ))
  }
}

#[cfg(not(windows))]
pub fn install() -> Result<String, String> {
  Err("一键安装只在 Windows 上可用".to_string())
}

/// 打开 Windows 的「声音设置」，让用户去把端点配成 7.1。
#[cfg(windows)]
pub fn open_sound_settings() -> Result<(), String> {
  use windows::core::{w, HSTRING};
  use windows::Win32::UI::Shell::ShellExecuteW;
  use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

  let target = HSTRING::from("ms-settings:sound");
  let ret = unsafe { ShellExecuteW(None, w!("open"), &target, None, None, SW_SHOWNORMAL) };
  if ret.0 as isize > 32 {
    Ok(())
  } else {
    // 老系统没有 ms-settings 协议，退回控制面板
    let cpl = HSTRING::from("mmsys.cpl");
    let r2 = unsafe { ShellExecuteW(None, w!("open"), &cpl, None, None, SW_SHOWNORMAL) };
    if r2.0 as isize > 32 {
      Ok(())
    } else {
      Err("打不开声音设置，请手动打开：控制面板 → 声音".to_string())
    }
  }
}

#[cfg(not(windows))]
pub fn open_sound_settings() -> Result<(), String> {
  Err("仅 Windows 可用".to_string())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn matches_real_cable_names() {
    assert!(looks_like_cable("CABLE Input (VB-Audio Virtual Cable)"));
    assert!(looks_like_cable("CABLE Input"));
    assert!(looks_like_cable("VB-Audio Virtual Cable"));
    // 真端点不会出现的名字，不能误判
    assert!(!looks_like_cable("耳机 (Realtek(R) Audio)"));
    assert!(!looks_like_cable("Speakers (USB Audio DAC)"));
    assert!(!looks_like_cable(""));
  }
}
