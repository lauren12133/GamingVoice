'use strict';

/* ==========================================================================
   GamingVoice · 面板逻辑
   - 通道顺序与 Rust 侧 `Speaker::ALL` 严格一致：下标就是 WASAPI 的声道下标
   - 推子改动 40 ms 合并发送，避免拖动时刷爆 HTTP
   - 主题 / 界面缩放存 localStorage，下次打开还是上次的样子
   ========================================================================== */

const CH = [
  { id: 'FL',  cn: '前左', az: -30 },
  { id: 'FR',  cn: '前右', az:  30 },
  { id: 'FC',  cn: '中置', az:   0 },
  { id: 'LFE', cn: '低频', az: null },
  { id: 'BL',  cn: '后左', az: -140 },
  { id: 'BR',  cn: '后右', az:  140 },
  { id: 'SL',  cn: '侧左', az: -90 },
  { id: 'SR',  cn: '侧右', az:  90 },
];

// 后端 `MAX_GAIN_DB` 必须与这里一致；不一致时后端会夹住，界面会"弹回去"
const DB_MIN = -30;
const DB_MAX = 20;
const MUTE_DB = -60;

const ZOOMS = ['0.9', '1', '1.15', '1.3'];
const LS_THEME = 'gamingvoice.theme';
const LS_ZOOM = 'gamingvoice.zoom';

let state = null;
let savedGain = new Array(8).fill(0);   // 静音前的值
let display = new Array(8).fill(0);     // 电平条（带衰减）
let userPickedDevice = false;
let sendTimer = null;
let dragging = false;

// ---------------------------------------------------------------- 主题 / 缩放

const root = document.documentElement;

function applyTheme(t) {
  root.dataset.theme = t;
  const btn = document.getElementById('btnTheme');
  // 按钮显示"点了会变成什么"，比显示当前状态更少歧义
  btn.textContent = t === 'dark' ? '白天' : '黑夜';
  btn.title = t === 'dark' ? '切换到白天模式' : '切换到黑夜模式';
  try { localStorage.setItem(LS_THEME, t); } catch (_) {}
}

function applyZoom(z) {
  root.dataset.zoom = z;
  try { localStorage.setItem(LS_ZOOM, z); } catch (_) {}
  drawRadar();
}

(function initPrefs() {
  let t = 'dark';
  let z = '1';
  try {
    t = localStorage.getItem(LS_THEME) || 'dark';
    z = localStorage.getItem(LS_ZOOM) || '1';
  } catch (_) {}
  if (!ZOOMS.includes(z)) z = '1';
  applyTheme(t === 'light' ? 'light' : 'dark');
  applyZoom(z);
})();

document.getElementById('btnTheme').addEventListener('click', () => {
  applyTheme(root.dataset.theme === 'dark' ? 'light' : 'dark');
});
document.getElementById('btnZoomIn').addEventListener('click', () => {
  const i = ZOOMS.indexOf(root.dataset.zoom);
  applyZoom(ZOOMS[Math.min(ZOOMS.length - 1, i + 1)]);
});
document.getElementById('btnZoomOut').addEventListener('click', () => {
  const i = ZOOMS.indexOf(root.dataset.zoom);
  applyZoom(ZOOMS[Math.max(0, i - 1)]);
});

// ---------------------------------------------------------------- 通道行

const list = document.getElementById('chList');
CH.forEach((c, i) => {
  const row = document.createElement('div');
  row.className = 'ch';
  row.dataset.i = i;
  row.innerHTML = `
    <div class="nm">
      <b>${c.cn}</b>
      <em>${c.id}${c.az === null ? ' · 无方向' : ' · ' + c.az + '°'}</em>
    </div>
    <div class="slider">
      <input type="range" min="${DB_MIN}" max="${DB_MAX}" step="0.5" value="0"
             aria-label="${c.cn} 增益">
      <div class="bar"><div class="fill"></div></div>
    </div>
    <div class="right">
      <div class="db">0.0</div>
      <button class="mute" title="静音 ${c.cn}">静音</button>
    </div>`;
  row.querySelector('.mute').addEventListener('click', () => toggleMute(i, row));
  row.querySelector('input').addEventListener('input', (e) => {
    const v = parseFloat(e.target.value);
    row.querySelector('.db').textContent = fmt(v);
    row.classList.remove('muted');
    row.querySelector('.mute').classList.remove('on');
    pushParams({ gains_db: collectGains() });
    drawRadar();
  });
  list.appendChild(row);
});

function fmt(v) {
  return (v > 0 ? '+' : '') + v.toFixed(1);
}

function collectGains() {
  return [...document.querySelectorAll('.ch')].map((row) => {
    if (row.classList.contains('muted')) return MUTE_DB;
    return parseFloat(row.querySelector('input').value);
  });
}

function toggleMute(i, row) {
  const input = row.querySelector('input');
  const btn = row.querySelector('.mute');
  if (row.classList.contains('muted')) {
    row.classList.remove('muted');
    btn.classList.remove('on');
    btn.textContent = '静音';
    input.value = savedGain[i];
  } else {
    savedGain[i] = parseFloat(input.value);
    row.classList.add('muted');
    btn.classList.add('on');
    btn.textContent = '已静音';
    input.value = DB_MIN;
  }
  row.querySelector('.db').textContent = row.classList.contains('muted')
    ? '静音' : fmt(parseFloat(input.value));
  pushParams({ gains_db: collectGains() });
  drawRadar();
}

// ---------------------------------------------------------------- 通信

function pushParams(obj) {
  clearTimeout(sendTimer);
  sendTimer = setTimeout(() => {
    fetch('/api/params', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(obj),
    }).catch(() => {});
  }, 40);
}

async function poll() {
  try {
    const r = await fetch('/api/state', { cache: 'no-store' });
    state = await r.json();
    render();
  } catch (_) { /* 后端没起来就安静重试 */ }
  setTimeout(poll, 120);
}

/**
 * 主动弹一条提示。
 *
 * 带"粘性"：面板每 120 ms 会用 state 重刷警告条，如果不设保护，
 * 用户刚看到的安装指引会被下一帧的状态覆盖掉（看起来像没点成功）。
 */
let warnSticky = 0;
function showWarn(text, ms = 9000) {
  warnSticky = Date.now() + ms;
  const w = document.getElementById('warn');
  document.getElementById('warnText').textContent = text;
  w.classList.remove('hidden');
}

function render() {
  const s = state;

  // 顶栏
  document.getElementById('led').className =
    'led ' + (s.running ? (s.error ? 'bad' : 'on') : '');
  document.getElementById('runText').textContent = s.running ? '运行中' : '未启动';
  const btn = document.getElementById('btnRun');
  btn.textContent = s.running ? '停止' : '启动';
  btn.classList.toggle('stop', !!s.running);

  // 警告（用户主动提示的优先级高于状态推导，别被下一帧冲掉）
  const warnEl = document.getElementById('warn');
  if (Date.now() < warnSticky) {
    // 用户主动提示还在有效期内，别覆盖
    warnEl.classList.remove('hidden');
  } else if (s.error) {
    // 这里不能走 showWarn：每 120 ms 续一次粘性，错误消失了横幅还要挂 9 秒
    document.getElementById('warnText').textContent = s.error;
    warnEl.classList.remove('hidden');
  } else {
    const surround = (s.endpoints || []).filter((e) => e.surround);
    if (!s.running && surround.length === 0) {
      document.getElementById('warnText').innerHTML =
        '<b>没有检测到 8 声道端点，游戏现在只输出立体声。</b>\n' +
        '「分方向音量」需要 8 条独立声道才有可分的对象 —— 用下面的一键安装装 VB-CABLE，' +
        '再到「声音设置 → 该设备 → 配置扬声器」改成 7.1。';
      warnEl.classList.remove('hidden');
    } else {
      warnEl.classList.add('hidden');
    }
  }

  fillSelect('selSource', s.endpoints, true);
  fillSelect('selSink', s.endpoints, false);
  updateMeta();
  fillHrir();
  fillPreset();
  renderEq();
  updateVb();

  // 指标
  document.getElementById('kBlock').textContent = s.status.block_ms.toFixed(1) + ' ms';
  document.getElementById('kBuf').textContent = s.status.buffer_ms.toFixed(0) + ' ms';
  document.getElementById('kXrun').textContent = s.status.underruns;
  document.getElementById('kUp').textContent = s.running
    ? fmtDuration((s.status.frames || 0) / 48000)
    : '—';

  document.getElementById('makeupVal').textContent = fmt(s.params.makeup_db);
  const mk = document.getElementById('makeup');
  if (document.activeElement !== mk) mk.value = s.params.makeup_db;

  // 推子（拖动时不回写，否则跟手指打架）
  if (!dragging) {
    document.querySelectorAll('.ch').forEach((row, i) => {
      const input = row.querySelector('input');
      const db = s.params.gains_db[i];
      const muteBtn = row.querySelector('.mute');
      if (db <= MUTE_DB + 1) {
        row.classList.add('muted');
        muteBtn.classList.add('on');
        muteBtn.textContent = '已静音';
        input.value = DB_MIN;
        row.querySelector('.db').textContent = '静音';
      } else {
        row.classList.remove('muted');
        muteBtn.classList.remove('on');
        muteBtn.textContent = '静音';
        input.value = Math.min(DB_MAX, Math.max(DB_MIN, db));
        row.querySelector('.db').textContent = fmt(db);
      }
    });
  }

  // 电平条
  document.querySelectorAll('.ch').forEach((row, i) => {
    const peak = s.levels.out_peak[i];
    const db = 20 * Math.log10(Math.max(peak, 1e-6));
    const pct = Math.max(0, Math.min(100, ((db + 60) / 60) * 100));
    display[i] = Math.max(pct, display[i] * 0.8);
    const fill = row.querySelector('.fill');
    fill.style.width = display[i].toFixed(1) + '%';
    fill.className = 'fill' + (db > -3 ? ' over' : db > -12 ? ' hot' : '');
  });

  // 削波风险：最大方向增益与总补偿相加后仍超过 +6 dB，就有明显削波可能；
  // 另外双耳峰值已经顶到 0 dBFS 时无条件提示。
  const live = s.params.gains_db.filter((g) => g > MUTE_DB + 1);
  const maxGain = live.length ? Math.max(...live) : 0;
  const hot = maxGain + s.params.makeup_db > 6;
  const clipping = Math.max(...(s.levels.ear_peak || [0])) > 0.985;
  document.getElementById('clipTag').classList.toggle('hidden', !(hot || clipping));

  drawRadar();
}

function fmtDuration(sec) {
  const m = Math.floor(sec / 60);
  const ss = Math.floor(sec % 60);
  return m + ':' + String(ss).padStart(2, '0');
}

document.addEventListener('pointerdown', (e) => {
  if (e.target.type === 'range') dragging = true;
});
document.addEventListener('pointerup', () => { dragging = false; });

// ---------------------------------------------------------------- 设备下拉

function fillSelect(id, eps, isSource) {
  const sel = document.getElementById(id);
  if (!eps || eps.length === 0) return;
  const sig = eps.map((e) => e.id + e.channels).join('|');
  if (sel.dataset.sig === sig) return;   // 没变就不动 DOM，避免打断用户点选
  sel.dataset.sig = sig;

  const prev = sel.value;
  sel.innerHTML = '';
  eps.forEach((e) => {
    const o = document.createElement('option');
    o.value = e.id;
    const who = e.name || e.label;
    o.textContent =
      (e.surround ? '★ ' : '') + who + '（' + e.channels + ' 声道）' +
      (e.default ? ' · 默认' : '');
    sel.appendChild(o);
  });

  if (prev && eps.some((e) => e.id === prev)) {
    sel.value = prev;
    return;
  }
  // 首次：源优先 8 声道，输出优先非 8 声道（通常是耳机）
  const pick = isSource
    ? eps.find((e) => e.surround) || eps[0]
    : eps.find((e) => !e.surround) || eps[0];
  if (!userPickedDevice) sel.value = pick.id;
}

function updateMeta() {
  const eps = (state && state.endpoints) || [];
  const byId = (id) => eps.find((e) => e.id === id);
  const src = byId(document.getElementById('selSource').value);
  const snk = byId(document.getElementById('selSink').value);
  const meta = (e) => {
    if (!e) return '—';
    const tag = e.surround ? '可作 7.1 源' : '非 8 声道，只能当普通输出';
    return e.channels + ' 声道 · ' + tag;
  };
  document.getElementById('srcMeta').textContent = meta(src);
  document.getElementById('snkMeta').textContent = snk
    ? snk.channels + ' 声道 · 耳机/输出'
    : '—';
}

document.getElementById('btnRefresh').addEventListener('click', async () => {
  const b = document.getElementById('btnRefresh');
  b.disabled = true;
  await fetch('/api/refresh', { method: 'POST' }).catch(() => {});
  document.getElementById('selSource').dataset.sig = '';
  document.getElementById('selSink').dataset.sig = '';
  userPickedDevice = false;
  b.disabled = false;
});

document.getElementById('selSource').addEventListener('change', () => {
  userPickedDevice = true;
  updateMeta();
});
document.getElementById('selSink').addEventListener('change', () => {
  userPickedDevice = true;
  updateMeta();
});

// ---------------------------------------------------------------- 声场图

function drawRadar() {
  const svg = document.getElementById('radar');
  if (!svg) return;
  const cx = 150, cy = 154, R = 106;
  const gains = state ? state.params.gains_db : new Array(8).fill(0);
  const peaks = state ? state.levels.out_peak : new Array(8).fill(0);

  let g = `<defs>
    <radialGradient id="glow" cx="50%" cy="50%" r="50%">
      <stop class="glow-a" offset="0%"/>
      <stop class="glow-b" offset="100%"/>
    </radialGradient>
  </defs>`;
  g += `<circle cx="${cx}" cy="${cy}" r="${R}" fill="url(#glow)"/>`;

  [0.34, 0.67, 1].forEach((k) => {
    g += `<circle class="gridline" cx="${cx}" cy="${cy}" r="${(R * k).toFixed(1)}"/>`;
  });
  g += `<line class="gridline" x1="${cx}" y1="${cy - R - 12}" x2="${cx}" y2="${cy + R + 12}"/>`;
  g += `<line class="gridline" x1="${cx - R - 12}" y1="${cy}" x2="${cx + R + 12}" y2="${cy}"/>`;

  // 方向标签固定画在外圈：点会随增益伸缩，标签跟着跑就会互相压住
  CH.forEach((c) => {
    if (c.az === null) return;
    const rad = (c.az * Math.PI) / 180;
    const lr = R + 20;
    const x = cx + lr * Math.sin(rad);
    const y = cy - lr * Math.cos(rad) + 4.5;
    g += `<text class="axis-label" x="${x.toFixed(1)}" y="${y.toFixed(1)}"
      text-anchor="middle">${c.cn}</text>`;
  });

  const pts = [];
  CH.forEach((c, i) => {
    const db = gains[i];
    const k = Math.max(0.16, Math.min(1, (db + 30) / (DB_MAX + 30)));
    const r = R * k;
    let x, y;
    if (c.az === null) {
      x = cx;
      y = cy + R * 0.5;
    } else {
      const rad = (c.az * Math.PI) / 180;
      x = cx + r * Math.sin(rad);
      y = cy - r * Math.cos(rad);
    }
    pts.push({ x, y, c, db, muted: db <= MUTE_DB + 1, peak: peaks[i] || 0 });
  });

  pts.forEach((p) => {
    if (p.c.az === null) return;
    g += `<line class="${p.muted ? 'spoke dim' : 'spoke live'}"
      x1="${cx}" y1="${cy}" x2="${p.x.toFixed(1)}" y2="${p.y.toFixed(1)}"/>`;
  });

  pts.forEach((p) => {
    // 圆点亮度跟实时电平走：静的时候暗，有声音时亮
    const lv = Math.max(0, Math.min(1, (20 * Math.log10(Math.max(p.peak, 1e-6)) + 60) / 60));
    const opacity = p.muted ? 0.3 : 0.35 + 0.65 * lv;
    const cls = p.muted ? 'dot' : p.db > 3 ? 'dot hot' : 'dot';
    g += `<circle class="${cls}" cx="${p.x.toFixed(1)}" cy="${p.y.toFixed(1)}"
      r="${p.c.az === null ? 4.5 : 5.5}" opacity="${opacity.toFixed(2)}"/>`;
    if (p.c.az === null) {
      // 低频没有方向，单独在中心下方标一下，避免和"后"挤在一起
      g += `<text class="dot-label dim" x="${p.x.toFixed(1)}" y="${(p.y + 18).toFixed(1)}">低频</text>`;
    }
  });

  g += `<circle class="center" cx="${cx}" cy="${cy}" r="3"/>`;
  svg.innerHTML = g;
}

// ---------------------------------------------------------------- 预设（下拉 + 自定义）

let presetSig = '';

async function postPreset(body) {
  const r = await fetch('/api/presets', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(body),
  })
    .then((x) => x.json())
    .catch((e) => ({ ok: false, error: String(e) }));
  if (!r.ok) {
    showWarn(r.error || '预设操作失败');
  } else {
    document.getElementById('warn').classList.add('hidden');
  }
  return r;
}

function fillPreset() {
  const sel = document.getElementById('selPreset');
  const del = document.getElementById('btnPresetDel');
  if (!sel || !state) return;
  const list = state.presets || [];
  const sig = list.map((p) => p.name + (p.builtin ? 'B' : 'U')).join('|');
  if (sig !== presetSig) {
    presetSig = sig;
    sel.innerHTML = '';
    list.forEach((p) => {
      const o = document.createElement('option');
      o.value = p.name;
      // 自定的加个后缀，用户一眼能分清哪些是自己存的
      o.textContent = p.builtin ? p.name : p.name + ' · 自定';
      sel.appendChild(o);
    });
  }
  if (state.preset && document.activeElement !== sel) sel.value = state.preset;
  const cur = list.find((p) => p.name === sel.value);
  // 内置的删不掉 —— 直接禁用比点了报错更省事
  if (del) {
    del.disabled = !cur || cur.builtin;
    del.style.opacity = del.disabled ? 0.4 : 1;
    del.style.cursor = del.disabled ? 'not-allowed' : 'pointer';
  }
}

document.getElementById('selPreset').addEventListener('change', async (e) => {
  await postPreset({ action: 'apply', name: e.target.value });
});

const presetNameEl = document.getElementById('presetName');
const presetOkEl = document.getElementById('btnPresetOk');

document.getElementById('btnPresetSave').addEventListener('click', () => {
  // 内联输入而不是 prompt()：prompt 在很多环境下被拦，而且样式无法与面板统一
  const showing = !presetNameEl.hidden;
  presetNameEl.hidden = showing;
  presetOkEl.hidden = showing;
  if (!showing) {
    presetNameEl.value = (state && state.preset) || '';
    presetNameEl.focus();
    presetNameEl.select();
  }
});

async function commitPreset() {
  const name = presetNameEl.value.trim();
  if (!name) {
    presetNameEl.focus();
    return;
  }
  await postPreset({ action: 'save', name, gains_db: collectGains() });
  presetNameEl.hidden = true;
  presetOkEl.hidden = true;
}

presetOkEl.addEventListener('click', commitPreset);
presetNameEl.addEventListener('keydown', (e) => {
  if (e.key === 'Enter') commitPreset();
  if (e.key === 'Escape') {
    presetNameEl.hidden = true;
    presetOkEl.hidden = true;
  }
});

document.getElementById('btnPresetDel').addEventListener('click', async () => {
  const sel = document.getElementById('selPreset');
  if (!sel.value) return;
  await postPreset({ action: 'delete', name: sel.value });
});

// 总补偿
const mkEl = document.getElementById('makeup');
mkEl.addEventListener('input', (e) => {
  const v = parseFloat(e.target.value);
  document.getElementById('makeupVal').textContent = fmt(v);
  pushParams({ makeup_db: v });
});

// 块长控件：HTML 里已注释，这里做了存在性判断 —— 将来取消注释就能直接用
const blockEl = document.getElementById('block');
if (blockEl) {
  blockEl.addEventListener('change', (e) => {
    pushParams({ block: parseInt(e.target.value, 10) });
  });
}

// ---------------------------------------------------------------- HRIR 下拉

let hrirPicked = null;

function fillHrir() {
  const h = (state && state.hrir) || {};
  const files = h.files || [];
  const sel = document.getElementById('selHrir');
  const meta = document.getElementById('hrirMeta');
  if (!sel || !meta) return;

  const sig = files.join('|');
  if (sel.dataset.sig !== sig) {
    sel.dataset.sig = sig;
    const prev = sel.value;
    sel.innerHTML = '';
    if (files.length === 0) {
      const o = document.createElement('option');
      o.value = '';
      o.textContent = '（空 · 用内置合成模型）';
      sel.appendChild(o);
    } else {
      files.forEach((f) => {
        const o = document.createElement('option');
        o.value = f;
        o.textContent = f;
        sel.appendChild(o);
      });
    }
    if (prev && files.includes(prev)) sel.value = prev;
  }

  // 生效中的那个（没在跑的时候 h.active 为空，保持用户的选择）
  if (h.active && files.includes(h.active) && document.activeElement !== sel) {
    sel.value = h.active;
  }

  if (h.measured) {
    meta.textContent = state.running ? '实测数据 · 前后可分' : '已载入 · 点启动生效';
    meta.classList.remove('bad');
  } else if (files.length === 0) {
    meta.textContent = '内置合成模型（Woodworth）· 只有左右，前后分不清';
    meta.classList.add('bad');
  } else {
    meta.textContent = '已选但未生效 · 点启动后加载';
    meta.classList.remove('bad');
  }
}

async function restartWith(hrir) {
  await fetch('/api/stop', { method: 'POST' }).catch(() => {});
  await fetch('/api/start', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({
      source: document.getElementById('selSource').value || null,
      sink: document.getElementById('selSink').value || null,
      hrir,
    }),
  }).catch(() => {});
}

document.getElementById('selHrir').addEventListener('change', async () => {
  const v = document.getElementById('selHrir').value;
  if (!v) return;
  hrirPicked = v;
  if (state && state.running) await restartWith(v);
});

// ---------------------------------------------------------------- 启动 / 停止

document.getElementById('btnRun').addEventListener('click', async () => {
  const btn = document.getElementById('btnRun');
  if (state && state.running) {
    btn.disabled = true;
    await fetch('/api/stop', { method: 'POST' }).catch(() => {});
    btn.disabled = false;
    return;
  }
  btn.disabled = true;
  btn.textContent = '启动中…';
  const r = await fetch('/api/start', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({
      source: document.getElementById('selSource').value || null,
      sink: document.getElementById('selSink').value || null,
      hrir: hrirPicked || document.getElementById('selHrir').value || null,
    }),
  })
    .then((x) => x.json())
    .catch((e) => ({ ok: false, error: String(e) }));
  btn.disabled = false;
  const msg = r.error || r.warning;
  if (msg) showWarn(msg);
});

// ---------------------------------------------------------------- 耳机 EQ
//
// 这里的每个频点是一条 biquad，参数和 Rust 侧 `dsp/surround/eq.rs` 必须一模一样：
// 界面画的曲线是**算**出来的，不是采样回来的。两边公式一旦分叉，用户看到的和听到的就是两回事。

const EQ_MIN_FREQ = 20;
const EQ_MAX_FREQ = 20000;
const EQ_MIN_Q = 0.1;
const EQ_MAX_Q = 18;
const EQ_MAX_DB = 12;
// 上限以后端 `eq::MAX_BANDS` 为准，这里只是首帧（state 还没到）的占位
let EQ_MAX_BANDS = 16;
const EQ_DEFAULT_FREQS = [31.5, 63, 125, 250, 500, 1000, 2000, 4000, 8000, 16000];

const EQ_KINDS = [
  { v: 'peak', cn: '峰值 Peak', cls: 'peak', gain: true },
  { v: 'lowshelf', cn: '低架 LowShelf', cls: 'shelf', gain: true },
  { v: 'highshelf', cn: '高架 HighShelf', cls: 'shelf', gain: true },
  { v: 'highpass', cn: '高通 HighPass', cls: 'cut', gain: false },
  { v: 'lowpass', cn: '低通 LowPass', cls: 'cut', gain: false },
];
const eqKind = (t) => EQ_KINDS.find((k) => k.v === t) || EQ_KINDS[0];

/** 数字 → 短字符串：去掉二进制浮点留下的 1.4099999 这种尾巴。 */
const eqNum = (v, digits) => String(Number(Number(v).toFixed(digits)));

// 曲线几何：viewBox 固定 1100×300，等比缩到容器宽度
const EQ_W = 1100;
const EQ_H = 300;
const EQ_L = 44;
const EQ_R = 14;
const EQ_T = 16;
const EQ_B = 26;
const EQ_PW = EQ_W - EQ_L - EQ_R;
const EQ_PH = EQ_H - EQ_T - EQ_B;
const EQ_DB_RANGE = 15;

const SR = 48000;

const eqF2X = (hz) => {
  const lo = Math.log10(EQ_MIN_FREQ);
  const hi = Math.log10(EQ_MAX_FREQ);
  const f = Math.min(EQ_MAX_FREQ, Math.max(EQ_MIN_FREQ, hz));
  return EQ_L + ((Math.log10(f) - lo) / (hi - lo)) * EQ_PW;
};
const eqX2F = (x) => {
  const lo = Math.log10(EQ_MIN_FREQ);
  const hi = Math.log10(EQ_MAX_FREQ);
  const t = Math.min(1, Math.max(0, (x - EQ_L) / EQ_PW));
  return Math.pow(10, lo + t * (hi - lo));
};
const eqDb2Y = (db) => EQ_T + (EQ_PH / 2) * (1 - db / EQ_DB_RANGE);
const eqY2Db = (y) => ((EQ_T + EQ_PH / 2 - y) / (EQ_PH / 2)) * EQ_DB_RANGE;

let eqBands = [];
let eqEditAt = 0;      // 最近一次本地编辑（用来压制远端覆盖）
let eqRowSig = '';     // 表格结构签名（只有数量变了才重建 DOM）
let eqDrawSig = '';    // 曲线签名（没变就不重画）
let eqDrag = -1;       // 正在拖的频点下标

const eqCanon = (b) =>
  [b.type, Number(b.freq).toFixed(1), Number(b.q).toFixed(3), Number(b.gain).toFixed(2)].join(':');

/** RBJ Audio EQ Cookbook —— 与 `eq.rs` 的 `Biquad::compute` 逐行对应。 */
function eqBiquad(b) {
  const f0 = Math.min(EQ_MAX_FREQ, Math.max(EQ_MIN_FREQ, b.freq), SR * 0.49);
  const w0 = (2 * Math.PI * f0) / SR;
  const cs = Math.cos(w0);
  const sn = Math.sin(w0);
  const q = Math.min(EQ_MAX_Q, Math.max(EQ_MIN_Q, b.q));
  const alpha = sn / (2 * q);
  const A = Math.pow(10, b.gain / 40);
  const shelfQ = Math.min(4, Math.max(0.3, q));
  const k = (2 * Math.sqrt(A) * sn) / (2 * shelfQ);
  let b0, b1, b2, a0, a1, a2;

  switch (b.type) {
    case 'lowshelf':
      b0 = A * (A + 1 - (A - 1) * cs + k);
      b1 = 2 * A * (A - 1 - (A + 1) * cs);
      b2 = A * (A + 1 - (A - 1) * cs - k);
      a0 = A + 1 + (A - 1) * cs + k;
      a1 = -2 * (A - 1 + (A + 1) * cs);
      a2 = A + 1 + (A - 1) * cs - k;
      break;
    case 'highshelf':
      b0 = A * (A + 1 + (A - 1) * cs + k);
      b1 = -2 * A * (A - 1 + (A + 1) * cs);
      b2 = A * (A + 1 + (A - 1) * cs - k);
      a0 = A + 1 - (A - 1) * cs + k;
      a1 = 2 * (A - 1 - (A + 1) * cs);
      a2 = A + 1 - (A - 1) * cs - k;
      break;
    case 'lowpass':
      b0 = (1 - cs) / 2;
      b1 = 1 - cs;
      b2 = (1 - cs) / 2;
      a0 = 1 + alpha;
      a1 = -2 * cs;
      a2 = 1 - alpha;
      break;
    case 'highpass':
      b0 = (1 + cs) / 2;
      b1 = -(1 + cs);
      b2 = (1 + cs) / 2;
      a0 = 1 + alpha;
      a1 = -2 * cs;
      a2 = 1 - alpha;
      break;
    default:
      b0 = 1 + alpha * A;
      b1 = -2 * cs;
      b2 = 1 - alpha * A;
      a0 = 1 + alpha / A;
      a1 = -2 * cs;
      a2 = 1 - alpha / A;
  }
  const inv = Math.abs(a0) < 1e-12 ? 0 : 1 / a0;
  return { b0: b0 * inv, b1: b1 * inv, b2: b2 * inv, a1: a1 * inv, a2: a2 * inv };
}

/** 二阶节在 hz 处的幅度（dB）。系数已按 a0 归一化。 */
function eqMagDb(c, hz) {
  const w = (2 * Math.PI * hz) / SR;
  const cw = Math.cos(w);
  const sw = Math.sin(w);
  const c2 = Math.cos(2 * w);
  const s2 = Math.sin(2 * w);
  const nr = c.b0 + c.b1 * cw + c.b2 * c2;
  const ni = -(c.b1 * sw + c.b2 * s2);
  const dr = 1 + c.a1 * cw + c.a2 * c2;
  const di = -(c.a1 * sw + c.a2 * s2);
  const num = nr * nr + ni * ni;
  const den = dr * dr + di * di;
  return 10 * Math.log10(num / Math.max(den, 1e-20));
}

/** 整条链在 hz 处的总响应。 */
function eqResponse(hz) {
  let sum = 0;
  for (const b of eqBands) sum += eqMagDb(eqBiquad(b), hz);
  return sum;
}

// ---------------------------------------------------------------- 曲线

function drawEqChart(force) {
  const svg = document.getElementById('eqChart');
  if (!svg) return;
  const sig =
    eqBands.map(eqCanon).join('|') +
    '#' +
    eqDrag +
    '#' +
    (document.documentElement.dataset.theme || '');
  if (!force && sig === eqDrawSig) return;
  eqDrawSig = sig;

  let g = '';

  // 竖向网格（频率）
  const freqs = [20, 50, 100, 200, 500, 1000, 2000, 5000, 10000, 20000];
  const fLabel = { 20: '20', 50: '50', 100: '100', 200: '200', 500: '500', 1000: '1k' };
  for (const f of freqs) {
    const x = eqF2X(f).toFixed(1);
    g += `<line class="egrid" x1="${x}" y1="${EQ_T}" x2="${x}" y2="${EQ_T + EQ_PH}"/>`;
    const tx = f >= 1000 ? f / 1000 + 'k' : fLabel[f] || f;
    g += `<text class="etext" x="${x}" y="${EQ_T + EQ_PH + 16}" text-anchor="middle">${tx}</text>`;
  }

  // 横向网格（dB）
  for (const db of [-12, -6, 0, 6, 12]) {
    const y = eqDb2Y(db).toFixed(1);
    const zero = db === 0 ? ' zero' : '';
    g += `<line class="egrid${zero}" x1="${EQ_L}" y1="${y}" x2="${EQ_L + EQ_PW}" y2="${y}"/>`;
    const t = (db > 0 ? '+' : '') + db;
    g += `<text class="etext axis" x="${EQ_L - 8}" y="${Number(y) + 4}" text-anchor="end">${t}</text>`;
  }
  g += `<text class="etext axis" x="${EQ_L - 8}" y="${EQ_T + 10}" text-anchor="end">dB</text>`;

  // 曲线
  const N = 320;
  const pts = [];
  let mx = -99;
  let mn = 99;
  for (let i = 0; i <= N; i++) {
    const x = EQ_L + (EQ_PW * i) / N;
    const hz = eqX2F(x);
    // 超过 Nyquist 的部分不画：48 kHz 下 24 k 以上没有意义，曲线在那里会乱跳
    const db = hz >= SR / 2 ? null : eqResponse(hz);
    if (db !== null) {
      mx = Math.max(mx, db);
      mn = Math.min(mn, db);
      pts.push([x, eqDb2Y(Math.max(-EQ_DB_RANGE, Math.min(EQ_DB_RANGE, db)))]);
    }
  }
  if (pts.length) {
    const line = pts.map(([x, y]) => `${x.toFixed(1)},${y.toFixed(1)}`).join(' ');
    const base = eqDb2Y(0).toFixed(1);
    g += `<polygon class="efill" points="${EQ_L},${base} ${line} ${(
      EQ_L + EQ_PW
    ).toFixed(1)},${base}"/>`;
    g += `<polyline class="ecurve" points="${line}"/>`;
  }

  // 空白处的双击热区要画在**圆点下面**：SVG 没有 z-index，后画的在上。
  // 之前它画在最后，把所有圆点盖住了 —— 点哪儿都命中矩形，拖不动。
  g += `<rect class="ehit" x="${EQ_L}" y="${EQ_T}" width="${EQ_PW}" height="${EQ_PH}"/>`;

  // 频点标记：位置上就是这个频点的真实响应点（架式在半坡、高通在拐点），
  // 拖动时手指和曲线对得上，不用心算。
  eqBands.forEach((b, i) => {
    const x = eqF2X(b.freq);
    const db = eqResponse(b.freq);
    const y = eqDb2Y(Math.max(-EQ_DB_RANGE, Math.min(EQ_DB_RANGE, db)));
    const k = eqKind(b.type);
    const sel = i === eqDrag ? ' sel' : '';
    g += `<circle class="edot ${k.cls}${sel}" data-i="${i}" cx="${x.toFixed(1)}"
      cy="${y.toFixed(1)}" r="9"/>`;
  });

  svg.innerHTML = `<defs><linearGradient id="eqGrad" x1="0" y1="0" x2="0" y2="1">
      <stop class="eg0" offset="0%"/>
      <stop class="eg1" offset="100%"/>
    </linearGradient></defs>` + g;

  const sum = document.getElementById('eqSum');
  if (sum) {
    const flat = eqBands.every((b) => eqKind(b.type).gain && Math.abs(b.gain) < 1e-4);
    sum.textContent = flat
      ? '平直 · 直通'
      : `峰值 ${mx > 0 ? '+' : ''}${mx.toFixed(1)} dB · 谷值 ${mn.toFixed(1)} dB`;
  }
}

// ---------------------------------------------------------------- 频点表

function ensureEqRows() {
  const host = document.getElementById('eqRows');
  if (!host) return;
  const sig = String(eqBands.length);
  if (sig !== eqRowSig) {
    eqRowSig = sig;
    host.innerHTML = '';
    eqBands.forEach((b, i) => host.appendChild(eqRowEl(i)));
  }
  document.getElementById('eqEmpty').classList.toggle('hidden', eqBands.length > 0);
  syncEqValues();
}

function eqRowEl(i) {
  const row = document.createElement('div');
  row.className = 'eqBandRow eqRow';

  const idx = document.createElement('span');
  idx.className = 'c-idx';
  idx.textContent = String(i + 1);

  const sel = document.createElement('select');
  sel.className = 'c-kind';
  EQ_KINDS.forEach((k) => {
    const o = document.createElement('option');
    o.value = k.v;
    o.textContent = k.cn;
    sel.appendChild(o);
  });

  const freq = document.createElement('input');
  freq.className = 'c-freq';
  freq.type = 'number';
  freq.min = EQ_MIN_FREQ;
  freq.max = EQ_MAX_FREQ;
  freq.step = 'any';
  freq.title = '中心/拐点频率（20–20000 Hz）';

  const q = document.createElement('input');
  q.className = 'c-q';
  q.type = 'number';
  q.min = EQ_MIN_Q;
  q.max = EQ_MAX_Q;
  q.step = 'any';
  q.title = 'Q 值（0.1–18）：越大越窄';

  const gainBox = document.createElement('div');
  gainBox.className = 'c-gain';
  const gr = document.createElement('input');
  gr.type = 'range';
  gr.min = -EQ_MAX_DB;
  gr.max = EQ_MAX_DB;
  gr.step = 0.5;
  gr.className = 'gR';
  const gn = document.createElement('input');
  gn.type = 'number';
  gn.className = 'gN';
  gn.min = -EQ_MAX_DB;
  gn.max = EQ_MAX_DB;
  gn.step = 0.5;
  gn.title = '增益（dB），可直接输入';
  gainBox.append(gr, gn);

  const del = document.createElement('button');
  del.className = 'delBtn';
  del.textContent = '×';
  del.title = '删除这个频点';

  // ---- 事件：改哪个字段就只改哪个字段，其余保持 ----
  sel.addEventListener('change', () => {
    eqBands[i].type = sel.value;
    // 高通/低通不吃增益：控件置灰（值保留，切回峰/架式还在）。
    // 不禁用的话用户会拖着滑条纳闷为什么曲线不动。
    syncEqValues();
    eqCommit();
  });

  const commitNum = (el, key, lo, hi, digits) => {
    const v = parseFloat(el.value);
    if (!Number.isFinite(v)) {
      el.value = eqNum(eqBands[i][key], digits);
      return;
    }
    eqBands[i][key] = Math.min(hi, Math.max(lo, Number(v.toFixed(digits))));
    el.value = eqNum(eqBands[i][key], digits);
    eqCommit();
  };
  freq.addEventListener('change', () => commitNum(freq, 'freq', EQ_MIN_FREQ, EQ_MAX_FREQ, 1));
  q.addEventListener('change', () => commitNum(q, 'q', EQ_MIN_Q, EQ_MAX_Q, 2));
  gn.addEventListener('change', () => commitNum(gn, 'gain', -EQ_MAX_DB, EQ_MAX_DB, 1));

  // 滑条：拖动过程实时下发（40 ms 合并），松手不回弹
  gr.addEventListener('input', () => {
    eqBands[i].gain = parseFloat(gr.value);
    gn.value = eqNum(eqBands[i].gain, 1);
    eqCommit(false);
  });

  del.addEventListener('click', () => {
    eqBands.splice(i, 1);
    eqRowSig = '';
    eqCommit();
  });

  row.append(idx, sel, freq, q, gainBox, del);
  return row;
}

/** 把 eqBands 写回控件。正在输入的框不覆盖，否则打字会被打断。 */
function syncEqValues() {
  document.querySelectorAll('#eqRows .eqBandRow').forEach((row, i) => {
    const b = eqBands[i];
    if (!b) return;
    const act = document.activeElement;
    const sel = row.querySelector('.c-kind');
    if (act !== sel) sel.value = b.type;
    const freq = row.querySelector('.c-freq');
    if (act !== freq) freq.value = eqNum(b.freq, 1);
    const q = row.querySelector('.c-q');
    if (act !== q) q.value = eqNum(b.q, 2);
    const usesGain = eqKind(b.type).gain;
    const gr = row.querySelector('.gR');
    const gn = row.querySelector('.gN');
    gr.disabled = !usesGain;
    gn.disabled = !usesGain;
    if (act !== gr) gr.value = b.gain;
    if (act !== gn) gn.value = eqNum(b.gain, 1);
    row.classList.toggle('sel', i === eqDrag);
  });
}

/** 提交一次改动：立即重画 + 合并下发。 */
function eqCommit(redraw = true) {
  eqEditAt = Date.now();
  if (redraw) drawEqChart();
  pushParams({
    eq_bands: eqBands.map((b) => ({
      type: b.type,
      freq: b.freq,
      q: b.q,
      gain: b.gain,
    })),
  });
}

function addEqBand(freq) {
  if (eqBands.length >= EQ_MAX_BANDS) {
    showWarn(`最多 ${EQ_MAX_BANDS} 个频点。再多曲线会叠到没法调，先删几个。`);
    return;
  }
  eqBands.push({
    type: 'peak',
    freq: Math.min(EQ_MAX_FREQ, Math.max(EQ_MIN_FREQ, Math.round(freq))),
    q: 1.41,
    gain: 0,
  });
  eqRowSig = '';
  ensureEqRows();
  eqCommit();
  // 新增的点在末行，滚过去，不然用户以为没加上
  const host = document.getElementById('eqRows');
  host.scrollTop = host.scrollHeight;
}

// ---------------------------------------------------------------- 曲线交互

(function bindEqChart() {
  const svg = document.getElementById('eqChart');
  if (!svg) return;

  const toVB = (e) => {
    const r = svg.getBoundingClientRect();
    return {
      x: ((e.clientX - r.left) / r.width) * EQ_W,
      y: ((e.clientY - r.top) / r.height) * EQ_H,
    };
  };

  svg.addEventListener('pointerdown', (e) => {
    const dot = e.target.closest ? e.target.closest('.edot') : null;
    if (!dot) {
      if (eqDrag >= 0) {
        eqDrag = -1;
        syncEqValues();
        drawEqChart();
      }
      return;
    }
    eqDrag = Number(dot.dataset.i);
    syncEqValues();
    drawEqChart();
    if (svg.setPointerCapture) svg.setPointerCapture(e.pointerId);
    e.preventDefault();
  });

  svg.addEventListener('pointermove', (e) => {
    if (eqDrag < 0) return;
    const b = eqBands[eqDrag];
    if (!b) {
      eqDrag = -1;
      return;
    }
    const p = toVB(e);
    const hz = eqX2F(p.x);
    // 频率按 1 Hz 取整；10 Hz 以下保留小数点没有意义，直接四舍五入
    b.freq = Math.min(EQ_MAX_FREQ, Math.max(EQ_MIN_FREQ, Math.round(hz)));
    if (eqKind(b.type).gain) {
      const db = eqY2Db(p.y);
      b.gain = Math.min(EQ_MAX_DB, Math.max(-EQ_MAX_DB, Math.round(db * 2) / 2));
    }
    syncEqValues();
    drawEqChart();
    eqCommit(false);
    e.preventDefault();
  });

  const stop = () => {
    if (eqDrag < 0) return;
    eqDrag = -1;
    syncEqValues();
    drawEqChart();
  };
  svg.addEventListener('pointerup', stop);
  svg.addEventListener('pointercancel', stop);

  // 空白处双击加频点：在什么频率上双击就加在什么频率，不用先去表里改数字
  svg.addEventListener('dblclick', (e) => {
    if (e.target.closest && e.target.closest('.edot')) return;
    const p = toVB(e);
    if (p.x < EQ_L || p.x > EQ_L + EQ_PW || p.y < EQ_T || p.y > EQ_T + EQ_PH) return;
    addEqBand(eqX2F(p.x));
  });
})();

document.getElementById('btnEqAdd').addEventListener('click', () => {
  // 默认加在曲线最高点的位置没有意义；给一个不容易打架的中频
  addEqBand(1000);
});

document.getElementById('btnEqFlat').addEventListener('click', () => {
  eqBands.forEach((b) => {
    b.gain = 0;
  });
  syncEqValues();
  eqCommit();
});

document.getElementById('btnEqReset').addEventListener('click', () => {
  eqBands = EQ_DEFAULT_FREQS.map((f) => ({ type: 'peak', freq: f, q: 1.41, gain: 0 }));
  eqRowSig = '';
  ensureEqRows();
  eqCommit();
});

document.getElementById('eqOn').addEventListener('change', (e) => {
  eqEditAt = Date.now();
  pushParams({ eq_on: e.target.checked });
});

/** 每帧（120 ms）调用：把后端状态同步进来，但别覆盖用户正在改的东西。 */
function renderEq() {
  if (!state || !state.params) return;
  const chk = document.getElementById('eqOn');
  const on = state.params.eq_on !== false;
  if (document.activeElement !== chk) chk.checked = on;

  if (state.params.eq_max) EQ_MAX_BANDS = state.params.eq_max;

  const remote = state.params.eq_bands || [];
  // 本地刚改过就先不听远端的 —— 请求还在路上时回冲会让滑块"弹回去"
  if (Date.now() - eqEditAt > 900) {
    const a = remote.map(eqCanon).join('|');
    const b = eqBands.map(eqCanon).join('|');
    if (a !== b) {
      eqBands = remote.map((x) => ({
        type: x.type,
        freq: x.freq,
        q: x.q,
        gain: x.gain,
      }));
      eqRowSig = '';
    }
  }
  ensureEqRows();
  drawEqChart();
  // 总开关关掉时压暗曲线：参数还在，但耳朵听不到，不压暗会让人以为调了没反应
  const chart = document.getElementById('eqChart');
  if (chart) chart.classList.toggle('off', !on);
}

// ---------------------------------------------------------------- VB-CABLE 前置

function updateVb() {
  const vb = (state && state.vbcable) || {};
  const card = document.getElementById('vbCard');
  if (!card) return;
  const need = !vb.installed || !vb.surround;
  card.classList.toggle('hidden', !need);
  if (!need) return;

  const btn = document.getElementById('btnVbInstall');
  const title = card.querySelector('.vbBody > b');
  const text = card.querySelector('.vbBody > p');
  if (!vb.installed) {
    btn.textContent = '一键安装 VB-CABLE';
    btn.dataset.mode = 'install';
    title.textContent = '还差一步：VB-CABLE 虚拟声卡';
    text.innerHTML =
      '「单独调每个方向」的前提是游戏输出 <b>8 条独立声道</b>。没有虚拟 7.1 端点时，' +
      '系统只会给一条已经混好的立体声，8 个推子就退化成 1 个总音量。' +
      '装完还要在「声音设置 → 该设备 → 配置扬声器」里选 <b>7.1 环绕</b> —— 默认是立体声。';
  } else {
    btn.textContent = '打开声音设置';
    btn.dataset.mode = 'guide';
    title.textContent = 'VB-CABLE 已装，但还没配成 7.1';
    text.innerHTML =
      '现在已经能选到 CABLE Input，但它还是 <b>2 声道</b>。' +
      '去「声音设置 → 声音控制面板 → CABLE Input → 配置扬声器」选 <b>7.1 环绕</b>，' +
      '回来点「刷新设备」即可。不改这一步，游戏照样只输出 2 声道。';
  }
}

document.getElementById('btnVbInstall').addEventListener('click', async (e) => {
  const btn = e.currentTarget;
  if (btn.dataset.mode === 'guide') {
    fetch('/api/setup-guide', { method: 'POST' }).catch(() => {});
    return;
  }
  btn.disabled = true;
  const old = btn.textContent;
  btn.textContent = '正在启动安装…';
  const r = await fetch('/api/vbcable/install', { method: 'POST' })
    .then((x) => x.json())
    .catch(() => ({ ok: false, error: '请求失败' }));
  btn.disabled = false;
  btn.textContent = old;
  if (r.ok) {
    showWarn(
      '已启动 VB-CABLE 安装程序：在弹出的窗口里点 Install（需要允许管理员权限），' +
        '装完重启电脑，回到本页点「刷新设备」，再把端点配成 7.1。'
    );
  } else {
    showWarn(r.error || '安装程序启动失败');
  }
  updateVb();
});

for (const id of ['btnVbGuide', 'btnGuide']) {
  document.getElementById(id).addEventListener('click', () => {
    fetch('/api/setup-guide', { method: 'POST' }).catch(() => {});
  });
}

// ---------------------------------------------------------------- 标签页切换

const LS_TAB = 'gamingvoice.tab';

function switchTab(name) {
  const id = name === 'eq' ? 'pageEq' : 'pageMix';
  document.querySelectorAll('.tab').forEach((b) => {
    const on = b.dataset.tab === name;
    b.classList.toggle('on', on);
    b.setAttribute('aria-selected', on ? 'true' : 'false');
  });
  document.querySelectorAll('.tabPage').forEach((p) => {
    p.classList.toggle('hidden', p.id !== id);
  });
  try {
    localStorage.setItem(LS_TAB, name);
  } catch (_) {}
  // 切回来时按当前主题重算一遍：隐藏期间 SVG 的 getBoundingClientRect 全是 0，
  // 有些尺寸相关的量要重新取。
  if (name === 'eq') drawEqChart(true);
  else drawRadar();
}

document.querySelectorAll('.tab').forEach((b) => {
  b.addEventListener('click', () => switchTab(b.dataset.tab));
});

(function initTab() {
  let t = 'mix';
  try {
    t = localStorage.getItem(LS_TAB) || 'mix';
  } catch (_) {}
  switchTab(t === 'eq' ? 'eq' : 'mix');
})();

drawRadar();
poll();
