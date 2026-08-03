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
