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

function render() {
  const s = state;

  // 顶栏
  document.getElementById('led').className =
    'led ' + (s.running ? (s.error ? 'bad' : 'on') : '');
  document.getElementById('runText').textContent = s.running ? '运行中' : '未启动';
  const btn = document.getElementById('btnRun');
  btn.textContent = s.running ? '停止' : '启动';
  btn.classList.toggle('stop', !!s.running);

  // 警告
  const warn = document.getElementById('warn');
  if (s.error) {
    warn.textContent = s.error;
    warn.classList.remove('hidden');
  } else {
    const surround = (s.endpoints || []).filter((e) => e.surround);
    if (!s.running && surround.length === 0) {
      warn.innerHTML =
        '<b>没有检测到 8 声道端点，游戏现在只输出立体声。</b>\n' +
        '「分方向音量」需要 8 条独立声道才有可分的对象。装一个虚拟 7.1 端点（VB-CABLE，免费），' +
        '并且必须在「声音设置 → 该设备 → 配置扬声器」里改成 <b>7.1 环绕</b> —— ' +
        '装完默认是立体声，不改这一步游戏照样只输出 2 声道。';
      warn.classList.remove('hidden');
    } else {
      warn.classList.add('hidden');
    }
  }

  fillSelect('selSource', s.endpoints, true);
  fillSelect('selSink', s.endpoints, false);
  updateMeta();
  fillHrir();

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

// ---------------------------------------------------------------- 预设

const PRESETS = {
  flat:   [0, 0, 0, 0, 0, 0, 0, 0],
  // 压前方与中置、抬后方：脚步/换弹更突出（这是竞技最常用的一组）
  tac:    [-3, -3, -7, 0, 12, 12, 4, 4],
  // 中置与低频抬：对白与爆炸
  cinema: [1, 1, 4, 6, 2, 2, 0, 0],
  // 只听前方：后四路压到 -18，仍留一点方位感（不是彻底砍掉）
  front:  [2, 2, 0, 0, -18, -18, -18, -18],
  muteBack: [0, 0, -3, 0, MUTE_DB, MUTE_DB, MUTE_DB, MUTE_DB],
};

document.querySelectorAll('[data-preset]').forEach((b) => {
  b.addEventListener('click', () => {
    const arr = PRESETS[b.dataset.preset];
    document.querySelectorAll('.ch').forEach((row, i) => {
      const input = row.querySelector('input');
      const db = arr[i];
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
        input.value = db;
        row.querySelector('.db').textContent = fmt(db);
      }
      if (db > MUTE_DB + 1) savedGain[i] = db;
    });
    pushParams({ gains_db: arr.map((v) => (v <= MUTE_DB + 1 ? MUTE_DB : v)) });
    drawRadar();
  });
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
  const warn = document.getElementById('warn');
  const msg = r.error || r.warning;
  if (msg) {
    warn.textContent = msg;
    warn.classList.remove('hidden');
  }
});

drawRadar();
poll();
