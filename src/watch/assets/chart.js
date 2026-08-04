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

/** Draw annotations behind the data.
 *
 *  Behind, and in one recessive colour, because an annotation is context: the moment a mark is as loud as
 *  the line it explains, the chart is about the marks. A run occupied an interval and is drawn as a band; a
 *  version change happened at an instant and is drawn as a dashed rule. The two are told apart by shape as
 *  well as by weight, and named in the list beneath the charts rather than labelled in the frame, which at
 *  a fortnight's range would be a wall of overlapping text.
 */
function annotationPlugin(read) {
  return {
    hooks: {
      drawClear: (plot) => {
        const marks = read();
        if (marks.length === 0) return;
        const { ctx } = plot;
        const top = plot.bbox.top;
        const height = plot.bbox.height;
        const stroke = token('--annotation');
        const band = token('--annotation-band');
        ctx.save();
        // Clip to the plotting area so a run that began before the range does not paint the axes.
        ctx.beginPath();
        ctx.rect(plot.bbox.left, top, plot.bbox.width, height);
        ctx.clip();
        for (const mark of marks) {
          const x = plot.valToPos(mark.ts / 1000, 'x', true);
          if (mark.ended !== null && mark.ended !== undefined) {
            const end = plot.valToPos(mark.ended / 1000, 'x', true);
            // At least a pixel wide: a two-second run must not vanish entirely.
            ctx.fillStyle = band;
            ctx.fillRect(x, top, Math.max(1, end - x), height);
            continue;
          }
          ctx.strokeStyle = stroke;
          ctx.lineWidth = 1;
          ctx.setLineDash(mark.kind === 'run' ? [] : [3, 3]);
          ctx.beginPath();
          ctx.moveTo(Math.round(x) + 0.5, top);
          ctx.lineTo(Math.round(x) + 0.5, top + height);
          ctx.stroke();
        }
        ctx.restore();
      },
    },
  };
}

/** Width the y-axis needs for the labels it is about to draw.
 *
 *  uPlot calls this twice per layout: once before it knows the labels, then again with them. The second
 *  call is the one that can measure, so the first returns the previous width to avoid a jump.
 */
function axisWidth(self, values, axisIdx, cycleNum) {
  if (cycleNum > 1) return self.axes[axisIdx]._size;
  const longest = (values ?? []).reduce((max, value) => Math.max(max, String(value).length), 0);
  // Six pixels per character at 11px plus a gutter, floored at the width a percentage needs.
  return Math.max(52, 14 + longest * 6.5);
}

/** Options shared by every chart, reading colours from CSS tokens. */
function options(width, format, label, plugins) {
  return {
    width,
    height: 220,
    plugins,
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
        // Measured, not assumed. A fixed 52px fits "30.0%" and clips "7,129 ops/s" to "000 ops/s",
        // which reads as a chart of tiny numbers rather than as a truncated label.
        size: axisWidth,
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
        // Markers left on uPlot's automatic rule rather than switched off. It shows them only once points
        // are far enough apart to be distinguishable, which is also the only case where they are load-
        // bearing: a lone observation between two gaps has no line segment to belong to, so with markers
        // off it is drawn as nothing at all.
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
  let marks = [];

  const width = () => Math.max(320, element.clientWidth || element.offsetWidth || 640);
  // Read through a closure so new annotations redraw with the next frame rather than rebuilding the plot.
  const plugins = [annotationPlugin(() => marks)];

  const render = () => {
    if (plot) {
      plot.setData(current);
      return;
    }
    plot = new uPlot(options(width(), format, label, plugins), current, element);
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
    /** Rename the measure the tooltip states.
     *
     *  uPlot writes a series label into the legend DOM at construction, so a panel whose metric can
     *  change has to rebuild rather than mutate — the same path a colour-scheme change already takes.
     *  Guarded on a real change, so the minute poll reselecting the same metric rebuilds nothing.
     */
    setLabel(next) {
      if (next === label) return;
      label = next;
      if (!plot) return;
      plot.destroy();
      plot = null;
      render();
    },
    /** Replace the marks drawn behind the data. Redraws only if the plot already exists. */
    setAnnotations(next) {
      marks = next ?? [];
      if (plot) plot.redraw();
    },
    isEmpty() {
      return current[0].length === 0;
    },
  };
}
