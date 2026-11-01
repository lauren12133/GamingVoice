//! 方向增益预设：内置 5 组 + 用户自定义（可命名、可删、落盘）。
//!
//! 落盘位置在**用户数据目录**而不是程序目录：装到 `Program Files` 时程序目录不可写，
//! 而预设是用户资产，不能因为换个安装位置就丢。
//!
//! 只存方向增益（8 个 dB 值）。总补偿 `makeup_db` 是全局电平，跟着设备走，不属于预设。

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use crate::dsp::surround::layout::CHANNELS;
use crate::engine::Shared;

/// 低于这个值视为静音（和 `engine::Params` 的判定保持一致）。
pub const MUTE_DB: f32 = -60.0;

/// 一组方向增益。
#[derive(Clone, Debug)]
pub struct Preset {
  pub name: String,
  /// 内置的不能删、不能覆盖
  pub builtin: bool,
  pub gains: [f32; CHANNELS],
}

impl Preset {
  pub fn to_json(&self) -> Value {
    json!({ "name": self.name, "builtin": self.builtin, "gains_db": self.gains })
  }
}

/// 内置的 5 组，顺序就是下拉里的顺序。
pub fn builtins() -> Vec<Preset> {
  let mk = |name: &str, g: [f32; CHANNELS]| Preset {
    name: name.to_string(),
    builtin: true,
    gains: g,
  };
  vec![
    mk("平直", [0.0; CHANNELS]),
    // 压前方与中置、抬后方：脚步/换弹更突出，竞技最常用的一组
    mk("战术·脚步", [-3.0, -3.0, -7.0, 0.0, 12.0, 12.0, 4.0, 4.0]),
    // 中置与低频抬：对白与爆炸
    mk("影院", [1.0, 1.0, 4.0, 6.0, 2.0, 2.0, 0.0, 0.0]),
    // 只听前方：后四路压到 -18，仍留一点方位感（不是彻底砍掉）
    mk("只听前方", [2.0, 2.0, 0.0, 0.0, -18.0, -18.0, -18.0, -18.0]),
    mk(
      "后方静音",
      [0.0, 0.0, -3.0, 0.0, MUTE_DB, MUTE_DB, MUTE_DB, MUTE_DB],
    ),
  ]
}

// ---------------------------------------------------------------- 存储

fn data_dir() -> PathBuf {
  let base = std::env::var_os("APPDATA")
    .map(PathBuf::from)
    .or_else(|| std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from))
    .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
    .unwrap_or_else(std::env::temp_dir);
  base.join("GamingVoice")
}

fn store_path() -> PathBuf {
  data_dir().join("presets.json")
}

/// 读用户预设。文件不存在 / 坏了都当"没有"，不让一个坏 JSON 拦住启动。
fn load_user() -> Vec<Preset> {
  let Ok(text) = std::fs::read_to_string(store_path()) else {
    return Vec::new();
  };
  let Ok(v) = serde_json::from_str::<Value>(&text) else {
    return Vec::new();
  };
  parse_list(v.get("presets"))
}

fn parse_list(node: Option<&Value>) -> Vec<Preset> {
  let Some(arr) = node.and_then(|x| x.as_array()) else {
    return Vec::new();
  };
  let mut out = Vec::new();
  for item in arr {
    let Some(name) = item.get("name").and_then(|x| x.as_str()) else {
      continue;
    };
    let Some(g) = item.get("gains_db").and_then(|x| x.as_array()) else {
      continue;
    };
    if g.len() < CHANNELS {
      continue;
    }
    let mut gains = [0.0f32; CHANNELS];
    for (i, s) in g.iter().take(CHANNELS).enumerate() {
      gains[i] = s.as_f64().unwrap_or(0.0) as f32;
    }
    out.push(Preset {
      name: name.to_string(),
      builtin: false,
      gains,
    });
  }
  out
}

fn write_user(list: &[Preset]) -> Result<(), String> {
  let dir = data_dir();
  std::fs::create_dir_all(&dir).map_err(|e| format!("建目录失败 {}：{e}", dir.display()))?;
  let arr: Vec<Value> = list.iter().map(|p| p.to_json()).collect();
  let text = json!({ "version": 1, "presets": arr }).to_string();
  std::fs::write(store_path(), text).map_err(|e| format!("写预设失败：{e}"))
}

/// 内置 + 用户，内置排前面。
pub fn list() -> Vec<Preset> {
  let mut all = builtins();
  all.extend(load_user());
  all
}

/// 保存（同名覆盖）。内置名保留，不许用户覆盖。
pub fn save(name: &str, gains: [f32; CHANNELS]) -> Result<(), String> {
  let name = name.trim();
  if name.is_empty() {
    return Err("预设名不能为空".to_string());
  }
  if name.chars().count() > 16 {
    return Err("预设名最长 16 个字".to_string());
  }
  if builtins().iter().any(|p| p.name == name) {
    return Err(format!("「{name}」是内置预设，换一个名字"));
  }
  let mut user = load_user();
  if let Some(slot) = user.iter_mut().find(|p| p.name == name) {
    slot.gains = gains;
  } else {
    user.push(Preset {
      name: name.to_string(),
      builtin: false,
      gains,
    });
  }
  write_user(&user)
}

/// 删除用户预设。内置的删不掉。
pub fn delete(name: &str) -> Result<(), String> {
  let mut user = load_user();
  let before = user.len();
  user.retain(|p| p.name != name);
  if user.len() == before {
    return Err(format!("没有名为「{name}」的自定义预设"));
  }
  write_user(&user)
}

// ---------------------------------------------------------------- 应用

/// 当前生效的预设名（托盘菜单打勾用）。
static ACTIVE: Mutex<String> = Mutex::new(String::new());

pub fn active_name() -> String {
  ACTIVE.lock().map(|g| g.clone()).unwrap_or_default()
}

fn set_active(name: &str) {
  if let Ok(mut g) = ACTIVE.lock() {
    *g = name.to_string();
  }
}

/// 把预设写进实时参数。失败（名字不存在）返回人话。
pub fn apply(name: &str, shared: &Arc<Shared>) -> Result<(), String> {
  let Some(p) = list().into_iter().find(|p| p.name == name) else {
    return Err(format!("没有名为「{name}」的预设"));
  };
  {
    let mut params = shared.params.lock().map_err(|_| "参数锁失败".to_string())?;
    params.gains_db = p.gains;
  }
  set_active(&p.name);
  Ok(())
}

/// 把当前推子的值取出来（保存预设用）。
pub fn current_gains(shared: &Arc<Shared>) -> [f32; CHANNELS] {
  shared
    .params
    .lock()
    .map(|p| p.gains_db)
    .unwrap_or([0.0; CHANNELS])
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn builtins_have_expected_shapes() {
    let b = builtins();
    assert_eq!(b.len(), 5);
    assert!(b.iter().all(|p| p.builtin && !p.name.is_empty()));
    // 平直必须真的是全 0，否则"回到默认"这个说法就不成立
    assert!(b[0].gains.iter().all(|g| *g == 0.0));
  }

  #[test]
  fn rejects_builtin_name_collision() {
    let r = save("平直", [0.0; CHANNELS]);
    assert!(r.is_err());
    assert!(r.unwrap_err().contains("内置"));
  }

  #[test]
  fn rejects_empty_and_too_long_names() {
    assert!(save("   ", [0.0; CHANNELS]).is_err());
    assert!(save(&"字".repeat(17), [0.0; CHANNELS]).is_err());
  }
}
