// uPlot wiring for a single-series time chart.
//
// Deliberately one series per chart and one y-axis. Two measures on two scales in one frame is the
// most common charting mistake there is. Charts stack instead, and share a cursor: reading straight
// down one vertical line across system CPU and agent tool latency is how a slow afternoon gets
// explained, and it is the reason these are separate frames rather than one crowded one.

import { token, clockTime, dateTime } from './format.js';

/** One cursor across every chart on the page. */
const CURSOR_GROUP = uPlot.sync('agentbench');

/** Break the line wherever sampling stopped, rather than drawing through the gap.
 *
 *  A suspended laptop leaves hours with no observations. Joining across them would render a
 *  confident straight line through time that was never measured. `gapMs` comes from the server,
 *  derived from the observed cadence.
 */
function toSeries(points, gapMs) {
  const xs = [];
  const ys = [];
  let previous = null;
  for (const point of points) {
    if (previous !== null && gapMs > 0 && point.ts - previous > gapMs) {
      // A null y at the midpoint of the gap makes uPlot lift the pen.
      xs.push((previous + (point.ts - previous) / 2) / 1000);
      ys.push(null);
    }
    xs.push(point.ts / 1000);
    ys.push(point.value);
    previous = point.ts;
  }
  return [xs, ys];
}

/** Options shared by every chart, reading colours from CSS tokens. */
function options(width, format, label) {
  return {
    width,
    height: 220,
    // Recessive frame: the data should be the most prominent thing in the panel.
    padding: [10, 8, 0, 0],
    cursor: {
      // Crosshair plus tooltip is the default interaction for a line chart, not an extra.
      x: true,
      y: true,
      points: { size: 6, width: 1.5 },
      drag: { x: true, y: false },
      // Only time is shared. Matching the y position across charts of different measures would
      // draw a line implying a relationship between percentages and milliseconds.
      sync: { key: CURSOR_GROUP.key, scales: ['x', null] },
    },
    legend: { live: true },
    scales: { x: { time: true } },
    axes: [
      {
        stroke: token('--ink-muted'),
        grid: { stroke: token('--grid'), width: 1 },
        ticks: { stroke: token('--axis'), width: 1 },
        font: '11px ui-sans-serif, system-ui, sans-serif',
        values: (_self, splits) => splits.map((seconds) => clockTime(seconds * 1000)),
      },
      {
        stroke: token('--ink-muted'),
        grid: { stroke: token('--grid'), width: 1 },
        ticks: { show: false },
        font: '11px ui-sans-serif, system-ui, sans-serif',
        size: 52,
        values: (_self, splits) => splits.map((value) => format(value)),
      },
    ],
    series: [
      { value: (_self, seconds) => (seconds === null ? '—' : dateTime(seconds * 1000)) },
      {
        label,
        // Thin mark. Width is about legibility, not emphasis.
        stroke: token('--series-1'),
        width: 1.5,
        points: { show: false },
        value: (_self, value) => format(value),
      },
    ],
  };
}

/**
 * A resizable single-series chart.
 *
 * `format` is shared with the tiles so the tooltip and the tile can never disagree, and `label` names
 * the measure in the tooltip, since one chart's line is never explained by a legend.
 */
export function createChart(element, format, label = 'value') {
  let plot = null;
  let current = [[], []];

  const width = () => Math.max(320, element.clientWidth || element.offsetWidth || 640);

  const render = () => {
    if (plot) {
      plot.setData(current);
      return;
    }
    plot = new uPlot(options(width(), format, label), current, element);
  };

  // Re-lay-out on container resize rather than on window resize: the panel can change width
  // without the window doing so.
  if (typeof ResizeObserver !== 'undefined') {
    new ResizeObserver(() => {
      if (plot) plot.setSize({ width: width(), height: 220 });
    }).observe(element);
  }

  // Colours are tokens, so a light/dark switch must rebuild rather than keep stale strokes.
  const scheme = window.matchMedia('(prefers-color-scheme: dark)');
  scheme.addEventListener('change', () => {
    if (!plot) return;
    plot.destroy();
    plot = null;
    render();
  });

  return {
    update(points, gapMs) {
      current = toSeries(points, gapMs);
      render();
    },
    isEmpty() {
      return current[0].length === 0;
    },
  };
}
