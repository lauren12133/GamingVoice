//! 开机启动：写 `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`。
//!
//! 选 HKCU 而不是 HKLM：不需要管理员权限，也不会给别的用户装上；卸载时用户自己删掉即可。
//! 值里存的是**带引号的绝对路径** —— 路径含空格时不加引号会启动失败，这是最常见的坑。

#[cfg(windows)]
mod imp {
  use windows::core::PCWSTR;
  use windows::Win32::Foundation::ERROR_SUCCESS;
  use windows::Win32::System::Registry::{
    RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER,
    KEY_QUERY_VALUE, KEY_SET_VALUE, REG_SZ,
  };

  /// 整段路径写成字面量，避免 `w!` 宏在 const 上下文里的类型麻烦。
  const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
  const VALUE_NAME: &str = "GamingVoice";

  fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
  }

  fn open_key(write: bool) -> Option<HKEY> {
    let sub = wide(RUN_KEY);
    let mut key = HKEY::default();
    let access = if write {
      KEY_QUERY_VALUE | KEY_SET_VALUE
    } else {
      KEY_QUERY_VALUE
    };
    let r = unsafe {
      RegOpenKeyExW(
        HKEY_CURRENT_USER,
        PCWSTR(sub.as_ptr()),
        Some(0),
        access,
        &mut key,
      )
    };
    if r == ERROR_SUCCESS {
      Some(key)
    } else {
      None
    }
  }

  /// 读一下值在不在。值存在就算启用（不比对路径：程序搬家后旧路径也算"已启用"，
  /// 用户重新勾一次就会覆盖成新路径）。
  pub fn enabled() -> bool {
    let Some(key) = open_key(false) else {
      return false;
    };
    let name = wide(VALUE_NAME);
    let mut size = 0u32;
    let r = unsafe {
      windows::Win32::System::Registry::RegGetValueW(
        key,
        PCWSTR::null(),
        PCWSTR(name.as_ptr()),
        windows::Win32::System::Registry::RRF_RT_REG_SZ,
        None,
        None,
        Some(&mut size),
      )
    };
    unsafe {
      let _ = RegCloseKey(key);
    }
    r == ERROR_SUCCESS
  }

  pub fn set(on: bool) -> Result<(), String> {
    let key = open_key(true).ok_or_else(|| "打不开注册表 Run 键".to_string())?;
    let name = wide(VALUE_NAME);
    let r = if on {
      let exe = std::env::current_exe().map_err(|e| format!("取程序路径失败：{e}"))?;
      // 引号不能省：`C:\Program Files\...\GamingVoice.exe` 不加引号会被当成
      // `C:\Program.exe` 去启动，开机时静默失败。
      let raw = format!("\"{}\"", exe.display());
      let val = wide(&raw);
      let bytes = unsafe { std::slice::from_raw_parts(val.as_ptr() as *const u8, val.len() * 2) };
      unsafe { RegSetValueExW(key, PCWSTR(name.as_ptr()), Some(0), REG_SZ, Some(bytes)) }
    } else {
      unsafe { RegDeleteValueW(key, PCWSTR(name.as_ptr())) }
    };
    unsafe {
      let _ = RegCloseKey(key);
    }
    // 关的时候值本来就不存在也算成功（ERROR_FILE_NOT_FOUND = 2）
    if r == ERROR_SUCCESS || r.0 == 2 {
      Ok(())
    } else {
      Err(format!("写注册表失败（代码 {}）", r.0))
    }
  }
}

#[cfg(not(windows))]
mod imp {
  pub fn enabled() -> bool {
    false
  }
  pub fn set(_on: bool) -> Result<(), String> {
    Err("开机启动只在 Windows 上可用".to_string())
  }
}

pub use imp::{enabled, set};
