//! Host information for Settings ▸ Memory & CPU (RAM reserved for other applications, reducing
//! caches when the system is low on memory) and Help ▸ System Compatibility Report.
//!
//! Pure Rust, no `unsafe`: the values come from the operating system's own reporting
//! (`/proc/meminfo` on Linux, the kernel's `sysctl` counters on macOS through the safe `sysctl`
//! and `memory-stats` crates, PowerShell on Windows). Every query may fail (sandboxes, the web):
//! callers treat `None` as "unknown" and keep their configured budgets. The periodic readings
//! behind the cache budgets and the memory chip ([`MemoryWatch`], [`ProcessWatch`]) take a few
//! microseconds and run inline; only Windows reads in the background (PowerShell takes about a
//! second). MEM3: macOS used to start `vm_stat`, `sysctl` and `ps` every 2 to 5 s.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use serde::Serialize;

/// Physical memory in bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct SysMemory {
    pub total: u64,
    /// Memory the system can hand out without swapping: free pages and caches it can drop
    /// (file-backed pages), never pages of other apps it would have to compress or swap out.
    pub available: u64,
    /// The system's memory pressure at the same reading, where it reports one (macOS).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pressure: Option<MemoryPressure>,
}

impl SysMemory {
    /// The system is low on memory: less than 10% (or less than 1 GB) is available, or macOS
    /// reports memory pressure (Warning or Critical: it compresses and swaps to keep up).
    pub fn is_low(&self) -> bool {
        self.pressure.is_some_and(|p| p >= MemoryPressure::Warning) || (self.total > 0 && (self.available < self.total / 10 || self.available < 1 << 30))
    }

    /// How much the caches shrink at this reading: 1 (not at all), 2 while memory is low or
    /// under Warning pressure, 4 under Critical pressure (the system asks apps to give memory
    /// back).
    pub fn cache_divisor(&self) -> u64 {
        if self.pressure == Some(MemoryPressure::Critical) {
            4
        } else if self.is_low() {
            2
        } else {
            1
        }
    }
}

/// How hard the operating system finds it to hand out memory (macOS's memory pressure level,
/// as Activity Monitor's Memory Pressure graph shows it).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub enum MemoryPressure {
    Normal,
    /// The system compresses and swaps memory to keep up (yellow in Activity Monitor).
    Warning,
    /// Apps are being asked to give memory back (red).
    Critical,
}

/// Parse macOS `sysctl -n kern.memorystatus_vm_pressure_level` (1 normal, 2 warning, 4 critical).
pub fn parse_pressure_level(text: &str) -> Option<MemoryPressure> {
    pressure_level(text.trim().parse::<u64>().ok()?)
}

/// macOS `kern.memorystatus_vm_pressure_level` as a [`MemoryPressure`].
pub fn pressure_level(level: u64) -> Option<MemoryPressure> {
    match level {
        1 => Some(MemoryPressure::Normal),
        2 => Some(MemoryPressure::Warning),
        4 => Some(MemoryPressure::Critical),
        _ => None,
    }
}

/// The system's memory pressure, where the system reports one (macOS).
pub fn memory_pressure() -> Option<MemoryPressure> {
    #[cfg(target_os = "macos")]
    {
        return pressure_level(macos::read("kern.memorystatus_vm_pressure_level")?);
    }
    #[allow(unreachable_code)]
    None
}

/// The kernel's counters on macOS, read with `sysctlbyname` through the `sysctl` crate (no
/// process started, no `unsafe` here).
#[cfg(target_os = "macos")]
mod macos {
    use sysctl::Sysctl;

    /// A numeric sysctl value.
    pub(super) fn read(name: &str) -> Option<u64> {
        sysctl::Ctl::new(name).ok()?.value_string().ok()?.trim().parse().ok()
    }

    /// Total and available memory (see [`super::macos_memory`]).
    pub(super) fn memory() -> Option<super::SysMemory> {
        let total = read("hw.memsize")?;
        let pages = super::MacPages {
            page: read("vm.pagesize").or_else(|| read("hw.pagesize"))?,
            free: read("vm.page_free_count")?,
            speculative: read("vm.page_speculative_count").unwrap_or(0),
            purgeable: read("vm.page_purgeable_count").unwrap_or(0),
            file_backed: read("vm.page_pageable_external_count").unwrap_or(0),
        };
        let level = read("kern.memorystatus_level");
        let pressure = read("kern.memorystatus_vm_pressure_level").and_then(super::pressure_level);
        Some(super::macos_memory(total, pages, level, pressure))
    }
}

/// macOS page counts (`vm.page_*_count`) and their page size.
#[derive(Clone, Copy, Debug, Default)]
pub struct MacPages {
    pub page: u64,
    pub free: u64,
    pub speculative: u64,
    pub purgeable: u64,
    /// File-backed (external) pageable pages: cached file data the system drops without
    /// writing anything out.
    pub file_backed: u64,
}

/// macOS memory from the kernel's counters: available = free + speculative + purgeable +
/// file-backed pages, and never more than the kernel's own estimate (`kern.memorystatus_level`,
/// the percentage it considers available). Inactive anonymous pages are other apps' memory that
/// the system would have to compress or swap out first: counting them as free (as `vm_stat`'s
/// "Pages inactive" did here before MEM3) read a Mac with 210 MB free and 9 GB swapped as having
/// plenty, so the caches never shrank.
pub fn macos_memory(total: u64, p: MacPages, level: Option<u64>, pressure: Option<MemoryPressure>) -> SysMemory {
    let pages = p.free.saturating_add(p.speculative).saturating_add(p.purgeable).saturating_add(p.file_backed);
    let mut available = pages.saturating_mul(p.page).min(total);
    if let Some(level) = level.filter(|l| *l <= 100) {
        available = available.min(total / 100 * level);
    }
    SysMemory { total, available, pressure }
}

/// The system's memory, read in the background: [`MemoryWatch::poll`] starts a reading (unless
/// one is running) and [`MemoryWatch::latest`] returns the last one without waiting.
#[derive(Clone, Default)]
pub struct MemoryWatch {
    latest: Arc<Mutex<Option<SysMemory>>>,
    pressure: Arc<Mutex<Option<MemoryPressure>>>,
    busy: Arc<AtomicBool>,
}

impl MemoryWatch {
    /// Take a new reading: inline where it is a few system calls (macOS, Linux, the web), on a
    /// background thread on Windows (PowerShell).
    pub fn poll(&self) {
        if self.busy.swap(true, Ordering::AcqRel) {
            return;
        }
        let (latest, pressure, busy) = (self.latest.clone(), self.pressure.clone(), self.busy.clone());
        let read = move || {
            let m = memory();
            if let Some(m) = m {
                *latest.lock().unwrap_or_else(PoisonError::into_inner) = Some(m);
            }
            *pressure.lock().unwrap_or_else(PoisonError::into_inner) = m.and_then(|m| m.pressure).or_else(memory_pressure);
            busy.store(false, Ordering::Release);
        };
        #[cfg(target_os = "windows")]
        if std::thread::Builder::new().name("ec-memory".into()).spawn(read).is_err() {
            self.busy.store(false, Ordering::Release);
        }
        #[cfg(not(target_os = "windows"))]
        read();
    }

    /// The last reading (`None` until one finished).
    pub fn latest(&self) -> Option<SysMemory> {
        *self.latest.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The memory pressure at the last reading (`None`: unknown, or the system has no such
    /// level).
    pub fn pressure(&self) -> Option<MemoryPressure> {
        *self.pressure.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// This process's resident memory and the system's memory pressure, for the status bar's
/// memory chip and the Memory panel. [`ProcessWatch::poll`] takes a reading (the chip asks at
/// most every 2 s); [`ProcessWatch::resident`] is an atomic load. On macOS a reading is two
/// kernel calls (`task_info` through `memory-stats`, a `sysctl`), on Linux it reads
/// `/proc/self/status`, both inline; FreeBSD still runs `ps` on a background thread; elsewhere
/// it is unknown and the chip shows the bytes the caches track instead.
#[derive(Clone, Default)]
pub struct ProcessWatch {
    /// Resident bytes at the last reading (0 = unknown).
    resident: Arc<AtomicU64>,
    /// [`MemoryPressure`] at the last reading: 0 unknown, 1 normal, 2 warning, 3 critical.
    pressure: Arc<std::sync::atomic::AtomicU8>,
    busy: Arc<AtomicBool>,
}

impl ProcessWatch {
    /// Take a reading: inline, except where it starts a process (FreeBSD's `ps`).
    pub fn poll(&self) {
        if self.busy.swap(true, Ordering::AcqRel) {
            return;
        }
        let (resident, pressure, busy) = (self.resident.clone(), self.pressure.clone(), self.busy.clone());
        let read = move || {
            resident.store(process_resident().unwrap_or(0), Ordering::Relaxed);
            let level = match memory_pressure() {
                None => 0,
                Some(MemoryPressure::Normal) => 1,
                Some(MemoryPressure::Warning) => 2,
                Some(MemoryPressure::Critical) => 3,
            };
            pressure.store(level, Ordering::Relaxed);
            busy.store(false, Ordering::Release);
        };
        #[cfg(target_os = "freebsd")]
        if std::thread::Builder::new().name("ec-process-memory".into()).spawn(read).is_err() {
            self.busy.store(false, Ordering::Release);
        }
        #[cfg(not(target_os = "freebsd"))]
        read();
    }

    /// The process's resident bytes at the last reading (`None`: unknown, or none yet).
    pub fn resident(&self) -> Option<u64> {
        Some(self.resident.load(Ordering::Relaxed)).filter(|b| *b > 0)
    }

    /// The memory pressure at the last reading (`None`: unknown).
    pub fn pressure(&self) -> Option<MemoryPressure> {
        match self.pressure.load(Ordering::Relaxed) {
            1 => Some(MemoryPressure::Normal),
            2 => Some(MemoryPressure::Warning),
            3 => Some(MemoryPressure::Critical),
            _ => None,
        }
    }
}

/// Parse the resident set (`VmRSS:`) of Linux `/proc/<pid>/status`, in bytes.
pub fn parse_proc_status_rss(text: &str) -> Option<u64> {
    let line = text.lines().find(|l| l.starts_with("VmRSS:"))?;
    let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    kb.checked_mul(1024)
}

/// Parse `ps -o rss=` output (kilobytes), in bytes.
pub fn parse_ps_rss(text: &str) -> Option<u64> {
    text.trim().parse::<u64>().ok()?.checked_mul(1024).filter(|b| *b > 0)
}

/// This process's resident memory in bytes, when the system reports it. (macOS's Activity
/// Monitor shows the physical footprint, which also counts compressed memory; the resident size
/// `task_info` reports stands in: it is lower while the system compresses.)
pub fn process_resident() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        return parse_proc_status_rss(&std::fs::read_to_string("/proc/self/status").ok()?);
    }
    #[cfg(target_os = "macos")]
    {
        return memory_stats::memory_stats().and_then(|m| u64::try_from(m.physical_mem).ok()).filter(|b| *b > 0);
    }
    #[cfg(target_os = "freebsd")]
    {
        let pid = std::process::id().to_string();
        return parse_ps_rss(&run("ps", &["-o", "rss=", "-p", &pid])?);
    }
    #[allow(unreachable_code)]
    None
}

#[cfg(not(target_arch = "wasm32"))]
#[allow(dead_code)]
fn run(cmd: &str, args: &[&str]) -> Option<String> {
    let mut c = crate::system_tool::command(cmd);
    c.args(args);
    // A console program started from the windowed app would flash a console window.
    #[cfg(target_os = "windows")]
    std::os::windows::process::CommandExt::creation_flags(&mut c, 0x0800_0000); // CREATE_NO_WINDOW
    let out = c.output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).to_string())
}

/// Parse `/proc/meminfo` (kB values).
pub fn parse_meminfo(text: &str) -> Option<SysMemory> {
    let field = |name: &str| -> Option<u64> {
        let line = text.lines().find(|l| l.starts_with(name))?;
        let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
        Some(kb * 1024)
    };
    let total = field("MemTotal:")?;
    let available = field("MemAvailable:").or_else(|| Some(field("MemFree:")? + field("Cached:").unwrap_or(0)))?;
    Some(SysMemory { total, available, pressure: None })
}

/// Parse `sysctl -n hw.physmem hw.pagesize vm.stats.vm.v_free_count vm.stats.vm.v_inactive_count`
/// (FreeBSD: one number per line, in that order).
pub fn parse_freebsd_sysctl(text: &str) -> Option<SysMemory> {
    let mut it = text.split_whitespace().map(|v| v.parse::<u64>().ok());
    let total = it.next()??;
    let page = it.next()??;
    let free = it.next()??.saturating_add(it.next()??);
    Some(SysMemory { total, available: free.saturating_mul(page).min(total), pressure: None })
}

/// Total and available physical memory, when the system reports them.
pub fn memory() -> Option<SysMemory> {
    #[cfg(target_os = "linux")]
    {
        return parse_meminfo(&std::fs::read_to_string("/proc/meminfo").ok()?);
    }
    #[cfg(target_os = "macos")]
    {
        return macos::memory();
    }
    #[cfg(target_os = "freebsd")]
    {
        return parse_freebsd_sysctl(&run("sysctl", &["-n", "hw.physmem", "hw.pagesize", "vm.stats.vm.v_free_count", "vm.stats.vm.v_inactive_count"])?);
    }
    #[cfg(target_os = "windows")]
    {
        let text = run(
            "powershell",
            &["-NoProfile", "-Command", "$o=Get-CimInstance Win32_OperatingSystem; \"$($o.TotalVisibleMemorySize) $($o.FreePhysicalMemory)\""],
        )?;
        let mut it = text.split_whitespace().filter_map(|v| v.parse::<u64>().ok());
        return Some(SysMemory { total: it.next()? * 1024, available: it.next()? * 1024, pressure: None });
    }
    #[allow(unreachable_code)]
    None
}

/// CPU model name.
pub fn cpu_name() -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        let t = std::fs::read_to_string("/proc/cpuinfo").ok()?;
        return t.lines().find(|l| l.starts_with("model name")).and_then(|l| l.split(':').nth(1)).map(|s| s.trim().to_string());
    }
    #[cfg(target_os = "macos")]
    {
        return run("sysctl", &["-n", "machdep.cpu.brand_string"]).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    }
    #[cfg(target_os = "freebsd")]
    {
        return run("sysctl", &["-n", "hw.model"]).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    }
    #[cfg(target_os = "windows")]
    {
        return std::env::var("PROCESSOR_IDENTIFIER").ok();
    }
    #[allow(unreachable_code)]
    None
}

/// Operating system name and version.
pub fn os_version() -> String {
    #[cfg(target_os = "macos")]
    if let Some(v) = run("sw_vers", &["-productVersion"]) {
        return format!("macOS {}", v.trim());
    }
    #[cfg(target_os = "linux")]
    if let Ok(t) = std::fs::read_to_string("/etc/os-release")
        && let Some(n) = t.lines().find_map(|l| l.strip_prefix("PRETTY_NAME="))
    {
        return n.trim_matches('"').to_string();
    }
    #[cfg(target_os = "freebsd")]
    if let Some(v) = run("freebsd-version", &[]) {
        return format!("FreeBSD {}", v.trim());
    }
    #[cfg(target_os = "windows")]
    if let Some(v) = run("cmd", &["/C", "ver"]) {
        return v.trim().to_string();
    }
    format!("{} ({})", std::env::consts::OS, std::env::consts::ARCH)
}

/// Logical CPU cores.
pub fn cpu_cores() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)
}

/// The graphics lines of Settings ▸ GPU Information and Help ▸ System Report: whether the GPU
/// compositor runs (and on which adapter), else why compositing is on the CPU, and the adapter
/// the window itself got. Without this a card the compositor couldn't use looked undetected (#596).
pub fn gpu_summary(s: &crate::Session) -> String {
    let mut text = match s.accel.as_ref().map(|a| a.name()) {
        Some(name) => format!("Compositing: GPU, {name}.\n"),
        None => {
            let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
            let why = s.accel_note.as_deref().unwrap_or("no reason was recorded");
            format!("Compositing: CPU, {threads} threads (pure Rust, rayon).\nGPU compositing is off: {why}\n")
        }
    };
    match &s.window_adapter {
        Some(adapter) => text.push_str(&format!("Window graphics: {adapter}\n")),
        None => text.push_str("Window graphics: not known yet (the window has not drawn its first frame).\n"),
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn freebsd_sysctl_memory_parses() {
        let m = parse_freebsd_sysctl("17179869184\n4096\n1000000\n500000\n").expect("parses");
        assert_eq!(m.total, 17_179_869_184);
        assert_eq!(m.available, 1_500_000 * 4096);
        assert!(parse_freebsd_sysctl("17179869184\n4096\n").is_none());
        assert!(parse_freebsd_sysctl("garbage").is_none());
        // Available never exceeds total, even with nonsense counts.
        let m = parse_freebsd_sysctl("1000\n4096\n999999999\n999999999").expect("parses");
        assert_eq!(m.available, 1000);
    }

    #[test]
    fn parses_proc_meminfo_and_vm_stat() {
        let m = parse_meminfo("MemTotal:       16000000 kB\nMemFree:  1000 kB\nMemAvailable:    8000000 kB\n").unwrap();
        assert_eq!(m, SysMemory { total: 16_000_000 * 1024, available: 8_000_000 * 1024, pressure: None });
        assert!(!m.is_low());
    }

    /// MEM3: the Mac of 22 Oct 2026 (64 GB, 210 MB free, 9 GB swapped) read as having memory to
    /// spare because inactive pages counted as free. Only free, speculative, purgeable and
    /// file-backed pages count now, capped by the kernel's own estimate.
    #[test]
    fn macos_available_memory_leaves_out_other_apps_inactive_pages() {
        let total = 64u64 << 30;
        let page = 16384;
        // 210 MB free, 1 GB of file cache; the 20 GB of inactive anonymous pages don't count.
        let swapping = MacPages { page, free: (210 << 20) / page, speculative: 0, purgeable: 0, file_backed: (1 << 30) / page };
        let m = macos_memory(total, swapping, Some(40), Some(MemoryPressure::Warning));
        assert_eq!(m.available, (210 << 20) + (1 << 30));
        assert!(m.is_low());
        assert_eq!(m.cache_divisor(), 2);
        // The live reading of the same Mac after a reboot: 3.2 GB free, 1 GB speculative, 10 GB
        // file-backed: about 15 GB, not low; the kernel estimate (81%) is higher and doesn't cap.
        let calm = MacPages { page, free: 204_599, speculative: 60_968, purgeable: 16_297, file_backed: 625_637 };
        let m = macos_memory(total, calm, Some(81), Some(MemoryPressure::Normal));
        assert_eq!(m.available, (204_599 + 60_968 + 16_297 + 625_637) * page);
        assert!(!m.is_low());
        assert_eq!(m.cache_divisor(), 1);
        // The kernel's estimate caps the page sum; never more than the machine has.
        assert_eq!(macos_memory(total, calm, Some(10), None).available, total / 100 * 10);
        assert_eq!(macos_memory(total, MacPages { page, free: u64::MAX, ..calm }, None, None).available, total);
        // Critical pressure shrinks the caches to a quarter even with memory "available".
        assert_eq!(macos_memory(total, calm, Some(81), Some(MemoryPressure::Critical)).cache_divisor(), 4);
    }

    /// On this Mac the kernel calls return real numbers without starting a process.
    #[cfg(target_os = "macos")]
    #[test]
    fn macos_reads_memory_and_resident_size_from_the_kernel() {
        let m = memory().expect("hw.memsize and the page counts");
        assert!(m.total >= 1 << 30 && m.available <= m.total);
        assert!(m.pressure.is_some(), "kern.memorystatus_vm_pressure_level");
        assert!(process_resident().is_some_and(|b| b > 1 << 20));
        let w = ProcessWatch::default();
        w.poll();
        assert!(w.resident().is_some(), "an inline reading");
    }

    #[test]
    fn parses_process_resident_size() {
        assert_eq!(parse_proc_status_rss("Name:\tx\nVmRSS:\t  2048 kB\nVmSwap: 0 kB\n"), Some(2048 * 1024));
        assert_eq!(parse_proc_status_rss("Name: x\n"), None);
        assert_eq!(parse_ps_rss(" 8059600\n"), Some(8_059_600 * 1024));
        assert_eq!(parse_ps_rss(""), None);
        assert_eq!(parse_ps_rss("0"), None);
        let w = ProcessWatch::default();
        assert_eq!(w.resident(), None);
        assert_eq!(w.pressure(), None);
    }

    #[test]
    fn parses_macos_memory_pressure_level() {
        assert_eq!(parse_pressure_level("1\n"), Some(MemoryPressure::Normal));
        assert_eq!(parse_pressure_level("2"), Some(MemoryPressure::Warning));
        assert_eq!(parse_pressure_level(" 4 "), Some(MemoryPressure::Critical));
        assert_eq!(parse_pressure_level("0"), None);
        assert_eq!(parse_pressure_level("garbage"), None);
        assert!(MemoryPressure::Warning > MemoryPressure::Normal && MemoryPressure::Critical > MemoryPressure::Warning);
    }
}
