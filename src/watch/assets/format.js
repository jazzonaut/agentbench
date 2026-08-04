// Value formatting. Kept separate so both the tiles and the chart tooltip render a number
// identically — a tooltip disagreeing with a tile is a bug users notice immediately.

/** Read a CSS custom property from :root, so JS never hardcodes a colour. */
export function token(name) {
  return getComputedStyle(document.documentElement).getPropertyValue(name).trim();
}

export function percent(value) {
  return value === null || value === undefined ? '—' : `${value.toFixed(1)}%`;
}

export function gib(bytes) {
  if (bytes === null || bytes === undefined) return '—';
  return `${(bytes / 1073741824).toFixed(1)} GiB`;
}

export function mib(bytes) {
  if (bytes === null || bytes === undefined) return '—';
  return `${(bytes / 1048576).toFixed(0)} MiB`;
}

export function count(value) {
  return value === null || value === undefined ? '—' : value.toLocaleString();
}

/** A latency, kept in the unit a reader can compare: "5 µs", "11 ms", "1.20 s".
 *
 *  The microsecond branch is not decoration. A probe's SQLite lookup on a working machine is four or five
 *  microseconds, and rounding that to "0 ms" turns a metric the dashboard judges into a tile that reads
 *  zero for ever — which is how a chart says "nothing to see here" about a number that is doing its job.
 */
export function latency(ms) {
  if (ms === null || ms === undefined) return '—';
  if (Math.abs(ms) < 1) {
    const micros = ms * 1000;
    return `${micros.toFixed(Math.abs(micros) < 10 ? 1 : 0)} µs`;
  }
  if (ms < 1000) return `${Math.round(ms)} ms`;
  return `${(ms / 1000).toFixed(2)} s`;
}

/** A quantity of bytes, in the unit it is legible in: "103 GiB", "512 MiB", "8 KiB".
 *
 *  Adaptive rather than fixed to one scale, for the same reason {@link latency} has a microsecond branch:
 *  the agent's resident memory is a couple of hundred MiB and the machine's is a couple of GiB, and one
 *  divisor makes one of the two read as zero.
 */
export function bytes(value) {
  if (value === null || value === undefined) return '—';
  const magnitude = Math.abs(value);
  if (magnitude >= 1073741824) return `${(value / 1073741824).toFixed(1)} GiB`;
  if (magnitude >= 1048576) return `${(value / 1048576).toFixed(0)} MiB`;
  if (magnitude >= 1024) return `${(value / 1024).toFixed(0)} KiB`;
  return `${value.toFixed(0)} B`;
}

/** A throughput in bytes per second: "1,066 MiB/s", "17 KiB/s".
 *
 *  The quiet end is the one that matters. An idle desktop writes about 17 KiB/s, and rendering that as
 *  "0.0 MiB/s" says the disk was doing nothing on a chart whose whole purpose is to show that it was not.
 */
export function byteRate(value) {
  if (value === null || value === undefined) return '—';
  const magnitude = Math.abs(value);
  if (magnitude >= 1048576) {
    return `${(value / 1048576).toLocaleString(undefined, { maximumFractionDigits: 1 })} MiB/s`;
  }
  if (magnitude >= 1024) return `${(value / 1024).toFixed(0)} KiB/s`;
  return `${value.toFixed(0)} B/s`;
}

/** A formatter for a metric whose unit the server supplied rather than the page hardcoding it.
 *
 *  Every series reports its unit now, not only the probe ones, and every axis and tooltip on the page is
 *  derived from it. That is what makes the switchable frames safe: a frame that changes metric at runtime
 *  would otherwise keep the formatter it was built with, and render bytes as a percentage with nothing on
 *  screen looking wrong.
 *
 *  The vocabulary is deliberately small and closed — see `SampleSeries::unit` for the list and why the
 *  per-core distinction is a note rather than a unit. An unrecognised unit still renders, with the unit
 *  appended verbatim, because the probe catalogue owns its own units ("ops/s", "Mops/s", "GiB/s") and this
 *  page has no business being the second place they are enumerated.
 */
export function unitFormatter(unit) {
  if (unit === 'ms') return latency;
  if (unit === '%') return percent;
  if (unit === 'B') return bytes;
  if (unit === 'B/s') return byteRate;
  if (unit === 'ratio') return ratio;
  // A bare count: "401 processes" is the label's job, not the value's.
  if (unit === '') return count;
  return (value) => {
    if (value === null || value === undefined) return '—';
    // Large rates read better whole; small ones lose their meaning rounded. Probe metrics span
    // three hundred thousand rows a second and fractions of a GiB, so the precision follows the value
    // rather than the unit.
    const magnitude = Math.abs(value);
    const digits = magnitude >= 100 ? 0 : magnitude >= 1 ? 1 : 3;
    return `${value.toLocaleString(undefined, { maximumFractionDigits: digits })} ${unit}`;
  };
}

/** A share of a whole, as a percentage. Takes 0…1, not 0…100. */
export function ratio(value) {
  return value === null || value === undefined ? '—' : `${(value * 100).toFixed(0)}%`;
}

/** A short, human duration: "3s", "4m", "2h 15m", "3d 4h". */
export function duration(ms) {
  if (ms === null || ms === undefined) return '—';
  const seconds = Math.max(0, Math.round(ms / 1000));
  if (seconds < 60) return `${seconds}s`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes}m`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours}h ${minutes % 60}m`;
  return `${Math.floor(hours / 24)}d ${hours % 24}h`;
}

/** Local wall-clock time. Day boundaries and labels must be local, never UTC. */
export function clockTime(ms) {
  return new Date(ms).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
}

export function dateTime(ms) {
  return new Date(ms).toLocaleString([], {
    month: 'short',
    day: 'numeric',
    hour: '2-digit',
    minute: '2-digit',
    second: '2-digit',
  });
}
