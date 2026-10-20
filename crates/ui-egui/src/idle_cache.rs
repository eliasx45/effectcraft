//! Composition ▸ Preview ▸ Cache Frames When Idle: when it waits for memory.
//!
//! Its frames render on a few low-priority threads (`frames::idle`); this is the other half of
//! keeping it from slowing the machine down. While the system is short of memory it caches
//! nothing (and doesn't fill the disk cache either), and it picks up where it was when memory
//! frees up again. The desktop app reads the system's memory every few seconds while it is on
//! (`prefs_live`).

use effectcraft_engine::sysinfo::{MemoryPressure, SysMemory};

/// Why Cache Frames When Idle waits now, or `None` to go on: the system reports memory pressure
/// (macOS: Warning or Critical), it is low on memory ([`SysMemory::is_low`]), or less memory is
/// available than Memory & CPU ▸ RAM Reserved for Other Applications (`reserved_gb`). Unknown
/// readings never stop it.
pub(crate) fn memory_wait(mem: Option<SysMemory>, pressure: Option<MemoryPressure>, reserved_gb: u32) -> Option<&'static str> {
    if pressure.is_some_and(|p| p >= MemoryPressure::Warning) {
        return Some("the system is under memory pressure");
    }
    let mem = mem.filter(|m| m.total > 0)?;
    if mem.is_low() {
        return Some("the system is low on memory");
    }
    (mem.available < (reserved_gb as u64) << 30).then_some("less memory is free than RAM Reserved for Other Applications")
}

#[cfg(test)]
mod tests {
    use super::*;

    const GB: u64 = 1 << 30;

    #[test]
    fn idle_caching_waits_while_memory_is_short() {
        let mem = |available: u64| Some(SysMemory { total: 64 * GB, available });
        // Plenty free, normal pressure or none reported: go on.
        assert_eq!(memory_wait(mem(30 * GB), Some(MemoryPressure::Normal), 16), None);
        assert_eq!(memory_wait(mem(30 * GB), None, 16), None);
        // Less free than RAM Reserved for Other Applications.
        assert!(memory_wait(mem(15 * GB), None, 16).is_some());
        assert_eq!(memory_wait(mem(15 * GB), None, 6), None);
        // Low on memory whatever the setting (under 10 % or 1 GB free).
        assert!(memory_wait(mem(5 * GB), None, 0).is_some());
        // The system's memory pressure, even with memory apparently free.
        assert!(memory_wait(mem(30 * GB), Some(MemoryPressure::Warning), 16).is_some());
        assert!(memory_wait(mem(30 * GB), Some(MemoryPressure::Critical), 0).is_some());
        // Nothing known: go on.
        assert_eq!(memory_wait(None, None, 16), None);
        assert_eq!(memory_wait(Some(SysMemory { total: 0, available: 0 }), None, 16), None);
        // Pressure alone is enough when the totals are unknown.
        assert!(memory_wait(None, Some(MemoryPressure::Warning), 16).is_some());
    }
}
