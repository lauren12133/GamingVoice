//! 应用图标：把 `assets/icon.ico` 直接内嵌，运行时创建 `HICON`。
//!
//! 为什么不用 `build.rs` + `rc.exe`：交叉编译到 `x86_64-pc-windows-msvc` 时没有
//! RC 工具链，而 `CreateIconFromResourceEx` 认的正是 ICO 里的 DIB 数据 ——
//! 存成字节、用的时候建一次，就够了。
//!
//! 图标本身和网页面板左上角那个 `.logo` 是同一套：深绿渐变圆角 + 淡青 `GV`。

#[cfg(windows)]
mod imp {
  use std::sync::OnceLock;

  use windows::Win32::UI::WindowsAndMessaging::{CreateIconFromResourceEx, HICON, IMAGE_FLAGS};

  const ICO: &[u8] = include_bytes!("../assets/icon.ico");

  fn u16le(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
  }

  fn u32le(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
  }

  /// 挑尺寸最接近 `want` 的那一帧，建成 `HICON`。
  ///
  /// 托盘要 16（小图标模式）/32（默认），任务栏按钮和 Alt-Tab 要大一些，
  /// 所以不能一次建死 —— 但同一尺寸只建一次（见 [`shared`]）。
  pub fn hicon_at(want: i32) -> Option<HICON> {
    let count = u16le(ICO, 4) as usize;
    let mut best: Option<(i32, usize, usize, i32, i32)> = None;
    for i in 0..count {
      let e = 6 + i * 16;
      // ICO 目录里 0 表示 256
      let w = if ICO[e] == 0 { 256 } else { i32::from(ICO[e]) };
      let h = if ICO[e + 1] == 0 {
        256
      } else {
        i32::from(ICO[e + 1])
      };
      let size = u32le(ICO, e + 8) as usize;
      let off = u32le(ICO, e + 12) as usize;
      if off + size > ICO.len() {
        continue;
      }
      let dist = (w - want).abs();
      if best.is_none_or(|(d, ..)| dist < d) {
        best = Some((dist, off, size, w, h));
      }
    }
    let (_, off, size, w, h) = best?;
    // LR_DEFAULTCOLOR == 0
    unsafe {
      CreateIconFromResourceEx(
        &ICO[off..off + size],
        true,
        0x0003_0000,
        w,
        h,
        IMAGE_FLAGS(0),
      )
      .ok()
    }
  }

  /// 32 px 版本，全局共用一份（窗口 + 托盘）。
  pub fn shared() -> Option<HICON> {
    static CACHE: OnceLock<usize> = OnceLock::new();
    let raw = *CACHE.get_or_init(|| hicon_at(32).map_or(0, |h| h.0 as usize));
    if raw == 0 {
      None
    } else {
      Some(HICON(raw as *mut core::ffi::c_void))
    }
  }
}

#[cfg(windows)]
pub use imp::shared;
