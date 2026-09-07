/** Inline SVG charts. No chart library, so the dashboard loads with no network. */

const NS = 'http://www.w3.org/2000/svg';
const W = 600;
const H = 168;
const PAD = { top: 10, right: 8, bottom: 20, left: 40 };

function svg(children, title) {
  const el = document.createElementNS(NS, 'svg');
  el.setAttribute('class', 'chart');
  el.setAttribute('viewBox', `0 0 ${W} ${H}`);
  // Uniform scaling: stretching would squash the axis labels on a phone.
  el.setAttribute('preserveAspectRatio', 'xMidYMid meet');
  el.setAttribute('role', 'img');
  if (title) el.setAttribute('aria-label', title);
  for (const c of children.flat()) if (c) el.append(c);
  return el;
}

function node(name, attrs, text) {
  const el = document.createElementNS(NS, name);
  for (const [k, v] of Object.entries(attrs)) el.setAttribute(k, v);
  if (text !== undefined) el.textContent = text;
  return el;
}

function scale(values) {
  const max = Math.max(1, ...values.map((v) => Number(v) || 0));
  const plotH = H - PAD.top - PAD.bottom;
  return {
    max,
    y: (v) => PAD.top + plotH - ((Number(v) || 0) / max) * plotH,
    plotH,
    plotW: W - PAD.left - PAD.right,
  };
}

function gridlines(s) {
  const out = [];
  for (let i = 0; i <= 2; i++) {
    const value = (s.max / 2) * i;
    const y = s.y(value);
    out.push(node('line', { class: 'grid-line', x1: PAD.left, x2: W - PAD.right, y1: y, y2: y }));
    out.push(node('text', { x: 2, y: y + 3.5 }, shortNum(value)));
  }
  return out;
}

function xLabels(labels, s) {
  const out = [];
  const step = Math.max(1, Math.ceil(labels.length / 6));
  labels.forEach((label, i) => {
    if (i % step !== 0 && i !== labels.length - 1) return;
    const x = PAD.left + (labels.length === 1 ? s.plotW / 2 : (i / (labels.length - 1)) * s.plotW);
    out.push(node('text', {
      x: Math.min(W - PAD.right, Math.max(PAD.left, x)),
      y: H - 5,
      'text-anchor': i === 0 ? 'start' : i === labels.length - 1 ? 'end' : 'middle',
    }, label));
  });
  return out;
}

/** Filled line chart over a time series. */
export function lineChart(points, { label = '', format = shortNum } = {}) {
  if (!points.length) return emptyChart();
  const values = points.map((p) => p.value);
  const s = scale(values);
  const x = (i) => PAD.left + (points.length === 1 ? s.plotW / 2 : (i / (points.length - 1)) * s.plotW);

  const line = points.map((p, i) => `${i === 0 ? 'M' : 'L'}${x(i).toFixed(1)},${s.y(p.value).toFixed(1)}`).join(' ');
  const area = `${line} L${x(points.length - 1).toFixed(1)},${(H - PAD.bottom).toFixed(1)} L${x(0).toFixed(1)},${(H - PAD.bottom).toFixed(1)} Z`;

  // With only a handful of buckets a bare polyline is hard to read, and a
  // single bucket draws nothing at all, so the vertices get their own markers.
  const markers = points.length <= 31
    ? points.map((p, i) => node('circle', {
      class: 'line',
      cx: x(i).toFixed(1),
      cy: s.y(p.value).toFixed(1),
      r: points.length === 1 ? 4 : 2.5,
      fill: 'var(--accent)',
    }))
    : [];

  const valueLabels = points.length <= 8
    ? points.map((p, i) => node('text', {
      x: x(i).toFixed(1),
      y: Math.max(9, s.y(p.value) - 7).toFixed(1),
      'text-anchor': 'middle',
      fill: 'var(--text)',
    }, format(p.value)))
    : [];

  return svg([
    gridlines(s),
    points.length > 1 ? node('path', { class: 'area', d: area }) : null,
    points.length > 1 ? node('path', { class: 'line', d: line }) : null,
    markers,
    valueLabels,
    xLabels(points.map((p) => p.label), s),
  ], `${label}: latest ${format(points.at(-1).value)}`);
}

/** Grouped bars, one or two series per bucket. */
export function barChart(points, { series = ['value'], label = '' } = {}) {
  if (!points.length) return emptyChart();
  const all = points.flatMap((p) => series.map((k) => p[k] ?? 0));
  const s = scale(all);
  const slot = s.plotW / points.length;
  const barW = Math.max(1.5, (slot * 0.72) / series.length);

  const bars = [];
  points.forEach((p, i) => {
    series.forEach((key, k) => {
      const value = Number(p[key]) || 0;
      const x = PAD.left + i * slot + slot * 0.14 + k * barW;
      const y = s.y(value);
      bars.push(node('rect', {
        class: k === 0 ? 'bar' : 'bar alt',
        x: x.toFixed(1),
        y: y.toFixed(1),
        width: barW.toFixed(1),
        height: Math.max(0, H - PAD.bottom - y).toFixed(1),
        rx: 1.5,
      }));
    });
  });

  return svg([gridlines(s), bars, xLabels(points.map((p) => p.label), s)], label);
}

function emptyChart() {
  return svg([node('text', { x: W / 2, y: H / 2, 'text-anchor': 'middle' }, 'No data yet')]);
}

export function shortNum(v) {
  const n = Number(v) || 0;
  if (n >= 1e9) return `${(n / 1e9).toFixed(1)}B`;
  if (n >= 1e6) return `${(n / 1e6).toFixed(1)}M`;
  if (n >= 1e3) return `${(n / 1e3).toFixed(1)}k`;
  return n % 1 === 0 ? String(n) : n.toFixed(1);
}
