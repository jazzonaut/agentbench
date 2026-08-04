// The benchmark page: builds a `bench` invocation from a form, starts it, and follows it.
//
// Every option this form offers is described by /api/bench/options rather than by this file. A preset's
// duration and disk budget live in `bench::preset::Limits` and are published from there, so the sentence
// under the preset selector cannot claim a `stress` run writes ten gigabytes on the day that number
// changes. The same endpoint says whether this daemon will start a run at all.

import { bytes, count, duration } from './format.js';

/** How often the run's state is re-read while one is in flight.
 *
 *  A second. Phases are announced tens of seconds apart, so this is not about resolution — it is about a
 *  stopped or crashed child being noticed promptly, because until it is, the page is showing a gauge for
 *  something that is no longer running.
 */
const RUN_POLL_MS = 1000;

/** Cadence while nothing is running: enough to notice a run started from somewhere else. */
const IDLE_POLL_MS = 10000;

const dom = {
  refusal: document.getElementById('refusal'),
  fields: document.getElementById('fields'),
  form: document.getElementById('bench-form'),
  preset: document.getElementById('preset'),
  presetHint: document.getElementById('preset-hint'),
  targetDir: document.getElementById('target-dir'),
  scratchDir: document.getElementById('scratch-dir'),
  offline: document.getElementById('offline'),
  liveLlm: document.getElementById('live-llm'),
  liveLlmHint: document.getElementById('live-llm-hint'),
  routeField: document.getElementById('route-field'),
  llmRoute: document.getElementById('llm-route'),
  modelField: document.getElementById('model-field'),
  llmModel: document.getElementById('llm-model'),
  capField: document.getElementById('cap-field'),
  llmCostCap: document.getElementById('llm-cost-cap'),
  capHint: document.getElementById('cap-hint'),
  start: document.getElementById('start'),
  cancel: document.getElementById('cancel'),
  busy: document.getElementById('busy'),
  runMessage: document.getElementById('run-message'),
  runCard: document.getElementById('run-card'),
  phase: document.getElementById('phase'),
  gaugeFill: document.getElementById('gauge-fill'),
  elapsed: document.getElementById('elapsed'),
  runSummary: document.getElementById('run-summary'),
  runError: document.getElementById('run-error'),
  runDone: document.getElementById('run-done'),
  reportsDir: document.getElementById('reports-dir'),
};

/** Presets as the server described them, keyed by name. */
let presets = new Map();

/** Phases a run announces, until the server says otherwise. */
let phaseCount = 8;

/** Whether this daemon will start runs at all. */
let allowed = false;

/** Handle of the poll in flight, so the cadence can change without stacking timers. */
let pollTimer = null;

/** Fetch JSON, surfacing the server's own message rather than a bare status code. */
async function api(path, options) {
  const response = await fetch(path, { cache: 'no-store', ...options });
  const payload = await response.json().catch(() => null);
  if (!response.ok) {
    throw new Error(payload?.error ?? `${path} returned ${response.status}`);
  }
  return payload;
}

/** Send a body to a path that acts.
 *
 *  The content type is not decoration. The server refuses a write that is not `application/json`, because
 *  that is what forces a browser to preflight the request and so what stops a form on another site from
 *  starting a benchmark here. Sending the wrong one gets a 403, not a parse error.
 */
function post(path, body) {
  return api(path, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(body ?? {}),
  });
}

/** Show one of the page's notices, or hide it when there is nothing to say. */
function notice(node, text) {
  node.textContent = text ?? '';
  node.hidden = !text;
}

/** Describe what a preset commits the machine to, from the limits the server published. */
function describePreset(preset) {
  if (!preset) return '';
  const parts = [
    `up to ${duration(preset.duration_limit_seconds * 1000)}`,
    `${bytes(preset.disk_limit_bytes)} written`,
    `${count(preset.small_files)} small files`,
  ];
  if (preset.minimum_duration_seconds > 0) {
    parts.push(`at least ${duration(preset.minimum_duration_seconds * 1000)}`);
  }
  return parts.join(' · ');
}

/** Show or hide the three controls that only mean something when live cases are on.
 *
 *  Hidden rather than disabled: a route, a model and a cost cap for calls that will not be made are three
 *  questions the form has no business asking, and greying them out still leaves them to be read past.
 */
function syncLiveLlm() {
  const on = dom.liveLlm.checked;
  for (const field of [dom.routeField, dom.modelField, dom.capField]) {
    field.hidden = !on;
  }
}

/** Draw the form from the server's description of it. */
async function loadOptions() {
  let options;
  try {
    options = await api('/api/bench/options');
  } catch (error) {
    notice(dom.refusal, `Cannot reach the daemon: ${error.message}`);
    dom.fields.disabled = true;
    return;
  }
  allowed = options.allowed;
  phaseCount = options.phase_count;
  presets = new Map(options.presets.map((preset) => [preset.name, preset]));

  dom.preset.replaceChildren();
  for (const preset of options.presets) {
    const option = document.createElement('option');
    option.value = preset.name;
    option.textContent = preset.name;
    dom.preset.append(option);
  }
  dom.preset.value = options.default_preset;
  dom.presetHint.textContent = describePreset(presets.get(dom.preset.value));

  dom.targetDir.value = options.default_target_dir;
  dom.llmModel.value = 'sonnet';
  dom.llmCostCap.value = '5';
  dom.llmCostCap.max = String(options.max_cost_cap_usd);
  dom.capHint.textContent = `US dollars, up to $${options.max_cost_cap_usd} from this page.`
    + ' The run stops before exceeding it.';
  dom.liveLlmHint.textContent = 'Off by default. These calls are billed to whichever API key this daemon'
    + ` runs with, up to the cap below — never more than $${options.max_cost_cap_usd} from this page.`;
  if (options.reports_dir) dom.reportsDir.textContent = options.reports_dir;

  if (!allowed) {
    notice(dom.refusal, options.refusal ?? 'This daemon does not start benchmarks.');
    dom.fields.disabled = true;
    dom.start.disabled = true;
  }
  syncLiveLlm();
}

/** The request body, from whatever the form currently says.
 *
 *  Only what live cases need is sent when they are on. Sending a model and a cap alongside `live_llm:
 *  false` would be sending values nothing will read, and the summary the server hands back would then have
 *  to decide whether to display them.
 */
function requestBody() {
  const body = {
    preset: dom.preset.value,
    target_dir: dom.targetDir.value.trim(),
    offline: dom.offline.checked,
    live_llm: dom.liveLlm.checked,
  };
  const scratch = dom.scratchDir.value.trim();
  if (scratch) body.scratch_dir = scratch;
  if (dom.liveLlm.checked) {
    body.llm_route = dom.llmRoute.value;
    body.llm_model = dom.llmModel.value.trim();
    const cap = Number.parseFloat(dom.llmCostCap.value);
    if (Number.isFinite(cap)) body.llm_cost_cap_usd = cap;
  }
  return body;
}

/** One line describing what a run was asked to do. */
function summarise(request) {
  const parts = [`preset ${request.preset}`, `target ${request.target_dir}`];
  if (request.scratch_dir) parts.push(`scratch ${request.scratch_dir}`);
  if (request.offline) parts.push('network probe skipped');
  parts.push(
    request.live_llm
      ? `live Claude via ${request.llm_route}, ${request.llm_model}, capped at $${request.llm_cost_cap_usd}`
      : 'no live Claude calls',
  );
  return parts.join(' · ');
}

/** Draw whatever the registry says it is doing. */
function renderRun(state) {
  const running = state.state === 'running';
  dom.cancel.hidden = !running;
  // The options are locked while a run is in flight, because the machine can only do one at a time and a
  // second submission would be refused anyway. A refusal the page could have prevented is a worse
  // experience than a button that is plainly unavailable.
  //
  // The two buttons are set separately from the options, and that is the point of them living outside the
  // fieldset: disabling the fieldset used to take "Stop it" with it, leaving a run that could be started
  // from the page and not stopped from it.
  dom.fields.disabled = running || !allowed;
  dom.start.disabled = running || !allowed;
  dom.cancel.disabled = false;
  dom.busy.textContent = running ? 'One benchmark at a time.' : '';

  if (state.state === 'idle') {
    dom.runCard.hidden = true;
    notice(dom.runError, null);
    notice(dom.runDone, null);
    notice(dom.runMessage, 'Nothing has been started from this page yet.');
    return;
  }

  notice(dom.runMessage, null);

  if (running) {
    dom.runCard.hidden = false;
    notice(dom.runError, null);
    notice(dom.runDone, null);
    const phase = state.phase;
    const total = phase?.total ?? phaseCount;
    // Before the first announcement the gauge is empty rather than guessing: a bar already a third of the
    // way along would be describing progress nobody reported.
    dom.phase.textContent = phase
      ? `[${phase.number}/${total}] ${phase.label}`
      : 'Starting…';
    dom.gaugeFill.style.width = phase ? `${(phase.number / total) * 100}%` : '0%';
    dom.elapsed.textContent = `running for ${duration(Date.now() - state.started_ms)}`;
    dom.runSummary.textContent = summarise(state.request);
    return;
  }

  // Finished, one way or another.
  dom.runCard.hidden = true;
  const took = duration(state.ended_ms - state.started_ms);
  if (state.ok) {
    notice(dom.runError, null);
    dom.runDone.hidden = false;
    dom.runDone.replaceChildren();
    const line = document.createElement('div');
    line.textContent = `The ${state.request.preset} benchmark finished in ${took}.`
      + ` Report: ${state.report_path}`;
    dom.runDone.append(line);
    if (state.markdown_path) {
      const summary = document.createElement('div');
      summary.className = 'field-hint';
      summary.textContent = `Summary: ${state.markdown_path}`;
      dom.runDone.append(summary);
    }
    const next = document.createElement('div');
    next.className = 'field-hint';
    next.textContent = 'Upload it on the compare page alongside an earlier report to see what changed.';
    dom.runDone.append(next);
    return;
  }

  notice(dom.runDone, null);
  dom.runError.hidden = false;
  dom.runError.replaceChildren();
  const reason = document.createElement('div');
  reason.textContent = state.cancelled
    ? `The ${state.request.preset} benchmark was stopped after ${took}, so it wrote no report.`
    : state.exit_code === null
      ? `The ${state.request.preset} benchmark was ended by the operating system after ${took}.`
      : `The ${state.request.preset} benchmark exited with status ${state.exit_code} after ${took}.`;
  dom.runError.append(reason);
  // The child's own last words, which is where the explanation actually is.
  if (state.stderr?.length) {
    const tail = document.createElement('pre');
    tail.textContent = state.stderr.join('\n');
    dom.runError.append(tail);
  }
}

/** Read the run's state and schedule the next read at a cadence that suits it. */
async function poll() {
  let state;
  try {
    state = await api('/api/bench/run');
  } catch (error) {
    notice(dom.runMessage, `Cannot reach the daemon: ${error.message}`);
    schedule(IDLE_POLL_MS);
    return;
  }
  renderRun(state);
  schedule(state.state === 'running' ? RUN_POLL_MS : IDLE_POLL_MS);
}

/** Replace the pending poll rather than adding to it. */
function schedule(delay) {
  if (pollTimer !== null) clearTimeout(pollTimer);
  pollTimer = setTimeout(() => void poll(), delay);
}

dom.preset.addEventListener('change', () => {
  dom.presetHint.textContent = describePreset(presets.get(dom.preset.value));
});
dom.liveLlm.addEventListener('change', syncLiveLlm);

// Both handlers disable their button for the round trip and then leave its state to {@link renderRun}, which
// is the only place that knows whether a run is now in flight. Re-enabling in a `finally` instead put Start
// back within reach the instant the request returned — while the benchmark it had just started was running.
dom.form.addEventListener('submit', async (event) => {
  event.preventDefault();
  notice(dom.runError, null);
  notice(dom.runDone, null);
  dom.start.disabled = true;
  try {
    await post('/api/bench', requestBody());
    // Poll at once rather than waiting out the idle cadence: the run is already under way, and a page that
    // sat still for ten seconds after the button was pressed would read as having ignored it.
    await poll();
  } catch (error) {
    notice(dom.runError, error.message);
    // Nothing started, so the button has to come back; a poll would also settle it, ten seconds later.
    dom.start.disabled = false;
  }
});

dom.cancel.addEventListener('click', async () => {
  dom.cancel.disabled = true;
  try {
    await post('/api/bench/cancel');
    await poll();
  } catch (error) {
    notice(dom.runError, error.message);
    dom.cancel.disabled = false;
  }
});

await loadOptions();
void poll();
