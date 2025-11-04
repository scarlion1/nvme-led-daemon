#!/usr/bin/env bash
set -euo pipefail
export LC_ALL=C

# Self-elevate
if [[ "${EUID:-$(id -u)}" -ne 0 ]]; then
  exec sudo --preserve-env=DAEMON_BIN,BASE_CONFIG,CONF_PATH,NVME_DEVICE,SAMPLE_SECONDS_IDLE,SAMPLE_SECONDS_ACTIVE,WARMUP,CSV_OUT "$0" "$@"
fi

DAEMON_BIN="${DAEMON_BIN:-/usr/local/bin/nvme-led-daemon}"
BASE_CONFIG="${BASE_CONFIG:-/etc/nvme-led-daemon.conf}"
CONF_PATH="${CONF_PATH:-/run/nvme-led-bench.toml}"
NVME_DEVICE="${NVME_DEVICE:-/dev/nvme0n1}"

SAMPLE_SECONDS_IDLE="${SAMPLE_SECONDS_IDLE:-15}"
SAMPLE_SECONDS_ACTIVE="${SAMPLE_SECONDS_ACTIVE:-15}"
WARMUP="${WARMUP:-0.6}"
CSV_OUT="${CSV_OUT:-}"

INTERVALS=(
  "6:6:12:ultra"
  "8:8:16:responsive"
  "10:10:20:balanced"
  "16:16:48:60fps"
  "20:20:40:battery"
  "50:50:100:ultra_saver"
)

command -v pidstat >/dev/null 2>&1 && HAVE_PIDSTAT=1 || HAVE_PIDSTAT=0
command -v perf >/dev/null 2>&1 && HAVE_PERF=1 || HAVE_PERF=0
[[ "$HAVE_PERF" -eq 0 ]] && echo "Note: perf not found; perf wakeups/sec will be N/A"

if [[ ! -x "$DAEMON_BIN" ]]; then echo "Error: $DAEMON_BIN missing"; exit 1; fi
if [[ ! -f "$BASE_CONFIG" ]]; then echo "Error: $BASE_CONFIG missing"; exit 1; fi

systemctl stop nvme-led.service >/dev/null 2>&1 || true
pkill -x nvme-led-daemon >/dev/null 2>&1 || true

printf "%-12s %-10s %-12s %-13s %-13s %-16s | %-13s %-13s %-16s %s\n" \
  "profile" "interval" "theory_wps" "cpu idle%" "ctxsw/s idle" "perf_wake/s idle" \
  "cpu act%" "ctxsw/s act" "perf_wake/s act" "notes"
printf "%-12s %-10s %-12s %-13s %-13s %-16s | %-13s %-13s %-16s %s\n" \
  "------------" "--------" "----------" "-----------" "------------" "----------------" \
  "-----------" "------------" "----------------" "-----"

[[ -n "$CSV_OUT" ]] && echo "profile,interval_ms,read_blink_ms,write_blink_ms,theory_wps,cpu_idle,ctxsw_idle,perf_idle,cpu_active,ctxsw_active,perf_active" > "$CSV_OUT"

# Run bounded I/O activity
run_activity() {
  local seconds="$1" end_ts start_ts
  start_ts=$(date +%s); end_ts=$((start_ts + seconds))
  if [[ ! -r "$NVME_DEVICE" ]]; then return 0; fi
  while : ; do
    local now=$(date +%s)
    (( now >= end_ts )) && break
    dd if="$NVME_DEVICE" of=/dev/null bs=1M count=128 iflag=direct status=none 2>/dev/null || true
  done
}

# Start daemon with precise PID capture
start_daemon() {
  pkill -x nvme-led-daemon >/dev/null 2>&1 || true
  "$DAEMON_BIN" --config "$CONF_PATH" >/dev/null 2>&1 &
  local pid=$!
  # Ensure it’s the right binary
  if ! ps -o comm= -p "$pid" 2>/dev/null | grep -qx "nvme-led-daemon"; then
    # If it execs, wait a moment and read its children
    sleep "$WARMUP"
    local child
    child=$(ps --ppid "$pid" -o pid= | awk 'NR==1{print $1}')
    [[ -n "$child" ]] && pid="$child"
  else
    sleep "$WARMUP"
  fi
  echo "$pid"
}

# Measure one phase (idle or active) for a given PID and duration
measure_phase() {
  local pid="$1" seconds="$2"
  local avg_cpu="N/A" ctx_rate="N/A" perf_rate="N/A"

  # CPU via pidstat over the same window
  if [[ "$HAVE_PIDSTAT" -eq 1 ]]; then
    local out
    out=$(LC_ALL=C pidstat -p "$pid" 1 "$seconds" 2>/dev/null || true)
    avg_cpu=$(
      awk '
        /UID[[:space:]]+PID/ { for (i=1;i<=NF;i++) if ($i=="%CPU") c=i }
        /^Average:[[:space:]]/ && c>0 { print $(c); exit }
      ' <<< "$out"
    )
    if [[ -z "$avg_cpu" ]]; then
      avg_cpu=$(
        awk '
          /UID[[:space:]]+PID/ { for (i=1;i<=NF;i++) if ($i=="%CPU") c=i; next }
          /^[0-9]/ && c>0 { sum+=$(c); n++ }
          END { if (n>0) printf("%.2f", sum/n) }
        ' <<< "$out"
      )
      [[ -z "$avg_cpu" ]] && avg_cpu="N/A"
    fi
  fi

  # Snapshot ctx switches before
  local vb vn_b
  vb=$(awk '/^voluntary_ctxt_switches/ {print $2}' /proc/"$pid"/status 2>/dev/null || echo "")
  vn_b=$(awk '/^nonvoluntary_ctxt_switches/ {print $2}' /proc/"$pid"/status 2>/dev/null || echo "")

  # Run perf concurrently (optional)
  local perf_tmp perf_pid=
  if [[ "$HAVE_PERF" -eq 1 ]]; then
    perf_tmp=$(mktemp)
    if perf list 2>/dev/null | grep -qE 'events/wakeup/'; then
      ( perf stat -e events/wakeup/ -p "$pid" -- sleep "$seconds" 2> "$perf_tmp" ) &
    else
      ( perf stat -e sched:sched_wakeup,sched:sched_wakeup_new -p "$pid" -- sleep "$seconds" 2> "$perf_tmp" ) &
    fi
    perf_pid=$!
  else
    # If no perf, still wait the same interval to match the CPU window
    sleep "$seconds" &
    perf_pid=$!
  fi

  # Wait for the window to elapse
  wait "$perf_pid" 2>/dev/null || true

  # Snapshot ctx switches after
  local va vn_a
  va=$(awk '/^voluntary_ctxt_switches/ {print $2}' /proc/"$pid"/status 2>/dev/null || echo "")
  vn_a=$(awk '/^nonvoluntary_ctxt_switches/ {print $2}' /proc/"$pid"/status 2>/dev/null || echo "")

  # Compute ctx/s if all four numbers are present and numeric
  if [[ "$vb" =~ ^[0-9]+$ && "$va" =~ ^[0-9]+$ && "$vn_b" =~ ^[0-9]+$ && "$vn_a" =~ ^[0-9]+$ ]]; then
    local total_before=$(( vb + vn_b ))
    local total_after=$(( va + vn_a ))
    if (( total_after >= total_before )); then
      ctx_rate=$(awk -v a="$total_after" -v b="$total_before" -v s="$seconds" 'BEGIN{ printf("%.2f", (a-b)/s) }')
    fi
  fi

  # Parse perf results if available
  if [[ -n "${perf_tmp:-}" && -s "$perf_tmp" ]]; then
    if grep -q wakeup "$perf_tmp"; then
      local wakes
      wakes=$(awk '/wakeup/ {gsub(",",""); sum+=$1} END{print sum+0}' "$perf_tmp")
      if [[ "$wakes" =~ ^[0-9]+$ ]]; then
        perf_rate=$(awk -v w="$wakes" -v s="$seconds" 'BEGIN{ printf("%.2f", w/s) }')
      else
        perf_rate="0.0"
      fi
    fi
    rm -f "$perf_tmp"
  fi

  echo "$avg_cpu|$ctx_rate|$perf_rate"
}

for cfg in "${INTERVALS[@]}"; do
  IFS=':' read -r ms rblink wblink label <<< "$cfg"
  theory_wps=$(awk -v ms="$ms" 'BEGIN { printf("%.1f", 1000.0/ms) }')

  # Build temp config from your known-good config
  awk -v ms="$ms" -v rb="$rblink" -v wb="$wblink" '
    BEGIN { si=sr=sw=0 }
    /^[[:space:]]*interval_ms[[:space:]]*=/     { print "interval_ms = " ms; si=1; next }
    /^[[:space:]]*read_blink_ms[[:space:]]*=/   { print "read_blink_ms = " rb; sr=1; next }
    /^[[:space:]]*write_blink_ms[[:space:]]*=/  { print "write_blink_ms = " wb; sw=1; next }
    { print }
    END {
      if (!si) print "interval_ms = " ms
      if (!sr) print "read_blink_ms = " rb
      if (!sw) print "write_blink_ms = " wb
    }
  ' "$BASE_CONFIG" > "$CONF_PATH"

  pid=$(start_daemon)
  if [[ -z "$pid" ]]; then
    printf "%-12s %-10s %-12s %-13s %-13s %-16s | %-13s %-13s %-16s %s\n" \
      "$label" "${ms}ms" "$theory_wps" "N/A" "N/A" "N/A" "N/A" "N/A" "N/A" "daemon failed to start"
    continue
  fi

  IFS='|' read -r cpu_idle ctx_idle perf_idle <<< "$(measure_phase "$pid" "$SAMPLE_SECONDS_IDLE")"

  run_activity "$SAMPLE_SECONDS_ACTIVE" &
  act_pid=$!
  IFS='|' read -r cpu_act ctx_act perf_act <<< "$(measure_phase "$pid" "$SAMPLE_SECONDS_ACTIVE")"
  kill "$act_pid" >/dev/null 2>&1 || true
  wait "$act_pid" >/dev/null 2>&1 || true

  printf "%-12s %-10s %-12s %-13s %-13s %-16s | %-13s %-13s %-16s %s\n" \
    "$label" "${ms}ms" "$theory_wps" "${cpu_idle:-N/A}" "${ctx_idle:-N/A}" "${perf_idle:-N/A}" \
    "${cpu_act:-N/A}" "${ctx_act:-N/A}" "${perf_act:-N/A}" ""

  [[ -n "$CSV_OUT" ]] && echo "$label,$ms,$rblink,$wblink,$theory_wps,$cpu_idle,$ctx_idle,$perf_idle,$cpu_act,$ctx_act,$perf_act" >> "$CSV_OUT"

  kill "$pid" >/dev/null 2>&1 || true
  sleep 1
done

echo
echo "Notes:"
echo "- If cpu columns remain '-', ensure sysstat (pidstat) is installed and LC_ALL=C helps parsing: LC_ALL=C pidstat ..."
echo "- perf wakeups may be near zero at idle; increase interval to see effect on ctxsw/s."
echo "- You can set CSV_OUT=bench.csv to save results."
