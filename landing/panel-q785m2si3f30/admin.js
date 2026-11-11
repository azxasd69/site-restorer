/* Панель аналитики lastcopy.ru.
   Данные лежат в data.bin: AES-256-GCM, ключ из пароля через PBKDF2-SHA256.
   Живых запросов к API нет — только чтение снимка, собранного сборщиком. */

(() => {
  'use strict';

  const $ = (sel, root = document) => root.querySelector(sel);
  const nf = new Intl.NumberFormat('ru-RU');

  const fmtInt = (v) => (v === null || v === undefined || Number.isNaN(v) ? '—' : nf.format(Math.round(v)));
  const fmt1 = (v) => (v === null || v === undefined || Number.isNaN(v) ? '—' : nf.format(Math.round(v * 10) / 10));
  const fmtPct = (v) => (v === null || v === undefined || Number.isNaN(v) ? '—' : fmt1(v) + ' %');
  const fmtDur = (s) => {
    if (s === null || s === undefined || Number.isNaN(s)) return '—';
    const t = Math.round(s);
    const m = Math.floor(t / 60);
    return m ? `${m} мин ${t % 60} с` : `${t} с`;
  };
  const fmtBytes = (b) => {
    if (!b && b !== 0) return '—';
    if (b < 1024) return b + ' Б';
    if (b < 1024 * 1024) return fmt1(b / 1024) + ' КБ';
    return fmt1(b / 1024 / 1024) + ' МБ';
  };
  const fmtDate = (iso) => {
    if (!iso) return '—';
    const d = new Date(iso.length <= 10 ? iso + 'T00:00:00' : iso);
    return d.toLocaleDateString('ru-RU', { day: '2-digit', month: '2-digit', year: 'numeric' });
  };
  const fmtDateTime = (iso) => {
    if (!iso) return '—';
    const d = new Date(iso);
    return d.toLocaleString('ru-RU', { day: '2-digit', month: '2-digit', year: 'numeric', hour: '2-digit', minute: '2-digit' });
  };
  const fmtShort = (iso) => {
    const d = new Date(iso + 'T00:00:00');
    return d.toLocaleDateString('ru-RU', { day: '2-digit', month: '2-digit' });
  };
  const visitWord = (n) => {
    const v = Math.abs(Number(n) || 0);
    const d10 = v % 10;
    const d100 = v % 100;
    if (d10 === 1 && d100 !== 11) return 'визит';
    if (d10 >= 2 && d10 <= 4 && (d100 < 12 || d100 > 14)) return 'визита';
    return 'визитов';
  };

  const esc = (s) => String(s === null || s === undefined ? '' : s)
    .replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;').replace(/'/g, '&#39;');

  /* ───────────── расшифровка ───────────── */

  const MAGIC = 'SRAD';
  const state = { data: null, period: 30 };

  async function loadEnvelope() {
    const res = await fetch('data.bin', { cache: 'no-cache' });
    if (!res.ok) throw new Error('ENVELOPE');
    const buf = new Uint8Array(await res.arrayBuffer());
    if (buf.length < 40) throw new Error('ENVELOPE');
    const magic = String.fromCharCode(buf[0], buf[1], buf[2], buf[3]);
    if (magic !== MAGIC) throw new Error('ENVELOPE');
    const version = buf[4];
    const iterations = new DataView(buf.buffer, buf.byteOffset).getUint32(5, false);
    const salt = buf.slice(9, 25);
    const iv = buf.slice(25, 37);
    const ct = buf.slice(37);
    return { version, iterations, salt, iv, ct };
  }

  async function decrypt(env, password) {
    const enc = new TextEncoder();
    const base = await crypto.subtle.importKey('raw', enc.encode(password), 'PBKDF2', false, ['deriveKey']);
    const key = await crypto.subtle.deriveKey(
      { name: 'PBKDF2', salt: env.salt, iterations: env.iterations, hash: 'SHA-256' },
      base, { name: 'AES-GCM', length: 256 }, false, ['decrypt']);
    const plain = await crypto.subtle.decrypt({ name: 'AES-GCM', iv: env.iv }, key, env.ct);
    return JSON.parse(new TextDecoder().decode(plain));
  }

  /* ───────────── расчёты по периоду ───────────── */

  function windowOf(data, days) {
    const daily = (data.metrika && data.metrika.daily) || [];
    if (!daily.length) return { cur: [], prev: [] };
    const end = daily.length;
    const cur = daily.slice(Math.max(0, end - days), end);
    const prev = daily.slice(Math.max(0, end - days * 2), Math.max(0, end - days));
    return { cur, prev };
  }

  const sum = (rows, key) => rows.reduce((a, r) => a + (r[key] || 0), 0);

  const dimsOf = (data, key) => (((data.metrika || {}).dims || {})[key] || {})[String(state.period)] || [];
  const extraOf = (data, key) => (((data.metrika || {}).extra || {})[key]);
  const sourceQuality = (data) => ((data.metrika || {}).source_quality || []);
  const goalsOf = (data) => (((data.metrika || {}).goals_by_period || {})[String(state.period)]) || ((data.metrika || {}).goals || []);
  const weighted = (rows, key, weight) => {
    const w = sum(rows, weight);
    if (!w) return null;
    return rows.reduce((a, r) => a + (r[key] || 0) * (r[weight] || 0), 0) / w;
  };

  function pctChange(cur, prev) {
    if (!prev) return null;
    return ((cur - prev) / prev) * 100;
  }

  function deltaHtml(cur, prev) {
    const d = pctChange(cur, prev);
    if (d === null) return '<span class="kpi-delta">нет базы</span>';
    const sign = d > 0 ? '+' : '';
    const cls = d > 0.5 ? 'up' : d < -0.5 ? 'down' : '';
    const arrow = d > 0.5 ? '▲' : d < -0.5 ? '▼' : '·';
    return `<span class="kpi-delta"><span class="${cls}">${arrow} ${sign}${fmt1(d)} %</span> к прошлому</span>`;
  }

  /* ───────────── график ───────────── */

  function chartSvg(labels, series, opts = {}) {
    const W = 720, H = opts.height || 200;
    const padL = 6, padR = 6, padT = 12, padB = 22;
    const n = labels.length;
    if (n < 2) return '';
    const max = Math.max(1, ...series.flatMap((s) => s.values));
    const x = (i) => padL + (i * (W - padL - padR)) / (n - 1);
    const y = (v) => padT + (H - padT - padB) * (1 - v / max);

    let grid = '';
    for (let g = 0; g <= 2; g++) {
      const val = (max / 2) * g;
      const yy = y(val);
      grid += `<line class="grid-line" x1="${padL}" y1="${yy}" x2="${W - padR}" y2="${yy}"/>`;
      grid += `<text class="axis" x="${padL + 2}" y="${yy - 3}">${fmtInt(val)}</text>`;
    }

    const paths = series.map((s) => {
      const d = s.values.map((v, i) => `${i ? 'L' : 'M'}${x(i).toFixed(1)},${y(v).toFixed(1)}`).join(' ');
      let out = '';
      if (s.area) {
        out += `<path class="area" d="${d} L${x(n - 1).toFixed(1)},${H - padB} L${padL},${H - padB} Z"/>`;
      }
      out += `<path class="${s.cls}" d="${d}" ${s.dash ? 'stroke-dasharray="3 3"' : ''}/>`;
      return out;
    }).join('');

    const dots = n <= 45
      ? series.filter((s) => s.dots).map((s) => s.values.map((v, i) =>
        `<circle class="dot" cx="${x(i).toFixed(1)}" cy="${y(v).toFixed(1)}" r="2.4"/>`).join('')).join('')
      : '';

    const ticks = [0, Math.floor((n - 1) / 2), n - 1];
    const axis = ticks.map((i) =>
      `<text class="axis" x="${x(i).toFixed(1)}" y="${H - 6}" text-anchor="${i === 0 ? 'start' : i === n - 1 ? 'end' : 'middle'}">${esc(labels[i])}</text>`).join('');

    return `<svg viewBox="0 0 ${W} ${H}" preserveAspectRatio="none" role="img" aria-label="${esc(opts.label || 'График')}">
      ${grid}${paths}${dots}${axis}
      <rect id="chart-hit" x="0" y="0" width="${W}" height="${H}" fill="transparent"/>
    </svg>`;
  }

  function attachChart(root, labels, series) {
    const svg = $('svg', root);
    const tip = $('.chart-tip', root);
    if (!svg || !tip) return;
    const hit = $('#chart-hit', svg);
    if (!hit) return;
    const n = labels.length;
    const W = 720, padL = 6, padR = 6;
    const move = (ev) => {
      const box = svg.getBoundingClientRect();
      const rel = ((ev.clientX - box.left) / box.width) * W;
      const i = Math.max(0, Math.min(n - 1, Math.round(((rel - padL) / (W - padL - padR)) * (n - 1))));
      tip.innerHTML = `<strong>${esc(labels[i])}</strong><br>` +
        series.map((s) => `${esc(s.name)}: ${fmtInt(s.values[i])}`).join('<br>');
      tip.style.left = `${((padL + (i * (W - padL - padR)) / (n - 1)) / W) * 100}%`;
      tip.style.top = `${(series[0].values[i] / Math.max(1, Math.max(...series.flatMap((s) => s.values)))) * 100}%`;
      tip.classList.add('is-on');
    };
    svg.addEventListener('pointermove', move);
    svg.addEventListener('pointerleave', () => tip.classList.remove('is-on'));
  }

  /* ───────────── блоки ───────────── */

  function kpiBlock(data) {
    const days = state.period;
    const { cur, prev } = windowOf(data, days);
    const p = (data.metrika.periods || {})[String(days)] || {};
    const visits = p.visits ?? sum(cur, 'visits');
    const users = p.users ?? Math.max(0, ...cur.map((r) => r.users || 0));
    const views = p.pageviews ?? sum(cur, 'pageviews');
    const bounce = p.bounce_rate ?? weighted(cur, 'bounce', 'visits');
    const dur = p.avg_duration ?? weighted(cur, 'duration', 'visits');

    const goals = goalsOf(data);
    const dl = goals.find((g) => !g.auto && /download/i.test(g.name));
    const sp = goals.find((g) => !g.auto && /support/i.test(g.name));
    const conv = dl && visits ? (dl.reaches / visits) * 100 : null;
    const gh = data.github || {};

    return `<section class="card">
      <div class="card-head"><h2>Ключевые числа</h2><span class="note">период: ${days} дней</span></div>
      <div class="kpi-grid">
        <div class="kpi"><div class="kpi-label">Визиты</div><div class="kpi-value">${fmtInt(visits)}</div>${deltaHtml(visits, sum(prev, 'visits'))}</div>
        <div class="kpi"><div class="kpi-label">Посетители</div><div class="kpi-value">${fmtInt(users)}</div><span class="kpi-delta">уникальные за период</span></div>
        <div class="kpi"><div class="kpi-label">Просмотры</div><div class="kpi-value">${fmtInt(views)}</div>${deltaHtml(views, sum(prev, 'pageviews'))}</div>
        <div class="kpi"><div class="kpi-label">Отказ</div><div class="kpi-value">${fmtPct(bounce)}</div><span class="kpi-delta">доля одиночных визитов</span></div>
        <div class="kpi"><div class="kpi-label">Среднее время</div><div class="kpi-value" style="font-size:19px">${fmtDur(dur)}</div><span class="kpi-delta">на визит</span></div>
        <div class="kpi is-accent"><div class="kpi-label">Скачивания</div><div class="kpi-value">${fmtInt(dl ? dl.reaches : null)}</div><span class="kpi-delta">цель за ${days} дн.${conv !== null ? ' · конверсия ' + fmtPct(conv) : ''}</span></div>
        <div class="kpi"><div class="kpi-label">Поддержка</div><div class="kpi-value">${fmtInt(sp ? sp.reaches : null)}</div><span class="kpi-delta">переходы в Boosty</span></div>
        <div class="kpi is-accent"><div class="kpi-label">GitHub, всего</div><div class="kpi-value">${fmtInt(gh.downloads_total)}</div><span class="kpi-delta">накопительно по релизам</span></div>
      </div>
    </section>`;
  }

  function chartBlock(data) {
    const { cur } = windowOf(data, state.period);
    const labels = cur.map((r) => fmtShort(r.date));
    const series = [
      { name: 'Визиты', values: cur.map((r) => r.visits || 0), cls: 'line-visits', area: true, dots: true },
      { name: 'Посетители', values: cur.map((r) => r.users || 0), cls: 'line-users', dash: true },
    ];
    const active = cur.filter((r) => (r.visits || 0) > 0).length;
    const note = active < 3 ? `трафик есть только в ${active} ${active === 1 ? 'дне' : 'днях'} из ${cur.length}` : '';

    return `<section class="card chart">
      <div class="card-head"><h2>Трафик по дням</h2><span class="note">${esc(note)}</span></div>
      ${chartSvg(labels, series, { label: 'Визиты и посетители по дням' })}
      <div class="chart-tip" role="presentation"></div>
      <div class="legend"><span><i></i>визиты</span><span><i class="dash"></i>посетители</span></div>
    </section>`;
  }

  function funnelBlock(data) {
    const days = state.period;
    const goals = goalsOf(data).filter((g) => !g.auto);
    const get = (re) => (goals.find((g) => re.test(g.name)) || {}).reaches || 0;
    const visits = ((data.metrika.periods || {})[String(days)] || {}).visits || 0;
    const dl = get(/download/i);
    const sp = get(/support/i);
    const base = Math.max(visits, 1);
    const row = (label, value, cls, share, cap) => `<div class="funnel-row">
        <span class="fl">${esc(label)}</span>
        <div class="funnel-bar"><span class="${cls}" style="width:${Math.max(1.2, Math.min(100, share * 100))}%"></span></div>
        <span class="fv">${fmtInt(value)}${cap ? `<br><span class="note">${cap}</span>` : ''}</span>
      </div>`;
    return `<section class="card">
      <div class="card-head"><h2>Воронка</h2><span class="note">за ${days} дней</span></div>
      <div class="funnel">
        ${row('Визит на сайт', visits, '', 1, '100 %')}
        ${row('Скачивание', dl, 'warm', dl / base, fmtPct((dl / base) * 100))}
        ${row('Поддержка (Boosty)', sp, 'cool', sp / base, fmtPct((sp / base) * 100))}
      </div>
      <p class="note" style="margin:12px 0 0">Ширина полосы — доля от визитов. Скачивания редкие: два клика на 90 визитов — это 2 %, а не ошибка.</p>
    </section>`;
  }

  function tableBlock(title, rows, note, emptyText, extra) {
    if (!rows || !rows.length) {
      return `<section class="card"><div class="card-head"><h2>${esc(title)}</h2></div>
        <div class="empty"><strong>Нет данных</strong>${esc(emptyText || 'За выбранный период значений нет.')}</div></section>`;
    }
    const max = Math.max(1, ...rows.map((r) => r.value));
    const total = rows.reduce((a, r) => a + r.value, 0) || 1;
    const body = rows.map((r) => `<tr>
        <td class="name" title="${esc(r.name)}">${esc(r.name)}</td>
        <td class="num">${fmtInt(r.value)}</td>
        <td class="num">${fmtPct((r.value / total) * 100)}</td>
        <td class="cell-bar"><div class="bar-track"><div class="bar-fill" style="width:${(r.value / max) * 100}%"></div></div></td>
      </tr>`).join('');
    return `<section class="card">
      <div class="card-head"><h2>${esc(title)}</h2>${note ? `<span class="note">${esc(note)}</span>` : ''}</div>
      <table><thead><tr><th>Значение</th><th class="num">Визиты</th><th class="num">Доля</th><th></th></tr></thead>
      <tbody>${body}</tbody></table>
      ${extra || ''}
    </section>`;
  }

  function sourcesBlock(data) {
    const rows = sourceQuality(data);
    const dims = dimsOf(data, 'sources');
    if (!rows.length && !dims.length) {
      return `<section class="card"><div class="card-head"><h2>Источники трафика</h2></div>
        <div class="empty"><strong>Нет данных</strong>Метрика не вернула источники за этот период.</div></section>`;
    }
    const list = rows.length ? rows : dims.map((d) => ({ name: d.name, visits: d.value, bounce: null, downloads: null }));
    const max = Math.max(1, ...list.map((r) => r.visits));
    const total = list.reduce((a, r) => a + r.visits, 0) || 1;
    const body = list.map((r) => `<tr>
        <td class="name" title="${esc(r.name)}">${esc(r.name)}</td>
        <td class="num">${fmtInt(r.visits)}</td>
        <td class="num">${r.bounce === null || r.bounce === undefined ? '—' : fmtPct(r.bounce)}</td>
        <td class="num">${r.downloads === null || r.downloads === undefined ? '—' : fmtInt(r.downloads)}</td>
        <td class="num">${fmtPct((r.visits / total) * 100)}</td>
        <td class="cell-bar"><div class="bar-track"><div class="bar-fill" style="width:${(r.visits / max) * 100}%"></div></div></td>
      </tr>`).join('');
    return `<section class="card">
      <div class="card-head"><h2>Источники трафика</h2><span class="note">отказы и скачивания по каналу, 30 дней</span></div>
      <table><thead><tr><th>Канал</th><th class="num">Визиты</th><th class="num">Отказы</th><th class="num">Скачали</th><th class="num">Доля</th><th></th></tr></thead>
      <tbody>${body}</tbody></table>
      <p class="note" style="margin:10px 0 0">Отказы считаются по визитам канала: у прямых заходов они выше, потому что часть из них — повторные открытия своей же ссылки.</p>
      ${extraSources(data)}
    </section>`;
  }

  function hoursBlock(data) {
    const hours = extraOf(data, 'hours') || [];
    if (!hours.length) {
      return `<section class="card"><div class="card-head"><h2>Время суток</h2></div>
        <div class="empty"><strong>Нет данных</strong>Метрика не вернула разбивку по часам.</div></section>`;
    }
    const max = Math.max(1, ...hours.map((h) => h.visits));
    const rows = hours.map((h) => `<div class="hour-row">
        <span class="hour-label">${esc(h.hour)}:00</span>
        <div class="bar-track"><div class="bar-fill" style="width:${(h.visits / max) * 100}%"></div></div>
        <span class="num">${fmtInt(h.visits)}</span>
      </div>`).join('');
    const peak = hours.reduce((a, h) => (h.visits > a.visits ? h : a), hours[0]);
    return `<section class="card">
      <div class="card-head"><h2>Время суток</h2><span class="note">визиты по часам, 30 дней</span></div>
      <div class="hour-grid">${rows}</div>
      <p class="note" style="margin:10px 0 0">Пик — ${esc(peak.hour)}:00 (${fmtInt(peak.visits)} визитов). Время московское, как в отчётах Метрики.</p>
    </section>`;
  }

  function popularBlock(data) {
    const rows = (extraOf(data, 'popular_pages') || []).map((r) => ({ name: r.name, value: r.value }));
    const text = rows.length ? '' : 'Метрика не вернула просмотры страниц.';
    const max = Math.max(1, ...rows.map((r) => r.value));
    const total = rows.reduce((a, r) => a + r.value, 0) || 1;
    const body = rows.map((r) => `<tr>
        <td class="name" title="${esc(r.name)}">${esc(r.name)}</td>
        <td class="num">${fmtInt(r.value)}</td>
        <td class="num">${fmtPct((r.value / total) * 100)}</td>
        <td class="cell-bar"><div class="bar-track"><div class="bar-fill" style="width:${(r.value / max) * 100}%"></div></div></td>
      </tr>`).join('');
    return `<section class="card">
      <div class="card-head"><h2>Популярные страницы</h2><span class="note">просмотры, 30 дней</span></div>
      ${rows.length ? `<table><thead><tr><th>Страница</th><th class="num">Просмотры</th><th class="num">Доля</th><th></th></tr></thead><tbody>${body}</tbody></table>`
        : `<div class="empty"><strong>Нет данных</strong>${esc(text)}</div>`}
    </section>`;
  }

  function extraSources(data) {
    const refs = dimsOf(data, 'referrers');
    const phrases = dimsOf(data, 'search_phrases');
    const list = (rows) => (rows.length
      ? `<table><tbody>${rows.slice(0, 6).map((r) => `<tr><td class="wrap">${esc(r.name)}</td><td class="num">${fmtInt(r.value)}</td></tr>`).join('')}</tbody></table>`
      : '<div class="empty" style="margin-top:6px"><strong>Ничего нет</strong>Переходов из поиска пока не было.</div>');
    return `<details>
      <summary>Ещё: рефереры и поисковые фразы</summary>
      <h3 class="eyebrow" style="margin:10px 0 6px">Площадки-рефереры</h3>
      ${list(refs)}
      <h3 class="eyebrow" style="margin:14px 0 6px">Поисковые фразы</h3>
      ${list(phrases)}
    </details>`;
  }

  function goalsBlock(data) {
    const goals = goalsOf(data);
    if (!goals.length) {
      return `<section class="card"><div class="card-head"><h2>Цели Метрики</h2></div>
        <div class="empty"><strong>Цели не получены</strong>Сборщик не смог прочитать список целей счётчика.</div></section>`;
    }
    const rows = goals.map((g) => `<tr>
        <td class="wrap" title="${esc(g.name)}">${esc(g.name.replace(/^Автоцель: /, 'автоцель · '))}</td>
        <td class="num">${fmtInt(g.reaches)}</td>
        <td class="num">${g.conversion === null || g.conversion === undefined ? '—' : fmt1(g.conversion) + ' %'}</td>
      </tr>`).join('');
    return `<section class="card">
      <div class="card-head"><h2>Цели Метрики</h2><span class="note">достижения и конверсия за ${state.period} дней</span></div>
      <table><thead><tr><th>Цель</th><th class="num">Достижений</th><th class="num">Конверсия</th></tr></thead>
      <tbody>${rows}</tbody></table>
    </section>`;
  }

  function audienceBlock(data) {
    const dims = (key) => dimsOf(data, key);
    const mini = (title, rows) => {
      if (!rows || !rows.length) return `<div><h3 class="eyebrow" style="margin:0 0 6px">${esc(title)}</h3><div class="empty">нет данных</div></div>`;
      const total = rows.reduce((a, r) => a + r.value, 0) || 1;
      const body = rows.slice(0, 5).map((r) => `<tr><td class="name" title="${esc(r.name)}">${esc(r.name)}</td><td class="num">${fmtPct((r.value / total) * 100)}</td></tr>`).join('');
      return `<div><h3 class="eyebrow" style="margin:0 0 6px">${esc(title)}</h3><table><tbody>${body}</tbody></table></div>`;
    };
    const loyalty = extraOf(data, 'loyalty') || [];
    const robots = extraOf(data, 'robots');
    const loyaltyRows = loyalty.map((l) => `<tr><td class="name">${esc(l.visits)} ${visitWord(l.visits)}</td><td class="num">${fmtInt(l.users)}</td></tr>`).join('');
    const loyaltyTotal = loyalty.reduce((a, l) => a + l.users, 0) || 1;
    const single = (loyalty.find((l) => l.visits === '1') || {}).users || 0;
    return `<section class="card">
      <div class="card-head"><h2>Аудитория</h2><span class="note">за ${state.period} дней</span></div>
      <div class="audience-grid">
        ${mini('Устройства', dims('devices'))}
        ${mini('Операционные системы', dims('os'))}
        ${mini('Браузеры', dims('browsers'))}
        ${mini('Страны', dims('countries'))}
      </div>
      <div class="audience-grid" style="margin-top:16px">
        <div><h3 class="eyebrow" style="margin:0 0 6px">Постоянство</h3>
          ${loyaltyRows ? `<table><tbody>${loyaltyRows}</tbody></table>` : '<div class="empty">нет данных</div>'}
          <p class="note" style="margin:8px 0 0">Сколько визитов принёс один посетитель за 30 дней. ${fmtPct((single / loyaltyTotal) * 100)} аудитории — один визит.</p>
        </div>
        <div><h3 class="eyebrow" style="margin:0 0 6px">Роботы</h3>
          ${robots ? `<table><tbody>
            <tr><td>Доля визитов роботов</td><td class="num">${fmtPct(robots.percent)}</td></tr>
            <tr><td>Визитов роботов</td><td class="num">${fmtInt(robots.visits)}</td></tr>
          </tbody></table>` : '<div class="empty">нет данных</div>'}
          <p class="note" style="margin:8px 0 0">Метрика отличает роботов по сигнатурам; их визиты в трафике и конверсиях не учитываются.</p>
        </div>
      </div>
    </section>`;
  }

  function githubBlock(data) {
    const g = data.github || {};
    if (g.status !== 'ok') {
      return `<section class="card"><div class="card-head"><h2>GitHub</h2></div>
        <div class="empty"><strong>Данные недоступны</strong>${esc(g.note || 'Токен GitHub не задан или API не ответил.')}</div></section>`;
    }
    const rel = (g.releases || [])[0];
    const assets = rel ? rel.assets.slice().sort((a, b) => b.downloads - a.downloads) : [];
    const rows = assets.map((a) => `<tr><td class="name" title="${esc(a.name)}">${esc(a.name)}</td>
        <td class="num">${fmtInt(a.downloads)}</td><td class="num">${fmtBytes(a.size)}</td></tr>`).join('');
    const vlabels = (g.views || []).map((v) => fmtShort(v.date));
    const vseries = [
      { name: 'Просмотры', values: (g.views || []).map((v) => v.count), cls: 'line-visits', area: true, dots: true },
      { name: 'Клоны', values: (g.clones || []).map((v) => v.count), cls: 'line-users' },
    ];
    return `<section class="card chart">
      <div class="card-head"><h2>GitHub</h2><span class="note">${esc(g.repo || '')}</span></div>
      <div class="kpi-grid" style="grid-template-columns:repeat(auto-fit,minmax(110px,1fr));margin-bottom:14px">
        <div class="kpi"><div class="kpi-label">Звёзды</div><div class="kpi-value">${fmtInt(g.stars)}</div></div>
        <div class="kpi"><div class="kpi-label">Форки</div><div class="kpi-value">${fmtInt(g.forks)}</div></div>
        <div class="kpi"><div class="kpi-label">Скачиваний</div><div class="kpi-value">${fmtInt(g.downloads_total)}</div></div>
        <div class="kpi"><div class="kpi-label">Релиз</div><div class="kpi-value" style="font-size:17px">${esc(rel ? rel.tag : '—')}</div><span class="kpi-delta">${rel ? esc(fmtDate(rel.published)) : ''}</span></div>
      </div>
      ${vlabels.length > 1 ? chartSvg(vlabels, vseries, { label: 'Просмотры репозитория по дням', height: 130 }) + '<div class="chart-tip" role="presentation"></div><div class="legend"><span><i></i>просмотры</span><span><i class="dash"></i>клоны</span></div>' : '<div class="empty">Статистика просмотров не пришла.</div>'}
      ${rows ? `<table style="margin-top:14px"><thead><tr><th>Файл релиза ${esc(rel.tag)}</th><th class="num">Скачиваний</th><th class="num">Размер</th></tr></thead><tbody>${rows}</tbody></table>` : ''}
      <p class="note" style="margin:10px 0 0">Скачивания ассетов — накопительные, счётчик не уменьшается. Статистика просмотров GitHub хранит 14 дней.</p>
      ${(g.popular_paths || []).length || (g.referrers || []).length ? `<details><summary>Откуда приходят на GitHub</summary>
        ${(g.popular_paths || []).length ? `<h3 class="eyebrow" style="margin:10px 0 6px">Популярные пути</h3>
          <table><tbody>${g.popular_paths.slice(0, 6).map((p) => `<tr><td class="wrap" title="${esc(p.path)}">${esc(p.title || p.path)}</td><td class="num">${fmtInt(p.count)}</td></tr>`).join('')}</tbody></table>` : ''}
        ${(g.referrers || []).length ? `<h3 class="eyebrow" style="margin:14px 0 6px">Рефереры</h3>
          <table><tbody>${g.referrers.slice(0, 6).map((r) => `<tr><td class="wrap">${esc(r.referrer)}</td><td class="num">${fmtInt(r.count)}</td></tr>`).join('')}</tbody></table>` : ''}
        <p class="note" style="margin:8px 0 0">GitHub хранит эти данные 14 дней и отдаёт без разбивки по дням.</p>
      </details>` : ''}
      ${(g.releases || []).length > 1 ? `<details><summary>Все релизы (${g.releases.length})</summary>
        <table><thead><tr><th>Релиз</th><th class="num">Опубликован</th><th class="num">Скачиваний</th></tr></thead>
        <tbody>${g.releases.map((r) => `<tr><td class="wrap">${esc(r.tag)}</td><td class="num">${esc(fmtDate(r.published))}</td><td class="num">${fmtInt(r.total)}</td></tr>`).join('')}</tbody></table>
      </details>` : ''}
    </section>`;
  }

  function webmasterBlock(data) {
    const w = data.webmaster || {};
    const diag = w.diagnostics;
    const quota = w.quota;
    const problems = diag ? diag.problems : [];
    const diagRows = diag ? diag.all.filter((d) => d.state !== 'ABSENT').slice(0, 40) : [];
    const quotaBar = quota ? `<div class="bar-track" style="height:7px"><div class="bar-fill" style="width:${(quota.quota_remainder / quota.daily_quota) * 100}%"></div></div>` : '';

    const badgeText = w.loaded ? 'данные загружены' : (w.data_status === 'NOT_LOADED' ? 'данные ещё не загружены' : (w.data_status || 'нет данных'));
    return `<section class="card">
      <div class="card-head"><h2>Яндекс.Вебмастер</h2>
        <span class="badge ${w.loaded ? 'badge-ok' : 'badge-warn'}">${esc(badgeText)}</span></div>
      ${w.loaded ? '' : `<div class="note-box">${esc(w.note || 'Яндекс ещё не загрузил данные хоста.')}</div>`}
      <table>
        <tbody>
          <tr><td>Хост подтверждён</td><td class="num">${w.verified ? 'да' : 'нет'}</td></tr>
          <tr><td>Проверок в диагностике</td><td class="num">${diag ? fmtInt(diag.total) : '—'}</td></tr>
          <tr><td>Проблем найдено</td><td class="num">${diag ? fmtInt(problems.length) : '—'}</td></tr>
          <tr><td>Карт сайта в Вебмастере</td><td class="num">${Array.isArray(w.sitemaps) ? w.sitemaps.length : '—'}</td></tr>
          <tr><td>Квота обхода</td><td class="num">${quota ? `${quota.quota_remainder} из ${quota.daily_quota}` : '—'}</td></tr>
        </tbody>
      </table>
      ${quotaBar}
      ${Array.isArray(w.sitemaps) && !w.sitemaps.length ? `<div class="note-box" style="margin-top:12px">${esc(w.sitemaps_note || 'Карта сайта не отправлена в Вебмастер.')}</div>` : ''}
      ${diagRows.length ? `<details><summary>Показать проверки с состоянием (${diagRows.length})</summary>
        <div class="diag-list"><table><tbody>${diagRows.map((d) => `<tr><td class="name" title="${esc(d.id)}">${esc(d.id)}</td><td class="num">${esc(d.state)}</td><td class="num">${esc(d.severity || '')}</td></tr>`).join('')}</tbody></table></div>
      </details>` : '<p class="note" style="margin:10px 0 0">Все проверки в состоянии «отсутствует» — проблем не найдено.</p>'}
    </section>`;
  }

  function siteBlock(data) {
    const s = data.site || {};
    const items = [];
    const add = (label, value, st) => items.push({ label, value, st: st || 'ok' });
    add('Страниц на сайте', fmtInt(s.pages));
    add('URL в sitemap.xml', fmtInt(s.sitemap_urls), s.sitemap_urls && s.pages && s.sitemap_urls >= s.pages * 0.6 ? 'ok' : 'warn');
    add('robots.txt', s.robots_txt ? 'есть' : 'нет', s.robots_txt ? 'ok' : 'bad');
    add('Страница 404', s['404_html'] ? 'своя' : 'нет', s['404_html'] ? 'ok' : 'warn');
    add('Ключ IndexNow', s.indexnow_key ? 'на месте' : 'нет', s.indexnow_key ? 'ok' : 'warn');
    add('Счётчик Метрики', s.has_metrika ? 'подключён' : s.has_consent ? 'через consent.js' : 'нет', s.has_metrika || s.has_consent ? 'ok' : 'warn');
    add('Рекламоподобных имён', fmtInt((s.ad_names || []).length), (s.ad_names || []).length ? 'warn' : 'ok');
    if (s.assets) add('Версии ассетов', Object.entries(s.assets).map(([k, v]) => `${k} ${v}`).join(' · '));
    add('Главная страница', fmtBytes(s.index_bytes));
    const v = s.vitals;
    if (v) {
      if (v.lcp_ms) add('LCP (мобильный)', v.lcp_ms + ' мс', v.lcp_ms < 2500 ? 'ok' : 'warn');
      if (v.cls !== undefined) add('CLS', String(v.cls), v.cls <= 0.1 ? 'ok' : 'warn');
      if (v.measured_at) add('Замер скорости', fmtDate(v.measured_at), 'ok');
    }
    const html = items.map((i) => `<div class="check ${i.st}">
        <span class="ic" aria-hidden="true">${i.st === 'ok' ? '✓' : i.st === 'bad' ? '✕' : '!'}</span>
        <span><span>${esc(i.label)}</span> <span class="cv">${esc(i.value)}</span></span>
      </div>`).join('');
    return `<section class="card">
      <div class="card-head"><h2>Здоровье сайта</h2><span class="note">самопроверки сборщика</span></div>
      <div class="checklist">${html}</div>
    </section>`;
  }

  function journalBlock(data) {
    const commits = (data.github || {}).commits || [];
    if (!commits.length) return '';
    return `<section class="card">
      <div class="card-head"><h2>Журнал изменений сайта</h2><span class="note">последние коммиты landing/</span></div>
      ${commits.map((c) => `<div class="commit"><code>${esc(c.sha)}</code><div>${esc(c.message)}<time>${esc(c.date)}</time></div></div>`).join('')}
    </section>`;
  }

  /* ───────────── сборка и рендер ───────────── */

  function render() {
    const data = state.data;
    const content = $('#content');
    content.innerHTML = [
      kpiBlock(data),
      `<div class="grid-2">${chartBlock(data)}${funnelBlock(data)}</div>`,
      `<div class="grid-2">${tableBlock('Страницы входа', dimsOf(data, 'entry_pages'), `за ${state.period} дней`, 'Заходов на страницы за период нет.')}${sourcesBlock(data)}</div>`,
      `<div class="grid-2">${popularBlock(data)}${hoursBlock(data)}</div>`,
      `<div class="grid-2">${goalsBlock(data)}${webmasterBlock(data)}</div>`,
      `<div class="grid-2">${githubBlock(data)}${siteBlock(data)}</div>`,
      audienceBlock(data),
      journalBlock(data),
    ].join('');

    document.querySelectorAll('.chart').forEach((card, i) => {
      const svg = $('svg', card);
      if (!svg) return;
      const labels = i === 0
        ? windowOf(data, state.period).cur.map((r) => fmtShort(r.date))
        : ((data.github || {}).views || []).map((v) => fmtShort(v.date));
      const series = i === 0
        ? [
          { name: 'Визиты', values: windowOf(data, state.period).cur.map((r) => r.visits || 0) },
          { name: 'Посетители', values: windowOf(data, state.period).cur.map((r) => r.users || 0) },
        ]
        : [
          { name: 'Просмотры', values: ((data.github || {}).views || []).map((v) => v.count) },
          { name: 'Клоны', values: ((data.github || {}).clones || []).map((v) => v.count) },
        ];
      attachChart(card, labels, series);
    });
  }

  function stamp(data) {
    const at = data.collected_at;
    $('#stamp').textContent = `снимок от ${fmtDateTime(at)} · собрано сборщиком на рабочей машине`;
    const ageMs = Date.now() - new Date(at).getTime();
    const days = Math.floor(ageMs / 86400000);
    const badge = $('#stale');
    if (days >= 2) {
      badge.hidden = false;
      badge.textContent = days >= 1 ? `снимок устарел на ${days} дн.` : 'снимок устарел';
    } else {
      badge.hidden = true;
    }
  }

  async function open(password) {
    const env = await loadEnvelope();
    const data = await decrypt(env, password);
    state.data = data;
    stamp(data);
    render();
    $('#lock').hidden = true;
    $('#app').hidden = false;
    document.title = 'Панель аналитики — зашифрованный снимок';
    window.scrollTo(0, 0);
  }

  /* ───────────── события ───────────── */

  const msg = () => $('#lock-msg');
  let attempts = 0;

  function setMsg(text, info) {
    const el = msg();
    el.textContent = text || '';
    el.classList.toggle('is-info', !!info);
  }

  const form = $('#unlock');
  if (form) {
    form.addEventListener('submit', async (ev) => {
      ev.preventDefault();
      const input = $('#pw');
      const btn = $('#unlock-btn');
      const pw = input.value;
      if (!pw) { setMsg('Введите пароль.'); input.focus(); return; }
      if (!window.crypto || !crypto.subtle) {
        setMsg('Браузер не умеет расшифровывать (Web Crypto доступен только на https).');
        return;
      }
      btn.disabled = true;
      btn.textContent = 'Проверяю…';
      setMsg('');
      try {
        await open(pw);
      } catch (e) {
        attempts += 1;
        const wait = Math.min(8000, 700 * attempts);
        const isEnvelope = e && e.message === 'ENVELOPE';
        input.value = '';
        btn.disabled = true;
        setMsg(isEnvelope
          ? 'Файл снимка не найден или повреждён.'
          : `Неверный пароль. Попытка ${attempts}.`);
        setTimeout(() => {
          btn.disabled = false;
          btn.textContent = 'Открыть панель';
          input.focus();
        }, isEnvelope ? 1200 : wait);
        return;
      }
      btn.disabled = false;
      btn.textContent = 'Открыть панель';
    });
  }

  const toggle = $('#pw-toggle');
  if (toggle) {
    toggle.addEventListener('click', () => {
      const input = $('#pw');
      const show = input.type === 'password';
      input.type = show ? 'text' : 'password';
      toggle.setAttribute('aria-pressed', String(show));
      toggle.setAttribute('aria-label', show ? 'Скрыть пароль' : 'Показать пароль');
      input.focus();
    });
  }

  document.querySelectorAll('[data-period]').forEach((btn) => {
    btn.addEventListener('click', () => {
      state.period = Number(btn.dataset.period);
      document.querySelectorAll('[data-period]').forEach((b) => {
        const on = b === btn;
        b.classList.toggle('is-on', on);
        b.setAttribute('aria-pressed', String(on));
      });
      if (state.data) render();
    });
  });

  const exportBtn = $('#export-btn');
  if (exportBtn) {
    exportBtn.addEventListener('click', () => {
      if (!state.data) return;
      const blob = new Blob([JSON.stringify(state.data, null, 2)], { type: 'application/json' });
      const a = document.createElement('a');
      a.href = URL.createObjectURL(blob);
      a.download = `lastcopy-analytics-${(state.data.collected_date || 'snapshot')}.json`;
      a.click();
      setTimeout(() => URL.revokeObjectURL(a.href), 2000);
    });
  }

  const lockBtn = $('#lock-btn');
  if (lockBtn) {
    lockBtn.addEventListener('click', () => {
      state.data = null;
      $('#app').hidden = true;
      $('#lock').hidden = false;
      $('#pw').value = '';
      $('#content').innerHTML = '';
      setMsg('Панель закрыта. Данные остаются только в памяти вкладки.', true);
      $('#pw').focus();
    });
  }
})();
