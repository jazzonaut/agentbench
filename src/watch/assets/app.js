// Dashboard boot: polls the API, renders the live tiles, the history chart, and daemon health.

import {
  percent, gib, mib, count, duration, dateTime, latency, ratio, unitFormatter,
} from './format.js';
import { createChart } from './chart.js';

/** Live tiles refresh on this cadence; the server samples independently of it.
 *
 *  Only /api/live rides this poll. It reads one row per stream, which is what makes it affordable
 *  five times a minute; the aggregate endpoints are on {@link HISTORY_POLL_MS} for the opposite reason.
 */
const LIVE_POLL_MS = 5000;

/** History, health, and verdicts reload here: they change slowly and each costs a scan to compute. */
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
  verdicts: document.getElementById('verdicts'),
  verdictsNote: document.getElementById('verdicts-note'),
  ranges: document.getElementById('ranges'),
  uncontended: document.getElementById('uncontended'),
  marks: document.getElementById('marks'),
  statusLine: document.getElementById('status-line'),
  events: document.querySelector('#events tbody'),
};

/** Stacked charts, in reading order. Each owns one metric and one y-axis.
 *
 *  A panel with no `format` derives one from the unit the server reports, which is how a probe series
 *  named after a catalogue entry gets a correct axis without the page knowing what it measures.
 */
const CHARTS = [
  { id: 'chart-cpu', metric: 'cpu_percent', format: percent, label: 'system CPU' },
  { id: 'chart-tools', metric: 'tool_read_ms', format: latency, label: 'median latency' },
  { id: 'chart-probe-fs', metric: 'probe:filesystem.small_file_ops_s', label: 'probe throughput' },
].map((config) => {
  const element = document.getElementById(config.id);
  const note = element.closest('.card')?.querySelector('.card-note') ?? null;
  const panel = {
    ...config,
    unit: '',
    note,
    // The note as authored in the markup, so a resolution caveat can be appended and later removed
    // without the original wording being lost after the first poll.
    baseNote: note?.textContent.trim() ?? '',
    empty: document.querySelector(`[data-empty-for="${config.id}"]`),
  };
  // The closure reads `panel.unit` on every call rather than capturing it, so the first response can
  // supply the unit without the plot having to be rebuilt.
  panel.chart = createChart(
    element,
    config.format ?? ((value) => unitFormatter(panel.unit)(value)),
    config.label,
  );
  return panel;
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

/** The most recent controlled measurement, and whether it was worth anything.
 *
 *  A contended probe is not a bad probe — it is deliberately collected and deliberately excluded later —
 *  so the tile says which it was rather than hiding it. Without that, a reader comparing the chart to the
 *  tile would find numbers that disagree and no explanation.
 */
function probeTile(probe) {
  if (!probe) {
    return tile('No probes yet', 'the first runs after one probe interval', true);
  }
  const age = duration(Date.now() - probe.ts);
  if (probe.contended) {
    const because = probe.agent_active
      ? 'an agent was working'
      : probe.scanner_at !== null && probe.scanner_at > 0
        ? 'a scanner was active'
        : 'the machine was busy';
    return tile(age, `since the last probe · contended, ${because}`);
  }
  const power = probe.on_battery === true ? ' · on battery' : '';
  return tile(age, `since the last probe · uncontended${power}`);
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
    probeTile(live.probe),
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

/** One series judged against its trailing baseline.
 *
 *  Every tile states the evidence as well as the finding. A verdict drawn from four probes on three days is
 *  a different thing from one drawn from ninety on seven, and a reader deciding whether to go and look at
 *  the machine needs to be able to tell them apart without opening the API.
 */
function verdictTile(comparison) {
  const format = unitFormatter(comparison.unit);
  const node = document.createElement('div');
  node.className = `verdict ${comparison.verdict}`;

  const head = document.createElement('div');
  head.className = 'verdict-head';
  const value = document.createElement('span');
  value.className = 'verdict-value';
  value.textContent = comparison.today === null ? '—' : format(comparison.today);
  const word = document.createElement('span');
  word.className = `verdict-word ${comparison.verdict}`;
  // The word is always present: the colour of the rule is a second encoding, never the only one.
  word.textContent = comparison.verdict === 'insufficient'
    ? 'no verdict'
    : comparison.delta_percent === null
      ? comparison.verdict
      : `${comparison.verdict} ${comparison.delta_percent >= 0 ? '+' : ''}${comparison.delta_percent.toFixed(1)}%`;
  head.append(value, word);

  const label = document.createElement('div');
  label.className = 'verdict-label';
  label.textContent = comparison.baseline === null
    ? comparison.label
    : `${comparison.label} · baseline ${format(comparison.baseline.median)} over ${count(comparison.baseline.days)} day(s)`;

  node.append(head, label);
  const evidence = comparison.baseline === null
    ? null
    : `${count(comparison.today_observations)} today, ${count(comparison.baseline.observations)} in the baseline`;
  for (const text of [evidence, comparison.note ?? null]) {
    if (!text) continue;
    const line = document.createElement('div');
    line.className = 'verdict-note';
    line.textContent = text;
    node.append(line);
  }
  return node;
}

function renderVerdicts(payload) {
  dom.verdicts.replaceChildren();
  for (const comparison of payload.comparisons) {
    dom.verdicts.append(verdictTile(comparison));
  }
  const judged = payload.comparisons.filter((one) => one.verdict !== 'insufficient').length;
  dom.verdictsNote.textContent = judged === 0
    ? `Since ${dateTime(payload.day_start_ms)}, against the previous ${payload.window_days} days. Nothing has enough comparable measurements to judge yet.`
    : `Since ${dateTime(payload.day_start_ms)}, against the median of the previous ${payload.window_days} days. Only uncontended probes count.`;
}

/** Name the marks drawn on the charts, in reading order. */
function renderMarks(annotations) {
  dom.marks.replaceChildren();
  if (annotations.length === 0) return;
  for (const mark of annotations) {
    const item = document.createElement('li');
    const swatch = document.createElement('span');
    swatch.className = `mark-swatch ${mark.kind}`;
    const text = document.createElement('span');
    // Dated, not just clocked. Ranges run to a week, and two marks a day apart both reading "10:48" is
    // worse than no label at all — it invites a reader to line up the wrong one with a step in a chart.
    text.textContent = `${mark.label} · ${dateTime(mark.ts)}`;
    if (mark.detail) item.title = mark.detail;
    item.append(swatch, text);
    dom.marks.append(item);
  }
}

function renderStatus(status) {
  const fresh = status.collecting;
  dom.statusLine.replaceChildren();

  const collecting = document.createElement('span');
  const dot = document.createElement('span');
  dot.className = `dot ${fresh ? 'ok' : 'stale'}`;
  collecting.append(dot);
  // A stopped writer is the one stall a timestamp cannot describe: the rows are recent and nothing
  // more will ever be added, so it is named rather than left to read as a quiet machine.
  const stopped = status.writer_running === false;
  collecting.append(
    document.createTextNode(
      stopped
        ? 'writer stopped · nothing is being recorded'
        : status.sample_age_ms === null
          ? 'no samples recorded'
          : fresh
            ? `collecting · last sample ${duration(status.sample_age_ms)} ago`
            : `stalled · last sample ${duration(status.sample_age_ms)} ago`,
    ),
  );

  const facts = [
    `${count(status.health.samples)} samples`,
    // Both numbers, because the clean subset is what a baseline can actually use and on a busy week it
    // is a small fraction of the total.
    `${count(status.health.probe_runs_clean)}/${count(status.health.probe_runs)} clean probes`,
    `${count(status.health.run_markers)} marked runs`,
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

/** Reported by /api/status, which is polled far less often than the tiles that display it. */
let uplotVersion = null;

async function loadLive() {
  try {
    const live = await api('/api/live');
    renderTiles(live);
    renderToday(live.today, live.day_start_ts);
    const machine = `machine ${live.machine_id.slice(0, 12)}`;
    dom.subtitle.textContent = uplotVersion ? `${machine} · uPlot ${uplotVersion}` : machine;
  } catch (error) {
    dom.subtitle.textContent = `cannot reach the daemon: ${error.message}`;
  }
}

/** Daemon health and today's verdicts, on the slow cadence.
 *
 *  Both used to ride along with the live tiles every five seconds, and between them they cost the
 *  machine more than the collectors they report on: /api/status runs six count(*) aggregates over the
 *  fact tables, and /api/verdicts re-derives a whole trailing window, one aggregation per day in it.
 *  The server handles requests one at a time on the main thread at normal priority, so an open
 *  dashboard was biasing the very series it was drawing.
 *
 *  Neither payload changes on a five-second timescale. A verdict moves when a probe lands, four times
 *  an hour; the health counts move by a handful of rows. A minute is still far finer than either.
 */
async function loadHealth() {
  try {
    const [status, verdicts] = await Promise.all([api('/api/status'), api('/api/verdicts')]);
    uplotVersion = status.uplot_version;
    renderVerdicts(verdicts);
    renderStatus(status);
  } catch (error) {
    dom.statusLine.textContent = `cannot reach the daemon: ${error.message}`;
  }
}

/** A note explaining a line whose character changes partway along it. */
function resolutionNote(series) {
  if (!series.resolution || series.resolution === 'raw') return null;
  const summary = series.rollup_reducer === 'max'
    ? 'per-minute peaks'
    : 'per-minute averages';
  return series.resolution === 'rollup'
    ? `${summary} · older samples have been summarised`
    : `${summary} before the step in cadence · recent samples are unsummarised`;
}

async function loadHistory() {
  const to = Date.now();
  const from = to - selectedRange.ms;
  // Only probe series carry contention, so the filter is appended only where it means something.
  const contended = dom.uncontended.checked ? '&contended=exclude' : '';

  // Marks are shared by every frame, so they are fetched once and handed to all of them. A failure here
  // leaves the charts drawn and unannotated rather than taking the history down.
  let annotations = [];
  try {
    annotations = (await api(`/api/annotations?from=${from}&to=${to}`)).annotations;
  } catch {
    annotations = [];
  }
  renderMarks(annotations);

  // Each chart loads independently, so one failing metric leaves the others readable.
  await Promise.all(
    CHARTS.map(async (panel) => {
      const filter = panel.metric.includes(':') ? contended : '';
      try {
        const query = `metric=${encodeURIComponent(panel.metric)}&from=${from}&to=${to}${filter}`;
        const series = await api(`/api/series?${query}`);
        if (series.unit) panel.unit = series.unit;
        panel.chart.setAnnotations(annotations);
        panel.chart.update(series.points, series.gap_ms);
        panel.empty.hidden = series.points.length > 0;
        const note = resolutionNote(series);
        if (note && panel.note) panel.note.textContent = `${panel.baseNote} · ${note}`;
        else if (panel.note) panel.note.textContent = panel.baseNote;
      } catch (error) {
        panel.empty.hidden = false;
        panel.empty.textContent = `Could not load ${panel.metric}: ${error.message}`;
      }
    }),
  );
}

renderRanges();
// Toggling what a chart counts must redraw it immediately: waiting a minute for the next poll would
// read as the filter having done nothing.
dom.uncontended.addEventListener('change', () => void loadHistory());
void loadLive();
void loadHealth();
void loadHistory();
setInterval(loadLive, LIVE_POLL_MS);
setInterval(loadHealth, HISTORY_POLL_MS);
setInterval(loadHistory, HISTORY_POLL_MS);
