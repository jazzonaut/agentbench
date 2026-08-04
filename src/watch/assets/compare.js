// The compare page: two report files in, one comparison out.
//
// The files are read here and the arithmetic is done in Rust. That division is deliberate — what counts as
// a regression depends on each metric's direction and on whether the metric is informational at all, and
// those rules live in `compare::interpretation`. A second implementation of them in this file would be a
// second opinion, and the two would eventually disagree about the same pair of files.

import { dateTime, unitFormatter } from './format.js';

const dom = {
  form: document.getElementById('compare-form'),
  baseline: document.getElementById('baseline'),
  candidate: document.getElementById('candidate'),
  run: document.getElementById('run-compare'),
  error: document.getElementById('error'),
  result: document.getElementById('result'),
  headingNote: document.getElementById('heading-note'),
  environment: document.getElementById('environment'),
  environmentNote: document.getElementById('environment-note'),
  metrics: document.querySelector('#metrics tbody'),
  metricsNote: document.getElementById('metrics-note'),
  profilesCard: document.getElementById('profiles-card'),
  profiles: document.querySelector('#profiles tbody'),
};

/** Show the error notice, or hide it. */
function fail(message) {
  dom.error.textContent = message ?? '';
  dom.error.hidden = !message;
}

/** Read a picked file and parse it as a report.
 *
 *  Parsed here as well as on the server, and not redundantly: a file the user picked by mistake — a
 *  markdown summary, a log, the wrong JSON — is caught before anything is uploaded, and the message can
 *  name which of the two pickers is wrong. The server parses it again because it does not trust this page,
 *  which is the correct relationship between the two.
 */
async function readReport(input, label) {
  const file = input.files?.[0];
  if (!file) throw new Error(`Pick a ${label} report.`);
  let text;
  try {
    text = await file.text();
  } catch (error) {
    throw new Error(`Could not read the ${label} file: ${error.message}`);
  }
  try {
    return JSON.parse(text);
  } catch {
    throw new Error(
      `The ${label} file (${file.name}) is not JSON. Pick the .json report rather than the .md summary.`,
    );
  }
}

/** Enable the button once both files are chosen.
 *
 *  The chosen filename is deliberately not echoed under the picker: the browser's own control already shows
 *  it, and printing it again gave every field the same name twice over. The hint keeps saying which of the
 *  two reports belongs here, which is the part a reader still needs after picking one.
 */
function syncPickers() {
  dom.run.disabled = !(dom.baseline.files?.length && dom.candidate.files?.length);
}

/** Signed percentage, with the sign always shown.
 *
 *  A change of `+0.4%` and one of `-0.4%` must not both render as `0.4%`: the direction is the reading.
 *
 *  A value that rounds to zero loses its sign entirely. `-0.04%` displayed as `-0.0%` reads as a bug in the
 *  formatter rather than as a run that did not move, and the sign is meaningless at that magnitude anyway.
 */
function signedPercent(value) {
  const rounded = value.toFixed(1);
  if (Number.parseFloat(rounded) === 0) return '0.0%';
  return `${value >= 0 ? '+' : ''}${rounded}%`;
}

/** Draw the environment differences, or say there were none. */
function renderEnvironment(comparison) {
  dom.environment.replaceChildren();
  if (comparison.environment.length === 0) {
    // The common case on one machine, and a positive finding rather than an absence: it is what makes the
    // metric deltas below worth reading at all.
    dom.environmentNote.textContent =
      'Nothing differs. The two runs saw the same OS, CPU, cores, memory and tool versions, which is what'
      + ' makes the deltas below attributable to something other than the machine.';
    return;
  }
  dom.environmentNote.textContent =
    'These differ between the two runs, so a delta below may be describing the difference rather than a'
    + ' change in the machine.';
  for (const difference of comparison.environment) {
    const item = document.createElement('li');
    item.style.display = 'block';
    const name = document.createElement('strong');
    name.textContent = `${difference.name}: `;
    const values = document.createElement('span');
    values.textContent = `${difference.baseline} → ${difference.candidate}`;
    item.append(name, values);
    dom.environment.append(item);
  }
}

/** Draw one metric row. */
function metricRow(delta) {
  const format = unitFormatter(delta.unit);
  const row = document.createElement('tr');

  const name = document.createElement('td');
  const label = document.createElement('code');
  label.className = 'metric-name';
  label.textContent = delta.name;
  const note = document.createElement('span');
  note.className = 'metric-note';
  note.textContent = delta.description;
  name.append(label, note);

  const baseline = document.createElement('td');
  baseline.className = 'number';
  baseline.textContent = format(delta.baseline);

  const candidate = document.createElement('td');
  candidate.className = 'number';
  candidate.textContent = format(delta.candidate);

  const change = document.createElement('td');
  change.className = 'number';
  change.textContent = signedPercent(delta.change_percent);

  // The word carries the meaning and the colour repeats it, never the other way round: a reader who cannot
  // distinguish the two colours still reads "regression".
  const reading = document.createElement('td');
  const tag = document.createElement('span');
  tag.className = `verdict-tag ${delta.interpretation}`;
  tag.textContent = delta.interpretation;
  reading.append(tag);

  row.append(name, baseline, candidate, change, reading);
  return row;
}

function renderMetrics(comparison) {
  dom.metrics.replaceChildren();
  if (comparison.metrics.length === 0) {
    const row = document.createElement('tr');
    const cell = document.createElement('td');
    cell.colSpan = 5;
    cell.className = 'empty';
    // Not an error: two reports can be comparable and still share no metric, which is what a profile run
    // compared with another profile run looks like.
    cell.textContent = 'No metric appears in both reports.';
    row.append(cell);
    dom.metrics.append(row);
    dom.metricsNote.textContent = '';
    return;
  }
  dom.metricsNote.textContent =
    `A change beyond ${comparison.threshold_percent}% either way is called a regression or an improvement;`
    + ' anything inside it is reported as similar, because two runs give one observation each. Metrics'
    + ' whose value depends on what the run contained rather than on the machine are informational.';
  for (const delta of comparison.metrics) {
    dom.metrics.append(metricRow(delta));
  }
}

function renderProfiles(comparison) {
  dom.profiles.replaceChildren();
  dom.profilesCard.hidden = comparison.profiles.length === 0;
  for (const delta of comparison.profiles) {
    const row = document.createElement('tr');
    for (const [text, number] of [
      [delta.label, false],
      [`${delta.baseline_ms.toFixed(0)} ms`, true],
      [`${delta.candidate_ms.toFixed(0)} ms`, true],
      [signedPercent(delta.change_percent), true],
    ]) {
      const cell = document.createElement('td');
      if (number) cell.className = 'number';
      cell.textContent = text;
      row.append(cell);
    }
    dom.profiles.append(row);
  }
}

function render(comparison) {
  const preset = comparison.preset ? `preset ${comparison.preset} · ` : '';
  dom.headingNote.textContent =
    `${preset}baseline ${comparison.baseline_run} of ${dateTime(Date.parse(comparison.baseline_created_at))}`
    + ` → candidate ${comparison.candidate_run} of`
    + ` ${dateTime(Date.parse(comparison.candidate_created_at))}`;
  renderEnvironment(comparison);
  renderMetrics(comparison);
  renderProfiles(comparison);
  dom.result.hidden = false;
}

dom.baseline.addEventListener('change', syncPickers);
dom.candidate.addEventListener('change', syncPickers);

dom.form.addEventListener('submit', async (event) => {
  event.preventDefault();
  fail(null);
  dom.run.disabled = true;
  try {
    const [baseline, candidate] = await Promise.all([
      readReport(dom.baseline, 'baseline'),
      readReport(dom.candidate, 'candidate'),
    ]);
    const response = await fetch('/api/compare', {
      method: 'POST',
      cache: 'no-store',
      // Required, and not merely conventional: the server refuses a write that is not JSON, which is what
      // stops a form on another site from reaching this endpoint at all.
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ baseline, candidate }),
    });
    const payload = await response.json().catch(() => null);
    if (!response.ok) {
      // The server's own sentence — "benchmark presets differ: quick vs standard" — is the whole answer,
      // and is shown unaltered.
      throw new Error(payload?.error ?? `the daemon returned ${response.status}`);
    }
    render(payload);
  } catch (error) {
    dom.result.hidden = true;
    fail(error.message);
  } finally {
    syncPickers();
  }
});

syncPickers();
