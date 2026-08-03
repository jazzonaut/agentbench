// Dashboard boot: polls the API, renders the live tiles, the history chart, and daemon health.

import { percent, gib, mib, count, duration, dateTime, latency, ratio } from './format.js';
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
  today: document.getElementById('today'),
  ranges: document.getElementById('ranges'),
  statusLine: document.getElementById('status-line'),
  events: document.querySelector('#events tbody'),
};

/** Stacked charts, in reading order. Each owns one metric and one y-axis. */
const CHARTS = [
  { id: 'chart-cpu', metric: 'cpu_percent', format: percent, label: 'system CPU' },
  { id: 'chart-tools', metric: 'tool_read_ms', format: latency, label: 'median latency' },
].map((config) => {
  const element = document.getElementById(config.id);
  return {
    ...config,
    chart: createChart(element, config.format, config.label),
    empty: document.querySelector(`[data-empty-for="${config.id}"]`),
  };
});

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

/** Today's agent activity, from the transcripts the daemon has already read. */
function renderToday(today, dayStart) {
  dom.today.replaceChildren();
  if (!today || today.turns === 0) {
    dom.today.append(
      tile('No agent activity', `since ${dateTime(dayStart)}`, true),
    );
    return;
  }
  const projects = today.projects === 1 ? '1 project' : `${count(today.projects)} projects`;
  dom.today.append(
    tile(count(today.turns), `requests in ${count(today.sessions)} session(s)`),
    tile(count(today.tool_calls), `tool calls · ${projects}`),
    // Absent is not zero: no file-tool calls yet means nothing was measured, not that it was instant.
    today.tool_read_p50_ms === null
      ? tile('no calls yet', 'median file-tool latency', true)
      : tile(latency(today.tool_read_p50_ms), 'median file-tool latency'),
    tile(count(today.output_tokens), 'output tokens'),
    today.cache_hit_ratio === null
      ? tile('—', 'prompt cache hits', true)
      : tile(ratio(today.cache_hit_ratio), 'prompt cache hits'),
    tile(
      today.last_activity_ts === null ? '—' : duration(Date.now() - today.last_activity_ts),
      'since the last agent activity',
      today.last_activity_ts === null,
    ),
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
    `${count(status.health.session_tools)} tool calls`,
    `${count(status.health.imported_files)} transcripts`,
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
    renderToday(live.today, live.day_start_ts);
    renderStatus(status);
    dom.subtitle.textContent = `machine ${live.machine_id.slice(0, 12)} · uPlot ${status.uplot_version}`;
  } catch (error) {
    dom.subtitle.textContent = `cannot reach the daemon: ${error.message}`;
  }
}

async function loadHistory() {
  const to = Date.now();
  const from = to - selectedRange.ms;
  // Each chart loads independently, so one failing metric leaves the others readable.
  await Promise.all(
    CHARTS.map(async (panel) => {
      try {
        const series = await api(`/api/series?metric=${panel.metric}&from=${from}&to=${to}`);
        panel.chart.update(series.points, series.gap_ms);
        panel.empty.hidden = series.points.length > 0;
      } catch (error) {
        panel.empty.hidden = false;
        panel.empty.textContent = `Could not load ${panel.metric}: ${error.message}`;
      }
    }),
  );
}

renderRanges();
void loadLive();
void loadHistory();
setInterval(loadLive, LIVE_POLL_MS);
setInterval(loadHistory, HISTORY_POLL_MS);
