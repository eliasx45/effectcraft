//! Compressed viewer frames: 8-bit premultiplied RGBA cut into horizontal strips, each strip LZ4
//! compressed on its own (`lz4_flex`, MIT), so a frame decodes on several threads at once.
//!
//! The RAM preview keeps frames this way once they leave the playhead's neighbourhood
//! (`plan/memory/compressed-cache.md`): lossless, 2.37× smaller on real 1080p frames, 4.1 ms to
//! decode on one core and 1.7 ms as 16 strips in parallel (`plan/memory/cache-prototypes.md`).
//! The bytes of a [`PackedFrame`] are exactly the disk cache's frame payload
//! ([`crate::disk_cache`]), so a frame is compressed once and the same bytes go to RAM and disk.
//!
//! Payload layout (little endian):
//!
//! | bytes | what |
//! |---|---|
//! | 4 | width |
//! | 4 | height |
//! | 4 | `0`: marks the strip layout (the older single-block payload has the raw size here, never 0) |
//! | 4 | strip count `n` |
//! | 8 × n | per strip: raw bytes, compressed bytes |
//! | … | the compressed strips, top to bottom |
//!
//! Strip `i` holds rows `h·i/n … h·(i+1)/n`. Everything read from a payload is checked before
//! anything is allocated or decompressed: a damaged payload is `None`, never a panic.

/// Strips a frame is cut into (fewer for frames under 16 rows).
pub const STRIPS: usize = 16;

/// The most strips a payload may declare.
const MAX_STRIPS: usize = 1024;

/// Bytes before the strip table.
const HEADER: usize = 16;

/// Largest frame (raw bytes) a payload may describe: 16K × 16K RGBA is 1 GiB.
pub const MAX_RAW_BYTES: usize = 1 << 30;

/// One strip: where its pixels go in the raw frame and where its compressed bytes are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Strip {
    raw_at: usize,
    raw_len: usize,
    at: usize,
    len: usize,
}

/// A compressed frame (see the module docs).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackedFrame {
    width: u32,
    height: u32,
    payload: Vec<u8>,
    strips: Vec<Strip>,
}

fn u32_at(p: &[u8], at: usize) -> Option<u32> {
    let b: [u8; 4] = p.get(at..at.checked_add(4)?)?.try_into().ok()?;
    Some(u32::from_le_bytes(b))
}

/// Raw bytes of a `width` × `height` RGBA8 frame (`None` when empty or over [`MAX_RAW_BYTES`]).
pub fn raw_len(width: u32, height: u32) -> Option<usize> {
    let n = usize::try_from(width).ok()?.checked_mul(usize::try_from(height).ok()?)?.checked_mul(4)?;
    (n > 0 && n <= MAX_RAW_BYTES).then_some(n)
}

/// Threads of [`decode_pool`]: with the calling thread, the viewer's four.
#[cfg(not(target_arch = "wasm32"))]
const DECODE_POOL_THREADS: usize = 3;

/// The threads a frame's strips decode on besides the caller's, made at the first parallel
/// decode and kept (`None` if the system refused them).
#[cfg(not(target_arch = "wasm32"))]
fn decode_pool() -> Option<&'static rayon::ThreadPool> {
    static POOL: std::sync::OnceLock<Option<rayon::ThreadPool>> = std::sync::OnceLock::new();
    POOL.get_or_init(|| rayon::ThreadPoolBuilder::new().num_threads(DECODE_POOL_THREADS).thread_name(|i| format!("ec-frame-unpack-{i}")).build().ok()).as_ref()
}

/// Decompress strips into their slices of the frame: whether every one decoded whole.
fn decode_group(group: Vec<(&[u8], &mut [u8])>) -> bool {
    group.into_iter().all(|(src, dst)| lz4_flex::block::decompress_into(src, dst).is_ok_and(|n| n == dst.len()))
}

/// Whether a frame payload has the strip layout (else it is the older single LZ4 block).
pub fn is_packed_payload(p: &[u8]) -> bool {
    p.len() >= HEADER && u32_at(p, 8) == Some(0)
}

impl PackedFrame {
    /// Compress a frame (`rgba`: `width * height * 4` bytes of premultiplied RGBA8). `None` when
    /// the size doesn't match the pixels (or is empty).
    pub fn encode(width: u32, height: u32, rgba: &[u8]) -> Option<PackedFrame> {
        let total = raw_len(width, height)?;
        if rgba.len() != total {
            return None;
        }
        let rows = usize::try_from(height).ok()?;
        let row_bytes = total / rows;
        let n = STRIPS.min(rows).max(1);
        let mut parts: Vec<(usize, Vec<u8>)> = Vec::new();
        parts.try_reserve_exact(n).ok()?;
        for i in 0..n {
            let (r0, r1) = (rows.checked_mul(i)? / n, rows.checked_mul(i + 1)? / n);
            let (a, b) = (r0.checked_mul(row_bytes)?, r1.checked_mul(row_bytes)?);
            let raw = rgba.get(a..b)?;
            parts.push((raw.len(), lz4_flex::block::compress(raw)));
        }
        let table = HEADER.checked_add(n.checked_mul(8)?)?;
        let body: usize = parts.iter().map(|(_, c)| c.len()).sum();
        let mut payload = Vec::new();
        payload.try_reserve_exact(table.checked_add(body)?).ok()?;
        payload.extend_from_slice(&width.to_le_bytes());
        payload.extend_from_slice(&height.to_le_bytes());
        payload.extend_from_slice(&0u32.to_le_bytes());
        payload.extend_from_slice(&u32::try_from(n).ok()?.to_le_bytes());
        for (raw, c) in &parts {
            payload.extend_from_slice(&u32::try_from(*raw).ok()?.to_le_bytes());
            payload.extend_from_slice(&u32::try_from(c.len()).ok()?.to_le_bytes());
        }
        for (_, c) in &parts {
            payload.extend_from_slice(c);
        }
        PackedFrame::from_payload(payload)
    }

    /// A frame from its payload (as [`PackedFrame::payload`] gives it, or read from disk);
    /// `None` when the payload is damaged or not in the strip layout.
    pub fn from_payload(payload: Vec<u8>) -> Option<PackedFrame> {
        if !is_packed_payload(&payload) {
            return None;
        }
        let (width, height) = (u32_at(&payload, 0)?, u32_at(&payload, 4)?);
        let total = raw_len(width, height)?;
        let n = usize::try_from(u32_at(&payload, 12)?).ok()?;
        if n == 0 || n > MAX_STRIPS {
            return None;
        }
        let table = HEADER.checked_add(n.checked_mul(8)?)?;
        if payload.len() < table {
            return None;
        }
        let mut strips = Vec::new();
        strips.try_reserve_exact(n).ok()?;
        let (mut raw_at, mut at) = (0usize, table);
        for i in 0..n {
            let e = HEADER.checked_add(i.checked_mul(8)?)?;
            let raw = usize::try_from(u32_at(&payload, e)?).ok()?;
            let len = usize::try_from(u32_at(&payload, e.checked_add(4)?)?).ok()?;
            strips.push(Strip { raw_at, raw_len: raw, at, len });
            raw_at = raw_at.checked_add(raw)?;
            at = at.checked_add(len)?;
        }
        (raw_at == total && at == payload.len()).then_some(PackedFrame { width, height, payload, strips })
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    /// The compressed payload (the disk cache's frame payload).
    pub fn payload(&self) -> &[u8] {
        &self.payload
    }

    /// The payload, given up.
    pub fn into_payload(self) -> Vec<u8> {
        self.payload
    }

    /// Bytes this frame takes in memory (what a budget counts).
    pub fn bytes(&self) -> usize {
        self.payload.len().saturating_add(self.strips.len().saturating_mul(std::mem::size_of::<Strip>()))
    }

    /// Bytes of the frame uncompressed (`width * height * 4`).
    pub fn raw_len(&self) -> usize {
        self.strips.iter().map(|s| s.raw_len).fold(0, usize::saturating_add)
    }

    /// Strips in the payload.
    pub fn strips(&self) -> usize {
        self.strips.len()
    }

    /// Decompress into `out` (exactly [`PackedFrame::raw_len`] bytes), on up to `threads`
    /// threads (the caller's included). `false` when `out` has the wrong size or a strip is
    /// damaged (`out` is then partly written).
    pub fn decode_into(&self, out: &mut [u8], threads: usize) -> bool {
        if out.len() != self.raw_len() {
            return false;
        }
        // Each strip with its own slice of the output.
        let mut jobs: Vec<(&[u8], &mut [u8])> = Vec::with_capacity(self.strips.len());
        let mut rest: &mut [u8] = out;
        for s in &self.strips {
            if rest.len() < s.raw_len {
                return false;
            }
            let (dst, tail) = std::mem::take(&mut rest).split_at_mut(s.raw_len);
            rest = tail;
            let Some(src) = s.at.checked_add(s.len).and_then(|end| self.payload.get(s.at..end)) else { return false };
            jobs.push((src, dst));
        }
        let threads = threads.clamp(1, jobs.len().max(1));
        if threads == 1 {
            return decode_group(jobs);
        }
        // Round-robin groups: every thread gets strips from the whole frame.
        let mut groups: Vec<Vec<(&[u8], &mut [u8])>> = (0..threads).map(|_| Vec::new()).collect();
        for (i, j) in jobs.into_iter().enumerate() {
            if let Some(g) = groups.get_mut(i % threads) {
                g.push(j);
            }
        }
        let mut groups = groups.into_iter();
        let mine = groups.next().unwrap_or_default();
        // The other groups go to the decode pool, made once and kept (MEM3: a new thread per
        // group per frame used to be started); without it, this thread decodes them all.
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(pool) = decode_pool() {
            let failed = std::sync::atomic::AtomicBool::new(false);
            let failed_ref = &failed;
            let ok = pool.in_place_scope(|s| {
                for g in groups {
                    s.spawn(move |_| {
                        if !decode_group(g) {
                            failed_ref.store(true, std::sync::atomic::Ordering::Relaxed);
                        }
                    });
                }
                decode_group(mine)
            });
            return ok && !failed.load(std::sync::atomic::Ordering::Relaxed);
        }
        groups.fold(decode_group(mine), |ok, g| decode_group(g) && ok)
    }

    /// The frame's RGBA8 pixels (`None` when damaged or memory runs out).
    pub fn decode(&self, threads: usize) -> Option<Vec<u8>> {
        let n = self.raw_len();
        let mut out = Vec::new();
        out.try_reserve_exact(n).ok()?;
        out.resize(n, 0);
        self.decode_into(&mut out, threads).then_some(out)
    }
}
