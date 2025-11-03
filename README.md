# nvme-led-daemon

A lightweight Linux daemon that mirrors NVMe disk activity to a chassis LED (e.g., ThinkPad power button LED) with minimal CPU overhead.

## Features

- **Zero dependencies** (except `libc`)
- **Precise off-timer**: dedicated timerfd for crisp LED edges
- **Per-direction signaling**: differentiate reads vs writes with distinct blink durations
- **Config file support**: `/etc/nvme-led-daemon.conf` for easy tuning
- **Epoll + timerfd**: efficient event loop, negligible CPU usage even at 8ms poll intervals
- **Active-high/low support**: works with various LED controller polarities
- **Two NVMe modes**: `io` (I/O completions) or `sectors` (bytes transferred)

## Requirements

- Linux kernel with `epoll`, `timerfd`, and sysfs LED class support
- Rust toolchain (for building)
- Root or appropriate permissions to write to `/sys/class/leds/*/brightness`

## Installation

### 1. Clone and build

```bash
git clone https://github.com/scarlion1/nvme-led-daemon.git
cd nvme-led-daemon
cargo build --release
```

### 2. Install binary

```bash
sudo cp target/release/nvme-led-daemon /usr/local/bin/
```

### 3. Create config file

```bash
sudo tee /etc/nvme-led-daemon.conf >/dev/null <<'EOF'
# NVMe LED Daemon Configuration
led_path = /sys/class/leds/tpacpi::power/brightness
nvme_path = /sys/block/nvme0n1/stat
interval_ms = 8
blink_ms = 12
read_blink_ms = 10
write_blink_ms = 22
active_high = true
nvme_mode = io
on_fields = both
quiet = false
EOF
```

**Adjust paths and values for your system:**

- `led_path`: find your LED with `ls /sys/class/leds/`
- `nvme_path`: find your NVMe device with `ls /sys/block/nvme*`
- `active_high`: set to `true` if writing `1` turns LED on, `false` if `0` turns it on

### 4. Disable LED trigger (if needed)

Some LEDs have kernel triggers that must be disabled:

```bash
echo none | sudo tee /sys/class/leds/tpacpi::power/trigger
```

### 5. Test manually

```bash
sudo /usr/local/bin/nvme-led-daemon
```

Generate some disk activity (e.g., `dd if=/dev/nvme0n1 of=/dev/null bs=1M count=100`) and watch the LED blink.

### 6. Install systemd service

```bash
sudo tee /etc/systemd/system/nvme-led.service >/dev/null <<'EOF'
[Unit]
Description=NVMe Power LED Activity Monitor
After=multi-user.target

[Service]
Type=simple
ExecStart=/usr/local/bin/nvme-led-daemon
Restart=on-failure
Nice=-10
ProtectSystem=full
ProtectHome=true
PrivateTmp=true
NoNewPrivileges=true
ReadOnlyDirectories=/
ReadWriteDirectories=/sys/class/leds/tpacpi::power

[Install]
WantedBy=multi-user.target
EOF

sudo systemctl daemon-reload
sudo systemctl enable --now nvme-led.service
systemctl status nvme-led.service --no-pager
```

## Configuration

All settings can be specified in `/etc/nvme-led-daemon.conf` (INI-style format) or overridden via CLI flags.

### Config file options

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `led_path` | string | `/sys/class/leds/tpacpi::power/brightness` | Path to LED brightness file |
| `nvme_path` | string | `/sys/block/nvme0n1/stat` | Path to NVMe stat file |
| `interval_ms` | u64 | `8` | Poll interval in milliseconds |
| `blink_ms` | u64 | `12` | Default LED on-duration in milliseconds |
| `read_blink_ms` | u64 | (optional) | Override blink duration for reads |
| `write_blink_ms` | u64 | (optional) | Override blink duration for writes |
| `active_high` | bool | `false` | `true` if writing `1` turns LED on |
| `nvme_mode` | string | `sectors` | `io` or `sectors` |
| `on_fields` | string | `both` | `reads`, `writes`, or `both` |
| `quiet` | bool | `false` | Suppress startup message |

### CLI flags (override config file)

```
--config PATH            Load config from PATH (default: /etc/nvme-led-daemon.conf)
--led PATH               LED brightness sysfs path
--nvme PATH              NVMe stat file path
--interval-ms N          Poll interval (ms)
--blink-ms N             Default blink duration (ms)
--read-blink-ms N        Blink duration for reads (ms)
--write-blink-ms N       Blink duration for writes (ms)
--on-fields reads|writes|both
--nvme-mode io|sectors
--active-high            LED is active-high
--quiet                  Suppress output
--help                   Show help
```

## Usage Examples

### Preset profiles

**Balanced (default in config):**
```ini
interval_ms = 8
read_blink_ms = 10
write_blink_ms = 22
nvme_mode = io
on_fields = both
```

**Reads only, short blink:**
```ini
interval_ms = 8
read_blink_ms = 10
nvme_mode = io
on_fields = reads
```

**Writes only, longer blink:**
```ini
interval_ms = 8
write_blink_ms = 30
nvme_mode = io
on_fields = writes
```

**Very responsive (more wakeups):**
```ini
interval_ms = 6
read_blink_ms = 8
write_blink_ms = 16
nvme_mode = io
on_fields = both
```

After editing `/etc/nvme-led-daemon.conf`, restart the service:
```bash
sudo systemctl restart nvme-led.service
```

## Troubleshooting

### LED doesn't blink

1. **Check LED path and permissions:**
   ```bash
   ls -l /sys/class/leds/tpacpi::power/brightness
   echo 1 | sudo tee /sys/class/leds/tpacpi::power/brightness
   echo 0 | sudo tee /sys/class/leds/tpacpi::power/brightness
   ```

2. **Disable LED trigger:**
   ```bash
   cat /sys/class/leds/tpacpi::power/trigger
   echo none | sudo tee /sys/class/leds/tpacpi::power/trigger
   ```

3. **Check NVMe stat file:**
   ```bash
   cat /sys/block/nvme0n1/stat
   # Generate activity and check again
   dd if=/dev/nvme0n1 of=/dev/null bs=1M count=10
   cat /sys/block/nvme0n1/stat
   ```

4. **Try the other nvme-mode:**
   - If `io` doesn't work, try `sectors` (or vice versa)

5. **Toggle active-high:**
   - Some LEDs are active-low; try flipping `active_high` in the config

6. **Test with a known LED:**
   - Try capslock LED: `/sys/class/leds/input*::capslock/brightness`

### LED stays solid during heavy I/O

This is expected with NVMe—unlike old SATA disks, NVMe completes I/Os in large bursts with very low latency.  The LED stays on because activity is nearly continuous.  Lower `blink_ms` values (e.g., 6–10ms) can make individual pulses more visible during lighter workloads.

### High CPU usage

During testing on my 5-year-old T14 Gen 1 (Intel i5-10310U and 16GiB DDR4-3200) with the default settings (8ms interval), CPU usage was negligible (<0.1%).  If you see high usage:

- Increase `interval_ms` (e.g., 20ms)
- Check for other system issues

## How It Works

1. **Epoll loop**: waits on two timerfds (poll timer + off timer)
2. **Poll timer fires**: reads `/sys/block/nvme*/stat`, compares read/write counters
3. **Activity detected**: turns LED on immediately, arms off-timer for precise duration
4. **Off timer fires**: turns LED off
5. **Repeat**: minimal syscalls, low wakeups, efficient even at high poll rates

## License

GPL-3.0-or-later

## Credits

Developed with assistance from Claude Sonnet 4.5 and GPT-5 provided by Abacus.AI ChatLLM Teams, a super awesome and affordable service.  Check them out and sign up with my referral link https://chatllm.abacus.ai/YwBngMwYCw and I'll give you a cookie.