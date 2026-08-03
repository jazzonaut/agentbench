// Dashboard boot: polls the API, renders the live tiles, the history chart, and daemon health.

import { percent, gib, mib, count, duration, dateTime } from './format.js';
import { createChart } from './chart.js';

/** Live tiles refresh on this cadence; the server samples independently of it. */
const LIVE_POLL_MS = 5000;

/** History reloads far less often than the tiles: it changes slowly and costs more to fetch. */
const HISTORY_POLL_MS = 60000;

const RANGES = [
  { label: '1h', ms: 3600000 },
  { label: '6h', ms: 21600000 },
  { label: '24h', ms: 86400000 },
  { label: '48h', ms: 172800000 },
  { label: '7d', ms: 604800000 },
];

let selectedRange = RANGES.find((range) => range.label === '48h');

const dom = {
  subtitle: document.getElementById('subtitle'),
  tiles: document.getElementById('tiles'),
  ranges: document.getElementById('ranges'),
  chart: document.getElementById('chart'),
  chartEmpty: document.getElementById('chart-empty'),
  statusLine: document.getElementById('status-line'),
  events: document.querySelector('#events tbody'),
};

const chart = createChart(dom.chart, percent);

/** Fetch JSON, surfacing the server's error message rather than a bare status code. */
async function api(path) {
  const response = await fetch(path, { cache: 'no-store' });
  const payload = await response.json().catch(() => null);
  if (!response.ok) {
    throw new Error(payload?.error ?? `${path} returned ${response.status}`);
  }
  return payload;
}

function tile(value, label, absent = false) {
  const node = document.createElement('div');
  node.className = 'tile';
  const valueNode = document.createElement('div');
  valueNode.className = absent ? 'tile-value absent' : 'tile-value';
  valueNode.textContent = value;
  const labelNode = document.createElement('div');
  labelNode.className = 'tile-label';
  labelNode.textContent = label;
  node.append(valueNode, labelNode);
  return node;
}

function renderTiles(live) {
  const sample = live.sample;
  dom.tiles.replaceChildren();
  if (!sample) {
    dom.tiles.append(tile('No samples yet', 'the daemon has not observed the machine', true));
    return;
  }
  const memoryLabel = sample.total_memory
    ? `memory of ${gib(sample.total_memory)}`
    : 'memory in use';
  dom.tiles.append(
    tile(percent(sample.cpu_percent), 'system CPU'),
    tile(gib(sample.used_memory), memoryLabel),
    tile(gib(sample.used_swap), 'swap in use'),
    tile(count(sample.process_count), 'processes'),
    // Absent is not zero: a missing scanner means none was found, not that it used no CPU.
    sample.scanner_cpu === null
      ? tile('none found', 'security scanner', true)
      : tile(percent(sample.scanner_cpu), 'security scanner CPU'),
    sample.agent_cpu === null
      ? tile('not running', 'coding agent', true)
      : tile(percent(sample.agent_cpu), `agent CPU · ${mib(sample.agent_rss)} · ${count(sample.agent_processes)} proc`),
  );
}

function renderStatus(status) {
  const fresh = status.collecting;
  dom.statusLine.replaceChildren();

  const collecting = document.createElement('span');
  const dot = document.createElement('span');
  dot.className = `dot ${fresh ? 'ok' : 'stale'}`;
  collecting.append(dot);
  collecting.append(
    document.createTextNode(
      status.sample_age_ms === null
        ? 'no samples recorded'
        : fresh
          ? `collecting · last sample ${duration(status.sample_age_ms)} ago`
          : `stalled · last sample ${duration(status.sample_age_ms)} ago`,
    ),
  );

  const facts = [
    `${count(status.health.samples)} samples`,
    `${count(status.health.probe_runs)} probes`,
    `${count(status.health.session_turns)} turns`,
    `import errors: ${count(status.health.import_errors)}`,
    `schema v${status.health.schema_version}`,
    `agentbench ${status.tool_version}`,
  ];
  dom.statusLine.append(collecting);
  for (const fact of facts) {
    const node = document.createElement('span');
    node.textContent = fact;
    dom.statusLine.append(node);
  }

  dom.events.replaceChildren();
  if (status.events.length === 0) {
    const row = document.createElement('tr');
    const cell = document.createElement('td');
    cell.colSpan = 3;
    cell.textContent = 'Nothing to report.';
    row.append(cell);
    dom.events.append(row);
    return;
  }
  for (const event of status.events) {
    const row = document.createElement('tr');
    const when = document.createElement('td');
    when.textContent = dateTime(event.ts);
    const source = document.createElement('td');
    source.textContent = event.source;
    const message = document.createElement('td');
    message.className = `level-${event.level}`;
    // Level is conveyed by the word as well as the colour, never colour alone.
    message.textContent = event.level === 'info' ? event.message : `${event.level}: ${event.message}`;
    row.append(when, source, message);
    dom.events.append(row);
  }
}

function renderRanges() {
  dom.ranges.replaceChildren();
  for (const range of RANGES) {
    const button = document.createElement('button');
    button.type = 'button';
    button.textContent = range.label;
    button.setAttribute('aria-pressed', String(range === selectedRange));
    button.addEventListener('click', () => {
      selectedRange = range;
      renderRanges();
      void loadHistory();
    });
    dom.ranges.append(button);
  }
}

async function loadLive() {
  try {
    const [live, status] = await Promise.all([api('/api/live'), api('/api/status')]);
    renderTiles(live);
    renderStatus(status);
    dom.subtitle.textContent = `machine ${live.machine_id.slice(0, 12)} · uPlot ${status.uplot_version}`;
  } catch (error) {
    dom.subtitle.textContent = `cannot reach the daemon: ${error.message}`;
  }
}

async function loadHistory() {
  try {
    const to = Date.now();
    const from = to - selectedRange.ms;
    const series = await api(
      `/api/series?metric=cpu_percent&from=${from}&to=${to}`,
    );
    chart.update(series.points, series.gap_ms);
    dom.chartEmpty.hidden = series.points.length > 0;
  } catch (error) {
    dom.chartEmpty.hidden = false;
    dom.chartEmpty.textContent = `Could not load history: ${error.message}`;
  }
}

renderRanges();
void loadLive();
void loadHistory();
setInterval(loadLive, LIVE_POLL_MS);
setInterval(loadHistory, HISTORY_POLL_MS);
