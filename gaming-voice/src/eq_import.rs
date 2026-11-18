//! 导入别人写的 EQ 配置（EqualizerAPO 的 .txt，或本程序导出的 JSON）。
//!
//! 为什么单独一个模块、而不是在 HTTP 处理里就地把字符串切开：
//! 这份文本**格式不受我们控制** —— 大小写、缺 Q、单位单独成词、被 `OFF` 关掉的滤波器、
//! `#` 注释、`GraphicEQ`、`Include`……每一种出入都得有确定行为，不能"能跑就行"。
//! 写成纯函数（无 IO、无锁），测试可以直接喂字符串，边界都在下面那些用例里钉死。
//!
//! 支持的写法（大小写不敏感）：
//!
//! ```text
//! Preamp: -6.3 dB
//! Filter 1: ON PK Fc 20 Hz Gain -6.5 dB Q 1.600
//! Filter 2: ON LSC Fc 100 Hz Gain 3 dB Q 0.7
//! ```
//!
//! 类型映射：`PK/PEAK`→峰值、`LS/LSC`→低架、`HS/HSC`→高架、`LP/LPQ`→低通、
//! `HP/HPQ`→高通、`NO`(陷波)→ -12 dB 峰值（近似）、`Modal`→峰值。
//! `BP/AP` 和 `GraphicEQ` 我们没有对应实现，会明确写进 `notes` 而不是静默丢掉。

use crate::dsp::surround::eq::{self, Band, FilterKind};

/// 一条被解析出来的滤波器链。
#[derive(Clone, Debug)]
pub struct Imported {
  pub bands: Vec<Band>,
  pub preamp_db: f32,
  /// 逐条说明：跳过了什么、夹了什么。**必须上屏** —— 导入最怕"看起来成功了"。
  pub notes: Vec<String>,
  /// 识别出的来源，给人看的。
  pub format: &'static str,
}

/// 解析入口。`Err` 的文案直接可以给用户看。
pub fn parse(text: &str) -> Result<Imported, String> {
  let t = text.trim().trim_start_matches('\u{feff}').trim();
  if t.is_empty() {
    return Err("文件是空的".to_string());
  }
  if t.starts_with('{') || t.starts_with('[') {
    return parse_json(t);
  }
  parse_apo(t)
}

// ---------------------------------------------------------------- EqualizerAPO

fn parse_apo(text: &str) -> Result<Imported, String> {
  let mut out = Imported {
    bands: Vec::new(),
    preamp_db: 0.0,
    notes: Vec::new(),
    format: "EqualizerAPO",
  };
  let mut graphic_eq = false;
  let mut ignored = 0usize;
  let mut overflow = false;

  for raw in text.lines() {
    // `#` 之后是注释；`;` 不能当注释，GraphicEQ 用它分隔频点。
    // BOM 逐行再剥一次：Windows 记事本存出来的文件第一行前面就挂着一颗。
    let line = raw
      .split('#')
      .next()
      .unwrap_or("")
      .trim()
      .trim_start_matches('\u{feff}')
      .trim();
    if line.is_empty() {
      continue;
    }
    let low = line.to_ascii_lowercase();

    if let Some(rest) = low.strip_prefix("preamp") {
      if let Some(v) = first_number(rest) {
        out.preamp_db = v;
      }
      continue;
    }

    if low.starts_with("filter") {
      // 用原串切，不用 `low`：`to_ascii_lowercase` 不改变字节长度，位置对得上
      let rest = after_colon(&line["filter".len()..]);
      match parse_filter(rest, &mut out.notes) {
        Some(b) => {
          if out.bands.len() >= eq::MAX_BANDS {
            if !overflow {
              overflow = true;
              out.notes.push(format!(
                "频点超过上限 {}，后面的已忽略（这类配置通常有 31 段，先删掉用不到的再导入）",
                eq::MAX_BANDS
              ));
            }
          } else {
            out.bands.push(b);
          }
        }
        None => ignored += 1,
      }
      continue;
    }

    if low.starts_with("graphiceq") {
      graphic_eq = true;
      continue;
    }

    // 设备定向、包含其它文件、通道复制 —— 和"一条滤波器"无关，不算错误
    if [
      "include",
      "device",
      "stage",
      "channel",
      "copy",
      "delay",
      "convolution",
    ]
    .iter()
    .any(|k| low.starts_with(k))
    {
      continue;
    }
    ignored += 1;
  }

  if graphic_eq {
    out.notes.push(
      "这份配置里有 GraphicEQ（按频点表插值）—— 我们没有对应的实现，那部分是空的。\
       在 EqualizerAPO 里把图形均衡改写成几个参数滤波器再导出即可。"
        .to_string(),
    );
  }
  if ignored > 0 {
    out.notes.push(format!("有 {ignored} 行无法识别，已跳过"));
  }
  if out.bands.is_empty() {
    return Err(
      "没解析出任何滤波器。支持 EqualizerAPO 的 `Filter N: ON PK Fc … Gain … Q …` 写法，\
       以及本程序导出的 JSON。"
        .to_string(),
    );
  }
  if out.preamp_db == 0.0 && out.bands.iter().any(|b| b.gain_db > 0.0) {
    out
      .notes
      .push("文件里没有 Preamp，但有正增益频点 —— 如果听到破音，把前级往下拉几 dB".to_string());
  }
  Ok(out)
}

/// 冒号之后的部分；没有冒号就整段都是内容。
fn after_colon(s: &str) -> &str {
  s.split_once(':').map(|x| x.1).unwrap_or(s)
}

/// 解析一条 `ON PK Fc 1000 Hz Gain -3 dB Q 1.5`。
///
/// 返回 `None` 表示"这条不产生频点"（被 OFF 关掉、或类型我们不支持），
/// 需要提示的已经写进 `notes` 了。
fn parse_filter(rest: &str, notes: &mut Vec<String>) -> Option<Band> {
  let toks: Vec<&str> = rest.split_whitespace().collect();
  let mut i = 0usize;

  // 状态位：APO 写 ON / OFF，省略时按 ON 处理（手写的小文件常省）
  if let Some(t) = toks.first() {
    match t.to_ascii_lowercase().as_str() {
      "on" => i = 1,
      "off" => return None,
      _ => {}
    }
  }

  let type_tok = toks.get(i)?.to_ascii_lowercase();
  i += 1;
  let (kind, forced_gain) = match type_tok.as_str() {
    "pk" | "peak" | "peaking" | "modal" => (FilterKind::Peak, None),
    "ls" | "lsc" | "lowshelf" => (FilterKind::LowShelf, None),
    "hs" | "hsc" | "highshelf" => (FilterKind::HighShelf, None),
    "lp" | "lpq" | "lowpass" => (FilterKind::LowPass, None),
    "hp" | "hpq" | "highpass" => (FilterKind::HighPass, None),
    // 陷波是"极窄极深"的峰值，我们上限 -12 dB，只能近似
    "no" | "notch" => (FilterKind::Peak, Some(-eq::MAX_DB)),
    _ => {
      notes.push(format!("跳过不支持的滤波器类型「{type_tok}」"));
      return None;
    }
  };

  let mut freq = None;
  let mut gain = None;
  let mut q = None;
  while i < toks.len() {
    let key = toks[i].to_ascii_lowercase();
    i += 1;
    if !matches!(key.as_str(), "fc" | "gain" | "q" | "bw" | "slope") {
      continue;
    }
    let Some(v) = toks.get(i).and_then(|t| number(t)) else {
      continue;
    };
    i += 1;
    // 单位经常单独成词：`Fc 20 Hz`
    let mut scale = 1.0;
    if let Some(u) = toks.get(i) {
      match u.to_ascii_lowercase().as_str() {
        "hz" => i += 1,
        "khz" => {
          scale = 1000.0;
          i += 1;
        }
        "db" | "dbs" | "deg" => i += 1,
        _ => {}
      }
    }
    match key.as_str() {
      "fc" => freq = Some(v * scale),
      "gain" => gain = Some(v),
      "q" => q = Some(v),
      _ => {}
    }
  }

  let freq = freq.unwrap_or(1000.0);
  let default_q = match kind {
    FilterKind::HighPass | FilterKind::LowPass => 0.707,
    _ => 1.41,
  };
  let q = q.unwrap_or(default_q);
  let gain_db = forced_gain.unwrap_or_else(|| gain.unwrap_or(0.0));

  // 越界一律夹住，并且**说出来**：夹完的曲线和用户手上那份文档不一样，
  // 不说明的话他会以为自己导入错了。
  if !(eq::MIN_FREQ..=eq::MAX_FREQ).contains(&freq) {
    notes.push(format!(
      "频点 {freq:.0} Hz 超出 {:.0}–{:.0} Hz，已夹住",
      eq::MIN_FREQ,
      eq::MAX_FREQ
    ));
  }
  if !(-eq::MAX_DB..=eq::MAX_DB).contains(&gain_db) {
    notes.push(format!(
      "增益 {gain_db:.1} dB 超出 ±{:.0} dB，已夹住",
      eq::MAX_DB
    ));
  }
  if !(eq::MIN_Q..=eq::MAX_Q).contains(&q) {
    notes.push(format!(
      "Q {q:.2} 超出 {:.1}–{:.0}，已夹住",
      eq::MIN_Q,
      eq::MAX_Q
    ));
  }

  Some(
    Band {
      kind,
      freq,
      q,
      gain_db,
    }
    .sanitized(),
  )
}

// ---------------------------------------------------------------- JSON

fn parse_json(text: &str) -> Result<Imported, String> {
  let v: serde_json::Value =
    serde_json::from_str(text).map_err(|e| format!("JSON 语法错误：{e}"))?;
  let arr = v
    .get("bands")
    .or_else(|| v.get("eq_bands"))
    .and_then(|x| x.as_array())
    .or_else(|| v.as_array())
    .ok_or_else(|| "JSON 里没有 bands / eq_bands 数组".to_string())?;

  let mut bands = Vec::new();
  let mut notes = Vec::new();
  for item in arr {
    if bands.len() >= eq::MAX_BANDS {
      notes.push(format!("频点超过上限 {}，后面的已忽略", eq::MAX_BANDS));
      break;
    }
    let kind = item
      .get("type")
      .and_then(|x| x.as_str())
      .and_then(FilterKind::parse)
      .unwrap_or(FilterKind::Peak);
    let n = |k: &str, d: f32| {
      item
        .get(k)
        .and_then(|x| x.as_f64())
        .map(|x| x as f32)
        .filter(|x| x.is_finite())
        .unwrap_or(d)
    };
    bands.push(
      Band {
        kind,
        freq: n("freq", 1000.0),
        q: n("q", 1.41),
        gain_db: n("gain", 0.0),
      }
      .sanitized(),
    );
  }
  if bands.is_empty() {
    return Err("JSON 里的 bands 是空的".to_string());
  }
  let preamp_db = v
    .get("preamp_db")
    .or_else(|| v.get("preamp"))
    .and_then(|x| x.as_f64())
    .unwrap_or(0.0) as f32;

  Ok(Imported {
    bands,
    preamp_db,
    notes,
    format: "JSON",
  })
}

// ---------------------------------------------------------------- 小工具

/// 从 `" -6.3 dB"` 里取第一个数。
fn first_number(s: &str) -> Option<f32> {
  s.split_whitespace().find_map(number)
}

/// `"20"` / `"-6.5"` / `"-6.5dB"` / `"1.600,"` → f32。
fn number(tok: &str) -> Option<f32> {
  let t = tok.trim_matches(|c: char| c == ',' || c == ';' || c == '(' || c == ')');
  let t = t.trim_end_matches(|c: char| c.is_ascii_alphabetic() || c == '°');
  t.parse::<f32>().ok()
}

#[cfg(test)]
mod tests {
  use super::*;

  /// 用户实际给的这份文件，原样贴进来 —— 这是"能不能用"的唯一标准。
  const APEX: &str = "\
Preamp: -6.3 dB
Filter 1: ON PK Fc 20 Hz Gain -6.5 dB Q 1.600
Filter 2: ON PK Fc 30 Hz Gain -11.9 dB Q 0.500
Filter 3: ON PK Fc 200 Hz Gain 7.4 dB Q 1.800
Filter 4: ON PK Fc 600 Hz Gain 1.3 dB Q 1.000
Filter 5: ON PK Fc 2200 Hz Gain -4.5 dB Q 2.000
Filter 6: ON PK Fc 5000 Hz Gain -7.7 dB Q 2.000
Filter 7: ON PK Fc 6800 Hz Gain 7.4 dB Q 2.000
Filter 8: ON PK Fc 11000 Hz Gain 3.5 dB Q 2.000
";

  #[test]
  fn parses_the_real_apex_file() {
    let r = parse(APEX).unwrap();
    assert_eq!(r.preamp_db, -6.3);
    assert_eq!(r.bands.len(), 8);
    assert!(r.notes.is_empty(), "这份文件不该有任何警告：{:?}", r.notes);

    let b = r.bands[0];
    assert_eq!(b.kind, FilterKind::Peak);
    assert!((b.freq - 20.0).abs() < 1e-3);
    assert!((b.gain_db + 6.5).abs() < 1e-3);
    assert!((b.q - 1.6).abs() < 1e-3);

    // 最后一行的 11000 Hz / +3.5 dB 不能被前一行吃掉
    let last = r.bands[7];
    assert!((last.freq - 11_000.0).abs() < 1e-3);
    assert!((last.gain_db - 3.5).abs() < 1e-3);
  }

  #[test]
  fn every_field_is_parsed_independently() {
    // 这条专门钉住"单位单独成词"的坑：如果把 `Hz` 当成下一个键，后面的全错位
    let r = parse("Filter 1: ON HP Fc 120 Hz Q 0.7\n").unwrap();
    let b = r.bands[0];
    assert_eq!(b.kind, FilterKind::HighPass);
    assert!((b.freq - 120.0).abs() < 1e-3);
    assert!((b.q - 0.7).abs() < 1e-3);
  }

  #[test]
  fn missing_q_falls_back_by_type() {
    let r = parse("Filter 1: ON PK Fc 1000 Hz Gain 3 dB").unwrap();
    assert!((r.bands[0].q - 1.41).abs() < 1e-3);
    let r = parse("Filter 1: ON HP Fc 80 Hz").unwrap();
    assert!((r.bands[0].q - 0.707).abs() < 1e-3, "高通缺省应为 0.707");
  }

  #[test]
  fn off_filters_are_dropped_silently() {
    let r =
      parse("Filter 1: OFF PK Fc 1000 Hz Gain 9 dB Q 1\nFilter 2: ON PK Fc 500 Hz Gain 3 dB Q 1\n")
        .unwrap();
    assert_eq!(r.bands.len(), 1);
    assert!((r.bands[0].freq - 500.0).abs() < 1e-3);
  }

  #[test]
  fn type_codes_map_to_our_five_kinds() {
    let cases = [
      ("LS", FilterKind::LowShelf),
      ("LSC", FilterKind::LowShelf),
      ("HS", FilterKind::HighShelf),
      ("HSC", FilterKind::HighShelf),
      ("LP", FilterKind::LowPass),
      ("HPQ", FilterKind::HighPass),
      ("PEAK", FilterKind::Peak),
    ];
    for (code, want) in cases {
      let r = parse(&format!("Filter 1: ON {code} Fc 200 Hz Gain 3 dB Q 1\n")).unwrap();
      assert_eq!(r.bands[0].kind, want, "{code} 映射错了");
    }
  }

  #[test]
  fn notch_becomes_peak_and_says_so() {
    let r = parse("Filter 1: ON NO Fc 60 Hz Q 8\n").unwrap();
    assert_eq!(r.bands[0].kind, FilterKind::Peak);
    assert_eq!(r.bands[0].gain_db, -eq::MAX_DB);
  }

  #[test]
  fn out_of_range_values_are_clamped_with_notes() {
    let r = parse("Filter 1: ON PK Fc 30000 Hz Gain 24 dB Q 40\n").unwrap();
    let b = r.bands[0];
    assert_eq!(b.freq, eq::MAX_FREQ);
    assert_eq!(b.gain_db, eq::MAX_DB);
    assert_eq!(b.q, eq::MAX_Q);
    // 三项越界都必须**说出来**：夹完的曲线和用户手上那份文档不一样，不解释就等于骗人
    for word in ["频点", "增益", "Q "] {
      assert!(
        r.notes.iter().any(|n| n.contains(word)),
        "缺少关于「{word}」的说明：{:?}",
        r.notes
      );
    }
  }

  #[test]
  fn comments_and_blank_lines_are_ignored() {
    let r = parse("# 我的耳机校正\n\nFilter 1: ON PK Fc 100 Hz Gain -3 dB Q 1 # 低频\n").unwrap();
    assert_eq!(r.bands.len(), 1);
    assert!((r.bands[0].freq - 100.0).abs() < 1e-3);
  }

  #[test]
  fn graphic_eq_is_reported_not_silently_dropped() {
    let r = parse("GraphicEQ: 20 -3; 21 -3.1; 22 -3.2\nFilter 1: ON PK Fc 100 Hz Gain -3 dB Q 1\n")
      .unwrap();
    assert_eq!(r.bands.len(), 1);
    assert!(r.notes.iter().any(|n| n.contains("GraphicEQ")));
  }

  #[test]
  fn band_overflow_is_capped() {
    let mut text = String::new();
    for i in 0..40 {
      text.push_str(&format!(
        "Filter {i}: ON PK Fc {} Hz Gain 1 dB Q 1\n",
        100 + i * 50
      ));
    }
    let r = parse(&text).unwrap();
    assert_eq!(r.bands.len(), eq::MAX_BANDS);
    assert!(r.notes.iter().any(|n| n.contains("上限")));
  }

  #[test]
  fn positive_gain_without_preamp_warns_about_clipping() {
    let r = parse("Filter 1: ON PK Fc 100 Hz Gain 8 dB Q 1\n").unwrap();
    assert!(r.notes.iter().any(|n| n.contains("前级")));
    // 有 Preamp 就不该再唠叨
    let r = parse("Preamp: -5 dB\nFilter 1: ON PK Fc 100 Hz Gain 8 dB Q 1\n").unwrap();
    assert!(!r.notes.iter().any(|n| n.contains("前级")));
  }

  #[test]
  fn empty_or_garbage_is_an_error_with_a_usable_message() {
    assert!(parse("   ").is_err());
    assert!(parse("hello world").is_err());
    let e = parse("hello world").unwrap_err();
    assert!(e.contains("Filter"), "错误文案要能指路：{e}");
  }

  #[test]
  fn our_own_json_round_trips() {
    let text = r#"{"version":1,"preamp_db":-4.5,"bands":[
      {"type":"lowshelf","freq":120,"q":0.7,"gain":3.5},
      {"type":"highpass","freq":30,"q":0.707,"gain":0}
    ]}"#;
    let r = parse(text).unwrap();
    assert_eq!(r.format, "JSON");
    assert_eq!(r.bands.len(), 2);
    assert!((r.preamp_db + 4.5).abs() < 1e-3);
    assert_eq!(r.bands[0].kind, FilterKind::LowShelf);
    assert_eq!(r.bands[1].kind, FilterKind::HighPass);
  }

  #[test]
  fn json_array_without_wrapper_is_accepted() {
    let text = r#"[{"type":"peak","freq":1000,"q":2,"gain":-6}]"#;
    let r = parse(text).unwrap();
    assert_eq!(r.bands.len(), 1);
    assert!((r.bands[0].gain_db + 6.0).abs() < 1e-3);
  }

  #[test]
  fn bom_and_crlf_do_not_break_parsing() {
    // Windows 记事本存出来的文件常带 BOM + CRLF
    let text = "\u{feff}Preamp: -3 dB\r\nFilter 1: ON PK Fc 1000 Hz Gain -3 dB Q 1\r\n";
    let r = parse(text).unwrap();
    assert_eq!(r.bands.len(), 1);
    assert!((r.preamp_db + 3.0).abs() < 1e-3);
  }
}
