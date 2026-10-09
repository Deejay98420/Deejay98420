/* PandaBrain Desk dashboard. Every value shown comes from the local API / event stream.
   All text is rendered with textContent (token metadata is untrusted). */
(() => {
  'use strict';
  const TOKEN = document.querySelector('meta[name="desk-token"]').content;
  const SEATS = ['CHIEF', 'SCAN', 'VET', 'SOCIAL', 'SIZE', 'FILLS', 'RISK', 'WATCH'];
  const INFO = {
    CHIEF: { role: 'PandaBrain · picks one or none', metric: 'engine' },
    SCAN: { role: 'discovery · arithmetic cut', metric: 'passed / seen' },
    VET: { role: 'mint · controls · valuation', metric: 'verified' },
    SOCIAL: { role: 'exact published handle only', metric: 'accounts read' },
    SIZE: { role: 'documented arithmetic', metric: 'last ticket' },
    FILLS: { role: 'simulated paper fills', metric: 'last fill' },
    RISK: { role: 'monitors · closes on its own', metric: 'vs entry' },
    WATCH: { role: 'durable watchlist', metric: 'active identities' },
  };
  const COLORS = {};
  const css = getComputedStyle(document.documentElement);
  SEATS.forEach(s => { COLORS[s] = css.getPropertyValue('--c-' + s).trim() || '#ccc'; });
  const reduced = window.matchMedia('(prefers-reduced-motion: reduce)').matches;

  // ------------------------------------------------------------ helpers
  const $ = (q, r = document) => r.querySelector(q);
  const SVGNS = 'http://www.w3.org/2000/svg';
  function h(tag, attrs, ...kids) {
    const e = document.createElement(tag);
    for (const [k, v] of Object.entries(attrs || {})) {
      if (v === null || v === undefined || v === false) continue;
      if (k === 'class') e.className = v;
      else if (k === 'text') e.textContent = v;
      else if (k.startsWith('on')) e.addEventListener(k.slice(2), v);
      else e.setAttribute(k, v);
    }
    for (const c of kids.flat()) if (c !== null && c !== undefined) e.append(c instanceof Node ? c : document.createTextNode(String(c)));
    return e;
  }
  function s(tag, attrs, ...kids) {
    const e = document.createElementNS(SVGNS, tag);
    for (const [k, v] of Object.entries(attrs || {})) {
      if (v === null || v === undefined) continue;
      if (k === 'text') e.textContent = v; else e.setAttribute(k, v);
    }
    for (const c of kids.flat()) if (c) e.append(c);
    return e;
  }
  const clear = el => { while (el.firstChild) el.removeChild(el.firstChild); return el; };
  const num = x => (x === null || x === undefined || x === '' ? null : Number(x));
  function usd(x, dp = 2) {
    const n = num(x); if (n === null || !isFinite(n)) return '—';
    const a = Math.abs(n);
    if (a >= 1e6) return (n < 0 ? '-$' : '$') + (a / 1e6).toFixed(2) + 'M';
    if (a >= 1e4) return (n < 0 ? '-$' : '$') + (a / 1e3).toFixed(1) + 'K';
    return (n < 0 ? '-$' : '$') + a.toLocaleString('en-US', { minimumFractionDigits: dp, maximumFractionDigits: dp });
  }
  function price(x) {
    const n = num(x); if (n === null || !isFinite(n)) return '—';
    if (n === 0) return '0';
    if (n < 0.0001) return n.toExponential(3);
    return n.toPrecision(5);
  }
  const pct = (x, dp = 1) => (num(x) === null ? '—' : (num(x) * 100).toFixed(dp) + '%');
  const short = a => (a ? a.slice(0, 4) + '…' + a.slice(-4) : '—');
  function ago(ts) {
    if (!ts) return '—';
    const d = Math.max(0, Date.now() / 1000 - ts);
    if (d < 60) return Math.round(d) + 's ago';
    if (d < 3600) return Math.round(d / 60) + 'm ago';
    if (d < 86400) return (d / 3600).toFixed(1) + 'h ago';
    return (d / 86400).toFixed(1) + 'd ago';
  }
  const utc = ts => (ts ? new Date(ts * 1000).toISOString().slice(11, 19) : '—');
  const mins = m => (m === null || m === undefined ? '—' : m < 120 ? m.toFixed(0) + 'm' : (m / 60).toFixed(1) + 'h');

  async function get(path) {
    const r = await fetch(path, { headers: { Accept: 'application/json' } });
    if (!r.ok) throw new Error((await r.json().catch(() => ({}))).error || r.status);
    return r.json();
  }
  async function post(path, body) {
    const r = await fetch(path, {
      method: 'POST', headers: { 'Content-Type': 'application/json', 'X-Desk-Token': TOKEN },
      body: JSON.stringify(body || {}),
    });
    const j = await r.json().catch(() => ({}));
    return { ok: r.ok, status: r.status, body: j };
  }
  function say(msg, cls) { const m = $('#ctlMsg'); m.textContent = msg || ''; m.className = 'ctl-msg ' + (cls || ''); }

  let S = null;            // latest /api/state
  let focus = null;        // candidate detail for thresholds panel
  let focusKey = null;

  // ------------------------------------------------------------ clock
  setInterval(() => { $('#clock').textContent = new Date().toISOString().slice(11, 19) + ' UTC'; }, 1000);

  // ------------------------------------------------------------ seats
  const seatEls = {};
  function buildSeats() {
    const root = $('#seats');
    for (const name of SEATS) {
      const el = h('article', { class: 'seat', 'aria-label': name + ' seat' });
      el.style.setProperty('--seat', COLORS[name]);
      const av = document.createElementNS(SVGNS, 'svg'); av.setAttribute('class', 'seat-av'); av.setAttribute('viewBox', '0 0 64 64');
      av.append(s('use', { href: '#panda' }));
      const pill = h('span', { class: 'pill', text: '—' });
      const mLabel = h('span', { text: INFO[name].metric }), mVal = h('b', { text: '—' });
      const bar = h('i'); const quote = h('div', { class: 'seat-quote', text: '—' }); const time = h('div', { class: 'seat-time', text: '' });
      el.append(h('div', { class: 'seat-head' }, av, h('div', {}, h('div', { class: 'seat-name', text: name === 'WATCH' ? 'WATCHLIST' : name }),
        h('div', { class: 'seat-role', text: INFO[name].role })), pill),
        h('div', { class: 'seat-metric' }, mLabel, mVal), h('div', { class: 'bar' }, bar), quote, time);
      root.append(el);
      seatEls[name] = { el, pill, mVal, bar, quote, time };
    }
  }
  function seatMetric(name, st, S) {
    const d = st.detail || {};
    switch (name) {
      case 'CHIEF': return [S.judge.engine + (S.judge.configured ? '' : ' (unconfigured)'), S.judge.configured ? 1 : 0];
      case 'SCAN': return st.metric ? [`${st.count ?? 0} / ${st.metric}`, Math.min(1, (st.count || 0) / Math.max(1, st.metric))] : ['—', 0];
      case 'VET': return [st.count !== null ? `${st.count} of ${st.metric ?? 0}` : '—', st.metric ? (st.count || 0) / st.metric : 0];
      case 'SOCIAL': return [st.count !== null ? String(st.count) : '—', 0];
      case 'SIZE': return [st.metric ? usd(st.metric) : '—', S.account && st.metric ? Math.min(1, st.metric / (Number(S.account.cash_usd) * S.settings.values.max_position_fraction + 1e-9)) : 0];
      case 'FILLS': return [d.slippage_bps !== undefined ? d.slippage_bps + ' bps' : '—', d.slippage_bps ? Math.min(1, d.slippage_bps / S.fees.max_slippage_bps) : 0];
      case 'RISK': return [st.metric !== null && st.metric !== undefined ? (st.metric >= 0 ? '+' : '') + pct(st.metric) : '—', st.metric ? Math.min(1, Math.abs(st.metric) / S.settings.values.stop_loss_pct) : 0];
      case 'WATCH': return [String(st.count ?? (S.watchlist_counts.active || 0)), Math.min(1, (st.count || 0) / S.settings.values.watchlist_max)];
    }
    return ['—', 0];
  }
  function renderSeats() {
    for (const name of SEATS) {
      const st = S.seats[name]; const e = seatEls[name];
      e.el.className = 'seat ' + st.state;
      e.pill.className = 'pill ' + st.state; e.pill.textContent = st.state;
      const [v, frac] = seatMetric(name, st, S);
      e.mVal.textContent = v; e.bar.style.width = Math.round(Math.max(0, Math.min(1, frac)) * 100) + '%';
      e.quote.textContent = st.action ? '“' + st.action + '”' : 'no activity recorded yet';
      e.time.textContent = st.last_at ? utc(st.last_at) + ' UTC · ' + ago(st.last_at) : '';
    }
  }

  // ------------------------------------------------------------ floor
  const DESKS = {
    SCAN: [330, 245], VET: [600, 245], SOCIAL: [870, 245],
    SIZE: [330, 372], FILLS: [600, 372], RISK: [870, 372], CHIEF: [135, 350], WATCH: [1078, 372],
  };
  const floor = {};
  function buildFloor() {
    const svg = $('#floor');
    const defs = s('defs', {}, s('linearGradient', { id: 'wall', x1: 0, y1: 0, x2: 0, y2: 1 },
      s('stop', { offset: '0', 'stop-color': '#0d1412' }), s('stop', { offset: '1', 'stop-color': '#080c0b' })),
    s('linearGradient', { id: 'eqgrad', x1: 0, y1: 0, x2: 0, y2: 1 }, s('stop', { offset: '0', 'stop-color': '#7ccb6b', 'stop-opacity': '.28' }),
      s('stop', { offset: '1', 'stop-color': '#7ccb6b', 'stop-opacity': '0' })));
    svg.append(defs);
    svg.append(s('rect', { x: 0, y: 0, width: 1200, height: 178, fill: 'url(#wall)' }));
    const grid = s('g', { class: 'floor-grid' });
    for (let i = 0; i <= 14; i++) grid.append(s('line', { x1: 600 + (i - 7) * 60, y1: 178, x2: 600 + (i - 7) * 170, y2: 470 }));
    for (let j = 0; j < 6; j++) { const y = 178 + j * j * 9 + j * 18; grid.append(s('line', { x1: 0, y1: y, x2: 1200, y2: y })); }
    svg.append(grid);
    // left banner with supplied art
    svg.append(s('rect', { x: 30, y: 22, width: 120, height: 140, rx: 6, fill: '#0b100e', stroke: '#2a3a33' }));
    svg.append(s('image', { href: '/static/assets/pandarianz.jpg', x: 45, y: 30, width: 90, height: 90 }));
    svg.append(s('text', { x: 90, y: 142, 'text-anchor': 'middle', class: 'label', text: 'PANDARIANZ' }));
    // big screen
    svg.append(s('rect', { x: 330, y: 18, width: 540, height: 150, rx: 6, class: 'screen' }));
    floor.screen = s('g', {}); svg.append(floor.screen);
    // clock + last pick board
    svg.append(s('rect', { x: 905, y: 34, width: 120, height: 58, rx: 5, class: 'screen' }));
    floor.clock = s('text', { x: 965, y: 66, 'text-anchor': 'middle', class: 'screen-title', text: '--:--' }); svg.append(floor.clock);
    svg.append(s('text', { x: 965, y: 84, 'text-anchor': 'middle', class: 'label', text: 'UTC' }));
    svg.append(s('rect', { x: 1040, y: 22, width: 140, height: 140, rx: 5, class: 'screen' }));
    floor.board = s('g', {}); svg.append(floor.board);
    // vault (the book)
    svg.append(s('rect', { x: 40, y: 400, width: 96, height: 56, rx: 4, fill: '#121a17', stroke: '#2a3a33' }));
    floor.vault = s('text', { x: 88, y: 432, 'text-anchor': 'middle', class: 'vault-text', text: 'BOOK FREE' }); svg.append(floor.vault);
    // radar (watchlist)
    svg.append(s('circle', { cx: 1078, cy: 372, r: 40, fill: '#0b100e', stroke: '#2a3a33' }));
    svg.append(s('circle', { cx: 1078, cy: 372, r: 24, fill: 'none', stroke: '#1d2a25' }));
    floor.sweep = s('path', { d: 'M1078 372 L1118 372 A40 40 0 0 0 1106 344 Z', fill: COLORS.WATCH, class: 'radar-sweep' }); svg.append(floor.sweep);
    svg.append(s('text', { x: 1078, y: 432, 'text-anchor': 'middle', class: 'label', text: 'WATCH RADAR' }));
    // desks
    for (const [name, [cx, cy]] of Object.entries(DESKS)) {
      if (name === 'WATCH') continue;
      const g = s('g', { class: 'seat-g' });
      if (name !== 'CHIEF') {
        g.append(s('rect', { x: cx - 34, y: cy - 62, width: 68, height: 42, rx: 3, class: 'monitor' }));
        g.append(s('rect', { x: cx - 30, y: cy - 58, width: 60, height: 34, class: 'monitor-glow' }));
        g.append(s('polygon', { points: `${cx - 80},${cy - 14} ${cx + 80},${cy - 14} ${cx + 96},${cy + 16} ${cx - 96},${cy + 16}`, class: 'desk-top' }));
        g.append(s('rect', { x: cx - 96, y: cy + 16, width: 192, height: 12, class: 'desk-front' }));
      } else {
        g.append(s('polygon', { points: `${cx - 70},${cy - 10} ${cx + 70},${cy - 10} ${cx + 84},${cy + 18} ${cx - 84},${cy + 18}`, class: 'desk-top' }));
        g.append(s('rect', { x: cx - 84, y: cy + 18, width: 168, height: 12, class: 'desk-front' }));
      }
      g.append(s('text', { x: cx, y: cy + 46, 'text-anchor': 'middle', class: 'label', text: name }));
      const av = s('g', { class: 'avatar' });
      av.append(s('circle', { cx: 0, cy: 0, r: 24, class: 'avatar-ring', stroke: COLORS[name] }));
      const u = s('use', { href: '#panda', x: -19, y: -19, width: 38, height: 38 }); u.style.color = COLORS[name];
      av.append(u);
      const bubble = s('g', { class: 'bubble' });
      const br = s('rect', { x: 0, y: -16, rx: 4, height: 22, width: 10 }); const bt = s('text', { x: 6, y: -1, text: '' });
      bubble.append(br, bt);
      av.append(bubble);
      g.append(av);
      svg.append(g);
      const home = name === 'CHIEF' ? [cx, cy - 34] : [cx + 112, cy - 18];
      const work = name === 'CHIEF' ? [cx, cy - 40] : [cx, cy - 30];
      floor[name] = { g, av, br, bt, home, work, bubble };
      setAvatar(name, 'idle', '');
    }
    floor.links = s('g', {}); svg.append(floor.links);
  }
  function setAvatar(name, state, text) {
    const f = floor[name]; if (!f) return;
    f.g.setAttribute('class', 'seat-g ' + state);
    const [x, y] = state === 'running' ? f.work : f.home;
    f.av.style.transform = `translate(${x}px, ${y}px)`;
    const t = (text || '').length > 38 ? text.slice(0, 37) + '…' : (text || '');
    f.bt.textContent = t;
    f.br.setAttribute('width', Math.max(12, t.length * 6.7 + 12));
    f.bubble.setAttribute('transform', `translate(-${Math.max(12, t.length * 6.7 + 12) / 2}, -34)`);
    f.bubble.style.display = t ? '' : 'none';
  }
  function anchor(name) {
    const f = floor[name]; if (!f) return DESKS[name] || [600, 300];
    const st = S && S.seats[name] ? S.seats[name].state : 'idle';
    return st === 'running' ? f.work : f.home;
  }
  let lastSeat = null;
  function handoff(from, to) {
    if (!from || !to || from === to || reduced) return;
    const [x1, y1] = from === 'WATCH' ? [1078, 372] : anchor(from); const [x2, y2] = to === 'WATCH' ? [1078, 372] : anchor(to);
    const mx = (x1 + x2) / 2, my = Math.min(y1, y2) - 70;
    const p = s('path', { d: `M${x1} ${y1} Q${mx} ${my} ${x2} ${y2}`, class: 'handoff', stroke: COLORS[from] });
    floor.links.append(p);
    for (let i = 1; i <= 4; i++) {
      const t = i / 5, x = (1 - t) * (1 - t) * x1 + 2 * (1 - t) * t * mx + t * t * x2, y = (1 - t) * (1 - t) * y1 + 2 * (1 - t) * t * my + t * t * y2;
      floor.links.append(s('circle', { cx: x + (i % 2 ? 6 : -6), cy: y + (i % 2 ? -5 : 5), r: 3, fill: i % 2 ? COLORS[to] : COLORS[from], class: 'node' }));
    }
    setTimeout(() => { while (floor.links.childNodes.length > 40) floor.links.removeChild(floor.links.firstChild); }, 3000);
    setTimeout(() => clear(floor.links), 3200);
  }
  function renderFloor() {
    for (const name of SEATS) if (floor[name]) setAvatar(name, S.seats[name].state, S.seats[name].action || '');
    floor.sweep.setAttribute('class', 'radar-sweep' + (S.seats.SCAN.state === 'running' || S.worker.scanning ? ' on' : ''));
    floor.clock.textContent = new Date().toISOString().slice(11, 16);
    floor.vault.textContent = S.position ? (S.position.status === 'CLOSE_PENDING' ? 'CLOSE PENDING' : 'BOOK HELD') : 'BOOK FREE';
    floor.vault.setAttribute('fill', S.position ? '#ffad3b' : '#8a9a93');
    // board: last outcome
    clear(floor.board);
    const c = S.funnel.cycle;
    const lines = c ? [`CYCLE #${c.id}`, c.outcome.toUpperCase(), (c.reason || '').slice(0, 18), `${(S.funnel.stats.seen || 0)} seen`, `${S.funnel.stats.survivors || 0} survived`] : ['NO CYCLES', 'YET'];
    lines.forEach((t, i) => floor.board.append(s('text', { x: 1050, y: 44 + i * 18, class: i ? 'screen-text' : 'screen-title', text: t })));
    // big screen
    clear(floor.screen);
    const P = S.position;
    if (P) {
      floor.screen.append(s('text', { x: 346, y: 40, class: 'screen-title', text: `HOLDING ${P.symbol || short(P.token_address)} · PAPER` }));
      floor.screen.append(s('text', { x: 854, y: 40, 'text-anchor': 'end', class: 'screen-text', text: `held ${mins(P.held_minutes)} · next check ${P.next_risk_in_s}s` }));
      drawSpark(floor.screen, P.marks.map(m => Number(m.price_usd)), 346, 52, 508, 104, Number(P.quote_price));
    } else {
      const ev = S.chief.last_event;
      floor.screen.append(s('text', { x: 346, y: 40, class: 'screen-title', text: 'PANDABRAIN · ' + (S.judge.engine || '') }));
      const msg = ev ? ev.message : 'waiting for the first cycle';
      floor.screen.append(s('text', { x: 346, y: 70, class: 'screen-text', text: msg.slice(0, 66) }));
      const st = S.funnel.stats || {};
      const pre = Object.entries(st.pre || {}).sort((a, b) => b[1] - a[1]).slice(0, 4).map(([k, v]) => `${k} ${v}`).join(' · ');
      floor.screen.append(s('text', { x: 346, y: 96, class: 'screen-text', text: pre ? 'top cuts: ' + pre : '' }));
      const vet = Object.entries({ ...(st.vet || {}), ...(st.valuation || {}) }).map(([k, v]) => `${k} ${v}`).join(' · ');
      floor.screen.append(s('text', { x: 346, y: 120, class: 'screen-text', text: vet ? 'vet: ' + vet.slice(0, 60) : '' }));
      floor.screen.append(s('text', { x: 346, y: 150, class: 'screen-text', text: S.worker.scanning ? 'scanning on' : 'scanning paused' }));
    }
  }
  function drawSpark(g, ys, x, y, w, hgt, ref) {
    const pts = ys.filter(v => isFinite(v));
    if (pts.length < 2) { g.append(s('text', { x: x, y: y + 50, class: 'screen-text', text: 'collecting real marks…' })); return; }
    const all = ref ? pts.concat([ref]) : pts;
    const lo = Math.min(...all), hi = Math.max(...all), span = hi - lo || hi * 0.01 || 1;
    const X = i => x + (i / (pts.length - 1)) * w, Y = v => y + hgt - ((v - lo) / span) * hgt;
    if (ref) g.append(s('line', { x1: x, x2: x + w, y1: Y(ref), y2: Y(ref), stroke: '#8a9a93', 'stroke-dasharray': '4 4' }));
    g.append(s('polyline', { points: pts.map((v, i) => `${X(i)},${Y(v)}`).join(' '), fill: 'none', stroke: '#7ccb6b', 'stroke-width': 2 }));
  }

  // ------------------------------------------------------------ top & controls
  let inflight = false;
  function renderTop() {
    $('#demoBadge').hidden = !S.demo;
    $('#modeBadge').textContent = S.execution_mode === 'SHADOW_ONLY' ? 'SHADOW · PAPER ONLY' : S.execution_mode;
    const eb = $('#engineBadge');
    eb.textContent = 'ENGINE ' + S.judge.engine + (S.judge.configured ? '' : ' · UNCONFIGURED');
    eb.className = 'badge' + (S.judge.configured ? '' : ' warnb');
    eb.title = S.judge.detail || '';
    const w = S.worker;
    $('#workerDot').className = 'dot ' + (w.alive ? (w.last_error ? 'warn' : 'ok') : 'bad');
    $('#workerText').textContent = w.alive ? `worker ${w.state}${w.busy ? ' · ' + (w.activity || 'busy') : ''}` : 'worker stopped';
    $('#nextCycle').textContent = S.position ? 'holding: scan suspended' : (w.scanning ? (w.next_cycle_at ? 'next cycle ' + Math.max(0, Math.round(w.next_cycle_at - Date.now() / 1000)) + 's' : 'cycle due') : 'scanning paused');
    if (!inflight) {
      const bs = $('#btnScan'); bs.textContent = w.scanning ? 'Pause scanning' : 'Start scanning'; bs.classList.toggle('on', w.scanning);
      $('#btnRun').disabled = w.busy || w.run_once_queued;
      const bw = $('#btnWorker'); bw.textContent = w.alive ? 'Stop worker' : 'Start worker'; bw.classList.toggle('danger', w.alive);
    }
    const a = S.account, st = S.stats;
    const eq = S.position && S.position.last_mark_price ? Number(a.cash_usd) + Number(S.position.last_mark_price) * Number(S.position.qty) : (a ? Number(a.cash_usd) : null);
    $('#tickerLine').textContent = `CASH ${usd(a && a.cash_usd)} · EQUITY ${usd(eq)} · TRADES ${st.trades} · ` +
      `PNL ${st.total_pnl_usd === null ? '—' : usd(st.total_pnl_usd)} · ${S.position ? 'OPEN ' + (S.position.symbol || '') : 'NO POSITION'} · CYCLE ${S.funnel.cycle ? '#' + S.funnel.cycle.id : '—'}`;
    $('#equityNow').textContent = usd(eq);
    $('#cashNow').textContent = 'cash ' + usd(a && a.cash_usd) + ' · start ' + usd(a && a.starting_cash_usd);
    $('#statLine').textContent = st.trades ? `${st.trades} closed · win rate ${pct(st.win_rate, 0)} · net ${usd(st.total_pnl_usd)} (fees + modelled slippage included)` : 'no closed trades yet — stats stay empty';
  }
  async function control(btn, fn) {
    if (inflight) return;
    inflight = true; btn.disabled = true; say('');
    try { await fn(); } catch (e) { say(String(e.message || e), 'bad'); } finally { inflight = false; btn.disabled = false; refresh(); }
  }
  $('#btnScan').addEventListener('click', e => control(e.currentTarget, async () => {
    const r = await post('/api/control/scan', { enabled: !(S && S.worker.scanning) });
    say(r.ok ? (r.body.scanning ? 'scanning on' + (r.body.worker_running ? '' : ' — worker is stopped, start it to run cycles') : 'scanning paused; RISK keeps monitoring any open position') : r.body.error);
  }));
  $('#btnRun').addEventListener('click', e => control(e.currentTarget, async () => {
    const r = await post('/api/control/run-once', {});
    say(r.ok ? (S && S.position ? 'risk check queued' : 'cycle queued') : r.body.error);
  }));
  $('#btnWorker').addEventListener('click', e => control(e.currentTarget, async () => {
    if (S && S.worker.alive) {
      let r = await post('/api/worker/stop', {});
      if (r.status === 409 && S.position) {
        if (!window.confirm('A paper position is open. Stopping the worker stops RISK monitoring. Stop anyway?')) return;
        r = await post('/api/worker/stop', { confirm_unmonitored: true });
      }
      say(r.ok ? 'worker stopped' : r.body.error);
    } else {
      const r = await post('/api/worker/start', {});
      say(r.ok ? 'worker started' : r.body.error);
    }
  }));

  // ------------------------------------------------------------ balance
  async function renderBalance() {
    const [eq, cyc] = await Promise.all([get('/api/equity'), get('/api/cycles')]);
    const svg = clear($('#equityChart'));
    svg.append(s('defs', {}, s('linearGradient', { id: 'eqgrad', x1: 0, y1: 0, x2: 0, y2: 1 },
      s('stop', { offset: '0', 'stop-color': '#7ccb6b', 'stop-opacity': '.28' }), s('stop', { offset: '1', 'stop-color': '#7ccb6b', 'stop-opacity': '0' }))));
    for (let i = 1; i < 4; i++) svg.append(s('line', { x1: 0, x2: 1000, y1: i * 45, y2: i * 45, class: 'grid' }));
    const ys = eq.map(r => Number(r.equity_usd));
    if (ys.length < 2) {
      svg.append(s('text', { x: 500, y: 95, 'text-anchor': 'middle', class: 'empty', text: ys.length ? 'one equity point recorded — the line appears after the next cycle' : 'no equity recorded yet — nothing is drawn until real cycles run' }));
    } else {
      const lo = Math.min(...ys), hi = Math.max(...ys), span = hi - lo || Math.max(1, hi * 0.002);
      const X = i => (i / (ys.length - 1)) * 1000, Y = v => 170 - ((v - lo) / span) * 150;
      const pts = ys.map((v, i) => `${X(i)},${Y(v)}`).join(' ');
      svg.append(s('polygon', { points: `0,180 ${pts} 1000,180`, class: 'eq-area' }));
      svg.append(s('polyline', { points: pts, class: 'eq-line' }));
    }
    const bars = clear($('#cycleBars'));
    const n = Math.max(cyc.length, 1), mx = Math.max(1, ...cyc.map(c => c.seen));
    cyc.forEach((c, i) => {
      const w = 1000 / Math.max(n, 30), x = i * w, hh = (c.seen / mx) * 46;
      bars.append(s('rect', { x, y: 50 - hh, width: w * 0.7, height: hh, class: 'seen' }));
      if (c.survivors) bars.append(s('rect', { x, y: 50 - Math.max(4, hh * 0.35), width: w * 0.7, height: Math.max(4, hh * 0.35), class: 'surv' }));
    });
  }

  // ------------------------------------------------------------ analysis & thresholds
  function kv(pairs) {
    return h('div', { class: 'kv' }, pairs.flatMap(([k, v]) => [h('div', { text: k }), v instanceof Node ? h('div', {}, v) : h('div', { text: v ?? '—' })]));
  }
  function renderAnalysis() {
    const root = clear($('#analysis'));
    $('#scoreType').textContent = S.judge.score_type || '';
    const pick = S.chief.last_pick, ev = S.chief.last_event;
    root.append(kv([
      ['engine', S.judge.engine + (S.judge.configured ? '' : ' — UNCONFIGURED (no fallback)')],
      ['judge status', S.judge.detail],
      ['score type', S.judge.score_type],
      ['thresholds', `${S.threshold_version} · settings v${S.settings.last_applied_version ?? '—'} applied / v${S.settings.latest_version} latest`],
    ]));
    const P = S.position;
    if (P) {
      const d = P.decision || {};
      root.append(h('div', { class: 'section-t', text: 'OPEN PAPER POSITION' }));
      root.append(kv([
        ['token', `${P.symbol || '?'} · ${P.chain}`], ['exact pool', P.pool_address], ['mint', P.token_address],
        ['picked by', `${d.engine || '—'} · ${d.model_id || '—'}`],
        ['score', d.engine === 'RULES' ? `rule score ${d.judgment && d.judgment.rule_score} (not a probability)` : (d.confidence !== null && d.confidence !== undefined ? `pick confidence ${d.confidence}` : 'single survivor gate')],
        ['valuation', d.valuation ? `${d.valuation.type} ${usd(d.valuation.value_usd, 0)}` : '—'],
        ['size notes', (d.size_notes || []).join('; ') || 'none'],
        ['status', P.status + (P.close_reason ? ' · ' + P.close_reason : '')],
      ]));
      const svg = document.createElementNS(SVGNS, 'svg'); svg.setAttribute('class', 'mini-chart'); svg.setAttribute('viewBox', '0 0 600 120'); svg.setAttribute('preserveAspectRatio', 'none');
      const ys = P.marks.map(m => Number(m.price_usd)).filter(isFinite);
      const ex = P.exits, q = Number(P.quote_price);
      if (ys.length >= 2) {
        const lo = Math.min(...ys, q), hi = Math.max(...ys, q), span = hi - lo || q * 0.01;
        const Y = v => 112 - ((v - lo) / span) * 104, X = i => (i / (ys.length - 1)) * 600;
        svg.append(s('line', { x1: 0, x2: 600, y1: Y(q), y2: Y(q), class: 'entry' }));
        svg.append(s('polyline', { points: ys.map((v, i) => `${X(i)},${Y(v)}`).join(' '), class: 'pl' }));
      } else svg.append(s('text', { x: 10, y: 60, fill: '#5d6c66', 'font-size': 12, text: 'marks appear after each RISK check' }));
      root.append(svg);
      root.append(h('div', { class: 'muted small', text: `stop ${price(ex.stop_loss_price)} · target ${price(ex.take_profit_price)} · max hold until ${utc(ex.max_hold_at)} UTC · volume ratio exit < ${ex.volume_ratio_exit}` }));
      return;
    }
    root.append(h('div', { class: 'section-t', text: 'LAST CHIEF DECISION' }));
    if (!pick && !ev) { root.append(h('div', { class: 'muted', text: 'No cycles yet. Start scanning or run once.' })); return; }
    const pr = pick && pick.result;
    const rows = [['last event', ev ? `${utc(ev.at)} · ${ev.message}` : '—']];
    if (pr) {
      rows.push(['pick', pr.candidate_id ? pr.candidate_id : 'none — ' + (pr.reason || '')]);
      rows.push(['provenance', `${pick.engine} · model ${pick.model_id || '—'} · ${pick.threshold_version} · settings v${pick.settings_version}`]);
      if (pr.detail && pr.detail.ranked) rows.push(['ranked', pr.detail.ranked.map(([id, sc]) => `${id} ${sc}`).join(' · ')]);
      if (pr.detail && pr.detail.worth_trading_at_all !== undefined) rows.push(['worth_trading_at_all', String(pr.detail.worth_trading_at_all)]);
      if (pr.confidence !== null && pr.confidence !== undefined) rows.push(['confidence', String(pr.confidence)]);
    }
    const c = S.funnel.cycle;
    if (c) rows.push(['last cycle', `#${c.id} ${c.outcome}${c.reason ? ' · ' + c.reason : ''} · ${ago(c.ended_at)}`]);
    root.append(kv(rows));
  }

  function thrRow(name, limit, val, ok) {
    return h('div', { class: 'thr-row' + (ok === false ? ' fail' : '') }, h('span', { class: 'n', text: name }),
      h('span', { class: 'v dim', text: limit }), h('span', { class: 'v', text: val }),
      h('span', { class: ok === null ? 'dim' : ok ? 'ok' : 'no', text: ok === null ? '·' : ok ? '✓' : '✗' }));
  }
  function renderThresholds() {
    const root = clear($('#thresholds'));
    const T = S.thresholds;
    $('#thrVersion').textContent = `${S.threshold_version} · settings v${S.settings.latest_version}`;
    const d = focus, snap = d && d.snapshots && d.snapshots[0], dz = d && d.dossier;
    root.append(h('div', { class: 'muted small', text: d ? `values: ${d.watch.symbol || short(d.watch.token_address)} (${short(d.watch.pool_address)}) · quote ${ago(snap && snap.fetched_at)} from ${snap ? snap.source : '—'}` : 'values appear once a candidate reaches the funnel' }));
    const now = Date.now() / 1000;
    const age = d && d.watch.pool_created_at ? (now - d.watch.pool_created_at) / 60 : null;
    const has = v => v !== null && v !== undefined;
    const trades = snap && has(snap.buys_h24) && has(snap.sells_h24) ? snap.buys_h24 + snap.sells_h24 : null;
    root.append(h('div', { class: 'section-t', text: 'HARD · pre-dossier arithmetic' }));
    root.append(thrRow('min_age_minutes', T.min_age_minutes, has(age) ? age.toFixed(0) : '—', has(age) ? age >= T.min_age_minutes : null));
    root.append(thrRow('max_age_hours', T.max_age_hours, has(age) ? (age / 60).toFixed(1) : '—', has(age) ? age <= T.max_age_hours * 60 : null));
    root.append(thrRow('min_liquidity_usd', usd(T.min_liquidity_usd, 0), snap ? usd(snap.liquidity_usd, 0) : '—', snap && has(snap.liquidity_usd) ? snap.liquidity_usd >= T.min_liquidity_usd : null));
    root.append(thrRow('min_volume_h24', usd(T.min_volume_h24, 0), snap ? usd(snap.volume_h24, 0) : '—', snap && has(snap.volume_h24) ? snap.volume_h24 >= T.min_volume_h24 : null));
    root.append(thrRow('min_trades_h24', T.min_trades_h24, has(trades) ? trades : '—', has(trades) ? trades >= T.min_trades_h24 : null));
    root.append(h('div', { class: 'section-t', text: 'VET · facts after the dossier' }));
    const m = dz && dz.mint;
    root.append(thrRow('mint_authority', 'none', m ? (m.mint_authority ? 'OPEN' : 'none') : '—', m && m.raw_supply ? !m.mint_authority : null));
    root.append(thrRow('freeze_authority', 'none', m ? (m.freeze_authority ? 'OPEN' : 'none') : '—', m && m.raw_supply ? !m.freeze_authority : null));
    root.append(thrRow('extensions', 'metadata-only', m ? (m.extensions.join(',') || 'none') : '—', m && m.raw_supply ? m.reason !== 'unsupported_extension' : null));
    root.append(thrRow('max_top10_share', pct(T.max_top10_share, 0), dz && has(dz.top_10_percent) ? dz.top_10_percent.toFixed(1) + '%' : (dz ? 'missing' : '—'), dz ? (has(dz.top_10_percent) ? dz.top_10_percent / 100 <= T.max_top10_share : false) : null));
    root.append(thrRow('max_largest_account', pct(T.max_largest_account_share, 0), dz && has(dz.largest_account_share) ? pct(dz.largest_account_share) : '—', dz && has(dz.largest_account_share) ? dz.largest_account_share <= T.max_largest_account_share : null));
    root.append(thrRow('min_holders', T.min_holders, dz && has(dz.holder_count) ? dz.holder_count : (dz ? 'missing' : '—'), dz ? (has(dz.holder_count) ? dz.holder_count >= T.min_holders : false) : null));
    root.append(h('div', { class: 'section-t', text: 'VALUATION · separate limits per type' }));
    const v = dz && dz.valuation;
    if (v && v.type === 'TOTAL_SUPPLY_VALUATION') root.append(thrRow('supply_valuation', `${usd(T.supply_valuation_min_usd, 0)}–${usd(T.supply_valuation_max_usd, 0)}`, usd(v.value_usd, 0), Number(v.value_usd) >= T.supply_valuation_min_usd && Number(v.value_usd) <= T.supply_valuation_max_usd));
    else root.append(thrRow('provider_mcap', `${usd(T.mcap_min_usd, 0)}–${usd(T.mcap_max_usd, 0)}`, v ? usd(v.value_usd, 0) : '—', v ? Number(v.value_usd) >= T.mcap_min_usd && Number(v.value_usd) <= T.mcap_max_usd : null));
    root.append(h('div', { class: 'muted small', text: v ? `${v.type}${v.type === 'TOTAL_SUPPLY_VALUATION' ? ' — not circulating market cap' : ' via ' + v.source}` : 'no valuation computed for this candidate' }));
    root.append(h('div', { class: 'section-t', text: 'JUDGE' }));
    const j = d && d.judgments && d.judgments.find(x => x.question_set === 'candidate');
    if (S.judge.engine === 'RULES') {
      const sc = j && j.result && j.result.values ? j.result.values.rule_score : null;
      root.append(thrRow('rules_min_score', T.rules_min_score, has(sc) ? sc : '—', has(sc) ? sc >= T.rules_min_score : null));
    } else {
      root.append(thrRow('typed answers', 'see drawer', j ? (j.result.passed ? 'pass' : j.result.reason) : '—', j ? j.result.passed : null));
    }
  }
  function renderFunnel() {
    const root = clear($('#funnel'));
    const st = S.funnel.stats;
    if (!st) { root.append(h('span', { class: 'muted small', text: 'funnel fills after the first cycle' })); return; }
    const sum = o => Object.values(o || {}).reduce((a, b) => a + b, 0);
    const prePass = (st.due || 0) - sum(st.pre) - (st.deferred || 0);
    const steps = [['discovered', st.seen], ['due', st.due], ['passed arithmetic', Math.max(0, prePass)], ['dossiers', st.dossiers],
      ['survivors', st.survivors], ['outcome', S.funnel.cycle.outcome]];
    steps.forEach(([k, v]) => root.append(h('span', { class: 'fpill' }, k, h('b', { text: String(v ?? 0) }))));
    const kills = { ...st.pre, ...st.vet, ...st.valuation, ...st.judge };
    Object.entries(kills).sort((a, b) => b[1] - a[1]).forEach(([k, v]) => root.append(h('span', { class: 'fpill kill' }, k, h('b', { text: String(v) }))));
    if (st.outages && st.outages.length) root.append(h('span', { class: 'fpill kill', text: 'provider outage: ' + st.outages.join(', ') }));
    if (st.deferred) root.append(h('span', { class: 'fpill', text: `deferred to next cycle (budget) ${st.deferred}` }));
  }
  async function loadFocus() {
    let key = null;
    if (S.position) key = S.position.chain + ':' + S.position.pool_address;
    else if (S.focus) key = S.focus.chain + ':' + S.focus.pool_address;
    if (!key) { focus = null; focusKey = null; return; }
    const [chain, pool] = [key.split(':')[0], key.slice(key.indexOf(':') + 1)];
    try { focus = await get(`/api/candidate?chain=${encodeURIComponent(chain)}&pool=${encodeURIComponent(pool)}`); focusKey = key; } catch { focus = null; }
  }

  // ------------------------------------------------------------ watchlist
  let wlSort = 'last_checked_at', wlDir = 'desc';
  const VAL_LABEL = { PROVIDER_MARKET_CAP: 'MCAP·provider', TOTAL_SUPPLY_VALUATION: 'TOTAL SUPPLY', 'PROVIDER_MARKET_CAP (unvetted)': 'MCAP·unvetted' };
  async function renderWatchlist() {
    const q = encodeURIComponent($('#wlSearch').value.trim()), st = $('#wlStatus').value;
    const rows = await get(`/api/watchlist?sort=${wlSort}&dir=${wlDir}&q=${q}&status=${st}&limit=300`);
    const tb = clear($('#wlTable tbody'));
    document.querySelectorAll('#wlTable th[data-sort]').forEach(th => th.classList.toggle('sorted', th.dataset.sort === wlSort));
    const c = S ? S.watchlist_counts : {};
    $('#wlCounts').textContent = `${c.active || 0} active · ${c.rejected || 0} rejected · ${c.retired || 0} retired`;
    if (!rows.length) { tb.append(h('tr', {}, h('td', { colspan: 11, class: 'muted', text: 'empty — discovery has not run on this book yet' }))); return; }
    for (const r of rows) {
      const tr = h('tr', { tabindex: 0, onclick: () => openDrawer(r.chain, r.pool_address), onkeydown: e => { if (e.key === 'Enter') openDrawer(r.chain, r.pool_address); } },
        h('td', {}, h('b', { text: r.symbol || '?' }), ' ', h('span', { class: 'dim', text: (r.name || '').slice(0, 18) })),
        h('td', { class: 'addr', title: `${r.chain} pool ${r.pool_address} / mint ${r.token_address}`, text: `${r.chain} · ${short(r.pool_address)}` }),
        h('td', { text: mins(r.age_minutes) }), h('td', { text: price(r.price_usd) }), h('td', { text: usd(r.liquidity_usd, 0) }),
        h('td', { text: usd(r.volume_h24, 0) }), h('td', { text: r.buys_h1 === null ? '—' : `${r.buys_h1}/${r.sells_h1}` }),
        h('td', { title: r.valuation_type || '', text: r.valuation_type ? `${VAL_LABEL[r.valuation_type] || r.valuation_type} ${usd(r.valuation_usd, 0)}` : '—' }),
        h('td', {}, r.evidence_gaps.map(g => h('span', { class: 'chip gap', text: g }))),
        h('td', {}, h('span', { class: 'chip ' + (r.last_reason ? 'rej' : (r.last_stage && r.last_stage.startsWith('deferred') ? 'gap' : (r.last_checked_at ? 'okc' : ''))),
          text: r.last_reason || (r.last_stage && r.last_stage.startsWith('deferred') ? r.last_stage.replace(':', ' · ') : (r.last_checked_at ? 'passed ' + (r.last_stage || '') : 'new')) })),
        h('td', { text: ago(r.last_checked_at) }));
      tb.append(tr);
    }
  }
  document.querySelectorAll('#wlTable th[data-sort]').forEach(th => th.addEventListener('click', () => {
    if (wlSort === th.dataset.sort) wlDir = wlDir === 'desc' ? 'asc' : 'desc'; else { wlSort = th.dataset.sort; wlDir = 'desc'; }
    renderWatchlist();
  }));
  let wlT; $('#wlSearch').addEventListener('input', () => { clearTimeout(wlT); wlT = setTimeout(renderWatchlist, 250); });
  $('#wlStatus').addEventListener('change', renderWatchlist);

  // ------------------------------------------------------------ drawer
  async function openDrawer(chain, pool) {
    const d = await get(`/api/candidate?chain=${encodeURIComponent(chain)}&pool=${encodeURIComponent(pool)}`);
    const w = d.watch, body = clear($('#drawerBody'));
    $('#drawerTitle').textContent = `${w.symbol || '?'} · ${w.chain}`;
    const sn = d.snapshots[0] || {};
    body.append(h('div', { class: 'section-t', text: 'IDENTITY (exact pool)' }), kv([
      ['name (untrusted text)', w.name || '—'], ['pool', w.pool_address], ['mint', w.token_address], ['dex', w.dex || '—'],
      ['pool created', w.pool_created_at ? new Date(w.pool_created_at * 1000).toISOString() : 'unknown'],
      ['first seen', ago(w.first_seen_at)], ['status', `${w.status} · checks ${w.checks}`], ['next check', w.next_check_at ? utc(w.next_check_at) + ' UTC' : '—'],
    ]));
    body.append(h('div', { class: 'section-t', text: 'LATEST QUOTE' }), kv([
      ['source', sn.source || '—'], ['fetched', sn.fetched_at ? `${utc(sn.fetched_at)} UTC (${ago(sn.fetched_at)})` : '—'],
      ['provider response time', sn.provider_ts ? utc(sn.provider_ts) + ' UTC (HTTP Date header)' : 'not supplied'], ['price', price(sn.price_usd)],
      ['liquidity', usd(sn.liquidity_usd, 0)], ['volume 1h/6h/24h', `${usd(sn.volume_h1, 0)} / ${usd(sn.volume_h6, 0)} / ${usd(sn.volume_h24, 0)}`],
      ['buys/sells 1h', sn.buys_h1 === undefined ? '—' : `${sn.buys_h1} / ${sn.sells_h1}`], ['buys/sells 24h', sn.buys_h24 === undefined ? '—' : `${sn.buys_h24} / ${sn.sells_h24}`],
      ['provider market cap', sn.mcap_usd ? `${usd(sn.mcap_usd, 0)} (${sn.mcap_source})` : 'not reported (null)'],
      ['provider FDV', sn.fdv_usd ? usd(sn.fdv_usd, 0) + ' — never used as market cap' : '—'],
    ]));
    const dz = d.dossier;
    body.append(h('div', { class: 'section-t', text: 'CONTROLS & CONCENTRATION' }));
    if (dz) {
      const m = dz.mint || {};
      body.append(kv([
        ['program', m.program || '—'], ['raw supply', m.raw_supply || '—'], ['decimals', m.decimals ?? '—'],
        ['parsed/raw agree', String(m.parsed_raw_agree)], ['mint authority', m.mint_authority || 'none'], ['freeze authority', m.freeze_authority || 'none'],
        ['extensions', (m.extensions || []).join(', ') || 'none'], ['mint check', m.ok ? 'passed' : (m.reason || '—') + (m.detail ? ' · ' + m.detail : '')],
        ['top 10 share', dz.top_10_percent ?? 'missing'], ['largest account share', dz.largest_account_share !== null && dz.largest_account_share !== undefined ? pct(dz.largest_account_share) + ' (may be pool vault)' : 'missing'],
        ['holders', dz.holder_count ?? 'missing'],
      ]));
      body.append(h('div', { class: 'section-t', text: 'VALUATION' }));
      body.append(dz.valuation ? kv(Object.entries(dz.valuation).map(([k, v]) => [k, typeof v === 'number' && (k.endsWith('_at') || k.endsWith('_ts')) ? utc(v) + ' UTC' : String(v)])) : h('div', { class: 'muted', text: 'none computed' }));
      body.append(h('div', { class: 'section-t', text: 'SOCIAL EVIDENCE' }));
      const so = dz.social;
      body.append(so ? kv([['status', so.status], ['handle', so.handle || 'none published'], ['source', so.handle_source], ['adapter', so.adapter]]) : h('div', { class: 'muted', text: 'not reached (candidate stopped before SOCIAL)' }));
    } else body.append(h('div', { class: 'muted', text: 'not vetted yet — no dossier spent on this candidate' }));
    body.append(h('div', { class: 'section-t', text: 'GAPS' }), h('div', {}, d.evidence_gaps.map(g => h('span', { class: 'chip gap', text: g }))));
    if (d.bench) body.append(h('div', { class: 'section-t', text: 'COOLDOWN' }), kv([['reason', d.bench.reason], ['until', utc(d.bench.until) + ' UTC'], ['policy', d.bench.policy], ['honoured now', String(d.bench.valid)]]));
    body.append(h('div', { class: 'section-t', text: 'DECISION HISTORY' }));
    body.append(h('div', {}, d.evaluations.map(e => h('div', { class: 'log-row ' + (e.outcome === 'reject' ? 'warn' : '') },
      h('span', { class: 'dim', text: utc(e.at) }), h('span', { text: e.stage }), h('span', { text: e.outcome }), h('span', { class: 'msg', text: (e.reason || '') + ' · ' + e.policy + ' · settings v' + e.settings_version })))));
    if (d.judgments.length) {
      body.append(h('div', { class: 'section-t', text: 'JUDGMENTS (raw, with provenance)' }));
      d.judgments.forEach(j => body.append(h('div', { class: 'muted small', text: `${utc(j.at)} · ${j.engine} · ${j.model_id || '—'} · ${j.question_set} · ${j.score_type} · ${j.threshold_version}` }),
        h('pre', { text: JSON.stringify({ result: j.result, raw: j.raw }, null, 1) })));
    }
    $('#drawer').classList.add('open'); $('#drawer').setAttribute('aria-hidden', 'false'); $('#scrim').hidden = false;
    $('#drawerClose').focus();
  }
  function closeDrawer() { $('#drawer').classList.remove('open'); $('#drawer').setAttribute('aria-hidden', 'true'); $('#scrim').hidden = true; }
  $('#drawerClose').addEventListener('click', closeDrawer); $('#scrim').addEventListener('click', closeDrawer);
  document.addEventListener('keydown', e => { if (e.key === 'Escape') closeDrawer(); });

  // ------------------------------------------------------------ book tab
  async function renderBook() {
    const root = clear($('#tab-book'));
    const trades = await get('/api/trades');
    const a = S.account || {}, st = S.stats, P = S.position, f = S.fees;
    const cards = h('div', { class: 'cards' });
    cards.append(h('div', { class: 'card' }, h('h4', { text: 'PAPER ACCOUNT' }), kv([['cash', usd(a.cash_usd)], ['starting cash', usd(a.starting_cash_usd)], ['mode', S.execution_mode + ' · ' + S.book_mode + ' book']])));
    cards.append(h('div', { class: 'card' }, h('h4', { text: 'FEE & SLIPPAGE ASSUMPTIONS (simulated)' }), kv([
      ['fee per side', `${(f.fee_rate * 100).toFixed(2)}% (min ${usd(f.min_fee_usd)})`], ['base slippage', f.base_slippage_bps + ' bps'],
      ['impact', `x${f.impact_factor} · ticket/liquidity`], ['max entry slippage', f.max_slippage_bps + ' bps']])));
    cards.append(h('div', { class: 'card' }, h('h4', { text: 'SUMMARY (net of fees & slippage)' }), kv([
      ['closed trades', st.trades], ['win rate', st.win_rate === null ? '— (no trades)' : pct(st.win_rate, 0)],
      ['net pnl', st.total_pnl_usd === null ? '—' : usd(st.total_pnl_usd)], ['avg / best / worst', st.trades ? `${usd(st.avg_pnl_usd)} / ${usd(st.best_pnl_usd)} / ${usd(st.worst_pnl_usd)}` : '—'],
      ['fees paid', st.total_fees_usd === null ? '—' : usd(st.total_fees_usd)], ['avg hold', st.avg_hold_minutes === null ? '—' : st.avg_hold_minutes + 'm'],
      ['note', 'paper results; no guarantee of future outcomes']])));
    if (P) {
      cards.append(h('div', { class: 'card' }, h('h4', { text: 'OPEN POSITION · RISK' }), kv([
        ['token', `${P.symbol || '?'} (${short(P.pool_address)})`], ['status', P.status + (P.close_reason ? ' · ' + P.close_reason : '')],
        ['qty', Number(P.qty).toLocaleString('en-US', { maximumFractionDigits: 2 })], ['ticket', usd(P.ticket_usd)],
        ['quote / fill', `${price(P.quote_price)} / ${price(P.entry_price)} (${P.entry_slippage_bps} bps)`], ['entry fee', usd(P.entry_fee_usd)],
        ['last mark', `${price(P.last_mark_price)} · ${P.mark_age_s ?? '—'}s old`], ['unrealised', usd(P.unrealized_usd_before_exit_costs) + ' before exit costs'],
        ['held', mins(P.held_minutes)], ['next RISK check', P.next_risk_in_s + 's'], ['failed marks', P.mark_fail_count],
        ['exits', `stop ${price(P.exits.stop_loss_price)} · target ${price(P.exits.take_profit_price)} · max hold ${utc(P.exits.max_hold_at)} UTC · vol ratio < ${P.exits.volume_ratio_exit} · ${P.exits.quote_fail_close_after} failed marks → close pending`]])));
    } else cards.append(h('div', { class: 'card' }, h('h4', { text: 'OPEN POSITION' }), h('div', { class: 'muted', text: 'none — the book is free' })));
    root.append(cards);
    root.append(h('div', { class: 'section-t', text: 'CLOSED PAPER TRADES' }));
    const tbl = h('table', { class: 'table' }, h('thead', {}, h('tr', {}, ['Closed', 'Token', 'Ticket', 'Entry', 'Exit', 'Fees', 'Slip in/out', 'PnL (net)', 'Reason', 'Engine'].map(t => h('th', { text: t })))));
    const tb = h('tbody');
    if (!trades.length) tb.append(h('tr', {}, h('td', { colspan: 10, class: 'muted', text: 'no closed trades' })));
    trades.forEach(t => tb.append(h('tr', {}, h('td', { text: utc(t.closed_at) + ' · ' + ago(t.closed_at) }), h('td', { text: t.symbol || short(t.pool_address) }),
      h('td', { text: usd(t.ticket_usd) }), h('td', { text: price(t.entry_price) }), h('td', { text: price(t.exit_price) }),
      h('td', { text: usd(Number(t.entry_fee_usd) + Number(t.exit_fee_usd)) }), h('td', { text: `${t.entry_slippage_bps}/${t.exit_slippage_bps}` }),
      h('td', { class: Number(t.pnl_usd) >= 0 ? 'good' : 'bad', text: usd(t.pnl_usd) }), h('td', { text: t.close_reason }),
      h('td', { text: (t.decision && t.decision.engine) || '—' }))));
    tbl.append(tb); root.append(h('div', { class: 'table-wrap' }, tbl));
  }

  // ------------------------------------------------------------ event log
  const logSeat = $('#logSeat'); SEATS.forEach(sn => logSeat.append(h('option', { value: sn, text: sn })));
  let logRows = [];
  function logRow(e) {
    const r = h('div', { class: 'log-row ' + e.level }, h('span', { class: 'dim', text: utc(e.at) }), h('span', { class: 'seat-tag', text: e.seat }),
      h('span', { class: 'lvl dim', text: e.level }), h('span', { class: 'msg', text: e.message }));
    r.querySelector('.seat-tag').style.color = COLORS[e.seat] || '';
    return r;
  }
  async function renderLog() {
    const q = encodeURIComponent($('#logSearch').value.trim()), seat = logSeat.value, lvl = $('#logLevel').value;
    logRows = await get(`/api/events?q=${q}&seat=${seat}&level=${lvl}&limit=300`);
    const root = clear($('#log'));
    if (!logRows.length) root.append(h('div', { class: 'muted', text: 'no events match' }));
    logRows.forEach(e => root.append(logRow(e)));
    const qs = `q=${q}&seat=${seat}&level=${lvl}`;
    $('#exportJson').href = '/api/events/export?format=json&' + qs; $('#exportCsv').href = '/api/events/export?format=csv&' + qs;
  }
  let logT; $('#logSearch').addEventListener('input', () => { clearTimeout(logT); logT = setTimeout(renderLog, 250); });
  logSeat.addEventListener('change', renderLog); $('#logLevel').addEventListener('change', renderLog);
  function appendLiveEvent(e) {
    if ($('#logSearch').value || logSeat.value || $('#logLevel').value) return;
    const root = $('#log'); root.prepend(logRow(e));
    while (root.childNodes.length > 300) root.removeChild(root.lastChild);
  }

  // ------------------------------------------------------------ settings
  async function renderSettings() {
    const root = clear($('#tab-settings'));
    const cfg = await get('/api/settings');
    root.append(h('div', { class: 'muted small', text: `Active v${cfg.version}. Changes create a new version and apply at the next cycle boundary (never mid-cycle). These are policy choices, not recommendations. TOTAL_SUPPLY_VALUATION limits are a separate v3 policy: price × verified total supply, not circulating market cap.` }));
    const form = h('form', { class: 'settings-grid', novalidate: true });
    const inputs = {};
    let group = null;
    for (const f of cfg.schema) {
      if (f.group !== group) { group = f.group; form.append(h('div', { class: 'group-t', text: group.replace('_', ' ').toUpperCase() })); }
      const id = 'set-' + f.key, val = cfg.values[f.key];
      let inp;
      if (f.type === 'bool') inp = h('select', { id }, h('option', { value: 'true', text: 'true', selected: val ? 'selected' : null }), h('option', { value: 'false', text: 'false', selected: !val ? 'selected' : null }));
      else if (f.key === 'judge_engine') inp = h('select', { id }, ['RULES', 'TYPED_AI'].map(o => h('option', { value: o, text: o, selected: val === o ? 'selected' : null })));
      else if (f.type === 'list') inp = h('input', { id, type: 'text', value: (val || []).join(',') });
      else if (f.type === 'str') inp = h('input', { id, type: 'text', value: val });
      else inp = h('input', { id, type: 'number', step: 'any', value: val, min: f.min ?? null, max: f.max ?? null });
      inputs[f.key] = { inp, f };
      form.append(h('div', { class: 'field', 'data-key': f.key }, h('label', { for: id, text: f.key }), inp, h('div', { class: 'help', text: f.label + (f.min !== null ? ` [${f.min}–${f.max}]` : '') })));
    }
    const note = h('input', { type: 'text', placeholder: 'change note (optional)', maxlength: 200 });
    const save = h('button', { class: 'btn on', type: 'submit', text: 'Save new version' });
    const msg = h('span', { class: 'small muted' });
    form.append(h('div', { class: 'group-t' }, h('div', { class: 'toolbar' }, note, save, msg)));
    form.addEventListener('submit', async ev => {
      ev.preventDefault(); if (save.disabled) return;
      form.querySelectorAll('.field').forEach(x => { x.classList.remove('err'); const t = x.querySelector('.errt'); if (t) t.remove(); });
      const values = {};
      for (const [k, { inp, f }] of Object.entries(inputs)) {
        let v = inp.value;
        if (f.type === 'bool') v = v === 'true';
        else if (f.type === 'list') v = v.split(',').map(x => x.trim()).filter(Boolean);
        else if (f.type === 'int' || f.type === 'float') v = v === '' ? NaN : Number(v);
        if (JSON.stringify(v) !== JSON.stringify(cfg.values[k])) values[k] = v;
      }
      if (!Object.keys(values).length) { msg.textContent = 'no changes'; return; }
      save.disabled = true;
      const r = await post('/api/settings', { values, note: note.value, expected_version: cfg.version });
      save.disabled = false;
      if (r.ok) { msg.textContent = `saved v${r.body.version}; applies at the next cycle boundary`; renderSettings(); }
      else if (r.body.fields) {
        msg.textContent = 'not saved: fix the highlighted fields';
        for (const [k, e] of Object.entries(r.body.fields)) {
          const fld = form.querySelector(`.field[data-key="${CSS.escape(k)}"]`);
          if (fld) { fld.classList.add('err'); fld.append(h('div', { class: 'errt', text: e })); } else msg.textContent += ` (${k}: ${e})`;
        }
      } else msg.textContent = r.body.error || 'error';
    });
    root.append(form);
    root.append(h('div', { class: 'section-t', text: 'VERSION HISTORY' }), h('div', {}, cfg.history.map(x => h('div', { class: 'small', text: `v${x.version} · ${new Date(x.created_at * 1000).toISOString().slice(0, 19)}Z · ${x.note || ''}` }))));
  }

  // ------------------------------------------------------------ health
  function renderHealth() {
    const root = clear($('#tab-health'));
    const w = S.worker;
    const cards = h('div', { class: 'cards' });
    cards.append(h('div', { class: 'card' }, h('h4', { text: 'WORKER' }), kv([
      ['state', w.alive ? w.state : 'stopped'], ['activity', w.activity || 'idle'], ['scanning', String(w.scanning)],
      ['last cycle', w.last_cycle ? `#${w.last_cycle.id} ${w.last_cycle.outcome} ${w.last_cycle.reason || ''}` : '—'],
      ['last error', w.last_error || 'none'], ['lease', w.lease ? `${w.lease.owner} · ${w.lease.live ? 'live' : 'expired'}` : 'none']])));
    cards.append(h('div', { class: 'card' }, h('h4', { text: 'JUDGE' }), kv([['engine', S.judge.engine], ['configured', String(S.judge.configured)], ['detail', S.judge.detail], ['score type', S.judge.score_type]])));
    cards.append(h('div', { class: 'card' }, h('h4', { text: 'BOOK' }), kv([['mode', S.book_mode + (S.demo ? ' (synthetic, isolated)' : '')], ['execution', S.execution_mode],
      ['watchlist', Object.entries(S.watchlist_counts).map(([k, v]) => `${k} ${v}`).join(' · ') || 'empty'],
      ['cooldowns honoured', Object.entries(S.bench).map(([k, v]) => `${k} ${v}`).join(' · ') || 'none'], ['RPC endpoint', S.rpc_endpoint]])));
    root.append(cards);
    root.append(h('div', { class: 'section-t', text: 'PROVIDERS (real request outcomes)' }));
    const pt = h('table', { class: 'table' }, h('thead', {}, h('tr', {}, ['Provider', 'State', 'Last ok', 'Last error', 'Error'].map(t => h('th', { text: t })))));
    const pb = h('tbody');
    if (!S.providers.length) pb.append(h('tr', {}, h('td', { colspan: 5, class: 'muted', text: 'no requests made yet' })));
    S.providers.forEach(p => pb.append(h('tr', {}, h('td', { text: p.name }), h('td', { class: p.state === 'ok' ? 'good' : 'warn', text: p.state }),
      h('td', { text: ago(p.last_ok_at) }), h('td', { text: ago(p.last_error_at) }), h('td', { text: p.last_error || '' }))));
    pt.append(pb); root.append(h('div', { class: 'table-wrap' }, pt));
    root.append(h('div', { class: 'section-t', text: 'INTEGRATIONS' }));
    const it = h('table', { class: 'table' }, h('thead', {}, h('tr', {}, ['Integration', 'Role', 'Status', 'Detail'].map(t => h('th', { text: t })))));
    const ib = h('tbody');
    S.integrations.forEach(i => ib.append(h('tr', {}, h('td', { text: i.name }), h('td', { text: i.role }),
      h('td', { class: ['connected', 'configured', 'enabled'].includes(i.status) ? 'good' : 'warn', text: i.status }), h('td', { text: i.detail }))));
    it.append(ib); root.append(h('div', { class: 'table-wrap' }, it));
  }

  // ------------------------------------------------------------ tabs
  let activeTab = 'watch';
  try { activeTab = localStorage.getItem('pb-tab') || 'watch'; } catch { /* ignore */ }
  function showTab(t) {
    activeTab = t;
    try { localStorage.setItem('pb-tab', t); } catch { /* ignore */ }
    document.querySelectorAll('.tab').forEach(b => { b.classList.toggle('active', b.dataset.tab === t); b.setAttribute('aria-selected', b.dataset.tab === t); });
    document.querySelectorAll('.tab-body').forEach(b => { b.hidden = b.id !== 'tab-' + t; });
    renderTab(true);
  }
  function renderTab(force) {
    if (!S) return;
    if (activeTab === 'watch') renderWatchlist().catch(() => {});
    else if (activeTab === 'book') renderBook().catch(() => {});
    else if (activeTab === 'log' && force) renderLog().catch(() => {});
    else if (activeTab === 'settings' && force) renderSettings().catch(() => {});
    else if (activeTab === 'health') renderHealth();
  }
  document.querySelectorAll('.tab').forEach(b => b.addEventListener('click', () => showTab(b.dataset.tab)));

  // ------------------------------------------------------------ state flow
  let lastFocusFetch = 0, lastBalance = 0, lastTab = 0;
  async function applyState(st) {
    S = st;
    renderTop(); renderSeats(); renderFloor(); renderAnalysis(); renderFunnel();
    const key = S.position ? S.position.chain + ':' + S.position.pool_address : (S.focus ? S.focus.chain + ':' + S.focus.pool_address : null);
    if (key !== focusKey || Date.now() - lastFocusFetch > 10000) { lastFocusFetch = Date.now(); await loadFocus(); }
    renderThresholds();
    if (Date.now() - lastBalance > 15000) { lastBalance = Date.now(); renderBalance().catch(() => {}); }
    if (Date.now() - lastTab > 8000 && activeTab !== 'settings' && activeTab !== 'log') { lastTab = Date.now(); renderTab(false); }
  }
  async function refresh() { try { await applyState(await get('/api/state')); } catch (e) { say('backend unreachable: ' + e.message, 'bad'); } }

  let es = null, pollTimer = null;
  function connect() {
    es = new EventSource('/api/stream');
    es.addEventListener('state', m => { $('#streamText').textContent = 'live stream'; applyState(JSON.parse(m.data)); });
    es.addEventListener('ev', m => {
      const e = JSON.parse(m.data);
      if (activeTab === 'log') appendLiveEvent(e);
      if (SEATS.includes(e.seat)) { handoff(lastSeat, e.seat); lastSeat = e.seat; }
      if (e.kind === 'cycle_end' || e.kind === 'paper_close' || e.kind === 'paper_fill') { lastBalance = 0; lastTab = 0; }
    });
    es.onerror = () => {
      $('#streamText').textContent = 'stream reconnecting (polling)';
      if (!pollTimer) pollTimer = setInterval(refresh, 4000);
    };
    es.onopen = () => { if (pollTimer) { clearInterval(pollTimer); pollTimer = null; } };
  }

  buildSeats(); buildFloor();
  refresh().then(() => { showTab(activeTab); renderBalance().catch(() => {}); });
  connect();
})();
