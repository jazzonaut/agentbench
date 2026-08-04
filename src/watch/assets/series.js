// What each history frame can be pointed at, and what every one of those measurements actually is.
//
// A catalogue rather than behaviour, which is why it is not in app.js: four switchable frames offer
// twenty-seven measurements between them, and the prose that stops each of them being misread is most of
// the text. Kept together so it can be read end to end, for the same reason the tiles' `NOTES` block is
// one block — a set of caveats is far easier to keep honest when nothing about it is scattered.
//
// Every choice carries four things:
//
//   label    the button. Short, because nine of them share one row with a title.
//   metric   the series name, exactly as `/api/series` answers to. Two guard tests in
//            `serve/assets.rs` read these out of this file: one fails the build if a name here is not
//            a series the server accepts, and one fails it if a collected series has no button
//            anywhere in this file. Collection nothing can read is cost without benefit.
//   caption  the line under the frame's title. Whatever is part of the reading rather than of the
//            explanation: per core versus per machine, what a gap means, what the workload was. The
//            page appends the resolution caveat to it when a range crosses the retention boundary.
//   note     the paragraph behind the info mark, for the reader who wants to know why the number is
//            the way it is. Never restates the caption; the two are read together.
//
// No unit appears here. The server reports one per series and the axis, the tooltip and the verdict
// tiles all derive from that, so a label in this file can never disagree with the catalogue.

/** The machine itself, from the passive sampler.
 *
 *  Every series in `SampleSeries`, in the order the live tiles above the charts list them: the machine
 *  first, then the two process trees the tool is about. The per-core scales and the absent-is-not-zero
 *  rules are the whole reason each of these needs a note.
 */
export const SYSTEM_SERIES = [
  {
    label: 'CPU',
    metric: 'cpu_percent',
    caption: 'whole machine · every core averaged, so never above 100%',
    note: {
      title: 'System CPU',
      text: 'The whole machine, every core averaged together, so it never exceeds 100%. Sampled every'
        + ' 5 seconds while the machine is busy and every 30 seconds while it is idle, by default, and'
        + ' summarised to per-minute averages once samples pass the retention window.',
    },
  },
  {
    label: 'memory',
    metric: 'used_memory',
    caption: 'physical memory in use · the whole machine, not the agent alone',
    note: {
      title: 'Memory in use',
      text: 'Physical memory in use across the machine. Everything running counts, not only the agent.'
        + ' Summarised as a per-minute average rather than a peak, because the average is what the'
        + ' machine was living with.',
    },
  },
  {
    label: 'swap',
    metric: 'used_swap',
    caption: 'moved out to disk · per-minute peak once summarised',
    note: {
      title: 'Swap in use',
      text: 'Memory the machine has moved out to disk. Anything above zero while memory is nearly full'
        + ' explains more slow days than any other line on this page. Summarised to the per-minute peak'
        + ' rather than the average: half a minute of swapping is the event, and its mean over a minute'
        + ' hides it.',
    },
  },
  {
    label: 'processes',
    metric: 'process_count',
    caption: 'counted once a minute, however fine the range',
    note: {
      title: 'Processes',
      text: 'How many processes existed when the sampler last walked the whole table, which is once a'
        + ' minute by default rather than on every sample. This line is therefore coarser than the ones'
        + ' beside it however far the range is zoomed in.',
    },
  },
  {
    label: 'scanner CPU',
    metric: 'scanner_cpu',
    caption: 'per core, not per machine · a gap means no scanner was found',
    note: {
      title: 'Security scanner CPU',
      text: 'Summed over the processes matching the scanner names in the collect settings. Each process'
        + ' is measured against one core, so a scanner working hard on a sixteen-core machine reads well'
        + ' above 100% and this line shares no scale with system CPU. A gap is not a zero: nothing'
        + ' matched, rather than something measured at nothing.',
    },
  },
  {
    label: 'agent CPU',
    metric: 'agent_cpu',
    caption: 'per core · the agent process and everything it started',
    note: {
      title: 'Coding agent CPU',
      text: 'The agent process and every process it started, summed, each measured against one core — so'
        + ' the line can exceed 100% and cannot be compared with system CPU without dividing by the core'
        + ' count. The set of processes is rediscovered once a minute. A gap means no agent was running.',
    },
  },
  {
    label: 'agent memory',
    metric: 'agent_rss',
    caption: 'resident memory of the agent tree · per-minute peak once summarised',
    note: {
      title: 'Coding agent memory',
      text: 'Resident memory summed over the agent process and its children. A gap means no agent process'
        + ' was found at the last discovery pass, rather than an agent that used no memory.',
    },
  },
  {
    label: 'agent writes',
    metric: 'agent_write_bytes_s',
    caption: 'bytes per second · absent on a discovery tick, never zero',
    note: {
      title: 'Agent disk writes',
      text: 'Bytes per second the agent tree wrote, from the difference between two refreshes. The tick'
        + ' after every discovery pass is drawn as a gap because it has no rate to report: the first'
        + " reading for a newly seen process is its whole lifetime's traffic, so a rediscovered agent"
        + ' would otherwise plant a spike of gigabytes per second. Per-minute peak once summarised.',
    },
  },
  {
    label: 'scanner writes',
    metric: 'scanner_write_bytes_s',
    caption: 'bytes per second · a flat zero here is an unelevated reader, not a quiet scanner',
    note: {
      title: 'Security scanner disk writes',
      text: 'Bytes per second the matched scanners wrote. On Windows this reads exactly zero for'
        + ' Defender, and that is the daemon staying unelevated rather than a defect: an ordinary process'
        + " can see a SYSTEM-owned process's CPU and not its I/O. A user-owned scanner does register"
        + ' here. Whole-machine throughput, which does count the writers this cannot, is a probe'
        + ' covariate instead — "disk writes" in the conditions frame below.',
    },
  },
];

/** What the agent actually experienced, derived from its own transcripts.
 *
 *  Every series in `SessionSeries`. Four of them are latencies of one tool family each, and that
 *  separation is load-bearing: see the `Read` note. The token series describe the work asked of the
 *  agent rather than what the machine did with it, which is why no verdict is drawn from them.
 */
export const AGENT_SERIES = [
  {
    label: 'Read',
    metric: 'tool_read_ms',
    caption: 'median per bucket · Read only',
    note: {
      title: 'Agent file-read latency',
      text: 'The median successful Read in each time bucket. One tool rather than a family: measured over'
        + ' 15,035 real calls, Read runs at 11 ms where Edit is 35, Grep is 72 and Glob is 223, so a'
        + ' pooled line moved with whichever tool the model reached for rather than with the machine.'
        + ' This is the one session series a verdict is drawn from.',
    },
  },
  {
    label: 'Edit',
    metric: 'tool_edit_ms',
    caption: 'median per bucket · Edit and Write together',
    note: {
      title: 'Agent file-write latency',
      text: 'A filesystem cost too, and the one a security scanner inspects — but an Edit also pays for'
        + ' matching the text it is replacing, which is a property of the edit rather than of the disk.'
        + ' Charted for that reason and never judged.',
    },
  },
  {
    label: 'search',
    metric: 'tool_search_ms',
    caption: 'median per bucket · Grep and Glob together',
    note: {
      title: 'Agent search latency',
      text: 'The closest thing here to a directory-walk measurement, and the line that would show a'
        + ' filter driver or a cloud-sync placeholder provider making enumeration expensive. It scales'
        + ' with the size of the tree being searched, so it moves when the agent changes project, which'
        + ' is why it is charted rather than judged.',
    },
  },
  {
    label: 'Bash',
    metric: 'tool_bash_ms',
    caption: 'median per bucket · dominated by the command, not the machine',
    note: {
      title: 'Agent shell latency',
      text: 'How long Bash calls took, which is mostly how long the commands legitimately ran. Failed,'
        + ' refused and interrupted calls are excluded from every latency line here — each returned early'
        + ' or spent its time waiting for a person, so counting them would make the machine look faster'
        + ' the more went wrong.',
    },
  },
  {
    label: 'first response',
    metric: 'first_response_ms',
    caption: 'prompt to first assistant message · not a time to first token',
    note: {
      title: 'Time to first response',
      text: 'The interval from a prompt to the first assistant message. It contains the whole thinking'
        + ' block, and a prompt typed while the agent was still working waits in a queue before the'
        + ' request is even sent. Three different quantities in one number, so no verdict rests on it.',
    },
  },
  {
    label: 'output tokens',
    metric: 'output_tokens',
    caption: 'summed per bucket · a bare count, not a rate',
    note: {
      title: 'Output tokens',
      text: 'Tokens the model produced, summed over each bucket rather than averaged. It describes the'
        + ' work asked of the agent rather than what the machine did with it.',
    },
  },
  {
    label: 'tokens/s',
    metric: 'output_tokens_per_s',
    caption: 'multi-row responses only · tokens summed over seconds summed',
    note: {
      title: 'Output tokens per second',
      text: 'The span of a response is its first assistant row to its last, so a single-row request has'
        + ' none: 1,844 of 2,926 real requests emitted more than one row, leaving about 37% with nothing'
        + ' to measure. Those are excluded rather than divided by, which makes this a series about'
        + ' multi-row responses and not about all output. A response whose end nothing in the transcript'
        + ' proves is excluded too, rather than being measured to wherever the last poll landed. Each'
        + ' bucket is tokens summed over seconds summed, never a mean of rates, so one two-token reply'
        + ' cannot weigh the same as a thousand-token one. End of stream rather than end of generation.',
    },
  },
  {
    label: 'cache hits',
    metric: 'cache_hit_ratio',
    caption: 'share of prompt tokens served from cache · ratio of the totals',
    note: {
      title: 'Prompt cache hits',
      text: 'The share of prompt tokens served from the cache rather than sent fresh, as a ratio of the'
        + ' bucket totals rather than a mean of per-request ratios. A property of the conversations, not'
        + ' of the machine.',
    },
  },
];

/** The controlled measurements: the same small workload every time, which is what makes two days
 *  comparable.
 *
 *  Exactly the four the comparison judges, in the order it lists them — see
 *  `comparison::subjects::SUBJECTS`. Charting exactly the judged set is the point: a verdict tile a
 *  reader wants to check has a line to check it against, and a line that earns a verdict is never
 *  silently absent from the chart. The other thirty-odd catalogued metrics stay off this switch
 *  deliberately: eighteen metrics from two sources is a list, not a set of charts.
 *
 *  Every entry carries its own caption because the working set is part of the reading. 200 files is not
 *  8 MiB, and a caption left over from the previous selection would describe a different workload than
 *  the line on screen.
 */
export const PROBE_METRICS = [
  {
    label: 'small-file ops',
    metric: 'probe:filesystem.small_file_ops_s',
    caption: 'controlled workload · 200 files · not comparable to a bench report',
    note: {
      title: 'Small-file operations',
      text: 'The same 200 files created, stat-ed, renamed and deleted on every probe, in the daemon\'s'
        + ' own scratch directory. Fixed so that two days can be compared, which also means it is not'
        + " comparable with a bench report's figure for the same metric: that one is a different scale of"
        + ' the same workload. A contended run is charted here and excluded from the verdict above.',
    },
  },
  {
    label: 'sequential write',
    metric: 'probe:filesystem.sequential_write_mib_s',
    caption: 'controlled workload · 8 MiB write · write only, a read this size is page cache',
    note: {
      title: 'Sequential write throughput',
      text: 'An 8 MiB sequential write. The read half is deliberately not collected: at this size it'
        + ' would be served entirely from the OS page cache and would report memory bandwidth — thousands'
        + ' of MiB/s — under a name that means disk everywhere else in the tool. Sizing the file past the'
        + ' cache would have meant gigabytes of writes a day for one number.',
    },
  },
  {
    label: 'SQLite lookup',
    metric: 'probe:sqlite.lookup_ms',
    caption: 'controlled workload · 2,000 rows · lower is better',
    note: {
      title: 'SQLite lookup latency',
      text: 'Indexed lookups against a 2,000-row table the probe builds itself. On a working machine this'
        + ' is a few microseconds, which is why the axis changes unit rather than rounding the whole'
        + ' series to 0 ms.',
    },
  },
  {
    label: 'single-core CPU',
    metric: 'probe:cpu.single_mops_s',
    caption: 'controlled workload · one core for 200 ms · never all cores',
    note: {
      title: 'Single-core CPU throughput',
      text: 'One core for 200 ms, never all of them: a machine whose other fifteen cores are saturated'
        + ' can still score well here. What that leaves out is exactly what the conditions frame below'
        + ' records — the clock the core was running at, and what else was using the machine.',
    },
  },
];

/** What the machine was like when each probe ran.
 *
 *  Every series in `CondSeries`. These are covariates rather than measurements: they explain the frame
 *  above rather than being judged themselves, which is why none of them reports a direction. A clock at
 *  137% of nominal is not better than one at 128%.
 *
 *  This frame shares the "uncontended probes only" filter with the probe frame, so a cursor read down both
 *  reaches runs the two frames agree are comparable. That has a consequence each caption has to state:
 *  with the filter on, a covariate that defines contention cannot exceed its own threshold here, because
 *  every run that did was excluded by definition.
 *
 *  It is not quite the same set of runs, and "machine CPU" is where that shows. A probe series is one
 *  source at a time — the daemon's own probes or a benchmark's marker, never both, since they are
 *  different scales of one workload — while a covariate is the same kind of fact whoever recorded it, so
 *  this frame charts marker runs too. A marker records no covariate but the CPU it started under, so
 *  that is the only line the difference can appear on.
 */
export const CONDITION_SERIES = [
  {
    label: 'clock',
    metric: 'cond:clock_percent',
    caption: 'percent of nominal · above 100 is boost, below is throttle',
    note: {
      title: 'Clock at each probe',
      text: 'The clock as a percentage of its nominal rate. Not a figure in MHz: the absolute clock an'
        + ' ordinary process can read on Windows is a static value the registry holds from boot, measured'
        + ' flat at 3801 MHz across readings spanning 8% to 98% machine CPU. A MHz line would have sat'
        + ' permanently level under a judged CPU series, which reads as a machine behaving. A gap means'
        + ' the platform declined to answer, which is the documented outcome on macOS.',
    },
  },
  {
    label: 'disk writes',
    metric: 'cond:disk_write_bytes_s',
    caption: 'whole machine · unattributed · capped by the threshold when filtered',
    note: {
      title: 'Disk writes at each probe',
      text: 'Whole-machine write throughput over the ~200 ms before the workloads ran. Machine-wide'
        + ' because an unelevated reader cannot see SYSTEM-owned I/O at all — Defender, Windows Update'
        + ' and the search indexer report their CPU and exactly zero bytes — so this counter is the only'
        + ' thing that counts them, at the cost of attributing nothing. The window catches sustained'
        + ' background I/O and misses a burst that starts mid-probe: the chance of catching one is'
        + ' roughly its duration over the probe interval. With "uncontended probes only" on, this line'
        + ' cannot exceed the 20 MiB/s that defines disk contention, because every run that did was'
        + ' excluded by definition.',
    },
  },
  {
    label: 'free space',
    metric: 'cond:scratch_free_bytes',
    caption: 'on the volume the probe writes to',
    note: {
      title: 'Free space at each probe',
      text: 'Free space on the volume the probe writes its files to. This is the covariate for the slow'
        + ' monotonic drift the tool exists to detect: a filesystem series that has been falling for'
        + ' three weeks on a volume that has been filling for three weeks is one finding, not two.',
    },
  },
  {
    label: 'machine CPU',
    metric: 'cond:cpu_at',
    caption: 'whole machine, every core · immediately before the workloads',
    note: {
      title: 'Machine CPU at each probe',
      text: 'The whole machine across every core, read in the same ~200 ms window as the disk rate and'
        + ' before the workloads themselves ran. A run above 40% here is tagged contended and kept out of'
        + ' every verdict, so with "uncontended probes only" on this line is bounded by that threshold —'
        + ' which on a sixteen-core machine still leaves room for six cores of somebody else\'s work.',
    },
  },
  {
    label: 'scanner CPU',
    metric: 'cond:scanner_at',
    caption: 'per core, not per machine · a gap means no scanner was found',
    note: {
      title: 'Scanner CPU at each probe',
      text: 'Per core like the live tile: 10 means a tenth of one core, not a tenth of the machine.'
        + ' Charted but never named in a verdict\'s conditions line, because over uncontended runs it is'
        + ' bounded by its own threshold at a tenth of a core, and a move from a fiftieth to a twelfth is'
        + ' a large relative change that explains nothing about an 8% throughput drop. The judgement'
        + ' about whether it mattered is left to the reader here.',
    },
  },
  {
    label: 'agent CPU',
    metric: 'cond:agent_at',
    caption: 'per core · the raw figure behind "an agent was working"',
    note: {
      title: 'Agent CPU at each probe',
      text: 'The figure the contention flag was derived from, stored beside it so that a verdict stays'
        + ' recomputable when that threshold is revised — without it, every run recorded under the old'
        + ' constant would be impossible to reclassify. Per core, and bounded at a fifth of one core over'
        + ' uncontended runs, so like scanner CPU it is charted and never put in a conditions line.',
    },
  },
];
