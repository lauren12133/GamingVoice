//! EQ 预设：内置「平直」+ 用户自定义（手存 / 导入），落盘 `%APPDATA%\GamingVoice\eq_presets.json`。
//!
//! 和 `presets.rs`（方向增益）分开存：两类预设改的是完全不同的东西（8 个 dB 值 vs 一整条
//! 滤波器链 + 前级），塞一个文件里只会让"保存这一半顺手清掉那一半"这种 bug 有机可乘。
//!
//! 一个预设 = 前级 + 整条滤波器链。**两个都要存**：只存频点的话，那份带 `Preamp: -6.3 dB`
//! 的导入配置下次套用就会削波，用户听到的是"这个预设坏了"。

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use crate::dsp::surround::eq::{self, Band, FilterKind};
use crate::engine::Shared;

/// 名字最长多少个字。导入的档名经常带版本后缀（`Feaulle Lian Filters (apex)`），
/// 16 字那种限制会直接把它截断成两半。
pub const MAX_NAME: usize = 32;

/// 一组 EQ 预设。
#[derive(Clone, Debug)]
pub struct EqPreset {
  pub name: String,
  /// 内置的删不掉、也覆盖不了
  pub builtin: bool,
  pub preamp_db: f32,
  pub bands: Vec<Band>,
}

impl EqPreset {
  pub fn to_json(&self) -> Value {
    json!({
      "name": self.name,
      "builtin": self.builtin,
      "preamp_db": self.preamp_db,
      "bands": bands_json(&self.bands),
    })
  }
}

/// 频点 → JSON。字段名和前端 / `set_params` 的解析保持一致（`type/freq/q/gain`）。
pub fn bands_json(bands: &[Band]) -> Vec<Value> {
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

/// 内置预设。只有「平直」—— 编造"某个耳机的校正曲线"没有意义，
/// 那些数值应该由用户自己导入或调出来。
pub fn builtins() -> Vec<EqPreset> {
  vec![EqPreset {
    name: "平直".to_string(),
    builtin: true,
    preamp_db: 0.0,
    bands: eq::default_bands(),
  }]
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
  data_dir().join("eq_presets.json")
}

/// 读用户预设。文件不存在 / 坏了都当"没有" —— 一个坏 JSON 不该拦住启动。
fn load_user() -> Vec<EqPreset> {
  let Ok(text) = std::fs::read_to_string(store_path()) else {
    return Vec::new();
  };
  let Ok(v) = serde_json::from_str::<Value>(&text) else {
    return Vec::new();
  };
  parse_list(v.get("presets"))
}

fn parse_list(node: Option<&Value>) -> Vec<EqPreset> {
  let Some(arr) = node.and_then(|x| x.as_array()) else {
    return Vec::new();
  };
  let mut out = Vec::new();
  for item in arr {
    let Some(name) = item.get("name").and_then(|x| x.as_str()) else {
      continue;
    };
    let Some(list) = item.get("bands").and_then(|x| x.as_array()) else {
      continue;
    };
    let bands: Vec<Band> = list
      .iter()
      .take(eq::MAX_BANDS)
      .map(|b| {
        let n = |k: &str, d: f32| {
          b.get(k)
            .and_then(|x| x.as_f64())
            .map(|x| x as f32)
            .filter(|x| x.is_finite())
            .unwrap_or(d)
        };
        Band {
          kind: b
            .get("type")
            .and_then(|x| x.as_str())
            .and_then(FilterKind::parse)
            .unwrap_or(FilterKind::Peak),
          freq: n("freq", 1000.0),
          q: n("q", 1.41),
          gain_db: n("gain", 0.0),
        }
        .sanitized()
      })
      .collect();
    out.push(EqPreset {
      name: name.to_string(),
      builtin: false,
      preamp_db: item
        .get("preamp_db")
        .and_then(|x| x.as_f64())
        .unwrap_or(0.0)
        .clamp(eq::MIN_PREAMP_DB as f64, eq::MAX_PREAMP_DB as f64) as f32,
      bands,
    });
  }
  out
}

fn write_user(list: &[EqPreset]) -> Result<(), String> {
  let dir = data_dir();
  std::fs::create_dir_all(&dir).map_err(|e| format!("建目录失败 {}：{e}", dir.display()))?;
  let arr: Vec<Value> = list.iter().map(|p| p.to_json()).collect();
  let text = json!({ "version": 1, "presets": arr }).to_string();
  std::fs::write(store_path(), text).map_err(|e| format!("写 EQ 预设失败：{e}"))
}

/// 内置 + 用户，内置排前面。
pub fn list() -> Vec<EqPreset> {
  let mut all = builtins();
  all.extend(load_user());
  all
}

/// 保存（同名覆盖）。内置名保留。
pub fn save(name: &str, preamp_db: f32, bands: &[Band]) -> Result<(), String> {
  let name = name.trim();
  if name.is_empty() {
    return Err("预设名不能为空".to_string());
  }
  if name.chars().count() > MAX_NAME {
    return Err(format!("预设名最长 {MAX_NAME} 个字"));
  }
  if builtins().iter().any(|p| p.name == name) {
    return Err(format!("「{name}」是内置预设，换一个名字"));
  }
  let preamp_db = if preamp_db.is_finite() {
    preamp_db.clamp(eq::MIN_PREAMP_DB, eq::MAX_PREAMP_DB)
  } else {
    0.0
  };
  let bands: Vec<Band> = bands
    .iter()
    .take(eq::MAX_BANDS)
    .map(|b| b.sanitized())
    .collect();

  let mut user = load_user();
  if let Some(slot) = user.iter_mut().find(|p| p.name == name) {
    slot.preamp_db = preamp_db;
    slot.bands = bands.clone();
  } else {
    user.push(EqPreset {
      name: name.to_string(),
      builtin: false,
      preamp_db,
      bands: bands.clone(),
    });
  }
  write_user(&user)?;
  set_active(name, preamp_db, &bands);
  Ok(())
}

/// 删除用户预设。内置的删不掉。
pub fn delete(name: &str) -> Result<(), String> {
  let mut user = load_user();
  let before = user.len();
  user.retain(|p| p.name != name);
  if user.len() == before {
    return Err(format!("没有名为「{name}」的自定义 EQ 预设"));
  }
  write_user(&user)?;
  if active_name() == name {
    clear_active();
  }
  Ok(())
}

// ---------------------------------------------------------------- 应用

/// 生效中的预设：名字 + 内容。
///
/// 为什么要一起存内容：频率拖一下就会连着来几十次 `set_params`，每次都去磁盘读
/// `eq_presets.json` 比对比"这条曲线还是不是那个预设"是不行的。把内容留在内存里，
/// 判断退化成最多 16 次比较。
struct Active {
  name: String,
  preamp_db: f32,
  bands: Vec<Band>,
}

/// 空 = 用户手改过、还没存（面板显示「未保存的调整」）。
static ACTIVE: Mutex<Option<Active>> = Mutex::new(None);

pub fn active_name() -> String {
  ACTIVE
    .lock()
    .ok()
    .and_then(|g| g.as_ref().map(|a| a.name.clone()))
    .unwrap_or_default()
}

fn set_active(name: &str, preamp_db: f32, bands: &[Band]) {
  if let Ok(mut g) = ACTIVE.lock() {
    *g = Some(Active {
      name: name.to_string(),
      preamp_db,
      bands: bands.to_vec(),
    });
  }
}

/// 用户手改频点/前级了 —— 名称不再代表当前曲线，清掉。
pub fn clear_active() {
  if let Ok(mut g) = ACTIVE.lock() {
    *g = None;
  }
}

/// 传进来的曲线是否仍然是当前预设。用来决定"改了之后还挂不挂着这个名字"。
///
/// 精度阈值和 [`Band::sanitized`] 之后的存盘值一致：推子 0.1 dB 一档，
/// 存回来的 -6.3 和界面上的 -6.3 必须判等，否则下拉框会莫名其妙变成「未保存」。
pub fn still_matches(bands: &[Band], preamp_db: f32) -> bool {
  let Ok(g) = ACTIVE.lock() else { return true };
  let Some(a) = g.as_ref() else { return true };
  if (a.preamp_db - preamp_db).abs() > 1e-3 || a.bands.len() != bands.len() {
    return false;
  }
  a.bands.iter().zip(bands).all(|(x, y)| {
    x.kind == y.kind
      && (x.freq - y.freq).abs() < 0.01
      && (x.q - y.q).abs() < 1e-3
      && (x.gain_db - y.gain_db).abs() < 1e-3
  })
}

/// 把预设写进实时参数。
pub fn apply(name: &str, shared: &Arc<Shared>) -> Result<(), String> {
  let Some(p) = list().into_iter().find(|p| p.name == name) else {
    return Err(format!("没有名为「{name}」的 EQ 预设"));
  };
  {
    let mut params = shared.params.lock().map_err(|_| "参数锁失败".to_string())?;
    params.eq_bands = p.bands.clone();
    params.eq_preamp = p.preamp_db;
  }
  set_active(&p.name, p.preamp_db, &p.bands);
  Ok(())
}

/// 取当前实时值（保存预设用）。
pub fn current(shared: &Arc<Shared>) -> (f32, Vec<Band>) {
  match shared.params.lock() {
    Ok(p) => (p.eq_preamp, p.eq_bands.clone()),
    Err(_) => (0.0, Vec::new()),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn peak(freq: f32, gain: f32) -> Band {
    Band {
      kind: FilterKind::Peak,
      freq,
      q: 1.0,
      gain_db: gain,
    }
  }

  #[test]
  fn builtin_flat_really_is_flat() {
    let b = builtins();
    assert_eq!(b.len(), 1);
    assert!(b[0].builtin);
    assert_eq!(b[0].preamp_db, 0.0);
    assert!(b[0].bands.iter().all(|x| x.gain_db == 0.0));
    assert_eq!(b[0].bands.len(), eq::DEFAULT_FREQS.len());
  }

  #[test]
  fn rejects_builtin_name_and_bad_names() {
    assert!(save("平直", 0.0, &[peak(100.0, 3.0)]).is_err());
    assert!(save("  ", 0.0, &[]).is_err());
    assert!(save(&"字".repeat(MAX_NAME + 1), 0.0, &[]).is_err());
  }

  #[test]
  fn bands_json_uses_the_shared_field_names() {
    // 前端 / set_params 读的是 type/freq/q/gain，改名字会让导入的预设"看着有、听着没有"
    let j = bands_json(&[peak(1000.0, -3.0)]);
    assert_eq!(j[0]["type"], "peak");
    assert_eq!(j[0]["freq"], 1000.0);
    assert_eq!(j[0]["gain"], -3.0);
    assert!(j[0].get("q").is_some());
  }

  #[test]
  fn round_trips_through_the_file_format() {
    // 直接测序列化→反序列化这一环（不碰磁盘，避免测试互相踩 APPDATA）
    let p = EqPreset {
      name: "测试".into(),
      builtin: false,
      preamp_db: -6.3,
      bands: vec![peak(20.0, -6.5), peak(200.0, 7.4)],
    };
    let list = vec![p.to_json()];
    let back = parse_list(Some(&json!(list)));
    assert_eq!(back.len(), 1);
    assert_eq!(back[0].name, "测试");
    assert!((back[0].preamp_db + 6.3).abs() < 1e-3, "前级必须一起存回来");
    assert_eq!(back[0].bands.len(), 2);
    assert!((back[0].bands[1].gain_db - 7.4).abs() < 1e-3);
  }

  #[test]
  fn corrupt_file_yields_no_presets_instead_of_panicking() {
    assert!(parse_list(Some(&json!("不是数组"))).is_empty());
    assert!(parse_list(Some(&json!([{"name":"缺 bands"}]))).is_empty());
    assert!(parse_list(None).is_empty());
  }
}
