// Dashboard boot: polls the API, renders the live tiles, the history chart, and daemon health.

import {
  percent, gib, mib, count, duration, dateTime, latency, ratio, unitFormatter,
} from './format.js';
import {
  SYSTEM_SERIES, AGENT_SERIES, PROBE_METRICS, CONDITION_SERIES,
} from './series.js';
import { createChart } from './chart.js';

/** Live tiles refresh on this cadence; the server samples independently of it.
 *
 *  Only /api/live rides this poll, and it reads one row per stream, which is what makes it affordable
 *  twelve times a minute. Every endpoint that aggregates — status, verdicts, the day's activity — is on
 *  {@link HISTORY_POLL_MS} for the opposite reason.
 */
const LIVE_POLL_MS = 5000;

/** History, health, verdicts and the day's totals reload here: each costs a scan to compute. */
const HISTORY_POLL_MS = 60000;

const RANGES = [
  { label: '1h', ms: 3600000 },
  { label: '6h', ms: 21600000 },
  { label: '24h', ms: 86400000 },
  { label: '48h', ms: 172800000 },
  { label: '7d', ms: 604800000 },
];

/** A day, because that is the question the page is usually opened to answer.
 *
 *  Two days was the first choice and it reads as the wrong one on a machine that is actually used: a working
 *  day's worth of detail gets compressed into half the frame, and the yesterday half is history the reader
 *  did not ask for. The wider ranges are one click away and keep their place in the list.
 */
let selectedRange = RANGES.find((range) => range.label === '24h');

/** Events shown while the table is collapsed, and the most it will ever hold.
 *
 *  The expanded count is what `/api/status` is asked for, so expanding costs no request: that endpoint runs
 *  six aggregates over the fact tables to answer, and a click is not a good reason to pay for them again.
 *  Ninety extra rows is a few kilobytes on a payload that already carries every series name.
 */
const EVENTS_COLLAPSED = 10;
const EVENTS_EXPANDED = 100;

/** Whether the reader has opened the table, and the rows last fetched to redraw it from.
 *
 *  Held outside the render so a poll landing while the table is open does not close it — the health payload
 *  refreshes every minute, and a list that collapsed under the reader would be worse than no disclosure.
 */
let eventsExpanded = false;
let latestEvents = [];

const dom = {
  subtitle: document.getElementById('subtitle'),
  tiles: document.getElementById('tiles'),
  today: document.getElementById('today'),
  verdicts: document.getElementById('verdicts'),
  verdictsNote: document.getElementById('verdicts-note'),
  ranges: document.getElementById('ranges'),
  systemMetrics: document.getElementById('system-metrics'),
  agentMetrics: document.getElementById('agent-metrics'),
  probeMetrics: document.getElementById('probe-metrics'),
  conditionMetrics: document.getElementById('condition-metrics'),
  uncontended: document.getElementById('uncontended'),
  marks: document.getElementById('marks'),
  statusLine: document.getElementById('status-line'),
  events: document.querySelector('#events tbody'),
  eventsMore: document.getElementById('events-more'),
};

/** Stacked frames, in reading order: what the machine did, what the agent experienced, the controlled
 *  measurement, and the conditions that measurement was taken under.
 *
 *  Four frames rather than twenty-seven. Every frame owns one metric and one y-axis at a time and switches
 *  between the choices its catalogue offers, because these series share nothing but a timeline — percentages,
 *  bytes, milliseconds, tokens per second — and stacking them would give a page of axes to read where the
 *  whole design is one shared cursor read straight down.
 *
 *  No panel names a formatter. Every series reports its unit and the axis is derived from it, which is how a
 *  probe series named after a catalogue entry gets a correct axis without the page knowing what it measures
 *  — and, more to the point, what makes a switchable frame safe: a panel that hardcoded `percent` and was
 *  then pointed at a byte rate would render 1,066 MiB/s as "1066.0%" with nothing on screen looking wrong.
 */
const CHARTS = [
  { id: 'chart-cpu', switchNode: dom.systemMetrics, choices: SYSTEM_SERIES },
  { id: 'chart-tools', switchNode: dom.agentMetrics, choices: AGENT_SERIES },
  { id: 'chart-probe', switchNode: dom.probeMetrics, choices: PROBE_METRICS },
  { id: 'chart-conditions', switchNode: dom.conditionMetrics, choices: CONDITION_SERIES },
].flatMap((config) => {
  const element = document.getElementById(config.id);
  // Reported and skipped rather than thrown. This runs while the module is still evaluating, so an
  // exception here does not cost one chart — it stops the rest of the file, and the page renders nothing
  // at all. That is exactly what a stale cached copy of this script used to do after a panel was renamed.
  if (!element) {
    console.error(`no element #${config.id} in the page, so its chart is skipped`);
    return [];
  }
  const line = element.closest('.card')?.querySelector('.card-note') ?? null;
  const empty = document.querySelector(`[data-empty-for="${config.id}"]`);
  // Both halves of that line are written from the catalogue, so the markup holds an empty paragraph: a
  // caption authored in two places is how a frame comes to describe the previous selection's scale.
  let caption = null;
  let anchor = null;
  if (line) {
    caption = document.createElement('span');
    anchor = document.createElement('span');
    anchor.className = 'note-anchor';
    // The mark goes after the caption, and the anchor is what it hangs from: the note it reveals is
    // positioned against that, so it opens under the mark rather than under the whole frame.
    line.replaceChildren(caption, anchor);
  }
  const [selected] = config.choices;
  const panel = {
    ...config,
    selected,
    metric: selected.metric,
    unit: '',
    caption,
    anchor,
    // The caption as the switch last wrote it, so a resolution caveat can be appended and later removed
    // without the selection's own wording being lost after the first poll.
    baseCaption: selected.caption,
    empty,
    // Likewise the empty state, which a failed load overwrites with the reason. Without the original to
    // restore, a metric that failed once would keep explaining itself under the next metric selected.
    baseEmpty: empty?.textContent.trim() ?? '',
  };
  // The closure reads `panel.unit` on every call rather than capturing it, so a response can change the
  // unit — which switching the metric does — without the plot having to be rebuilt.
  panel.chart = createChart(element, (value) => unitFormatter(panel.unit)(value), panel.selected.label);
  return [panel];
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

/** What each tile measures, keyed by the tile's identity in its section.
 *
 *  Authored as one block of prose rather than scattered through the renderers, because this is the part of
 *  the page that stops a number being misread — the agent's CPU is counted per core where the machine's is
 *  not, a process count is a minute older than the tile beside it, an absent value is not a zero — and a
 *  set of caveats is far easier to keep honest when it can be read end to end.
 *
 *  A tile whose key has no entry here simply gets no mark, which is what the placeholder tiles want: "No
 *  samples yet" is already its own explanation.
 *
 *  The history frames' notes are not here but in `series.js`, beside the choice each one describes: a frame
 *  says something different for every measurement it can be pointed at, so its note belongs to the entry
 *  that selects it rather than to the panel that displays it.
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
      + ' towards a verdict — so the tile says which kind the last one was, and which threshold a contended'
      + ' one crossed. "Busiest" names the machine’s largest CPU consumer, measured over the interval'
      + ' since the previous probe rather than at the moment this one ran: it says what has been using the'
      + ' machine, which is not the same claim as what made this run slow.',
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
    text: 'Time since the most recent turn or tool call in an imported transcript. The age ticks with the'
      + ' live tiles, but the totals in this section are re-counted once a minute, so a turn that has just'
      + " landed can take that long to appear. Not a measure of the daemon's own freshness — that is at the"
      + ' foot of the page.',
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

/** What the server's word for a contention cause reads as on the tile. */
const CONTENTION_CAUSE = {
  agent: 'an agent was working',
  scanner: 'a scanner was active',
  disk: 'the disk was busy',
  machine: 'the machine was busy',
};

/** The most recent controlled measurement, and whether it was worth anything.
 *
 *  A contended probe is not a bad probe — it is deliberately collected and deliberately excluded later —
 *  so the tile says which it was rather than hiding it. Without that, a reader comparing the chart to the
 *  tile would find numbers that disagree and no explanation.
 *
 *  The cause comes from the server. It used to be inferred here from three covariates, which was wrong the
 *  moment a fourth threshold existed: a run tagged solely because something was writing 60 MiB/s fell
 *  through to "the machine was busy" at 16% CPU, because this page had no way to know what rate counts as
 *  busy — and no business knowing it.
 *
 *  The largest consumer is appended as attribution, never as the cause. Its figure spans the interval since
 *  the previous probe, so it says what has been using the machine rather than what made this run slow, and
 *  the wording has to keep those apart even when they happen to be the same process.
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
  const largest = probe.top_consumer ? ` · busiest ${probe.top_consumer.name}` : '';
  if (probe.contended) {
    // Falls back to the bare word rather than to a guess: an unrecognised cause means this page is older
    // than the daemon serving it, and inventing a reason would be worse than declining to give one.
    const because = CONTENTION_CAUSE[probe.cause] ?? 'something else was using the machine';
    return {
      key: 'probe',
      value: age,
      label: `since the last probe · contended, ${because}${largest}`,
    };
  }
  const power = probe.on_battery === true ? ' · on battery' : '';
  return {
    key: 'probe',
    value: age,
    label: `since the last probe · uncontended${power}${largest}`,
  };
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
  // Only said when there is a conditions line to explain, so a tile without one is not made to carry a
  // paragraph about a feature it is not using.
  const conditions = comparison.conditions
    ? ' The last line names the conditions those same clean runs were taken in, where one of them sits'
      + ' outside its own band on the same rule as the verdict above. It is an explanation, not a'
      + ' correction: nothing about the verdict has been adjusted for it.'
    : '';
  return {
    title: comparison.label,
    text: `${source}, reduced to one number a day, against the median of the previous ${windowDays} days.`
      + ` ${direction} Normal means today landed inside a band three sigma-equivalents either side of that`
      + ' median — derived from how much the days themselves varied, and never narrower than 5% of the'
      + ` median, so a very steady week cannot turn every small change into a finding.${conditions}`,
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
  // All three lines exist from the start and empty ones are hidden, so a refresh that gains or loses a
  // caveat does not have to rebuild the tile — and cannot close a note a reader is in the middle of.
  for (const slot of ['evidence', 'caveat', 'conditions']) {
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
  // Three lines, in the order they become useful: what the finding rests on, what qualifies it, then what
  // else was different at the same time. The last is an explanation rather than a caveat and is kept
  // separate for that reason — the server decides when there is one, and only for a verdict it reached.
  for (const [slot, text] of [
    ['evidence', evidence],
    ['caveat', comparison.note ?? ''],
    ['conditions', comparison.conditions?.summary ?? ''],
  ]) {
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

  renderEvents(status.events);
}

/** The events table and its disclosure, redrawn from whatever was last fetched.
 *
 *  Separate from {@link renderStatus} because the button calls it too: a click changes how many of the same
 *  rows are on screen and nothing else, so it must not need a payload to do it.
 */
function renderEvents(events) {
  latestEvents = events;
  dom.events.replaceChildren();
  if (events.length === 0) {
    const row = document.createElement('tr');
    const cell = document.createElement('td');
    cell.colSpan = 3;
    cell.textContent = 'Nothing to report.';
    row.append(cell);
    dom.events.append(row);
    dom.eventsMore.hidden = true;
    return;
  }
  const shown = eventsExpanded ? events : events.slice(0, EVENTS_COLLAPSED);
  for (const event of shown) {
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
  // Nothing behind it means no button: a disclosure that opens onto the same ten rows is a control that
  // reports the daemon is quiet by doing nothing when pressed.
  dom.eventsMore.hidden = events.length <= EVENTS_COLLAPSED;
  // The count is on the button rather than in prose beside it, so the label says what pressing it will do.
  dom.eventsMore.textContent = eventsExpanded
    ? `Show ${EVENTS_COLLAPSED} most recent`
    : `Show ${events.length - shown.length} more`;
  dom.eventsMore.setAttribute('aria-expanded', String(eventsExpanded));
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

/** Draw one frame's switch and point the frame at what it selects.
 *
 *  One renderer for all four, which is the only way the fourth frame was affordable: the switch, the
 *  caption, the tooltip label and the note behind the mark all have to change together, and four copies of
 *  that sequence is four chances for one of them to be left describing the previous selection.
 *
 *  The unit is left alone here. It is replaced by the response, and until then the axis still describes
 *  the data still on screen, which is the previous metric's.
 */
function renderSwitch(panel) {
  const choice = panel.selected;
  if (!panel.switchNode) {
    // A missing switch costs the reader the ability to change metric and nothing else, so the frame still
    // charts its default. Guarded because the alternative is `Cannot read properties of null` thrown from
    // here, which would take the rest of the page down over one absent element.
    console.error(`no switch element for #${panel.id}, so its metric cannot be changed`);
  } else {
    panel.switchNode.replaceChildren();
    for (const option of panel.choices) {
      const button = document.createElement('button');
      button.type = 'button';
      button.textContent = option.label;
      button.setAttribute('aria-pressed', String(option === choice));
      button.addEventListener('click', () => {
        panel.selected = option;
        renderSwitch(panel);
        void loadHistory();
      });
      panel.switchNode.append(button);
    }
  }

  panel.metric = choice.metric;
  panel.baseCaption = choice.caption;
  panel.chart.setLabel(choice.label);
  if (panel.caption) panel.caption.textContent = choice.caption;
  // Rewritten rather than rebuilt, like a verdict's note: recreating the mark would close a note the reader
  // has open and take the keyboard focus out of the page with it.
  if (panel.anchor) setNote(panel.anchor, choice.note);
  // Reset before the fetch rather than after it: an error from the metric being left behind must not
  // stand as the explanation for the one being selected.
  if (panel.empty) panel.empty.textContent = panel.baseEmpty;
}

/** Reported by /api/status, which is polled far less often than the tiles that display it. */
let uplotVersion = null;

/** The day's activity as last fetched, and the day it covers.
 *
 *  Held rather than refetched with the live tiles. /api/today is a scan of the whole day — two aggregates
 *  plus two percentile passes — and the importer feeding it polls every thirty seconds at best, so there is
 *  nothing there to see twelve times a minute. The one figure that does move continuously is "since the last
 *  agent activity", and that is a timestamp: re-rendering the cached payload against the browser's clock on
 *  every live poll keeps the tile ticking for no query at all.
 *
 *  The day start comes from the server both here and on /api/live, because a boundary computed from the
 *  browser's clock could name a different day than the one the totals were counted over.
 */
let todayActivity = null;
let dayStartTs = null;

async function loadLive() {
  try {
    const live = await api('/api/live');
    renderTiles(live);
    dayStartTs = live.day_start_ts;
    renderToday(todayActivity, dayStartTs);
    const machine = `machine ${live.machine_id.slice(0, 12)}`;
    dom.subtitle.textContent = uplotVersion ? `${machine} · uPlot ${uplotVersion}` : machine;
  } catch (error) {
    dom.subtitle.textContent = `cannot reach the daemon: ${error.message}`;
  }
}

/** Daemon health, today's verdicts and today's totals, on the slow cadence.
 *
 *  All three used to ride along with the live tiles every five seconds, and between them they cost the
 *  machine more than the collectors they report on: /api/status runs six count(*) aggregates over the
 *  fact tables, /api/verdicts re-derives a whole trailing window, one aggregation per day in it, and
 *  /api/today scans the day for a median and a cache ratio. The server handles requests one at a time on
 *  the main thread at normal priority, so an open dashboard was biasing the very series it was drawing.
 *
 *  None of the three changes on a five-second timescale. A verdict moves when a probe lands, four times
 *  an hour; the health counts move by a handful of rows; the day's totals cannot move faster than the
 *  importer that feeds them. A minute is still far finer than any of them.
 */
async function loadHealth() {
  try {
    const [status, verdicts, today] = await Promise.all([
      api(`/api/status?events=${EVENTS_EXPANDED}`), api('/api/verdicts'), api('/api/today'),
    ]);
    uplotVersion = status.uplot_version;
    todayActivity = today.today;
    dayStartTs = today.day_start_ts;
    renderToday(todayActivity, dayStartTs);
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
        // Assigned unconditionally, including the empty string a bare count reports: a `if (series.unit)`
        // here would leave the previous selection's unit in place for exactly the series that has none.
        panel.unit = series.unit ?? '';
        panel.chart.setAnnotations(annotations);
        panel.chart.update(series.points, series.gap_ms);
        panel.empty.hidden = series.points.length > 0;
        panel.empty.textContent = panel.baseEmpty;
        const note = resolutionNote(series);
        if (panel.caption) {
          panel.caption.textContent = note ? `${panel.baseCaption} · ${note}` : panel.baseCaption;
        }
      } catch (error) {
        panel.empty.hidden = false;
        panel.empty.textContent = `Could not load ${panel.metric}: ${error.message}`;
      }
    }),
  );
}

/** Wrap a loader so a tick arriving while the previous one is still in flight is dropped.
 *
 *  The server answers one request at a time, so a slow query does not merely delay one poll: every poll
 *  behind it queues, and an unguarded `setInterval` goes on adding to that queue while the page waits — so
 *  the moment the machine is busiest is the moment the dashboard asks it for the most. Dropping a tick costs
 *  nothing, because every payload is a snapshot rather than a delta and the next tick is already scheduled.
 *
 *  Only the timers are wrapped. A range button or the contention filter has to redraw the chart it was
 *  clicked for, so those keep calling the loader directly.
 */
function polled(load) {
  let inFlight = false;
  return async () => {
    if (inFlight) return;
    inFlight = true;
    try {
      await load();
    } finally {
      inFlight = false;
    }
  };
}

renderRanges();
// The mark is created once per frame and its wording rewritten by every later selection, for the same
// reason a tile's is: rebuilding it would close a note a reader is in the middle of.
for (const panel of CHARTS) {
  if (panel.anchor) addNote(panel.anchor, panel.selected.note);
  renderSwitch(panel);
}
// Toggling what a chart counts must redraw it immediately: waiting a minute for the next poll would
// read as the filter having done nothing.
dom.uncontended.addEventListener('change', () => void loadHistory());
// Redrawn from the rows already held, for the reason on renderEvents: revealing what has been fetched is
// not a reason to refetch it.
dom.eventsMore.addEventListener('click', () => {
  eventsExpanded = !eventsExpanded;
  renderEvents(latestEvents);
});
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
setInterval(polled(loadLive), LIVE_POLL_MS);
setInterval(polled(loadHealth), HISTORY_POLL_MS);
setInterval(polled(loadHistory), HISTORY_POLL_MS);
