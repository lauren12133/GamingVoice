//! 原生窗口 + 任务栏托盘（Windows）。
//!
//! 为什么要有这一层：网页面板好用，但"每次都要开浏览器"不是一个桌面软件该有的样子，
//! 而且竞技场景里最常见的操作是**一键静音/恢复**（裁判暂停、接电话），这时人要能
//! 从任务栏直接点到，而不是先开浏览器、再找标签页。
//!
//! 这一层只做三件事：
//!
//! 1. 一个原生窗口：选源/输出、启动/停止、打开完整面板；
//! 2. 一个托盘图标：左键单击显示窗口，右键菜单里能直接启动/停止/退出；
//! 3. 窗口关掉只是隐藏，音频继续跑 —— 否则"关窗口"等于"断音"，很容易误操作。
//!
//! 它和网页面板共用同一份 [`Shared`] 与同一套 `main::start/stop`，没有第二条音频路径。

use std::sync::{Arc, Mutex};

use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::{
  CreateFontW, DeleteObject, UpdateWindow, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, COLOR_WINDOW,
  DEFAULT_CHARSET, DEFAULT_PITCH, FF_DONTCARE, FW_NORMAL, HBRUSH, HGDIOBJ, OUT_DEFAULT_PRECIS,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
  Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::engine::Shared;
use crate::{endpoints_cached, is_running, open_browser, refresh, start, stop, PORT};

// ---------------------------------------------------------------- 控件 / 命令 id

const ID_HEAD: usize = 1000;
const ID_TIP_SRC: usize = 1001;
const ID_SRC: usize = 1002;
const ID_TIP_SNK: usize = 1003;
const ID_SNK: usize = 1004;
const ID_RUN: usize = 1005;
const ID_PANEL: usize = 1006;
const ID_REFRESH: usize = 1007;
const ID_STAT: usize = 1008;
const ID_HINT: usize = 1009;

const M_SHOW: usize = 2001;
const M_START: usize = 2002;
const M_STOP: usize = 2003;
const M_PANEL: usize = 2004;
const M_EXIT: usize = 2005;
/// 托盘里的 HRIR 子菜单：`M_HRIR_BASE + 序号`
const M_HRIR_BASE: usize = 3000;
/// 托盘里的方向预设子菜单：`M_PRESET_BASE + 序号`
const M_PRESET_BASE: usize = 4000;
const M_AUTOSTART: usize = 5000;
const M_VBCABLE: usize = 5001;
const M_GUIDE: usize = 5002;
/// 托盘里「导入 / 管理 EQ」—— 打开面板并直接切到 EQ 页
const M_EQ_PANEL: usize = 5003;
/// 托盘里的 EQ 预设子菜单：`M_EQ_BASE + 序号`
const M_EQ_BASE: usize = 6000;

/// 子菜单最多列这么多条（HeSuVi 的 hrir 目录能塞上百个，全列会撑爆屏幕）
const SUBMENU_MAX: usize = 40;

const WM_TRAY: u32 = WM_APP + 1;
const TIMER_ID: usize = 1;
const TRAY_UID: u32 = 1;

const CLASS_NAME: PCWSTR = w!("GamingVoiceMainWnd");

// ---------------------------------------------------------------- 全局状态

struct Ctx {
  shared: Arc<Shared>,
  font: isize,
  /// 下拉框里当前选中的端点 id（不直接读控件，避免重建时丢失）
  src: Option<String>,
  snk: Option<String>,
  /// 已经贴到托盘上了？
  tray: bool,
  /// 防止连点"启动"重入
  busy: bool,
  /// 托盘里选中的 HRIR（null = 自动取 hrir/ 第一个）
  hrir: Option<String>,
}

static CTX: Mutex<Option<Ctx>> = Mutex::new(None);
// ---------------------------------------------------------------- 入口

pub fn run(shared: Arc<Shared>) {
  unsafe {
    let _ = windows::Win32::System::Com::CoInitializeEx(
      None,
      windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
    );
  }

  let hinst = {
    let h = unsafe { GetModuleHandleW(None) }.unwrap_or_default();
    windows::Win32::Foundation::HINSTANCE(h.0)
  };

  let font = unsafe {
    CreateFontW(
      -14,
      0,
      0,
      0,
      FW_NORMAL.0 as i32,
      0,
      0,
      0,
      DEFAULT_CHARSET,
      OUT_DEFAULT_PRECIS,
      CLIP_DEFAULT_PRECIS,
      CLEARTYPE_QUALITY,
      DEFAULT_PITCH.0 as u32 | FF_DONTCARE.0 as u32,
      w!("Microsoft YaHei UI"),
    )
  };

  if let Ok(mut g) = CTX.lock() {
    *g = Some(Ctx {
      shared,
      font: font.0 as isize,
      src: None,
      snk: None,
      tray: false,
      busy: false,
      hrir: None,
    });
  }

  let wc = WNDCLASSW {
    style: CS_HREDRAW | CS_VREDRAW,
    lpfnWndProc: Some(wndproc),
    cbClsExtra: 0,
    cbWndExtra: 0,
    hInstance: hinst,
    hIcon: crate::icon::shared()
      .unwrap_or_else(|| unsafe { LoadIconW(None, IDI_APPLICATION).unwrap_or_default() }),
    hCursor: unsafe { LoadCursorW(None, IDC_ARROW) }.unwrap_or_default(),
    hbrBackground: HBRUSH((COLOR_WINDOW.0 as isize + 1) as *mut core::ffi::c_void),
    lpszMenuName: PCWSTR::null(),
    lpszClassName: CLASS_NAME,
  };
  unsafe { RegisterClassW(&wc) };

  let hwnd = unsafe {
    CreateWindowExW(
      WINDOW_EX_STYLE(0),
      CLASS_NAME,
      w!("GamingVoice · 虚拟 7.1 调音台"),
      WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX,
      CW_USEDEFAULT,
      CW_USEDEFAULT,
      520,
      330,
      None,
      None,
      Some(hinst),
      None,
    )
  }
  .unwrap_or_default();

  if hwnd.0.is_null() {
    eprintln!("创建窗口失败");
    return;
  }
  unsafe {
    let _ = ShowWindow(hwnd, SW_SHOW);
    let _ = UpdateWindow(hwnd);
  }

  // 消息循环
  let mut msg = MSG::default();
  while unsafe { GetMessageW(&mut msg, None, 0, 0) }.0 > 0 {
    unsafe {
      let _ = TranslateMessage(&msg);
      DispatchMessageW(&msg);
    }
  }
}

// ---------------------------------------------------------------- 建控件

// 控件创建本来就要传位置/尺寸/id/类名/样式，拆结构体只会更难读
#[allow(clippy::too_many_arguments)]
unsafe fn make(
  parent: HWND,
  class: PCWSTR,
  text: PCWSTR,
  style: WINDOW_STYLE,
  x: i32,
  y: i32,
  w: i32,
  h: i32,
  id: usize,
  font: isize,
) -> HWND {
  let ctl = unsafe {
    CreateWindowExW(
      WINDOW_EX_STYLE(0),
      class,
      text,
      style,
      x,
      y,
      w,
      h,
      Some(parent),
      Some(HMENU(id as *mut core::ffi::c_void)),
      None,
      None,
    )
  }
  .unwrap_or_default();
  if !ctl.0.is_null() && font != 0 {
    unsafe {
      SendMessageW(
        ctl,
        WM_SETFONT,
        Some(WPARAM(font as usize)),
        Some(LPARAM(1)),
      )
    };
  }
  ctl
}

unsafe fn build_ui(hwnd: HWND) {
  let font = CTX
    .lock()
    .ok()
    .and_then(|c| c.as_ref().map(|c| c.font))
    .unwrap_or(0);

  unsafe {
    make(
      hwnd,
      w!("STATIC"),
      w!("GamingVoice · 8 声道虚拟 7.1"),
      WS_CHILD | WS_VISIBLE,
      18,
      14,
      470,
      24,
      ID_HEAD,
      font,
    );
    make(
      hwnd,
      w!("STATIC"),
      w!("源（8 声道 loopback）"),
      WS_CHILD | WS_VISIBLE,
      18,
      46,
      300,
      18,
      ID_TIP_SRC,
      font,
    );
    make(
      hwnd,
      w!("COMBOBOX"),
      w!(""),
      WINDOW_STYLE(
        WS_CHILD.0 | WS_VISIBLE.0 | WS_VSCROLL.0 | CBS_DROPDOWNLIST as u32 | CBS_HASSTRINGS as u32,
      ),
      18,
      66,
      484,
      240,
      ID_SRC,
      font,
    );
    make(
      hwnd,
      w!("STATIC"),
      w!("输出（耳机）"),
      WS_CHILD | WS_VISIBLE,
      18,
      100,
      300,
      18,
      ID_TIP_SNK,
      font,
    );
    make(
      hwnd,
      w!("COMBOBOX"),
      w!(""),
      WINDOW_STYLE(
        WS_CHILD.0 | WS_VISIBLE.0 | WS_VSCROLL.0 | CBS_DROPDOWNLIST as u32 | CBS_HASSTRINGS as u32,
      ),
      18,
      120,
      484,
      240,
      ID_SNK,
      font,
    );

    make(
      hwnd,
      w!("BUTTON"),
      w!("启动"),
      WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | BS_PUSHBUTTON as u32),
      18,
      160,
      110,
      32,
      ID_RUN,
      font,
    );
    make(
      hwnd,
      w!("BUTTON"),
      w!("打开完整面板"),
      WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | BS_PUSHBUTTON as u32),
      136,
      160,
      130,
      32,
      ID_PANEL,
      font,
    );
    make(
      hwnd,
      w!("BUTTON"),
      w!("刷新设备"),
      WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | BS_PUSHBUTTON as u32),
      274,
      160,
      100,
      32,
      ID_REFRESH,
      font,
    );

    make(
      hwnd,
      w!("STATIC"),
      w!("未启动"),
      WS_CHILD | WS_VISIBLE,
      18,
      206,
      484,
      22,
      ID_STAT,
      font,
    );
    make(
      hwnd,
      w!("STATIC"),
      w!("关闭本窗口只是收起，音频继续。托盘图标右键可随时启停。"),
      WS_CHILD | WS_VISIBLE,
      18,
      232,
      484,
      20,
      ID_HINT,
      font,
    );
  }

  unsafe {
    fill_combos(hwnd);
    update_status(hwnd);
    SetTimer(Some(hwnd), TIMER_ID, 500, None);
    add_tray(hwnd);
  }

  // 首次打开就问一次：没装 VB-CABLE 时这程序基本没法用，与其让用户自己发现，
  // 不如在窗口刚出现时就给出口（装完还得配 7.1，所以不装也要能继续用界面）。
  let (vb_ok, _) = crate::vb_cable_state();
  if !vb_ok {
    let r = unsafe {
      MessageBoxW(
        Some(hwnd),
        w!("没有检测到 VB-CABLE 虚拟声卡。\n\n「单独调每个方向」需要游戏输出 8 条独立声道，没有它只能拿到一条混好的立体声。\n\n现在安装吗？（装完需要重启电脑，再回到「声音设置 → 该设备 → 配置扬声器」选 7.1）"),
        w!("缺少 VB-CABLE"),
        MB_YESNO | MB_ICONINFORMATION,
      )
    };
    if r == IDYES {
      do_install_vbcable(hwnd);
    }
  }
}

// ---------------------------------------------------------------- 下拉框

unsafe fn fill_combos(hwnd: HWND) {
  let eps = endpoints_cached();
  let (keep_src, keep_snk) = CTX
    .lock()
    .ok()
    .and_then(|c| c.as_ref().map(|c| (c.src.clone(), c.snk.clone())))
    .unwrap_or((None, None));

  for (id, want_surround, keep) in [(ID_SRC, true, keep_src), (ID_SNK, false, keep_snk)] {
    let ctl = unsafe { GetDlgItem(Some(hwnd), id as i32) }.unwrap_or_default();
    if ctl.0.is_null() {
      continue;
    }
    unsafe {
      SendMessageW(ctl, CB_RESETCONTENT, Some(WPARAM(0)), Some(LPARAM(0)));
    }
    for e in &eps {
      let star = if e.is_surround() { "★ " } else { "" };
      let line = HSTRING::from(format!(
        "{star}{}{}",
        e.label,
        if e.default { "   [默认]" } else { "" }
      ));
      unsafe {
        SendMessageW(
          ctl,
          CB_ADDSTRING,
          Some(WPARAM(0)),
          Some(LPARAM(line.as_ptr() as isize)),
        );
      }
    }

    // 选中项：优先用户上次选的，其次"源挑 8 声道 / 输出挑非 8 声道"
    let mut pick = keep
      .as_ref()
      .and_then(|k| eps.iter().position(|e| &e.id == k));
    if pick.is_none() {
      pick = if want_surround {
        eps.iter().position(|e| e.is_surround())
      } else {
        eps.iter().position(|e| !e.is_surround())
      };
    }
    let idx = pick.unwrap_or(0);
    if !eps.is_empty() {
      unsafe {
        SendMessageW(ctl, CB_SETCURSEL, Some(WPARAM(idx)), Some(LPARAM(0)));
      }
      let id_val = eps[idx].id.clone();
      if let Ok(mut g) = CTX.lock() {
        if let Some(c) = g.as_mut() {
          if id == ID_SRC {
            c.src = Some(id_val);
          } else {
            c.snk = Some(id_val);
          }
        }
      }
    }
  }
}

/// 读某个下拉框当前选中项的端点 id。
unsafe fn combo_id(hwnd: HWND, id: usize) -> Option<String> {
  let ctl = unsafe { GetDlgItem(Some(hwnd), id as i32) }.ok()?;
  let sel = unsafe { SendMessageW(ctl, CB_GETCURSEL, Some(WPARAM(0)), Some(LPARAM(0))) }.0;
  if sel < 0 {
    return None;
  }
  endpoints_cached().get(sel as usize).map(|e| e.id.clone())
}

// ---------------------------------------------------------------- 状态刷新

unsafe fn update_status(hwnd: HWND) {
  let running = is_running();
  let text = if running {
    let labels = crate::running_labels().unwrap_or_default();
    let src = labels.0.split(" · ").next().unwrap_or("—").to_string();
    let snk = labels.1.split(" · ").next().unwrap_or("—").to_string();
    format!("运行中　{src}  →  {snk}")
  } else {
    "未启动（点「启动」开始处理，或从托盘图标右键启动）".to_string()
  };
  if let Ok(ctl) = unsafe { GetDlgItem(Some(hwnd), ID_STAT as i32) } {
    let _ = unsafe { SetWindowTextW(ctl, &HSTRING::from(text)) };
  }
  if let Ok(btn) = unsafe { GetDlgItem(Some(hwnd), ID_RUN as i32) } {
    let _ =
      unsafe { SetWindowTextW(btn, &HSTRING::from(if running { "停止" } else { "启动" })) };
  }
  // 底部提示行兼职"前置条件检查"：没 VB-CABLE 时这里必须说清楚，
  // 否则用户只会觉得"启动按钮点了没反应"。
  let (vb_ok, vb_surround) = crate::vb_cable_state();
  let hint = if !vb_ok {
    "⚠ 没有 VB-CABLE：托盘右键 → 安装 VB-CABLE（必需）"
  } else if !vb_surround {
    "⚠ CABLE Input 还是 2 声道：声音设置 → 配置扬声器 → 7.1 环绕"
  } else {
    "关闭本窗口只是收起，音频继续。托盘图标右键可随时启停。"
  };
  if let Ok(ctl) = unsafe { GetDlgItem(Some(hwnd), ID_HINT as i32) } {
    let _ = unsafe { SetWindowTextW(ctl, &HSTRING::from(hint)) };
  }
}

// ---------------------------------------------------------------- 托盘

unsafe fn tray_data(hwnd: HWND) -> NOTIFYICONDATAW {
  let mut nid = NOTIFYICONDATAW {
    cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
    hWnd: hwnd,
    uID: TRAY_UID,
    ..Default::default()
  };
  nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
  nid.uCallbackMessage = WM_TRAY;
  nid.hIcon = crate::icon::shared()
    .unwrap_or_else(|| unsafe { LoadIconW(None, IDI_APPLICATION).unwrap_or_default() });
  let tip: Vec<u16> = "GamingVoice · 虚拟 7.1 调音台\0".encode_utf16().collect();
  let n = tip.len().min(nid.szTip.len());
  nid.szTip[..n].copy_from_slice(&tip[..n]);
  nid
}

unsafe fn add_tray(hwnd: HWND) {
  let nid = unsafe { tray_data(hwnd) };
  unsafe {
    let _ = Shell_NotifyIconW(NIM_ADD, &nid);
  }
  if let Ok(mut g) = CTX.lock() {
    if let Some(c) = g.as_mut() {
      c.tray = true;
    }
  }
}

unsafe fn del_tray(hwnd: HWND) {
  let nid = unsafe { tray_data(hwnd) };
  unsafe {
    let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
  }
}

unsafe fn show_menu(hwnd: HWND) {
  let Ok(menu) = (unsafe { CreatePopupMenu() }) else {
    return;
  };
  let running = is_running();
  let active_hrir = CTX
    .lock()
    .ok()
    .and_then(|g| g.as_ref().and_then(|c| c.hrir.clone()));

  unsafe {
    let _ = AppendMenuW(menu, MF_STRING, M_SHOW, w!("显示控制窗口"));
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
    let _ = AppendMenuW(
      menu,
      MF_STRING | if running { MF_GRAYED } else { MF_ENABLED },
      M_START,
      w!("启动音频"),
    );
    let _ = AppendMenuW(
      menu,
      MF_STRING | if running { MF_ENABLED } else { MF_GRAYED },
      M_STOP,
      w!("停止音频"),
    );
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());

    // ── 方位数据（HRIR）：HRIR 只在启动时加载，所以切换后要重启一次处理线程 ──
    if let Ok(sub) = CreatePopupMenu() {
      let files = crate::engine::list_hrir_files();
      if files.is_empty() {
        let _ = AppendMenuW(
          sub,
          MF_STRING | MF_GRAYED,
          0,
          w!("（hrir/ 里没有 .wav，当前是合成模型）"),
        );
      } else {
        for (i, p) in files.iter().take(SUBMENU_MAX).enumerate() {
          let name = p
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
          let flags = MF_STRING
            | if active_hrir.as_deref() == Some(name.as_str()) {
              MF_CHECKED
            } else {
              MF_ENABLED
            };
          let h = HSTRING::from(name);
          let _ = AppendMenuW(sub, flags, M_HRIR_BASE + i, &h);
        }
        if files.len() > SUBMENU_MAX {
          let _ = AppendMenuW(sub, MF_STRING | MF_GRAYED, 0, w!("…（更多请用网页面板）"));
        }
      }
      let _ = AppendMenuW(menu, MF_POPUP, sub.0 as usize, w!("方位数据（HRIR）"));
    }

    // ── 方向预设：实时生效，不用重启 ──
    if let Ok(sub) = CreatePopupMenu() {
      let active = crate::presets::active_name();
      for (i, p) in crate::presets::list().iter().take(SUBMENU_MAX).enumerate() {
        let flags = MF_STRING
          | if p.name == active {
            MF_CHECKED
          } else {
            MF_ENABLED
          };
        let h = HSTRING::from(p.name.clone());
        let _ = AppendMenuW(sub, flags, M_PRESET_BASE + i, &h);
      }
      let _ = AppendMenuW(menu, MF_POPUP, sub.0 as usize, w!("方向预设"));
    }

    // ── EQ 预设：同样实时生效（EQ 在卷积之后，换链不需要重启）──
    if let Ok(sub) = CreatePopupMenu() {
      let active = crate::eq_presets::active_name();
      for (i, p) in crate::eq_presets::list()
        .iter()
        .take(SUBMENU_MAX)
        .enumerate()
      {
        let flags = MF_STRING
          | if !active.is_empty() && p.name == active {
            MF_CHECKED
          } else {
            MF_ENABLED
          };
        let h = HSTRING::from(p.name.clone());
        let _ = AppendMenuW(sub, flags, M_EQ_BASE + i, &h);
      }
      let _ = AppendMenuW(sub, MF_SEPARATOR, 0, PCWSTR::null());
      let _ = AppendMenuW(sub, MF_STRING, M_EQ_PANEL, w!("导入 / 管理…"));
      let _ = AppendMenuW(menu, MF_POPUP, sub.0 as usize, w!("EQ 预设"));
    }

    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());

    // ── 前置条件：没装 VB-CABLE 时把安装入口放在最显眼的位置 ──
    let (installed, surround) = crate::vb_cable_state();
    if !installed {
      let _ = AppendMenuW(menu, MF_STRING, M_VBCABLE, w!("安装 VB-CABLE（必需）…"));
    } else if !surround {
      let _ = AppendMenuW(menu, MF_STRING, M_GUIDE, w!("把 CABLE 配成 7.1…"));
    }
    let auto_on = crate::autostart::enabled();
    let _ = AppendMenuW(
      menu,
      MF_STRING | if auto_on { MF_CHECKED } else { MF_ENABLED },
      M_AUTOSTART,
      w!("开机自动启动"),
    );

    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
    let _ = AppendMenuW(menu, MF_STRING, M_PANEL, w!("打开完整面板"));
    let _ = AppendMenuW(menu, MF_STRING, M_EXIT, w!("退出 GamingVoice"));

    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);
    let _ = SetForegroundWindow(hwnd);
    let _ = TrackPopupMenu(
      menu,
      TPM_LEFTALIGN | TPM_RIGHTBUTTON,
      pt.x,
      pt.y,
      Some(0),
      hwnd,
      None,
    );
    // 销毁父菜单会连带销毁子菜单
    let _ = DestroyMenu(menu);
    // 菜单消失后把焦点还回来，否则第一次点击会"点不中"
    let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
  }
}

// ---------------------------------------------------------------- 启动 / 停止

fn do_start(hwnd: HWND) {
  if is_running() {
    return;
  }
  {
    let Ok(mut g) = CTX.lock() else { return };
    let Some(c) = g.as_mut() else { return };
    if c.busy {
      return;
    }
    c.busy = true;
  }
  let hrir = CTX
    .lock()
    .ok()
    .and_then(|g| g.as_ref().and_then(|c| c.hrir.clone()));
  let body = unsafe {
    let src = combo_id(hwnd, ID_SRC);
    let snk = combo_id(hwnd, ID_SNK);
    // hrir 为 null 时后端自动取 hrir/ 里第一个
    serde_json::json!({ "source": src, "sink": snk, "hrir": hrir }).to_string()
  };
  let shared = CTX
    .lock()
    .ok()
    .and_then(|c| c.as_ref().map(|c| c.shared.clone()));
  if let Some(sh) = shared {
    let r = start(&sh, body.as_bytes());
    // 失败时把原因弹出来，否则用户只会看到"点了没反应"
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&r) {
      if v.get("ok").and_then(|x| x.as_bool()) != Some(true) {
        let msg = v
          .get("error")
          .and_then(|x| x.as_str())
          .unwrap_or("启动失败（未知原因）");
        unsafe {
          MessageBoxW(
            Some(hwnd),
            &HSTRING::from(msg),
            w!("启动失败"),
            MB_OK | MB_ICONWARNING,
          );
        }
      }
    }
  }
  if let Ok(mut g) = CTX.lock() {
    if let Some(c) = g.as_mut() {
      c.busy = false;
    }
  }
  unsafe { update_status(hwnd) };
}

fn do_stop(hwnd: HWND) {
  let _ = stop();
  unsafe { update_status(hwnd) };
}

fn do_refresh(hwnd: HWND) {
  let _ = refresh();
  unsafe {
    fill_combos(hwnd);
    update_status(hwnd);
  }
}

/// 换一套 HRIR。
///
/// HRIR 是在处理线程**启动时**加载的（卷积核要按 taps 预分配分区），
/// 所以运行中换必须重启一次 —— 直接改参数不会有任何效果。
fn do_switch_hrir(hwnd: HWND, name: &str) {
  if let Ok(mut g) = CTX.lock() {
    if let Some(c) = g.as_mut() {
      c.hrir = Some(name.to_string());
    }
  }
  if is_running() {
    let _ = stop();
    do_start(hwnd);
  }
}

/// 套用方向预设（写 params，实时生效）。
fn do_apply_preset(hwnd: HWND, name: &str) {
  let shared = CTX
    .lock()
    .ok()
    .and_then(|g| g.as_ref().map(|c| c.shared.clone()));
  if let Some(sh) = shared {
    if let Err(e) = crate::presets::apply(name, &sh) {
      unsafe {
        MessageBoxW(
          Some(hwnd),
          &HSTRING::from(e),
          w!("套用预设失败"),
          MB_OK | MB_ICONWARNING,
        );
      }
    }
  }
  unsafe { update_status(hwnd) };
}

/// 套用 EQ 预设（写 params，实时生效，不用重启处理线程）。
fn do_apply_eq_preset(hwnd: HWND, name: &str) {
  let shared = CTX
    .lock()
    .ok()
    .and_then(|g| g.as_ref().map(|c| c.shared.clone()));
  if let Some(sh) = shared {
    if let Err(e) = crate::eq_presets::apply(name, &sh) {
      unsafe {
        MessageBoxW(
          Some(hwnd),
          &HSTRING::from(e),
          w!("套用 EQ 预设失败"),
          MB_OK | MB_ICONWARNING,
        );
      }
    }
  }
}

fn do_toggle_autostart(hwnd: HWND) {
  let on = !crate::autostart::enabled();
  if let Err(e) = crate::autostart::set(on) {
    unsafe {
      MessageBoxW(
        Some(hwnd),
        &HSTRING::from(e),
        w!("开机启动"),
        MB_OK | MB_ICONWARNING,
      );
    }
  }
}

fn do_install_vbcable(hwnd: HWND) {
  let v = crate::vbcable_install();
  let ok = v.get("ok").and_then(|x| x.as_bool()).unwrap_or(false);
  let msg = v
    .get(if ok { "message" } else { "error" })
    .and_then(|x| x.as_str())
    .unwrap_or("未知错误")
    .to_string();
  unsafe {
    MessageBoxW(
      Some(hwnd),
      &HSTRING::from(msg),
      w!("VB-CABLE"),
      MB_OK
        | if ok {
          MB_ICONINFORMATION
        } else {
          MB_ICONWARNING
        },
    );
  }
}

fn open_panel() {
  open_browser(&format!("http://127.0.0.1:{PORT}/"));
}

/// 打开面板并直接落到 EQ 页（带 hash，前端 `initTab` 会认）。
fn open_eq_panel() {
  open_browser(&format!("http://127.0.0.1:{PORT}/#eq"));
}

// ---------------------------------------------------------------- 窗口过程

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
  match msg {
    WM_CREATE => {
      unsafe { build_ui(hwnd) };
      LRESULT(0)
    }
    WM_COMMAND => {
      let id = wp.0 & 0xFFFF;
      match id {
        ID_RUN => {
          if is_running() {
            do_stop(hwnd)
          } else {
            do_start(hwnd)
          }
        }
        ID_PANEL => open_panel(),
        ID_REFRESH => do_refresh(hwnd),
        ID_SRC | ID_SNK => {
          // 下拉选择变化：记下来，下次重建时才不会被"默认选择"覆盖
          let picked = unsafe { combo_id(hwnd, id) };
          if let Ok(mut g) = CTX.lock() {
            if let Some(c) = g.as_mut() {
              if id == ID_SRC {
                c.src = picked;
              } else {
                c.snk = picked;
              }
            }
          }
        }
        M_SHOW => unsafe {
          let _ = ShowWindow(hwnd, SW_SHOW);
          let _ = SetForegroundWindow(hwnd);
        },
        M_START => do_start(hwnd),
        M_STOP => do_stop(hwnd),
        M_PANEL => open_panel(),
        M_AUTOSTART => do_toggle_autostart(hwnd),
        M_VBCABLE => do_install_vbcable(hwnd),
        M_GUIDE => {
          let _ = crate::setup_guide();
        }
        // 子菜单项按序号还原成文件名 / 预设名
        id if (M_HRIR_BASE..M_HRIR_BASE + SUBMENU_MAX).contains(&id) => {
          let files = crate::engine::list_hrir_files();
          if let Some(p) = files.get(id - M_HRIR_BASE) {
            let name = p
              .file_name()
              .map(|s| s.to_string_lossy().into_owned())
              .unwrap_or_default();
            do_switch_hrir(hwnd, &name);
          }
        }
        id if (M_PRESET_BASE..M_PRESET_BASE + SUBMENU_MAX).contains(&id) => {
          let list = crate::presets::list();
          if let Some(p) = list.get(id - M_PRESET_BASE) {
            let name = p.name.clone();
            do_apply_preset(hwnd, &name);
          }
        }
        M_EQ_PANEL => open_eq_panel(),
        id if (M_EQ_BASE..M_EQ_BASE + SUBMENU_MAX).contains(&id) => {
          let list = crate::eq_presets::list();
          if let Some(p) = list.get(id - M_EQ_BASE) {
            let name = p.name.clone();
            do_apply_eq_preset(hwnd, &name);
          }
        }
        M_EXIT => unsafe {
          let _ = DestroyWindow(hwnd);
        },
        _ => {}
      }
      LRESULT(0)
    }
    WM_TIMER => {
      if wp.0 == TIMER_ID {
        unsafe { update_status(hwnd) };
      }
      LRESULT(0)
    }
    WM_TRAY => {
      let ev = lp.0 as u32;
      match ev {
        WM_LBUTTONDBLCLK => unsafe {
          let _ = ShowWindow(hwnd, SW_SHOW);
          let _ = SetForegroundWindow(hwnd);
        },
        WM_RBUTTONUP | WM_CONTEXTMENU => unsafe { show_menu(hwnd) },
        _ => {}
      }
      LRESULT(0)
    }
    WM_CLOSE => {
      // 收起而不是退出：关窗口断音是最容易被误触的事故
      unsafe {
        let _ = ShowWindow(hwnd, SW_HIDE);
      }
      LRESULT(0)
    }
    WM_DESTROY => {
      unsafe {
        del_tray(hwnd);
        let _ = KillTimer(Some(hwnd), TIMER_ID);
        let font = CTX.lock().ok().and_then(|c| c.as_ref().map(|c| c.font));
        if let Some(f) = font {
          let _ = DeleteObject(HGDIOBJ(f as *mut core::ffi::c_void));
        }
        let _ = stop();
        PostQuitMessage(0);
      }
      LRESULT(0)
    }
    _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
  }
}
