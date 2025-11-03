// SPDX-License-Identifier: GPL-3.0-or-later
// nvme-led-daemon: mirror NVMe activity to a power LED with minimal syscalls.
// Features: epoll+timerfd, precise off-timer, per-direction signaling, config file.
// Assisted by GPT-5 (Abacus.AI ChatLLM Teams)

use std::collections::HashMap;
use std::env;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::io::RawFd;
use std::process;

const DEFAULT_LED_PATH: &str = "/sys/class/leds/tpacpi::power/brightness";
const DEFAULT_NVME_STAT_PATH: &str = "/sys/block/nvme0n1/stat";
const DEFAULT_POLL_INTERVAL_MS: u64 = 8;
const DEFAULT_BLINK_ON_MS: u64 = 12;
const DEFAULT_CONFIG_PATH: &str = "/etc/nvme-led-daemon.conf";

#[derive(Copy, Clone, Debug)]
enum NvmeMode { Sectors, Io }

#[derive(Copy, Clone, Debug, PartialEq)]
enum Dir { Read, Write }

#[derive(Copy, Clone, Debug)]
enum FieldsSel { Reads, Writes, Both }

#[inline(always)]
fn ns_from_ms(ms: u64) -> i64 { (ms as i64) * 1_000_000 }

struct Epoll { fd: RawFd }
impl Epoll {
    fn new() -> io::Result<Self> {
        let fd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
        if fd < 0 { return Err(io::Error::last_os_error()); }
        Ok(Self { fd })
    }
    fn add_fd(&self, fd: RawFd, data_u64: u64, events: u32) -> io::Result<()> {
        let mut ev = libc::epoll_event { events, u64: data_u64 };
        if unsafe { libc::epoll_ctl(self.fd, libc::EPOLL_CTL_ADD, fd, &mut ev) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    fn wait(&self, events: &mut [libc::epoll_event]) -> io::Result<usize> {
        let n = unsafe { libc::epoll_wait(self.fd, events.as_mut_ptr(), events.len() as i32, -1) };
        if n < 0 { return Err(io::Error::last_os_error()); }
        Ok(n as usize)
    }
}
impl Drop for Epoll { fn drop(&mut self) { unsafe { libc::close(self.fd) }; } }

struct Tfd(RawFd);
impl Tfd {
    fn periodic(interval_ms: u64) -> io::Result<Self> {
        let fd = unsafe { libc::timerfd_create(libc::CLOCK_MONOTONIC, libc::TFD_NONBLOCK | libc::TFD_CLOEXEC) };
        if fd < 0 { return Err(io::Error::last_os_error()); }
        let spec = libc::itimerspec {
            it_interval: libc::timespec { tv_sec: (interval_ms/1000) as i64, tv_nsec: ns_from_ms(interval_ms % 1000) },
            it_value: libc::timespec { tv_sec: 0, tv_nsec: 1 },
        };
        if unsafe { libc::timerfd_settime(fd, 0, &spec, std::ptr::null_mut()) } < 0 {
            let e = io::Error::last_os_error(); unsafe { libc::close(fd) }; return Err(e);
        }
        Ok(Self(fd))
    }
    fn oneshot() -> io::Result<Self> {
        let fd = unsafe { libc::timerfd_create(libc::CLOCK_MONOTONIC, libc::TFD_NONBLOCK | libc::TFD_CLOEXEC) };
        if fd < 0 { return Err(io::Error::last_os_error()); }
        let zero = libc::itimerspec {
            it_interval: libc::timespec { tv_sec: 0, tv_nsec: 0 },
            it_value: libc::timespec { tv_sec: 0, tv_nsec: 0 },
        };
        unsafe { libc::timerfd_settime(fd, 0, &zero, std::ptr::null_mut()) };
        Ok(Self(fd))
    }
    fn arm_after_ms(&self, delay_ms: u64) -> io::Result<()> {
        let spec = libc::itimerspec {
            it_interval: libc::timespec { tv_sec: 0, tv_nsec: 0 },
            it_value: libc::timespec { tv_sec: (delay_ms/1000) as i64, tv_nsec: ns_from_ms(delay_ms % 1000) },
        };
        if unsafe { libc::timerfd_settime(self.0, 0, &spec, std::ptr::null_mut()) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    fn ack(&self, buf8: &mut [u8; 8]) { unsafe { libc::read(self.0, buf8.as_mut_ptr() as *mut _, 8); }; }
}
impl Drop for Tfd { fn drop(&mut self) { unsafe { libc::close(self.0) }; } }

struct Led {
    f: File,
    current_logical: u8,
    active_high: bool,
}
impl Led {
    fn new(path: &str, active_high: bool) -> io::Result<Self> {
        let f = OpenOptions::new().write(true).open(path)?;
        Ok(Self { f, current_logical: 255, active_high })
    }
    #[inline(always)]
    fn set(&mut self, on: bool) -> io::Result<()> {
        let want = if on { 1 } else { 0 };
        if self.current_logical == want { return Ok(()); }
        let phys = if self.active_high { if on { b'1' } else { b'0' } }
                   else { if on { b'0' } else { b'1' } };
        let buf = [phys, b'\n'];
        self.f.write_all(&buf)?;
        self.current_logical = want;
        Ok(())
    }
    #[inline(always)] fn on(&mut self) -> io::Result<()> { self.set(true) }
    #[inline(always)] fn off(&mut self) -> io::Result<()> { self.set(false) }
}

struct Nvme {
    path: String,
    last_reads: u128,
    last_writes: u128,
    mode: NvmeMode,
}
impl Nvme {
    fn new(path: &str, mode: NvmeMode) -> Self {
        Self { path: path.to_string(), last_reads: 0, last_writes: 0, mode }
    }
    fn activity_dir(&mut self, scratch: &mut [u8; 256]) -> io::Result<Option<Dir>> {
        let mut f = File::open(&self.path)?;
        let n = f.read(scratch)?;
        let s = std::str::from_utf8(&scratch[..n]).unwrap_or("");
        let mut idx = 0usize;
        let mut r = None;
        let mut w = None;
        for token in s.split_whitespace() {
            if let Ok(v) = token.parse::<u64>() {
                match self.mode {
                    NvmeMode::Sectors => {
                        if idx == 2 { r = Some(v as u128); }
                        if idx == 6 { w = Some(v as u128); if r.is_some() { break; } }
                    }
                    NvmeMode::Io => {
                        if idx == 0 { r = Some(v as u128); }
                        if idx == 4 { w = Some(v as u128); if r.is_some() { break; } }
                    }
                }
                idx += 1;
            } else {
                idx += 1;
            }
        }
        let (Some(rn), Some(wn)) = (r, w) else { return Ok(None) };
        let rchg = rn != self.last_reads;
        let wchg = wn != self.last_writes;
        self.last_reads = rn;
        self.last_writes = wn;
        if rchg && !wchg { Ok(Some(Dir::Read)) }
        else if wchg && !rchg { Ok(Some(Dir::Write)) }
        else if rchg && wchg { Ok(Some(Dir::Write)) }
        else { Ok(None) }
    }
}

#[derive(Clone)]
struct Config {
    led_path: String,
    nvme_path: String,
    poll_ms: u64,
    blink_ms: u64,
    read_blink_ms: Option<u64>,
    write_blink_ms: Option<u64>,
    active_high: bool,
    quiet: bool,
    nvme_mode: NvmeMode,
    on_fields: FieldsSel,
}

fn load_config(path: &str) -> io::Result<HashMap<String, String>> {
    let contents = std::fs::read_to_string(path)?;
    let mut map = HashMap::new();
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') { continue; }
        if let Some((k, v)) = line.split_once('=') {
            map.insert(k.trim().to_string(), v.trim().to_string());
        }
    }
    Ok(map)
}

fn get_bool(map: &HashMap<String, String>, key: &str, default: bool) -> bool {
    map.get(key).and_then(|v| match v.as_str() {
        "true" | "yes" | "1" => Some(true),
        "false" | "no" | "0" => Some(false),
        _ => None
    }).unwrap_or(default)
}

fn get_u64(map: &HashMap<String, String>, key: &str, default: u64) -> u64 {
    map.get(key).and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn get_str<'a>(map: &'a HashMap<String, String>, key: &str, default: &'a str) -> &'a str {
    map.get(key).map(|s| s.as_str()).unwrap_or(default)
}

fn help() -> ! {
    eprintln!(
"nvme-led-daemon (GPL-3.0-or-later)
Usage:
  nvme-led-daemon [--config PATH] [OPTIONS]

Config file (optional): {default_cfg}
CLI options override config file settings.

Options:
  --config PATH        Load config from PATH
  --led PATH
  --nvme PATH
  --interval-ms N
  --blink-ms N
  --read-blink-ms N
  --write-blink-ms N
  --on-fields reads|writes|both
  --nvme-mode io|sectors
  --active-high
  --quiet
  --help

Defaults:
  led_path       {lp}
  nvme_path      {np}
  interval_ms    {pi}
  blink_ms       {bm}
  nvme_mode      sectors
  on_fields      both
",
        default_cfg = DEFAULT_CONFIG_PATH,
        lp = DEFAULT_LED_PATH, np = DEFAULT_NVME_STAT_PATH,
        pi = DEFAULT_POLL_INTERVAL_MS, bm = DEFAULT_BLINK_ON_MS
    );
    process::exit(0)
}

fn parse_args() -> Config {
    // Try loading default config file first
    let config_map = load_config(DEFAULT_CONFIG_PATH).unwrap_or_else(|_| HashMap::new());

    let mut cfg = Config {
        led_path: get_str(&config_map, "led_path", DEFAULT_LED_PATH).to_string(),
        nvme_path: get_str(&config_map, "nvme_path", DEFAULT_NVME_STAT_PATH).to_string(),
        poll_ms: get_u64(&config_map, "interval_ms", DEFAULT_POLL_INTERVAL_MS),
        blink_ms: get_u64(&config_map, "blink_ms", DEFAULT_BLINK_ON_MS),
        read_blink_ms: config_map.get("read_blink_ms").and_then(|v| v.parse().ok()),
        write_blink_ms: config_map.get("write_blink_ms").and_then(|v| v.parse().ok()),
        active_high: get_bool(&config_map, "active_high", false),
        quiet: get_bool(&config_map, "quiet", false),
        nvme_mode: match get_str(&config_map, "nvme_mode", "sectors") {
            "io" => NvmeMode::Io,
            _ => NvmeMode::Sectors,
        },
        on_fields: match get_str(&config_map, "on_fields", "both") {
            "reads" => FieldsSel::Reads,
            "writes" => FieldsSel::Writes,
            _ => FieldsSel::Both,
        },
    };

    // CLI args override config file
    let mut it = env::args().skip(1).peekable();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--help" | "-h" => help(),
            "--quiet" => cfg.quiet = true,
            "--active-high" => cfg.active_high = true,
            "--led" => { cfg.led_path = it.next().unwrap_or_else(|| { eprintln!("--led requires PATH"); process::exit(2) }); }
            "--nvme" => { cfg.nvme_path = it.next().unwrap_or_else(|| { eprintln!("--nvme requires PATH"); process::exit(2) }); }
            "--interval-ms" => {
                cfg.poll_ms = it.next().and_then(|v| v.parse().ok()).unwrap_or_else(|| { eprintln!("invalid --interval-ms"); process::exit(2) });
                if cfg.poll_ms == 0 { cfg.poll_ms = 1; }
            }
            "--blink-ms" => {
                cfg.blink_ms = it.next().and_then(|v| v.parse().ok()).unwrap_or_else(|| { eprintln!("invalid --blink-ms"); process::exit(2) });
                if cfg.blink_ms == 0 { cfg.blink_ms = 1; }
            }
            "--read-blink-ms" => {
                let v: u64 = it.next().and_then(|v| v.parse().ok()).unwrap_or_else(|| { eprintln!("invalid --read-blink-ms"); process::exit(2) });
                cfg.read_blink_ms = Some(v.max(1));
            }
            "--write-blink-ms" => {
                let v: u64 = it.next().and_then(|v| v.parse().ok()).unwrap_or_else(|| { eprintln!("invalid --write-blink-ms"); process::exit(2) });
                cfg.write_blink_ms = Some(v.max(1));
            }
            "--nvme-mode" => {
                let v = it.next().unwrap_or_else(|| { eprintln!("--nvme-mode requires io|sectors"); process::exit(2) });
                cfg.nvme_mode = match v.as_str() {
                    "io" => NvmeMode::Io,
                    "sectors" => NvmeMode::Sectors,
                    _ => { eprintln!("--nvme-mode must be io or sectors"); process::exit(2) }
                }
            }
            "--on-fields" => {
                let v = it.next().unwrap_or_else(|| { eprintln!("--on-fields requires reads|writes|both"); process::exit(2) });
                cfg.on_fields = match v.as_str() {
                    "reads" => FieldsSel::Reads,
                    "writes" => FieldsSel::Writes,
                    "both" => FieldsSel::Both,
                    _ => { eprintln!("--on-fields must be reads|writes|both"); process::exit(2) }
                }
            }
            "--config" => {
                let path = it.next().unwrap_or_else(|| { eprintln!("--config requires PATH"); process::exit(2) });
                let new_map = load_config(&path).unwrap_or_else(|e| {
                    eprintln!("Failed to load config {}: {}", path, e);
                    process::exit(2)
                });
                // Re-apply config from custom path
                cfg.led_path = get_str(&new_map, "led_path", &cfg.led_path).to_string();
                cfg.nvme_path = get_str(&new_map, "nvme_path", &cfg.nvme_path).to_string();
                cfg.poll_ms = get_u64(&new_map, "interval_ms", cfg.poll_ms);
                cfg.blink_ms = get_u64(&new_map, "blink_ms", cfg.blink_ms);
                if let Some(v) = new_map.get("read_blink_ms").and_then(|v| v.parse().ok()) { cfg.read_blink_ms = Some(v); }
                if let Some(v) = new_map.get("write_blink_ms").and_then(|v| v.parse().ok()) { cfg.write_blink_ms = Some(v); }
                cfg.active_high = get_bool(&new_map, "active_high", cfg.active_high);
                cfg.quiet = get_bool(&new_map, "quiet", cfg.quiet);
                cfg.nvme_mode = match get_str(&new_map, "nvme_mode", match cfg.nvme_mode { NvmeMode::Io => "io", NvmeMode::Sectors => "sectors" }) {
                    "io" => NvmeMode::Io,
                    _ => NvmeMode::Sectors,
                };
                cfg.on_fields = match get_str(&new_map, "on_fields", match cfg.on_fields { FieldsSel::Reads => "reads", FieldsSel::Writes => "writes", FieldsSel::Both => "both" }) {
                    "reads" => FieldsSel::Reads,
                    "writes" => FieldsSel::Writes,
                    _ => FieldsSel::Both,
                };
            }
            other => { eprintln!("Unknown arg: {}", other); help(); }
        }
    }
    cfg
}

fn main() -> io::Result<()> {
    let cfg = parse_args();

    let ep = Epoll::new()?;
    let poll_tfd = Tfd::periodic(cfg.poll_ms)?;
    let off_tfd = Tfd::oneshot()?;

    const POLL_TAG: u64 = 1;
    const OFF_TAG: u64 = 2;

    ep.add_fd(poll_tfd.0, POLL_TAG, libc::EPOLLIN as u32)?;
    ep.add_fd(off_tfd.0, OFF_TAG, libc::EPOLLIN as u32)?;

    let mut led = Led::new(&cfg.led_path, cfg.active_high)?;
    let mut nvme = Nvme::new(&cfg.nvme_path, cfg.nvme_mode);

    let mut events = [libc::epoll_event { events: 0, u64: 0 }; 2];
    let mut tbuf = [0u8; 8];
    let mut sbuf = [0u8; 256];

    let mut led_on = false;

    if !cfg.quiet {
        println!(
            "nvme-led-daemon: led={} nvme={} interval={}ms blink={}ms read_blink={:?} write_blink={:?} active_high={} mode={:?} on_fields={:?} (pid={})",
            cfg.led_path, cfg.nvme_path, cfg.poll_ms, cfg.blink_ms, cfg.read_blink_ms, cfg.write_blink_ms,
            cfg.active_high,
            match cfg.nvme_mode { NvmeMode::Sectors => "sectors", NvmeMode::Io => "io" },
            match cfg.on_fields { FieldsSel::Reads => "reads", FieldsSel::Writes => "writes", FieldsSel::Both => "both" },
            std::process::id()
        );
    }

    let _ = led.off();

    loop {
        let n = ep.wait(&mut events)?;
        for i in 0..n {
            let tag = events[i].u64;
            match tag {
                POLL_TAG => {
                    poll_tfd.ack(&mut tbuf);
                    if let Some(dir) = nvme.activity_dir(&mut sbuf)? {
                        let relevant = match (cfg.on_fields, dir) {
                            (FieldsSel::Both, _) => true,
                            (FieldsSel::Reads, Dir::Read) => true,
                            (FieldsSel::Writes, Dir::Write) => true,
                            _ => false,
                        };
                        if relevant {
                            let mut dur = cfg.blink_ms;
                            if dir == Dir::Read { if let Some(r) = cfg.read_blink_ms { dur = r; } }
                            if dir == Dir::Write { if let Some(w) = cfg.write_blink_ms { dur = w; } }

                            if !led_on { led.on()?; led_on = true; }
                            off_tfd.arm_after_ms(dur)?;
                        }
                    }
                }
                OFF_TAG => {
                    off_tfd.ack(&mut tbuf);
                    if led_on {
                        led.off()?;
                        led_on = false;
                    }
                }
                _ => {}
            }
        }
    }
}
