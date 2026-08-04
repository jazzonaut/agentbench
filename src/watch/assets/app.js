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

/** Probe metrics the one probe frame switches between, and what each measurement actually is.
 *
 *  The four the comparison judges, in the order it lists them — see `comparison::subjects::SUBJECTS`.
 *  Charting exactly the judged set is the point: a verdict tile a reader wants to check has a line to
 *  check it against, and a line that earns a verdict is never silently absent from the chart.
 *
 *  Every entry carries its own note because the working set is part of the reading. 200 files is not
 *  8 MiB, and a note left over from the previous selection would describe a different workload than the
 *  line on screen. The unit is not listed: the server reports it per series and the axis derives from
 *  that, so a name here can never disagree with the catalogue.
 */
const PROBE_METRICS = [
  {
    label: 'small-file ops',
    metric: 'probe:filesystem.small_file_ops_s',
    note: 'controlled workload · 200 files · not comparable to a bench report',
  },
  {
    label: 'sequential write',
    metric: 'probe:filesystem.sequential_write_mib_s',
    // The read half is deliberately not collected: at 8 MiB it is served from the page cache, so it
    // would report memory bandwidth under a name that means disk everywhere else in the tool.
    note: 'controlled workload · 8 MiB write · write only, a read this size is page cache',
  },
  {
    label: 'SQLite lookup',
    metric: 'probe:sqlite.lookup_ms',
    note: 'controlled workload · 2,000 rows · lower is better',
  },
  {
    label: 'single-core CPU',
    metric: 'probe:cpu.single_mops_s',
    note: 'controlled workload · one core for 200 ms · never all cores',
  },
];

let selectedProbe = PROBE_METRICS[0];

const dom = {
  subtitle: document.getElementById('subtitle'),
  tiles: document.getElementById('tiles'),
  today: document.getElementById('today'),
  verdicts: document.getElementById('verdicts'),
  verdictsNote: document.getElementById('verdicts-note'),
  ranges: document.getElementById('ranges'),
  probeMetrics: document.getElementById('probe-metrics'),
  uncontended: document.getElementById('uncontended'),
  marks: document.getElementById('marks'),
  statusLine: document.getElementById('status-line'),
  events: document.querySelector('#events tbody'),
};

/** Stacked charts, in reading order. Each owns one metric and one y-axis.
 *
 *  A panel with no `format` derives one from the unit the server reports, which is how a probe series
 *  named after a catalogue entry gets a correct axis without the page knowing what it measures. That is
 *  also what lets the probe frame's metric be switched at runtime: see {@link PROBE_METRICS}. Its entry
 *  below opens on the default selection rather than hardcoding a metric of its own.
 */
const CHARTS = [
  { id: 'chart-cpu', metric: 'cpu_percent', format: percent, label: 'system CPU' },
  { id: 'chart-tools', metric: 'tool_read_ms', format: latency, label: 'median latency' },
  { id: 'chart-probe', metric: selectedProbe.metric, label: selectedProbe.label },
].map((config) => {
  const element = document.getElementById(config.id);
  const note = element.closest('.card')?.querySelector('.card-note') ?? null;
  const empty = document.querySelector(`[data-empty-for="${config.id}"]`);
  const panel = {
    ...config,
    unit: '',
    note,
    // The note as authored in the markup, so a resolution caveat can be appended and later removed
    // without the original wording being lost after the first poll.
    baseNote: note?.textContent.trim() ?? '',
    empty,
    // Likewise the empty state, which a failed load overwrites with the reason. Without the original to
    // restore, a metric that failed once would keep explaining itself under the next metric selected.
    baseEmpty: empty?.textContent.trim() ?? '',
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

/** The frame the probe switch drives, found by id so a renamed panel fails loudly at boot. */
const probePanel = CHARTS.find((panel) => panel.id === 'chart-probe');

/** Fetch JSON, surfacing the server's error message rather than a bare status code. */
async function api(path) {
  const response = await fetch(path, { cache: 'no-store' });
  const payload = await response.json().catch(() => null);
  if (!response.ok) {
    throw new Error(payload?.error ?? `${path} returned ${response.status}`);
  }
  return payload;
}

/** What each tile measures, keyed by the tile's identity in its section.
 *
 *  Authored as one block of prose rather than scattered through the renderers, because this is the part of
 *  the page that stops a number being misread — the agent's CPU is counted per core where the machine's is
 *  not, a process count is a minute older than the tile beside it, an absent value is not a zero — and a
 *  set of caveats is far easier to keep honest when it can be read end to end.
 *
 *  A tile whose key has no entry here simply gets no mark, which is what the placeholder tiles want: "No
 *  samples yet" is already its own explanation.
 */
const NOTES = {
  cpu: {
    title: 'System CPU',
    text: 'The whole machine at the moment of the last sample, every core averaged together, so it never'
      + ' exceeds 100%. Sampled every 5 seconds while the machine is busy and every 30 seconds while it is'
      + ' idle, by default.',
  },
  memory: {
    title: 'Memory in use',
    text: 'Physical memory in use across the machine, against the total installed. Everything running'
      + ' counts, not only the agent.',
  },
  swap: {
    title: 'Swap in use',
    text: 'Memory the machine has moved out to disk. Anything above zero while memory is nearly full'
      + ' explains more slow days than any other number on this page.',
  },
  processes: {
    title: 'Processes',
    text: 'Counted when the sampler last walked the whole process table — once a minute by default — so it'
      + ' can be that much older than the tiles beside it, however often this one refreshes.',
  },
  scanner: {
    title: 'Security scanner CPU',
    text: 'Summed over the processes matching the scanner names in the collect settings. Each process is'
      + ' measured against one core, so a scanner working hard on a many-core machine reads well above 100%.',
  },
  scannerAbsent: {
    title: 'Security scanner',
    text: 'No running process matched the scanner names in the collect settings. Absent is not zero:'
      + ' nothing was measured, rather than measured at nothing.',
  },
  agent: {
    title: 'Coding agent',
    text: 'The agent process and everything it started, summed: CPU against one core per process — so it'
      + ' can exceed 100% — its resident memory, and how many processes were seen. The set of processes is'
      + ' rediscovered once a minute.',
  },
  agentAbsent: {
    title: 'Coding agent',
    text: 'No agent process was running at the last discovery pass. Absent is not zero: the machine may'
      + ' simply not have had one running.',
  },
  probe: {
    title: 'Last probe',
    text: 'Probes are the controlled measurements: the same small workload every time, which is what makes'
      + ' two days comparable. A contended run is still recorded and still charted, but never counted'
      + ' towards a verdict — so the tile says which kind the last one was.',
  },
  probeAbsent: {
    title: 'Last probe',
    text: 'Probes run on their own interval, 15 minutes by default, and the first lands once one interval'
      + ' has passed with collection running. They can also be switched off entirely.',
  },
  requests: {
    title: "Today's requests",
    text: 'Requests to the agent since local midnight, read out of its own transcripts, and the number of'
      + ' separate sessions they belong to.',
  },
  toolCalls: {
    title: 'Tool calls',
    text: 'Every tool call in those requests, successful or not, and the projects they touched. Transcripts'
      + ' are imported at startup and every 30 seconds, so this trails the live tiles slightly.',
  },
  readLatency: {
    title: 'Median Read latency',
    text: 'The median successful Read the agent waited on today. Read alone: Grep and Glob are several'
      + ' times slower, so a pooled figure would move with whichever tool the model happened to reach for.',
  },
  outputTokens: {
    title: 'Output tokens',
    text: 'Tokens the model produced today. It describes the work asked of the agent rather than what the'
      + ' machine did with it, which is why no verdict is drawn from it.',
  },
  cacheHits: {
    title: 'Prompt cache hits',
    text: "The share of today's prompt tokens served from the cache rather than sent fresh. A property of"
      + ' the conversations, not of the machine.',
  },
  lastActivity: {
    title: 'Last agent activity',
    text: 'Time since the most recent turn or tool call in an imported transcript. Not a measure of the'
      + " daemon's own freshness — that is at the foot of the page.",
  },
};

let noteCount = 0;

/** The note a tile carries, whether it was passed one or names one in the catalogue. */
function noteFor(item) {
  return item.note ?? NOTES[item.key] ?? null;
}

/** Attach the corner mark and the note behind it, for a tile that has one to give.
 *
 *  Hovering reveals the note in CSS, so the pointer never waits on JavaScript; the click is here for touch
 *  screens and for keyboards, where hovering is not a gesture that exists.
 */
function addNote(node, note) {
  if (!note) return;
  const id = `note-${++noteCount}`;
  const mark = document.createElement('button');
  mark.type = 'button';
  mark.className = 'info-mark';
  mark.textContent = 'i';
  mark.setAttribute('aria-expanded', 'false');
  mark.setAttribute('aria-controls', id);

  const panel = document.createElement('div');
  panel.className = 'info-note';
  panel.id = id;
  panel.setAttribute('role', 'note');
  const title = document.createElement('strong');
  title.className = 'info-title';
  const text = document.createElement('span');
  text.className = 'info-text';
  panel.append(title, text);

  mark.addEventListener('click', () => toggleNote(mark, panel));
  // The reveal on hover is CSS, so the placement pass has to hang off the pointer itself.
  mark.addEventListener('pointerenter', () => keepInView(panel));
  mark.addEventListener('focus', () => keepInView(panel));
  // Immediately after the mark, and nothing between them: the reveal is a sibling selector.
  node.append(mark, panel);
  setNote(node, note);
}

/** Write a note's wording, which for a verdict changes with the window it was judged against. */
function setNote(node, note) {
  const mark = node.querySelector('.info-mark');
  if (!mark || !note) return;
  mark.setAttribute('aria-label', `About ${note.title}`);
  node.querySelector('.info-title').textContent = note.title;
  node.querySelector('.info-text').textContent = note.text;
}

/** Open one note, closing any other. */
function toggleNote(mark, panel) {
  const open = mark.getAttribute('aria-expanded') === 'true';
  closeNotes();
  if (open) return;
  mark.setAttribute('aria-expanded', 'true');
  keepInView(panel);
}

function closeNotes() {
  for (const mark of document.querySelectorAll('.info-mark[aria-expanded="true"]')) {
    mark.setAttribute('aria-expanded', 'false');
  }
}

/** Nudge a note back inside the viewport.
 *
 *  Notes hang from the corner of the tile they explain, and the tiles reach both edges of the page: anchored
 *  and left alone, the one in the last column would open past the right edge and read as a truncated
 *  sentence. Measured rather than computed from a column count, because the grid decides how many columns
 *  there are and only the browser knows how wide the text came out.
 */
function keepInView(panel) {
  const margin = 12;
  panel.style.setProperty('--nudge', '0px');
  const box = panel.getBoundingClientRect();
  const width = document.documentElement.clientWidth;
  const past = box.right - (width - margin);
  const short = margin - box.left;
  const nudge = past > 0 ? -past : short > 0 ? short : 0;
  if (nudge !== 0) panel.style.setProperty('--nudge', `${Math.round(nudge)}px`);
}

/** Draw a keyed list of tiles into a section, updating what is already there.
 *
 *  Replacing the children on every poll is simpler, and is what this page did until the tiles gained a note
 *  a reader can open: a rebuild every five seconds would close it under them and take the keyboard focus
 *  with it. So a section's shape is its list of keys, and while that is unchanged a poll rewrites only the
 *  text that moved.
 */
function reconcile(container, items, create, update) {
  const shape = items.map((item) => item.key).join('|');
  if (container.dataset.shape !== shape) {
    container.dataset.shape = shape;
    container.replaceChildren(...items.map((item) => create(item)));
    return;
  }
  items.forEach((item, index) => update(container.children[index], item));
}

/** One live number and what it is. */
function tileNode(item) {
  const node = document.createElement('div');
  node.className = 'tile';
  const value = document.createElement('div');
  value.className = 'tile-value';
  const label = document.createElement('div');
  label.className = 'tile-label';
  node.append(value, label);
  addNote(node, noteFor(item));
  updateTile(node, item);
  return node;
}

function updateTile(node, item) {
  const value = node.querySelector('.tile-value');
  value.className = item.absent ? 'tile-value absent' : 'tile-value';
  value.textContent = item.value;
  node.querySelector('.tile-label').textContent = item.label;
}

/** The most recent controlled measurement, and whether it was worth anything.
 *
 *  A contended probe is not a bad probe — it is deliberately collected and deliberately excluded later —
 *  so the tile says which it was rather than hiding it. Without that, a reader comparing the chart to the
 *  tile would find numbers that disagree and no explanation.
 */
function probeItem(probe) {
  if (!probe) {
    return {
      key: 'probeAbsent',
      value: 'No probes yet',
      label: 'the first runs after one probe interval',
      absent: true,
    };
  }
  const age = duration(Date.now() - probe.ts);
  if (probe.contended) {
    const because = probe.agent_active
      ? 'an agent was working'
      : probe.scanner_at !== null && probe.scanner_at > 0
        ? 'a scanner was active'
        : 'the machine was busy';
    return { key: 'probe', value: age, label: `since the last probe · contended, ${because}` };
  }
  const power = probe.on_battery === true ? ' · on battery' : '';
  return { key: 'probe', value: age, label: `since the last probe · uncontended${power}` };
}

/** The live tiles, in reading order. */
function liveItems(live) {
  const sample = live.sample;
  if (!sample) {
    return [{
      key: 'noSample',
      value: 'No samples yet',
      label: 'the daemon has not observed the machine',
      absent: true,
    }];
  }
  const memoryLabel = sample.total_memory
    ? `memory of ${gib(sample.total_memory)}`
    : 'memory in use';
  return [
    { key: 'cpu', value: percent(sample.cpu_percent), label: 'system CPU' },
    { key: 'memory', value: gib(sample.used_memory), label: memoryLabel },
    { key: 'swap', value: gib(sample.used_swap), label: 'swap in use' },
    { key: 'processes', value: count(sample.process_count), label: 'processes' },
    // Absent is not zero: a missing scanner means none was found, not that it used no CPU.
    sample.scanner_cpu === null
      ? { key: 'scannerAbsent', value: 'none found', label: 'security scanner', absent: true }
      : { key: 'scanner', value: percent(sample.scanner_cpu), label: 'security scanner CPU' },
    sample.agent_cpu === null
      ? { key: 'agentAbsent', value: 'not running', label: 'coding agent', absent: true }
      : {
        key: 'agent',
        value: percent(sample.agent_cpu),
        label: `agent CPU · ${mib(sample.agent_rss)} · ${count(sample.agent_processes)} proc`,
      },
    probeItem(live.probe),
  ];
}

function renderTiles(live) {
  reconcile(dom.tiles, liveItems(live), tileNode, updateTile);
}

/** Today's agent activity, from the transcripts the daemon has already read. */
function todayItems(today, dayStart) {
  if (!today || today.turns === 0) {
    return [{
      key: 'noActivity',
      value: 'No agent activity',
      label: `since ${dateTime(dayStart)}`,
      absent: true,
    }];
  }
  const projects = today.projects === 1 ? '1 project' : `${count(today.projects)} projects`;
  return [
    {
      key: 'requests',
      value: count(today.turns),
      label: `requests in ${count(today.sessions)} session(s)`,
    },
    { key: 'toolCalls', value: count(today.tool_calls), label: `tool calls · ${projects}` },
    // Absent is not zero: no reads yet means nothing was measured, not that it was instant.
    today.tool_read_p50_ms === null
      ? { key: 'readLatency', value: 'no reads yet', label: 'median Read latency', absent: true }
      : { key: 'readLatency', value: latency(today.tool_read_p50_ms), label: 'median Read latency' },
    { key: 'outputTokens', value: count(today.output_tokens), label: 'output tokens' },
    today.cache_hit_ratio === null
      ? { key: 'cacheHits', value: '—', label: 'prompt cache hits', absent: true }
      : { key: 'cacheHits', value: ratio(today.cache_hit_ratio), label: 'prompt cache hits' },
    {
      key: 'lastActivity',
      value: today.last_activity_ts === null
        ? '—'
        : duration(Date.now() - today.last_activity_ts),
      label: 'since the last agent activity',
      absent: today.last_activity_ts === null,
    },
  ];
}

function renderToday(today, dayStart) {
  reconcile(dom.today, todayItems(today, dayStart), tileNode, updateTile);
}

/** How the word on a verdict tile was arrived at.
 *
 *  Written from the payload rather than kept in {@link NOTES}, because the honest answer names the window
 *  the server actually used and says which stream the numbers came from — a probe verdict rests on clean
 *  runs of a fixed workload, and the one session verdict on whatever the agent happened to read.
 *
 *  The counts the rule turns on are deliberately not restated here. How many measurements a day needs, and
 *  how many days a band needs, are the server's constants; when either bites, the tile already carries the
 *  server's own sentence saying so.
 */
function verdictNote(comparison, windowDays) {
  const source = comparison.metric.startsWith('probe:')
    ? 'Uncontended probe runs only'
    : 'Every measurement the agent produced';
  const direction = comparison.lower_is_better ? 'Lower is better here.' : 'Higher is better here.';
  return {
    title: comparison.label,
    text: `${source}, reduced to one number a day, against the median of the previous ${windowDays} days.`
      + ` ${direction} Normal means today landed inside a band three sigma-equivalents either side of that`
      + ' median — derived from how much the days themselves varied, and never narrower than 5% of the'
      + ' median, so a very steady week cannot turn every small change into a finding.',
  };
}

/** One series judged against its trailing baseline.
 *
 *  Every tile states the evidence as well as the finding. A verdict drawn from four probes on three days is
 *  a different thing from one drawn from ninety on seven, and a reader deciding whether to go and look at
 *  the machine needs to be able to tell them apart without opening the API.
 */
function verdictNode(item) {
  const node = document.createElement('div');
  const head = document.createElement('div');
  head.className = 'verdict-head';
  const value = document.createElement('span');
  value.className = 'verdict-value';
  const word = document.createElement('span');
  word.className = 'verdict-word';
  head.append(value, word);
  const label = document.createElement('div');
  label.className = 'verdict-label';
  node.append(head, label);
  // Both lines exist from the start and empty ones are hidden, so a refresh that gains or loses a caveat
  // does not have to rebuild the tile — and cannot close a note a reader is in the middle of.
  for (const slot of ['evidence', 'caveat']) {
    const line = document.createElement('div');
    line.className = 'verdict-note';
    line.dataset.slot = slot;
    node.append(line);
  }
  addNote(node, item.note);
  updateVerdict(node, item);
  return node;
}

function updateVerdict(node, item) {
  const comparison = item.comparison;
  const format = unitFormatter(comparison.unit);
  node.className = `verdict ${comparison.verdict}`;
  node.querySelector('.verdict-value').textContent = comparison.today === null
    ? '—'
    : format(comparison.today);

  const word = node.querySelector('.verdict-word');
  word.className = `verdict-word ${comparison.verdict}`;
  // The word is always present: the colour of the rule is a second encoding, never the only one.
  word.textContent = comparison.verdict === 'insufficient'
    ? 'no verdict'
    : comparison.delta_percent === null
      ? comparison.verdict
      : `${comparison.verdict} ${comparison.delta_percent >= 0 ? '+' : ''}${comparison.delta_percent.toFixed(1)}%`;

  node.querySelector('.verdict-label').textContent = comparison.baseline === null
    ? comparison.label
    : `${comparison.label} · baseline ${format(comparison.baseline.median)} over ${count(comparison.baseline.days)} day(s)`;

  const evidence = comparison.baseline === null
    ? ''
    : `${count(comparison.today_observations)} today, ${count(comparison.baseline.observations)} in the baseline`;
  for (const [slot, text] of [['evidence', evidence], ['caveat', comparison.note ?? '']]) {
    const line = node.querySelector(`[data-slot="${slot}"]`);
    line.textContent = text;
    line.hidden = text === '';
  }
  setNote(node, item.note);
}

function renderVerdicts(payload) {
  const items = payload.comparisons.map((comparison) => ({
    key: comparison.metric,
    comparison,
    note: verdictNote(comparison, payload.window_days),
  }));
  reconcile(dom.verdicts, items, verdictNode, updateVerdict);
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

/** Draw the probe switch and point the probe frame at what it selects.
 *
 *  One frame rather than four stacked ones. The four measurements share nothing but a workload — ops/s,
 *  MiB/s, ms and Mops/s — so stacking them would add three y-axes to a page whose charts stack precisely
 *  because they can be read down a single shared cursor.
 *
 *  The unit is left alone here. It is replaced by the response, and until then the axis still describes
 *  the data still on screen, which is the previous metric's.
 */
function renderProbeMetrics() {
  dom.probeMetrics.replaceChildren();
  for (const choice of PROBE_METRICS) {
    const button = document.createElement('button');
    button.type = 'button';
    button.textContent = choice.label;
    button.setAttribute('aria-pressed', String(choice === selectedProbe));
    button.addEventListener('click', () => {
      selectedProbe = choice;
      renderProbeMetrics();
      void loadHistory();
    });
    dom.probeMetrics.append(button);
  }

  probePanel.metric = selectedProbe.metric;
  probePanel.baseNote = selectedProbe.note;
  probePanel.chart.setLabel(selectedProbe.label);
  if (probePanel.note) probePanel.note.textContent = selectedProbe.note;
  // Reset before the fetch rather than after it: an error from the metric being left behind must not
  // stand as the explanation for the one being selected.
  if (probePanel.empty) probePanel.empty.textContent = probePanel.baseEmpty;
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
        panel.empty.textContent = panel.baseEmpty;
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
renderProbeMetrics();
// Toggling what a chart counts must redraw it immediately: waiting a minute for the next poll would
// read as the filter having done nothing.
dom.uncontended.addEventListener('change', () => void loadHistory());
// A note opened by click stays open until it is dismissed, so both the usual dismissals are wired here
// rather than left to the poll that used to clear the tiles for us.
document.addEventListener('click', (event) => {
  const inside = event.target instanceof Element && event.target.closest('.info-mark, .info-note');
  if (!inside) closeNotes();
});
document.addEventListener('keydown', (event) => {
  if (event.key === 'Escape') closeNotes();
});
void loadLive();
void loadHealth();
void loadHistory();
setInterval(loadLive, LIVE_POLL_MS);
setInterval(loadHealth, HISTORY_POLL_MS);
setInterval(loadHistory, HISTORY_POLL_MS);
