// SPDX-License-Identifier: GPL-3.0-or-later
// nvme-led-daemon: mirror NVMe activity to a power LED with minimal syscalls.
// Features: epoll+timerfd, precise off-timer, per-direction signaling, config file.
// Assisted by GPT-5 (Abacus.AI ChatLLM Teams)

use std::collections::HashMap;
use std::env;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::io::RawFd;  // Raw file descriptor type for Unix systems
use std::process;

// ============================================================================
// CONSTANTS: Default configuration values
// ============================================================================

// Default path to ThinkPad power LED brightness control
// This sysfs file accepts "0" (off) or "1" (on) to control LED state
const DEFAULT_LED_PATH: &str = "/sys/class/leds/tpacpi::power/brightness";

// Default path to NVMe disk statistics file
// This file contains space-separated counters for I/O operations
// Format: reads completed, reads merged, sectors read, time reading (ms),
//		   writes completed, writes merged, sectors written, time writing (ms), ...
const DEFAULT_NVME_STAT_PATH: &str = "/sys/block/nvme0n1/stat";

// How often to poll the NVMe stat file for changes (in milliseconds)
// Lower values = more responsive but higher CPU usage
// 10ms provides good balance between responsiveness and efficiency
const DEFAULT_POLL_INTERVAL_MS: u64 = 10;

// How long to keep the LED illuminated after detecting activity (in milliseconds)
// This creates a visible "blink" effect even for very brief I/O operations
const DEFAULT_BLINK_ON_MS: u64 = 10;

// Path to optional configuration file
// If present, settings are loaded from here before applying CLI overrides
const DEFAULT_CONFIG_PATH: &str = "/etc/nvme-led-daemon.conf";

// ============================================================================
// ENUMS: Type definitions for configuration options
// ============================================================================

/// Determines which fields from /sys/block/nvme0n1/stat to monitor
/// The stat file contains multiple counters; we can track either:
/// - I/O operation counts (how many read/write operations)
/// - Sector counts (how much data transferred in 512-byte sectors)
#[derive(Copy, Clone, Debug)]
enum NvmeMode {
	/// Monitor sectors read/written (fields 2 and 6 in stat file)
	/// Better for detecting large sequential transfers
	Sectors,
	
	/// Monitor I/O operations count (fields 0 and 4 in stat file)
	/// Better for detecting small random I/O patterns
	Io
}

/// Direction of disk activity (read or write)
/// Used to determine which blink duration to apply and for filtering
#[derive(Copy, Clone, Debug, PartialEq)]
enum Dir { 
	Read,	// Data being read from disk
	Write	// Data being written to disk
}

/// Which types of operations should trigger the LED
/// Allows filtering to only show reads, only writes, or both
#[derive(Copy, Clone, Debug)]
enum FieldsSel {
	Reads,	 // Only read operations trigger LED
	Writes,  // Only write operations trigger LED
	Both	 // Both read and write operations trigger LED
}

// ============================================================================
// UTILITY FUNCTIONS
// ============================================================================

/// Convert milliseconds to nanoseconds for timer APIs
/// Linux timer APIs use timespec which requires separate seconds and nanoseconds
/// This helper converts our millisecond values to nanoseconds for the nsec field
#[inline(always)]
fn ns_from_ms(ms: u64) -> i64 { 
	(ms as i64) * 1_000_000  // 1 millisecond = 1,000,000 nanoseconds
}

// ============================================================================
// EPOLL WRAPPER: Efficient event monitoring
// ============================================================================

/// Wrapper around Linux epoll for efficient event monitoring
/// Epoll is a Linux kernel facility that allows a process to monitor multiple
/// file descriptors to see if I/O is possible on any of them. Unlike select/poll,
/// epoll scales well to large numbers of file descriptors.
/// 
/// In our case, we use it to wait on two timerfd file descriptors:
/// 1. A periodic timer for polling NVMe stats
/// 2. A one-shot timer for turning the LED off
struct Epoll { 
	fd: RawFd  // File descriptor for the epoll instance
}

impl Epoll {
	/// Create a new epoll instance with CLOEXEC flag
	/// CLOEXEC ensures the fd is closed if we exec() another program
	/// (not relevant for this daemon, but good practice)
	fn new() -> io::Result<Self> {
		// Call Linux epoll_create1 syscall
		let fd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
		if fd < 0 { 
			return Err(io::Error::last_os_error()); 
		}
		Ok(Self { fd })
	}
	
	/// Register a file descriptor to monitor with epoll
	/// 
	/// # Arguments
	/// * `fd` - The file descriptor to monitor (in our case, timerfd)
	/// * `data_u64` - User data to identify which fd triggered (our "tag")
	///				   This value is returned in events, letting us distinguish
	///				   between the poll timer and off timer
	/// * `events` - Bitmask of events to monitor (e.g., EPOLLIN for readable)
	///				 Timerfds become readable when they expire
	fn add_fd(&self, fd: RawFd, data_u64: u64, events: u32) -> io::Result<()> {
		// Create epoll_event structure with our tag in the u64 field
		let mut ev = libc::epoll_event { events, u64: data_u64 };
		
		// Register the fd with epoll using EPOLL_CTL_ADD operation
		if unsafe { libc::epoll_ctl(self.fd, libc::EPOLL_CTL_ADD, fd, &mut ev) } < 0 {
			return Err(io::Error::last_os_error());
		}
		Ok(())
	}
	
	/// Wait for events on any registered file descriptors
	/// This is the core of our event loop - it blocks until at least one
	/// of our timers expires, then returns information about which one(s)
	/// 
	/// # Arguments
	/// * `events` - Buffer to receive event information
	/// 
	/// # Returns
	/// Number of events that occurred (how many entries in events[] are valid)
	fn wait(&self, events: &mut [libc::epoll_event]) -> io::Result<usize> {
		// Call epoll_wait with timeout=-1 (block indefinitely until event)
		// This is efficient: the process sleeps and kernel wakes it when timer fires
		let n = unsafe { 
			libc::epoll_wait(
				self.fd,					// epoll instance
				events.as_mut_ptr(),		// output buffer
				events.len() as i32,		// buffer size
				-1							// timeout (-1 = infinite)
			) 
		};
		
		if n < 0 { 
			return Err(io::Error::last_os_error()); 
		}
		Ok(n as usize)
	}
}

/// Clean up epoll fd when dropped
/// Rust's RAII pattern ensures this is called automatically when Epoll goes out of scope
impl Drop for Epoll { 
	fn drop(&mut self) { 
		unsafe { libc::close(self.fd) }; 
	} 
}

// ============================================================================
// TIMERFD WRAPPER: Precise timing via file descriptors
// ============================================================================

/// Wrapper around Linux timerfd for precise timing
/// Timerfd is a Linux feature that creates a file descriptor which becomes
/// readable when a timer expires. This allows timers to be integrated with
/// epoll/select/poll for event-driven programming.
/// 
/// We use two timerfds:
/// 1. A periodic timer that fires every poll_ms to check NVMe stats
/// 2. A one-shot timer that fires once to turn the LED off after activity
struct Tfd(RawFd);	// Newtype wrapper around raw file descriptor

impl Tfd {
	/// Create a periodic timer that fires every interval_ms milliseconds
	/// Used for the polling timer that checks NVMe stats regularly
	/// 
	/// The timer starts immediately (after 1ns) and then repeats at the
	/// specified interval. This ensures we get the first poll quickly.
	fn periodic(interval_ms: u64) -> io::Result<Self> {
		// Create timerfd with CLOCK_MONOTONIC (not affected by system time changes)
		// TFD_NONBLOCK: reads won't block (we use epoll anyway)
		// TFD_CLOEXEC: close on exec (good practice)
		let fd = unsafe { 
			libc::timerfd_create(
				libc::CLOCK_MONOTONIC,						// clock type
				libc::TFD_NONBLOCK | libc::TFD_CLOEXEC		// flags
			) 
		};
		if fd < 0 { 
			return Err(io::Error::last_os_error()); 
		}
		
		// Set up repeating timer with specified interval
		// itimerspec has two timespec fields:
		// - it_interval: how often to repeat (0 = one-shot)
		// - it_value: initial expiration time (0 = disarm timer)
		let spec = libc::itimerspec {
			// Repeat interval: convert ms to seconds + nanoseconds
			it_interval: libc::timespec { 
				tv_sec: (interval_ms / 1000) as i64,		   // whole seconds
				tv_nsec: ns_from_ms(interval_ms % 1000)		   // remaining milliseconds as nanoseconds
			},
			// Initial expiration: 1 nanosecond (fire almost immediately)
			it_value: libc::timespec { 
				tv_sec: 0, 
				tv_nsec: 1 
			},
		};
		
		// Arm the timer with our specification
		// flags=0 means it_value is relative time (not absolute)
		if unsafe { libc::timerfd_settime(fd, 0, &spec, std::ptr::null_mut()) } < 0 {
			let e = io::Error::last_os_error(); 
			unsafe { libc::close(fd) };  // Clean up on error
			return Err(e);
		}
		Ok(Self(fd))
	}
	
	/// Create a one-shot timer (initially disarmed)
	/// Used for the LED off-timer that fires once after LED turns on
	/// 
	/// We create it disarmed (all zeros) and arm it later with arm_after_ms()
	/// when we detect activity. This is more efficient than creating/destroying
	/// the timer on each activity event.
	fn oneshot() -> io::Result<Self> {
		// Create timerfd with same flags as periodic timer
		let fd = unsafe { 
			libc::timerfd_create(
				libc::CLOCK_MONOTONIC, 
				libc::TFD_NONBLOCK | libc::TFD_CLOEXEC
			) 
		};
		if fd < 0 { 
			return Err(io::Error::last_os_error()); 
		}
		
		// Create disarmed timer (all zeros)
		// it_interval=0 means one-shot (no repeat)
		// it_value=0 means disarmed (not running)
		let zero = libc::itimerspec {
			it_interval: libc::timespec { tv_sec: 0, tv_nsec: 0 },
			it_value: libc::timespec { tv_sec: 0, tv_nsec: 0 },
		};
		
		// Set the timer to disarmed state
		unsafe { libc::timerfd_settime(fd, 0, &zero, std::ptr::null_mut()) };
		Ok(Self(fd))
	}
	
	/// Arm the one-shot timer to fire after delay_ms milliseconds
	/// Used to schedule LED turn-off after activity detected
	/// 
	/// If the timer is already armed, this resets it to the new delay.
	/// This is how we extend the LED blink on continuous activity:
	/// each new activity event resets the off-timer.
	fn arm_after_ms(&self, delay_ms: u64) -> io::Result<()> {
		// Create timer spec with no repeat (it_interval=0) and specified delay
		let spec = libc::itimerspec {
			it_interval: libc::timespec { tv_sec: 0, tv_nsec: 0 },	// No repeat (one-shot)
			it_value: libc::timespec { 
				tv_sec: (delay_ms / 1000) as i64,			   // whole seconds
				tv_nsec: ns_from_ms(delay_ms % 1000)		   // remaining milliseconds
			},
		};
		
		// Arm the timer - this replaces any previous setting
		if unsafe { libc::timerfd_settime(self.0, 0, &spec, std::ptr::null_mut()) } < 0 {
			return Err(io::Error::last_os_error());
		}
		Ok(())
	}
	
	/// Acknowledge timer expiration by reading from the fd
	/// When a timerfd expires, it becomes readable. Reading from it:
	/// 1. Clears the readable state (so epoll won't immediately trigger again)
	/// 2. Returns a u64 with the number of expirations since last read
	/// 
	/// We don't care about the count (we just want to clear the state),
	/// so we ignore the return value and any errors.
	fn ack(&self, buf8: &mut [u8; 8]) { 
		// Read 8 bytes (u64) from timerfd - this clears the readable state
		// We ignore errors because there's nothing useful to do if this fails
		unsafe { 
			libc::read(
				self.0,							// timerfd file descriptor
				buf8.as_mut_ptr() as *mut _,	// buffer to receive count
				8								// always read 8 bytes (u64)
			); 
		}; 
	}
}

/// Clean up timerfd when dropped
/// Ensures the file descriptor is closed when Tfd goes out of scope
impl Drop for Tfd { 
	fn drop(&mut self) { 
		unsafe { libc::close(self.0) }; 
	} 
}

// ============================================================================
// LED CONTROLLER: Manages LED state via sysfs
// ============================================================================

/// LED controller that writes to sysfs brightness file
/// 
/// Most Linux LED drivers expose a "brightness" file in sysfs that accepts
/// ASCII "0" or "1" to control the LED. We keep the file open and cache the
/// current state to avoid redundant writes (which cause unnecessary syscalls
/// and potential flickering).
struct Led {
	f: File,				  // Open file handle to LED brightness sysfs file
	current_logical: u8,	  // Cache of current state (0=off, 1=on, 255=unknown)
	active_high: bool,		  // LED polarity: true=1 is on, false=0 is on
}

impl Led {
	/// Open LED sysfs file and initialize state tracking
	/// 
	/// # Arguments
	/// * `path` - Path to LED brightness file (e.g., /sys/class/leds/tpacpi::power/brightness)
	/// * `active_high` - LED polarity (true if writing "1" turns LED on)
	/// 
	/// Some LEDs are "active low" meaning writing "0" turns them on.
	/// The active_high parameter handles this difference.
	fn new(path: &str, active_high: bool) -> io::Result<Self> {
		// Open the LED file for writing only
		// We keep it open for the lifetime of the program to avoid
		// repeated open/close syscalls
		let f = OpenOptions::new().write(true).open(path)?;
		
		Ok(Self { 
			f, 
			current_logical: 255,  // 255 = unknown state (forces first write)
			active_high 
		})
	}
	
	/// Set LED state, avoiding redundant writes
	/// 
	/// This is the core LED control function. It:
	/// 1. Checks if we're already in the desired state (avoids redundant writes)
	/// 2. Converts logical state (on/off) to physical value based on polarity
	/// 3. Writes the value to the sysfs file
	/// 4. Updates the cached state
	#[inline(always)]
	fn set(&mut self, on: bool) -> io::Result<()> {
		// Convert boolean to numeric state for comparison
		let want = if on { 1 } else { 0 };
		
		// Skip write if already in desired state
		// This is important for performance: avoiding unnecessary syscalls
		// and preventing potential LED flickering from redundant writes
		if self.current_logical == want { 
			return Ok(()); 
		}
		
		// Convert logical state to physical value based on polarity
		// For active-high LEDs: on=1, off=0
		// For active-low LEDs: on=0, off=1 (inverted)
		let phys = if self.active_high { 
			if on { b'1' } else { b'0' } 
		} else { 
			if on { b'0' } else { b'1' }  // Inverted for active-low LEDs
		};
		
		// Write ASCII digit followed by newline
		// Most sysfs files expect a newline-terminated value
		let buf = [phys, b'\n'];
		self.f.write_all(&buf)?;
		
		// Update cached state so next call can skip write if unchanged
		self.current_logical = want;
		Ok(())
	}
	
	/// Convenience method to turn LED on
	#[inline(always)] 
	fn on(&mut self) -> io::Result<()> { 
		self.set(true) 
	}
	
	/// Convenience method to turn LED off
	#[inline(always)] 
	fn off(&mut self) -> io::Result<()> { 
		self.set(false) 
	}
}

// ============================================================================
// NVME ACTIVITY MONITOR: Detects disk I/O by polling stat file
// ============================================================================

/// NVMe activity monitor that reads /sys/block/nvme0n1/stat
/// 
/// The Linux kernel exposes disk statistics in /sys/block/*/stat with the format:
/// Field  0: reads completed successfully
/// Field  1: reads merged
/// Field  2: sectors read
/// Field  3: time spent reading (ms)
/// Field  4: writes completed
/// Field  5: writes merged
/// Field  6: sectors written
/// Field  7: time spent writing (ms)
/// Field  8: I/Os currently in progress
/// Field  9: time spent doing I/Os (ms)
/// Field 10: weighted time spent doing I/Os (ms)
/// 
/// We monitor either fields 0&4 (I/O counts) or 2&6 (sector counts) and
/// detect activity by comparing to previous values.
struct Nvme {
	path: String,		  // Path to stat file (e.g., /sys/block/nvme0n1/stat)
	last_reads: u128,	  // Previous read counter value (u128 to avoid overflow)
	last_writes: u128,	  // Previous write counter value
	mode: NvmeMode,		  // Which fields to monitor (sectors vs I/O count)
}

impl Nvme {
	/// Create a new NVMe monitor
	/// 
	/// # Arguments
	/// * `path` - Path to the stat file
	/// * `mode` - Which counters to monitor (Sectors or Io)
	fn new(path: &str, mode: NvmeMode) -> Self {
		Self { 
			path: path.to_string(), 
			last_reads: 0,		// Start with zero (first poll will show activity)
			last_writes: 0, 
			mode 
		}
	}
	
	/// Check for disk activity by reading stat file and comparing to previous values
	/// 
	/// This is called on every poll timer tick. It:
	/// 1. Opens and reads the stat file
	/// 2. Parses the relevant counter fields
	/// 3. Compares to previous values to detect changes
	/// 4. Returns the direction of activity (read/write) or None if no activity
	/// 
	/// # Returns
	/// * `Some(Dir::Read)` - Only read counter increased
	/// * `Some(Dir::Write)` - Only write counter increased, or both increased
	/// * `None` - No activity detected
	/// 
	/// Note: If both counters increased, we report Write. This is arbitrary but
	/// ensures we always report something when there's activity.
	fn activity_dir(&mut self, scratch: &mut [u8; 256]) -> io::Result<Option<Dir>> {
		// Open and read entire stat file into buffer
		// We open/close on each poll rather than keeping it open because
		// the kernel updates the file contents on each read
		let mut f = File::open(&self.path)?;
		let n = f.read(scratch)?;
		
		// Convert bytes to string for parsing
		let s = std::str::from_utf8(&scratch[..n]).unwrap_or("");
		
		// Parse whitespace-separated fields
		let mut idx = 0usize;	   // Current field index
		let mut r = None;		   // Read counter value
		let mut w = None;		   // Write counter value
		
		// Iterate through whitespace-separated tokens
		for token in s.split_whitespace() {
			// Try to parse as u64 (all stat fields are numeric)
			if let Ok(v) = token.parse::<u64>() {
				// Extract the fields we care about based on mode
				match self.mode {
					NvmeMode::Sectors => {
						// Field 2: sectors read (512-byte sectors)
						if idx == 2 { r = Some(v as u128); }
						// Field 6: sectors written
						if idx == 6 { 
							w = Some(v as u128); 
							// Early exit once we have both values
							if r.is_some() { break; } 
						}
					}
					NvmeMode::Io => {
						// Field 0: read I/Os completed successfully
						if idx == 0 { r = Some(v as u128); }
						// Field 4: write I/Os completed
						if idx == 4 { 
							w = Some(v as u128); 
							// Early exit once we have both values
							if r.is_some() { break; } 
						}
					}
				}
				idx += 1;
			} else {
				// Non-numeric token (shouldn't happen, but handle gracefully)
				idx += 1;
			}
		}
		
		// Check if we successfully parsed both values
		// If not, return None (file format unexpected)
		let (Some(rn), Some(wn)) = (r, w) else { 
			return Ok(None); 
		};
		
		// Compare to previous values to detect changes
		// Any increase in counter indicates activity
		let rchg = rn != self.last_reads;
		let wchg = wn != self.last_writes;
		
		// Update cached values for next comparison
		// Important: do this before returning so next poll sees new baseline
		self.last_reads = rn;
		self.last_writes = wn;
		
		// Determine activity direction based on which counter(s) changed
		// Priority: if both changed, report as Write (arbitrary choice)
		if rchg && !wchg { 
			Ok(Some(Dir::Read))		 // Only reads increased
		} else if wchg && !rchg { 
			Ok(Some(Dir::Write))	 // Only writes increased
		} else if rchg && wchg { 
			Ok(Some(Dir::Write))	 // Both increased, report as write
		} else { 
			Ok(None)				 // No change detected
		}
	}
}

// ============================================================================
// CONFIGURATION: Settings loaded from file and/or CLI
// ============================================================================

/// Configuration loaded from file and/or command-line arguments
/// 
/// Settings are loaded in this order (later overrides earlier):
/// 1. Hard-coded defaults (DEFAULT_* constants)
/// 2. Default config file (/etc/nvme-led-daemon.conf) if present
/// 3. Custom config file (--config PATH) if specified
/// 4. Command-line arguments
#[derive(Clone)]
struct Config {
	led_path: String,				   // Path to LED sysfs file
	nvme_path: String,				   // Path to NVMe stat file
	poll_ms: u64,					   // Polling interval in milliseconds
	blink_ms: u64,					   // Default LED on duration in milliseconds
	read_blink_ms: Option<u64>,		   // Override blink duration for reads (if Some)
	write_blink_ms: Option<u64>,	   // Override blink duration for writes (if Some)
	active_high: bool,				   // LED polarity (true = writing "1" turns on)
	quiet: bool,					   // Suppress startup message
	nvme_mode: NvmeMode,			   // Which stat fields to monitor
	on_fields: FieldsSel,			   // Which operations trigger LED
}

/// Load configuration from a key=value file
/// 
/// File format:
/// - Lines starting with # are comments
/// - Empty lines are ignored
/// - Settings are key=value pairs
/// - Whitespace around = is trimmed
/// 
/// Example:
/// ```
/// # NVMe LED daemon configuration
/// led_path=/sys/class/leds/tpacpi::power/brightness
/// interval_ms=10
/// blink_ms=20
/// active_high=false
/// ```
fn load_config(path: &str) -> io::Result<HashMap<String, String>> {
	// Read entire file as string
	let contents = std::fs::read_to_string(path)?;
	let mut map = HashMap::new();
	
	// Parse line by line
	for line in contents.lines() {
		let line = line.trim();
		
		// Skip comments and empty lines
		if line.is_empty() || line.starts_with('#') { 
			continue; 
		}
		
		// Split on first '=' to get key and value
		if let Some((k, v)) = line.split_once('=') {
			// Trim whitespace and store in map
			map.insert(k.trim().to_string(), v.trim().to_string());
		}
	}
	Ok(map)
}

/// Parse boolean from config map with default fallback
/// Accepts: true/yes/1 for true, false/no/0 for false
/// Returns default if key not found or value not recognized
fn get_bool(map: &HashMap<String, String>, key: &str, default: bool) -> bool {
	map.get(key).and_then(|v| match v.as_str() {
		"true" | "yes" | "1" => Some(true),
		"false" | "no" | "0" => Some(false),
		_ => None  // Unrecognized value, use default
	}).unwrap_or(default)
}

/// Parse u64 from config map with default fallback
/// Returns default if key not found or value not a valid u64
fn get_u64(map: &HashMap<String, String>, key: &str, default: u64) -> u64 {
	map.get(key)
		.and_then(|v| v.parse().ok())  // Try to parse as u64
		.unwrap_or(default)				// Use default if parse fails
}

/// Get string from config map with default fallback
/// Returns reference to value in map, or default if key not found
fn get_str<'a>(map: &'a HashMap<String, String>, key: &str, default: &'a str) -> &'a str {
	map.get(key)
		.map(|s| s.as_str())  // Convert String to &str
		.unwrap_or(default)    // Use default if key not found
}

/// Print help message and exit
/// Called when user passes --help or invalid arguments
fn help() -> ! {
	eprintln!(
"nvme-led-daemon (GPL-3.0-or-later)
Usage:
  nvme-led-daemon [--config PATH] [OPTIONS]

Config file (optional): {default_cfg}
CLI options override config file settings.

Options:
  --config PATH    Load config from PATH
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
  led_path	  {lp}
  nvme_path    {np}
  interval_ms	 {pi}
  blink_ms	  {bm}
  nvme_mode    sectors
  on_fields    both
",
		default_cfg = DEFAULT_CONFIG_PATH,
		lp = DEFAULT_LED_PATH, 
		np = DEFAULT_NVME_STAT_PATH,
		pi = DEFAULT_POLL_INTERVAL_MS, 
		bm = DEFAULT_BLINK_ON_MS
	);
	process::exit(0)
}

/// Parse configuration from default config file and command-line arguments
/// 
/// Loading order:
/// 1. Try to load /etc/nvme-led-daemon.conf (silently ignore if missing)
/// 2. Apply defaults from config file or use hard-coded defaults
/// 3. Process CLI arguments, which override config file settings
/// 4. If --config specified, load that file and re-apply its settings
///    (but CLI args still take precedence)
/// 
/// This allows flexible configuration: you can use just CLI args, just a
/// config file, or a mix of both with CLI args overriding file settings.
fn parse_args() -> Config {
	// Try loading default config file first (silently ignore if missing)
	// unwrap_or_else returns empty HashMap if file doesn't exist
	let config_map = load_config(DEFAULT_CONFIG_PATH)
		.unwrap_or_else(|_| HashMap::new());

	// Initialize config with defaults from file or constants
	// get_* functions handle missing keys by returning defaults
	let mut cfg = Config {
		led_path: get_str(&config_map, "led_path", DEFAULT_LED_PATH).to_string(),
		nvme_path: get_str(&config_map, "nvme_path", DEFAULT_NVME_STAT_PATH).to_string(),
		poll_ms: get_u64(&config_map, "interval_ms", DEFAULT_POLL_INTERVAL_MS),
		blink_ms: get_u64(&config_map, "blink_ms", DEFAULT_BLINK_ON_MS),
		
		// Optional per-direction blink durations
		read_blink_ms: config_map.get("read_blink_ms")
			.and_then(|v| v.parse().ok()),
		write_blink_ms: config_map.get("write_blink_ms")
			.and_then(|v| v.parse().ok()),
		
		active_high: get_bool(&config_map, "active_high", false),
		quiet: get_bool(&config_map, "quiet", false),
		
		// Parse nvme_mode from string
		nvme_mode: match get_str(&config_map, "nvme_mode", "sectors") {
			"io" => NvmeMode::Io,
			_ => NvmeMode::Sectors,  // Default to sectors for any other value
		},
		
		// Parse on_fields from string
		on_fields: match get_str(&config_map, "on_fields", "both") {
			"reads" => FieldsSel::Reads,
			"writes" => FieldsSel::Writes,
			_ => FieldsSel::Both,  // Default to both for any other value
		},
	};

	// Process command-line arguments, overriding config file values
	// skip(1) skips the program name (argv[0])
	let mut it = env::args().skip(1).peekable();
	
	while let Some(a) = it.next() {
		match a.as_str() {
			"--help" | "-h" => help(),	// Print help and exit
			
			// Boolean flags (no argument)
			"--quiet" => cfg.quiet = true,
			"--active-high" => cfg.active_high = true,
			
			// Path arguments (require next argument)
			"--led" => { 
				cfg.led_path = it.next().unwrap_or_else(|| { 
					eprintln!("--led requires PATH"); 
					process::exit(2) 
				}); 
			}
			
			"--nvme" => { 
				cfg.nvme_path = it.next().unwrap_or_else(|| { 
					eprintln!("--nvme requires PATH"); 
					process::exit(2) 
				}); 
			}
			
			// Numeric arguments with validation
			"--interval-ms" => {
				cfg.poll_ms = it.next()
					.and_then(|v| v.parse().ok())
					.unwrap_or_else(|| { 
						eprintln!("invalid --interval-ms"); 
						process::exit(2) 
					});
				// Enforce minimum of 1ms (0 would cause busy loop)
				if cfg.poll_ms == 0 { cfg.poll_ms = 1; }
			}
			
			"--blink-ms" => {
				cfg.blink_ms = it.next()
					.and_then(|v| v.parse().ok())
					.unwrap_or_else(|| { 
						eprintln!("invalid --blink-ms"); 
						process::exit(2) 
					});
				// Enforce minimum of 1ms
				if cfg.blink_ms == 0 { cfg.blink_ms = 1; }
			}
			
			"--read-blink-ms" => {
				let v: u64 = it.next()
					.and_then(|v| v.parse().ok())
					.unwrap_or_else(|| { 
						eprintln!("invalid --read-blink-ms"); 
						process::exit(2) 
					});
				// Store as Some with minimum of 1ms
				cfg.read_blink_ms = Some(v.max(1));
			}
			
			"--write-blink-ms" => {
				let v: u64 = it.next()
					.and_then(|v| v.parse().ok())
					.unwrap_or_else(|| { 
						eprintln!("invalid --write-blink-ms"); 
						process::exit(2) 
					});
				// Store as Some with minimum of 1ms
				cfg.write_blink_ms = Some(v.max(1));
			}
			
			// Enum arguments with validation
			"--nvme-mode" => {
				let v = it.next().unwrap_or_else(|| { 
					eprintln!("--nvme-mode requires io|sectors"); 
					process::exit(2) 
				});
				cfg.nvme_mode = match v.as_str() {
					"io" => NvmeMode::Io,
					"sectors" => NvmeMode::Sectors,
					_ => { 
						eprintln!("--nvme-mode must be io or sectors"); 
						process::exit(2) 
					}
				}
			}
			
			"--on-fields" => {
				let v = it.next().unwrap_or_else(|| { 
					eprintln!("--on-fields requires reads|writes|both"); 
					process::exit(2) 
				});
				cfg.on_fields = match v.as_str() {
					"reads" => FieldsSel::Reads,
					"writes" => FieldsSel::Writes,
					"both" => FieldsSel::Both,
					_ => { 
						eprintln!("--on-fields must be reads|writes|both"); 
						process::exit(2) 
					}
				}
			}
			
			// Load custom config file
			// This re-applies config file settings, but CLI args already
			// processed still take precedence (we don't re-process them)
			"--config" => {
				let path = it.next().unwrap_or_else(|| { 
					eprintln!("--config requires PATH"); 
					process::exit(2) 
				});
				
				// Load the custom config file (error if it doesn't exist)
				let new_map = load_config(&path).unwrap_or_else(|e| {
					eprintln!("Failed to load config {}: {}", path, e);
					process::exit(2)
				});
				
				// Re-apply config from custom path
				// Use current values as defaults so CLI args aren't overridden
				cfg.led_path = get_str(&new_map, "led_path", &cfg.led_path).to_string();
				cfg.nvme_path = get_str(&new_map, "nvme_path", &cfg.nvme_path).to_string();
				cfg.poll_ms = get_u64(&new_map, "interval_ms", cfg.poll_ms);
				cfg.blink_ms = get_u64(&new_map, "blink_ms", cfg.blink_ms);
				
				// Optional values: only override if present in new config
				if let Some(v) = new_map.get("read_blink_ms").and_then(|v| v.parse().ok()) { 
					cfg.read_blink_ms = Some(v); 
				}
				if let Some(v) = new_map.get("write_blink_ms").and_then(|v| v.parse().ok()) { 
					cfg.write_blink_ms = Some(v); 
				}
				
				cfg.active_high = get_bool(&new_map, "active_high", cfg.active_high);
				cfg.quiet = get_bool(&new_map, "quiet", cfg.quiet);
				
				// Parse enum values with current value as default
				cfg.nvme_mode = match get_str(&new_map, "nvme_mode", 
					match cfg.nvme_mode { 
						NvmeMode::Io => "io", 
						NvmeMode::Sectors => "sectors" 
					}) {
					"io" => NvmeMode::Io,
					_ => NvmeMode::Sectors,
				};
				
				cfg.on_fields = match get_str(&new_map, "on_fields", 
					match cfg.on_fields { 
						FieldsSel::Reads => "reads", 
						FieldsSel::Writes => "writes", 
						FieldsSel::Both => "both" 
					}) {
					"reads" => FieldsSel::Reads,
					"writes" => FieldsSel::Writes,
					_ => FieldsSel::Both,
				};
			}
			
			// Unknown argument
			other => { 
				eprintln!("Unknown arg: {}", other); 
				help();  // Print help and exit
			}
		}
	}
	
	cfg
}

// ============================================================================
// MAIN: Event loop that ties everything together
// ============================================================================

/// Main event loop: monitor NVMe activity and blink LED accordingly
/// 
/// Architecture:
/// 1. Set up epoll with two timerfds (poll timer and off timer)
/// 2. Enter infinite loop waiting for timer events
/// 3. On poll timer: check NVMe stats, turn LED on if activity detected
/// 4. On off timer: turn LED off
/// 
/// The key insight is that we use two independent timers:
/// - Poll timer fires regularly (e.g., every 10ms) to check for activity
/// - Off timer is armed when activity detected and fires once to turn LED off
/// 
/// This allows precise control of LED on-duration while maintaining efficient
/// polling. The LED stays on as long as activity continues (each activity
/// event resets the off timer).
fn main() -> io::Result<()> {
	// Load configuration from file and CLI arguments
	let cfg = parse_args();

	// Set up epoll for event-driven I/O
	// This allows us to wait on multiple timers efficiently
	let ep = Epoll::new()?;
	
	// Create two timers:
	// 1. Periodic timer for polling NVMe stats at regular intervals
	let poll_tfd = Tfd::periodic(cfg.poll_ms)?;
	
	// 2. One-shot timer for turning LED off after blink duration
	//	  Created disarmed; we arm it when activity is detected
	let off_tfd = Tfd::oneshot()?;

	// Tags to identify which timer fired in epoll events
	// These are arbitrary u64 values we use to distinguish the timers
	const POLL_TAG: u64 = 1;  // Poll timer identifier
	const OFF_TAG: u64 = 2;   // Off timer identifier

	// Register both timers with epoll
	// EPOLLIN means we want to be notified when the fd is readable
	// (timerfds become readable when they expire)
	ep.add_fd(poll_tfd.0, POLL_TAG, libc::EPOLLIN as u32)?;
	ep.add_fd(off_tfd.0, OFF_TAG, libc::EPOLLIN as u32)?;

	// Initialize LED controller and NVMe monitor
	let mut led = Led::new(&cfg.led_path, cfg.active_high)?;
	let mut nvme = Nvme::new(&cfg.nvme_path, cfg.nvme_mode);

	// Buffers for epoll events and file reads
	// We only have 2 timers, so we only need space for 2 events
	let mut events = [libc::epoll_event { events: 0, u64: 0 }; 2];
	
	// Buffer for timer acknowledgment reads (timerfds return u64)
	let mut tbuf = [0u8; 8];
	
	// Buffer for reading NVMe stat file (256 bytes is plenty)
	let mut sbuf = [0u8; 256];

	// Track LED state to avoid redundant operations
	// This is redundant with Led::current_logical but makes the logic clearer
	let mut led_on = false;

	// Print startup message unless quiet mode
	// This helps with debugging and confirms the daemon started successfully
	if !cfg.quiet {
		println!(
			"nvme-led-daemon: led={} nvme={} interval={}ms blink={}ms read_blink={:?} write_blink={:?} active_high={} mode={:?} on_fields={:?} (pid={})",
			cfg.led_path,			// LED sysfs path
			cfg.nvme_path,			// NVMe stat file path
			cfg.poll_ms,			// Polling interval
			cfg.blink_ms,			// Default blink duration
			cfg.read_blink_ms,		// Read-specific blink duration (if set)
			cfg.write_blink_ms,		// Write-specific blink duration (if set)
			cfg.active_high,		// LED polarity
			match cfg.nvme_mode {	// Which stat fields we're monitoring
				NvmeMode::Sectors => "sectors", 
				NvmeMode::Io => "io" 
			},
			match cfg.on_fields {	// Which operations trigger LED
				FieldsSel::Reads => "reads", 
				FieldsSel::Writes => "writes", 
				FieldsSel::Both => "both" 
			},
			std::process::id()		// Our PID (useful for systemd, etc.)
		);
	}

	// Ensure LED starts in off state
	// Ignore errors here (LED might already be off)
	let _ = led.off();

	// Main event loop - runs forever until killed
	loop {
		// Wait for timer events (blocks until at least one timer expires)
		// This is efficient: the process sleeps and the kernel wakes it
		// when a timer fires. No busy-waiting or polling.
		let n = ep.wait(&mut events)?;
		
		// Process all events that occurred
		// Usually n=1 (one timer fired), but could be 2 if both fired
		// between epoll_wait calls (unlikely but possible)
		for i in 0..n {
			// Extract the tag we set when registering the fd
			// This tells us which timer fired
			let tag = events[i].u64;
			
			match tag {
				POLL_TAG => {
					// Polling timer fired - time to check for NVMe activity
					
					// First, acknowledge the timer to clear its readable state
					// This prevents epoll from immediately triggering again
					poll_tfd.ack(&mut tbuf);
					
					// Check if there's been any disk activity since last poll
					// Returns Some(Dir) if activity detected, None otherwise
					if let Some(dir) = nvme.activity_dir(&mut sbuf)? {
						// Activity detected! Determine if we should blink for it
						// based on the on_fields filter
						let relevant = match (cfg.on_fields, dir) {
							(FieldsSel::Both, _) => true,			   // Both: always relevant
							(FieldsSel::Reads, Dir::Read) => true,	   // Reads only: relevant if read
							(FieldsSel::Writes, Dir::Write) => true,   // Writes only: relevant if write
							_ => false,									// Filtered out
						};
						
						if relevant {
							// Determine blink duration
							// Start with default, then check for direction-specific override
							let mut dur = cfg.blink_ms;
							
							// Override with read-specific duration if set
							if dir == Dir::Read { 
								if let Some(r) = cfg.read_blink_ms { 
									dur = r; 
								} 
							}
							
							// Override with write-specific duration if set
							if dir == Dir::Write { 
								if let Some(w) = cfg.write_blink_ms { 
									dur = w; 
								} 
							}

							// Turn LED on if not already on
							// The LED::on() method will skip the write if already on
							if !led_on { 
								led.on()?; 
								led_on = true; 
							}
							
							// Schedule LED turn-off after blink duration
							// If the timer is already armed (from previous activity),
							// this resets it to the new duration. This is how we
							// extend the LED blink on continuous activity: each new
							// activity event pushes the off-time further into the future.
							off_tfd.arm_after_ms(dur)?;
						}
					}
				}
				
				OFF_TAG => {
					// Off-timer fired - time to turn LED off
					
					// Acknowledge the timer to clear its readable state
					off_tfd.ack(&mut tbuf);
					
					// Turn LED off if it's currently on
					// The LED::off() method will skip the write if already off
					if led_on {
						led.off()?;
						led_on = false;
					}
				}
				
				_ => {
					// Unknown tag (shouldn't happen with our setup)
					// We only registered two fds with specific tags
					// If we get here, something is very wrong
				}
			}
		}
	}
	
	// Note: we never reach here (infinite loop above)
	// If we did, Rust's Drop implementations would clean up:
	// - Epoll::drop() closes epoll fd
	// - Tfd::drop() closes both timerfd fds
	// - File in Led is automatically closed
}
