# shep versus pm2

A head-to-head measurement of the two process managers on one machine, run by
hand at a release and never in CI: it installs a third-party npm package and
times a wall clock, and a shared runner can hold neither still.

```sh
./benches/versus-pm2/versus-pm2.sh
```

`VERSUS_SCRATCH` picks where builds, the pm2 install and the raw samples go
(default `/tmp/shep-versus-pm2`); `SHEP_BIN` points at an already-built
release binary if you have one.

## What it measures, and why each is fair

Both tools run the *same two shell scripts* the harness writes, under the same
`/bin/sh`, with logs going to each tool's own default file capture.

| Metric | Workload |
| --- | --- |
| Idle daemon CPU and RSS | ten `while true; do sleep 5; done` apps |
| Log-plane CPU per line | one unthrottled `echo` loop, 62-byte lines |
| Start latency | ten apps, cold daemon and warm |
| Footprint | shep's binary against the pm2 install tree |

The shepherd is what is sampled, never the children. On the pm2 side that
means its God Daemon.

## The order is A/B/A on purpose

shep, then pm2, then shep again. A laptop that changes power state mid-run
shows up as disagreement between the two shep rounds instead of hiding inside
a ratio. The first run caught exactly that: the battery flipped from
discharging to charging, and the two shep rounds still agreed within 2.6%, so
the numbers stood.

Read the ratios, not the absolute figures. The box is not required to be idle,
and the run that produced the committed results had a load average near five
from unrelated work.

## Clean room

This benchmark drives the **published npm package** as a black box:
`npm install pm2`, then its own binary and its own `pm2 ecosystem simple`
generator for the config format. Observing what a shipped program does is not
porting it. Nothing here reads pm2's source, and neither should you.

## Results, 2026-08-29

shep 0.1.12 (`d113586`) against pm2 7.0.4 on node v26.5.0, macOS.

| Metric | shep | pm2 | Ratio |
| --- | --- | --- | --- |
| Idle daemon RSS, ten apps | 13.90 MiB | 71.16 MiB | 5.1x |
| Log-plane CPU per line | 2.10 us | 4.03 us | 1.9x |
| Start ten apps, cold | 0.158 s | 0.374 s | 2.4x |
| Start ten apps, warm | 0.056 s | 0.197 s | 3.5x |
| Install footprint | 14.23 MiB | 23.10 MiB | 1.6x |
| Idle daemon CPU | 0.045% | 0.020% | a tie at the noise floor |

Idle CPU is reported as the instrument read it. Both figures are hundredths
of one percent of one core; the difference is not a result.

The log-plane figure is worth its own sentence. shep cost 32.8 us per line
before the 2026-08-28 audit, so pm2 was ahead by 8x on this measure until the
day before these numbers were taken.

Verified while measuring, rather than assumed: neither tool prefixed or
timestamped a line, and the two out logs are byte-identical to the script's
own echo; each daemon grew exactly one file during the window; pm2 ran at its
defaults with only name, script and interpreter set.
