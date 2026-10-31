'use strict';

// 通道顺序与 Rust 侧 `Speaker::ALL` 严格一致：下标就是 WASAPI 的声道下标。
const CH = [
  { id: 'FL',  name: '前左', az: -30 },
  { id: 'FR',  name: '前右', az:  30 },
  { id: 'FC',  name: '中置', az:   0 },
  { id: 'LFE', name: '低频', az: null },
  { id: 'BL',  name: '后左', az: -140 },
  { id: 'BR',  name: '后右', az:  140 },
  { id: 'SL',  name: '侧左', az: -90 },
  { id: 'SR',  name: '侧右', az:  90 },
];

const DB_MIN = -30, DB_MAX = 12;
const MUTE_DB = -60;

let state = null;
let savedGain = new Array(8).fill(0);   // mute 前的值
let display = new Array(8).fill(0);     // 电平条（带衰减）
let userPickedDevice = false;
let sendTimer = null;

// ---------------------------------------------------------------- 初始化通道行

const list = document.getElementById('chList');
CH.forEach((c, i) => {
  const row = document.createElement('div');
  row.className = 'ch';
  row.dataset.i = i;
  row.innerHTML = `
    <div class="nm">${c.id}<em>${c.name}</em></div>
    <div class="slider">
      <input type="range" min="${DB_MIN}" max="${DB_MAX}" step="0.5" value="0" aria-label="${c.id} 增益">
      <div class="bar"><div class="fill"></div></div>
    </div>
    <div class="right">
      <div class="db">0.0</div>
      <button class="mute" title="静音 ${c.id}">静音</button>
    </div>`;
  row.querySelector('.mute').addEventListener('click', () => toggleMute(i, row));
  row.querySelector('input').addEventListener('input', (e) => {
    const v = parseFloat(e.target.value);
    row.querySelector('.db').textContent = v.toFixed(1);
    row.classList.remove('muted');
    pushParams({ gains_db: collectGains() });
    drawRadar();
  });
  list.appendChild(row);
});

function collectGains() {
  return [...document.querySelectorAll('.ch input')].map(e => {
    const row = e.closest('.ch');
    return row.classList.contains('muted') ? MUTE_DB : parseFloat(e.value);
  });
}

function toggleMute(i, row) {
  const input = row.querySelector('input');
  if (row.classList.contains('muted')) {
    row.classList.remove('muted');
    input.value = savedGain[i];
  } else {
    savedGain[i] = parseFloat(input.value);
    row.classList.add('muted');
    input.value = DB_MIN;
  }
  row.querySelector('.db').textContent = row.classList.contains('muted')
    ? '静音' : parseFloat(input.value).toFixed(1);
  pushParams({ gains_db: collectGains() });
  drawRadar();
}

// ---------------------------------------------------------------- 与后端通信

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
  } catch (_) { /* 后端关了也不刷屏 */ }
  setTimeout(poll, 100);
}

function render() {
  const s = state;
  document.getElementById('led').className = 'led ' + (s.running ? (s.error ? 'bad' : 'on') : '');
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
    const four = (s.endpoints || []).filter(e => e.surround);
    if (!s.running && four.length === 0) {
      warn.innerHTML = '<b>没有检测到 8 声道端点。</b>\n' +
        '游戏现在只能输出立体声，"分方向音量"没有可分的对象 —— 这不是软件问题。\n' +
        '装一个虚拟 7.1 端点（VB-CABLE，免费），并且必须在「声音设置 → 该设备 → 配置扬声器」里改成 <b>7.1 环绕</b>，' +
        '装完默认是立体声，不改这一步游戏照样只输出 2 声道。';
      warn.classList.remove('hidden');
    } else {
      warn.classList.add('hidden');
    }
  }

  // 设备下拉（只在第一次或端点变化时重建，避免打断用户选择）
  fillSelect('selSource', s.endpoints, true);
  fillSelect('selSink', s.endpoints, false);

  // 状态数字
  document.getElementById('kBlock').textContent = s.status.block_ms.toFixed(1) + ' ms';
  document.getElementById('kBuf').textContent = s.status.buffer_ms.toFixed(0) + ' ms';
  document.getElementById('kXrun').textContent = s.status.underruns;
  document.getElementById('makeupVal').textContent = (s.params.makeup_db >= 0 ? '+' : '') + s.params.makeup_db.toFixed(1);
  document.getElementById('makeup').value = s.params.makeup_db;
  document.getElementById('block').value = s.params.block;

  // 电平与数值（只在没在拖的时候同步，否则会跟手指打架）
  if (!dragging) {
    document.querySelectorAll('.ch').forEach((row, i) => {
      const input = row.querySelector('input');
      const db = s.params.gains_db[i];
      if (db <= MUTE_DB + 1) {
        row.classList.add('muted');
        input.value = DB_MIN;
        row.querySelector('.db').textContent = '静音';
      } else {
        row.classList.remove('muted');
        input.value = Math.min(DB_MAX, Math.max(DB_MIN, db));
        row.querySelector('.db').textContent = db.toFixed(1);
      }
      document.getElementById('makeup').value = s.params.makeup_db;
    });
  }

  // 电平条（带 0.82 的衰减，峰值才不会一闪而过）
  document.querySelectorAll('.ch').forEach((row, i) => {
    const peak = Math.max(s.levels.out_peak[i], s.levels.in_peak[i] * 0);
    const db = 20 * Math.log10(Math.max(peak, 1e-6));
    const pct = Math.max(0, Math.min(100, (db + 60) / 60 * 100));
    display[i] = Math.max(pct, display[i] * 0.82);
    const fill = row.querySelector('.fill');
    fill.style.width = display[i].toFixed(1) + '%';
    fill.className = 'fill' + (db > -3 ? ' over' : db > -12 ? ' hot' : '');
  });

  drawRadar();
}

let dragging = false;
document.addEventListener('pointerdown', e => { if (e.target.type === 'range') dragging = true; });
document.addEventListener('pointerup', () => { dragging = false; });

function fillSelect(id, eps, isSource) {
  const sel = document.getElementById(id);
  if (!eps || eps.length === 0) return;
  const sig = eps.map(e => e.id + e.channels).join('|');
  if (sel.dataset.sig === sig) return;      // 没变就不动 DOM
  sel.dataset.sig = sig;
  sel.innerHTML = '';
  eps.forEach(e => {
    const o = document.createElement('option');
    o.value = e.id;
    o.textContent = (e.surround ? '★ ' : '') + e.label + (e.default ? '  [默认]' : '');
    sel.appendChild(o);
  });
  // 首次：源优先挑 8 声道，输出优先挑非 8 声道（耳机）
  const pick = isSource
    ? eps.find(e => e.surround) || eps[0]
    : eps.find(e => !e.surround) || eps[0];
  if (!userPickedDevice) sel.value = pick.id;
}

document.getElementById('btnRefresh').addEventListener('click', async () => {
  const b = document.getElementById('btnRefresh');
  b.disabled = true;
  await fetch('/api/refresh', { method: 'POST' }).catch(() => {});
  // 让下一次 state 把下拉重建（清掉 sig，强制重绘）
  document.getElementById('selSource').dataset.sig = '';
  document.getElementById('selSink').dataset.sig = '';
  userPickedDevice = false;
  b.disabled = false;
});

document.getElementById('selSource').addEventListener('change', () => { userPickedDevice = true; });
document.getElementById('selSink').addEventListener('change', () => { userPickedDevice = true; });

// ---------------------------------------------------------------- 雷达

const AZ = CH.map(c => c.az);
function drawRadar() {
  const svg = document.getElementById('radar');
  const cx = 150, cy = 152, R = 108;
  const gains = state ? state.params.gains_db : new Array(8).fill(0);

  let g = `<defs>
    <radialGradient id="glow" cx="50%" cy="50%" r="50%">
      <stop offset="0%" stop-color="#34d399" stop-opacity=".22"/>
      <stop offset="100%" stop-color="#34d399" stop-opacity="0"/>
    </radialGradient>
  </defs>`;
  g += `<circle cx="${cx}" cy="${cy}" r="${R}" fill="url(#glow)"/>`;

  // 同心圆 + 十字
  [0.33, 0.66, 1].forEach(k => {
    g += `<circle cx="${cx}" cy="${cy}" r="${(R * k).toFixed(1)}" fill="none" stroke="#2a3a4d" stroke-width="1"/>`;
  });
  g += `<line x1="${cx}" y1="${cy - R - 10}" x2="${cx}" y2="${cy + R + 10}" stroke="#243141" stroke-width="1"/>`;
  g += `<line x1="${cx - R - 10}" y1="${cy}" x2="${cx + R + 10}" y2="${cy}" stroke="#243141" stroke-width="1"/>`;

  // 方位标签
  g += `<text x="${cx}" y="${cy - R - 16}" fill="#5b6a7e" font-size="10" text-anchor="middle" font-family="monospace">前</text>`;
  g += `<text x="${cx}" y="${cy + R + 26}" fill="#5b6a7e" font-size="10" text-anchor="middle" font-family="monospace">后</text>`;

  // LFE 在正下方单独标一个点（无方向）
  const pts = [];
  CH.forEach((c, i) => {
    const db = gains[i];
    // -30dB → 0.18R，+12dB → 1R
    const k = Math.max(0.18, Math.min(1, (db + 30) / 42));
    const r = R * k;
    let x, y;
    if (c.az === null) {
      x = cx; y = cy + R * 0.55;
    } else {
      const rad = c.az * Math.PI / 180;
      x = cx + r * Math.sin(rad);
      y = cy - r * Math.cos(rad);
    }
    const muted = db <= MUTE_DB + 1;
    pts.push({ x, y, c, db, muted, k });
  });

  // 先画连线再画点，点才盖得住
  pts.forEach(p => {
    if (p.c.az === null) return;
    g += `<line x1="${cx}" y1="${cy}" x2="${p.x.toFixed(1)}" y2="${p.y.toFixed(1)}"
      stroke="${p.muted ? '#33404f' : '#2f7f66'}" stroke-width="1.5" stroke-dasharray="${p.muted ? '2 3' : '0'}"/>`;
  });
  pts.forEach(p => {
    const col = p.muted ? '#3a4757' : (p.db > 3 ? '#fbbf24' : '#34d399');
    g += `<circle cx="${p.x.toFixed(1)}" cy="${p.y.toFixed(1)}" r="${p.c.az === null ? 4 : 5}"
      fill="${col}" opacity="${p.muted ? 0.45 : 0.95}"
      stroke="#0a0f16" stroke-width="1.5"/>`;
    g += `<text x="${p.x.toFixed(1)}" y="${(p.y + (p.c.az === null ? 16 : -9)).toFixed(1)}"
      fill="${p.muted ? '#4a5768' : '#9fb2c8'}" font-size="9.5" text-anchor="middle"
      font-family="monospace">${p.c.id}</text>`;
  });

  // 中心
  g += `<circle cx="${cx}" cy="${cy}" r="3" fill="#5b6a7e"/>`;
  svg.innerHTML = g;
}

// ---------------------------------------------------------------- 预设

const PRESETS = {
  flat:     [0, 0, 0, 0, 0, 0, 0, 0],
  tac:      [-2, -2, -6, 0, 8, 8, 2, 2],     // 压前方与中置，抬后方：脚步
  cinema:   [1, 1, 3, 4, 2, 2, 0, 0],        // 中置与 LFE 抬：对白与爆炸
  muteBack: [0, 0, -3, 0, MUTE_DB, MUTE_DB, MUTE_DB, MUTE_DB],
};
document.querySelectorAll('[data-preset]').forEach(b => {
  b.addEventListener('click', () => {
    const arr = PRESETS[b.dataset.preset];
    document.querySelectorAll('.ch').forEach((row, i) => {
      const input = row.querySelector('input');
      const db = arr[i];
      if (db <= MUTE_DB + 1) {
        row.classList.add('muted');
        input.value = DB_MIN;
        row.querySelector('.db').textContent = '静音';
      } else {
        row.classList.remove('muted');
        input.value = db;
        row.querySelector('.db').textContent = db.toFixed(1);
      }
      savedGain[i] = db <= MUTE_DB + 1 ? savedGain[i] : db;
    });
    pushParams({ gains_db: arr.map(v => v <= MUTE_DB + 1 ? MUTE_DB : v) });
    drawRadar();
  });
});

document.getElementById('makeup').addEventListener('input', (e) => {
  document.getElementById('makeupVal').textContent = (v => (v >= 0 ? '+' : '') + v.toFixed(1))(parseFloat(e.target.value));
  pushParams({ makeup_db: parseFloat(e.target.value) });
});
document.getElementById('block').addEventListener('change', (e) => {
  pushParams({ block: parseInt(e.target.value, 10) });
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
    }),
  }).then(x => x.json()).catch(e => ({ ok: false, error: String(e) }));
  btn.disabled = false;
  if (!r.ok) {
    const warn = document.getElementById('warn');
    warn.textContent = r.error || '启动失败';
    warn.classList.remove('hidden');
  }
});

drawRadar();
poll();
