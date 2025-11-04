# tools/bench-perf.sh — Daemon Performance Benchmark

This script measures the runtime overhead of the daemon under idle and active workloads.

It dynamically reconfigures interval_ms, read_blink_ms, and write_blink_ms for a series of common profiles, then records CPU% and wakeup statistics.

## Requirements
- root privileges (the script self-elevates with sudo if needed)
- sysstat (`pidstat`) for CPU metrics
- perf for kernel wakeup counting (optional)

Debian/Ubuntu installation:
```bash
sudo apt install sysstat linux-tools-common linux-tools-$(uname -r)
```

## Usage
```bash
chmod +x tools/bench-perf.sh
sudo tools/bench-perf.sh
```

To export results as CSV:
```bash
CSV_OUT=bench.csv sudo tools/bench-perf.sh
```

## What it measures
- **cpu idle% / act%:** Average CPU utilization during idle and constant I/O phases.
- **ctxsw/s:** Total (voluntary + nonvoluntary) context switches/sec, effectively equal to timer wakeups/sec.
- **perf_wake/s:** Kernel wakeup events/sec using perf (depends on kernel support for events/wakeup/).

## I/O Activity Simulation
During each "active" phase, the script continuously performs direct I/O reads (dd iflag=direct) against the NVMe device to keep disk activity high.

## Profiles
| Label | Interval (ms) | Typical Use |
|--------|----------------|--------------|
| ultra | 6 | Max responsiveness |
| responsive | 8 | Balanced realtime feel |
| balanced | 10 | General default |
| 60fps | 16 | Aligns with ~60Hz refresh |
| battery | 20 | Power-efficient |
| ultra_saver | 50 | Minimum wake rate |

## Expected Results
- ctxsw/s closely approximates 1000 / interval_ms.
- CPU% decreases with longer intervals.
- perf wakeups may approach zero at idle.

## Troubleshooting
- If you see "daemon failed to start," ensure DAEMON_BIN and BASE_CONFIG point to valid paths.
- Use LC_ALL=C for consistent pidstat output parsing: `LC_ALL=C ./bench-perf.sh`
- Verify sysstat and perf are installed and accessible in PATH.

## Output Example
```
profile      interval   theory_wps   cpu idle%     ctxsw/s idle  perf_wake/s idle | cpu act%      ctxsw/s act   perf_wake/s act  notes
------------ --------   ----------   -----------   ------------  ---------------- | -----------   ------------  ---------------- -----
ultra        6ms        166.7        2.67          171.47        0.00             | 1.93          171.40        0.00
responsive   8ms        125.0        2.00          128.33        0.00             | 1.40          128.60        0.13
balanced     10ms       100.0        1.00          102.27        0.07             | 0.80          102.00        0.00
...
```

## License / Notes
This benchmarking script inherits the project's general license (GPL). Adjust intervals or NVMe device path as needed for your platform.
