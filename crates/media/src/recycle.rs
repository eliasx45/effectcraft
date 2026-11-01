//! Plane buffers of CPU copies made on demand, recycled (Fix 3, 1 Nov 2026).
//!
//! A frame kept in a hardware decoder's surface gets its CPU samples only when a CPU reader asks
//! ([`crate::convert::Lazy`]). Those copies used to land in fresh allocations, and writing a fresh
//! 4K ProRes 4444 + alpha picture (66 MB of planes) costs the kernel zero-filling every new page
//! as it is first written (Fix 1 measured a 4-thread memcpy into fresh pages at ~5 ms, ~2.2 ms
//! into pages already there). The planes of a copy that is dropped come back here, and the next
//! copy of the same size writes into them; with Decode Footage at 8 Bits the full-depth copy the
//! 8-bit one is made from comes back at once. Bounded ([`MAX_IDLE_BYTES`], [`MAX_IDLE`]); Purge
//! empties it ([`clear`]).

use std::sync::{Mutex, PoisonError};

/// Idle buffers held at most (bytes; the oldest go first).
pub(crate) const MAX_IDLE_BYTES: usize = 384 << 20;
/// Idle buffers held at most (count).
pub(crate) const MAX_IDLE: usize = 24;

struct Free<T> {
    bufs: Vec<Vec<T>>,
    bytes: usize,
}

static U16: Mutex<Free<u16>> = Mutex::new(Free { bufs: Vec::new(), bytes: 0 });
static U8: Mutex<Free<u8>> = Mutex::new(Free { bufs: Vec::new(), bytes: 0 });

fn bytes_of<T>(v: &Vec<T>) -> usize {
    v.capacity().saturating_mul(std::mem::size_of::<T>())
}

/// An idle buffer holding `n` samples (or up to a quarter more), `None` without one.
fn take<T>(free: &Mutex<Free<T>>, n: usize) -> Option<Vec<T>> {
    if n == 0 {
        return None;
    }
    let mut f = free.lock().unwrap_or_else(PoisonError::into_inner);
    let i = f.bufs.iter().position(|v| v.len() >= n && v.len() <= n.saturating_add(n / 4))?;
    let v = f.bufs.swap_remove(i);
    f.bytes = f.bytes.saturating_sub(bytes_of(&v));
    Some(v)
}

/// Keep `v` for a later copy (or drop it past the bounds).
fn give<T>(free: &Mutex<Free<T>>, v: Vec<T>) {
    let b = bytes_of(&v);
    if v.is_empty() || b > MAX_IDLE_BYTES {
        return;
    }
    let mut gone = Vec::new();
    {
        let mut f = free.lock().unwrap_or_else(PoisonError::into_inner);
        while !f.bufs.is_empty() && (f.bufs.len() >= MAX_IDLE || f.bytes.saturating_add(b) > MAX_IDLE_BYTES) {
            let old = f.bufs.remove(0);
            f.bytes = f.bytes.saturating_sub(bytes_of(&old));
            gone.push(old);
        }
        f.bytes = f.bytes.saturating_add(b);
        f.bufs.push(v);
    }
    // (freed outside the lock)
    drop(gone);
}

/// A `u16` plane buffer to copy `n` samples into: an idle one that fits, else empty (the copy
/// allocates).
pub(crate) fn take16(n: usize) -> Vec<u16> {
    take(&U16, n).unwrap_or_default()
}

/// An 8-bit plane of exactly `n` samples, every one of which the caller overwrites: an idle
/// buffer when one fits, else a new zeroed one.
pub(crate) fn take8(n: usize) -> Vec<u8> {
    match take(&U8, n) {
        Some(mut v) => {
            v.truncate(n);
            v
        }
        None => vec![0; n],
    }
}

pub(crate) fn give16(v: Vec<u16>) {
    give(&U16, v);
}

pub(crate) fn give8(v: Vec<u8>) {
    give(&U8, v);
}

/// The planes of a CPU copy nobody else holds, back for later copies.
pub(crate) fn frame(f: filmcraft_frame::VideoFrame) {
    use filmcraft_frame::PixelData;
    match f.data {
        PixelData::Yuv16 { planes, alpha, .. } => {
            for p in planes.into_iter().chain(alpha) {
                if let Ok(v) = std::sync::Arc::try_unwrap(p) {
                    give16(v);
                }
            }
        }
        PixelData::Yuv8 { planes, alpha, .. } => {
            for p in planes.into_iter().chain(alpha) {
                if let Ok(v) = std::sync::Arc::try_unwrap(p) {
                    give8(v);
                }
            }
        }
        _ => {}
    }
}

/// Bytes of idle buffers now.
pub(crate) fn idle_bytes() -> usize {
    let a = U16.lock().unwrap_or_else(PoisonError::into_inner).bytes;
    a.saturating_add(U8.lock().unwrap_or_else(PoisonError::into_inner).bytes)
}

/// Let every idle buffer go (Purge).
pub(crate) fn clear() {
    let a = std::mem::take(&mut U16.lock().unwrap_or_else(PoisonError::into_inner).bufs);
    let b = std::mem::take(&mut U8.lock().unwrap_or_else(PoisonError::into_inner).bufs);
    U16.lock().unwrap_or_else(PoisonError::into_inner).bytes = 0;
    U8.lock().unwrap_or_else(PoisonError::into_inner).bytes = 0;
    drop((a, b));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffers_come_back_when_they_fit_and_stay_bounded() {
        clear();
        give16(vec![7; 1000]);
        assert!(take16(2000).is_empty(), "too small: not given");
        assert!(take16(500).is_empty(), "far too large: not given");
        let v = take16(900);
        assert_eq!(v.len(), 1000, "a little larger fits (the copy truncates it)");
        assert!(take16(1000).is_empty(), "taken once");
        give8(vec![1; 64]);
        assert_eq!(take8(60), vec![1; 60], "truncated to the size asked");
        assert_eq!(take8(60), vec![0; 60], "none idle: a new zeroed one");
        for _ in 0..MAX_IDLE + 5 {
            give8(vec![0; 10]);
        }
        assert!(U8.lock().unwrap().bufs.len() <= MAX_IDLE);
        give16(vec![0; MAX_IDLE_BYTES]);
        assert!(idle_bytes() <= MAX_IDLE_BYTES, "a buffer past the bound is not kept");
        clear();
    }
}
