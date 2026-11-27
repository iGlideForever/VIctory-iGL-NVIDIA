//! Buffers the CPU maps, given out again instead of made anew
//! (`VICTORY_BUFFER_REUSE`, on by default; `=0`: each one is made, as before).
//!
//! Why: in five minutes of play on a 32 GB laptop (RTX 5080 Laptop, Core
//! Ultra 9 275HX) the layer made 56,000 buffers, 12,206 upload buffers of
//! 14 GB and 15,671 shared ones of 8 GB among them, and waited 24.2 s for
//! the system to make them resident, the longest wait 855 ms, with 0.6 GB
//! of the PC's memory free: a new buffer is new memory, zeroed and made
//! resident while the system pages other memory out for it. The game froze
//! for a moment again and again (fork diag log, 2026-11-27).
//!
//! How: every upload, shared and mapped card buffer of [`LARGEST`] bytes or
//! less is kept with a reference of ours beside the one given out. Once a
//! second ([`scan`]) those whose only reference left is ours are free:
//! nothing of ours names them any more, and nothing still running on the
//! card does (lists and retired objects hold theirs until their
//! submissions have run). A free buffer of the kind and size asked for is
//! given out again ([`take`]), zeroed as new memory is, once it has been
//! free for [`SETTLE_MS`]. Free buffers are let go after a few seconds, or
//! the oldest first past a total (half of it under memory pressure).
//!
//! `VICTORY_BUFFER_REUSE=<MB>,<ms>`: the most free bytes kept and how long a
//! free one is kept (256 MB and 3000 ms unless given).

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};

use windows::Win32::Graphics::Direct3D12::ID3D12Resource;
use windows::core::Interface;

/// What a buffer is: with its size, the key it is kept under.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum Kind {
    /// [`crate::host::Host::upload_buffer`].
    Upload,
    /// [`crate::host::Host::shared_buffer`].
    Shared,
    /// [`crate::host::Host::mapped_card_buffer`], without and with unordered access.
    CardMapped,
    CardMappedWritten,
}

/// Bigger buffers are made and let go as before.
const LARGEST: u64 = 64 << 20;
/// How long a buffer stays free before it is given out again.
const SETTLE_MS: u64 = 250;

struct Kept {
    buffer: ID3D12Resource,
    data: usize,
    /// When it was found free (0: given out).
    since: u64,
}
// SAFETY: D3D12 resources are free-threaded; `data` is only an address.
unsafe impl Send for Kept {}

#[derive(Default)]
struct Pool {
    out: HashMap<(Kind, u64), Vec<Kept>>,
    /// The buffers in `out` by their raw pointer: those our reference is
    /// beside another's ([`references_outside`]).
    held: std::collections::HashSet<usize>,
    free: HashMap<(Kind, u64), VecDeque<Kept>>,
    free_bytes: u64,
    free_count: u64,
}

static POOL: Mutex<Option<Pool>> = Mutex::new(None);

/// Since the start: given out again (count, bytes); made new of the kinds
/// kept (count, bytes); let go from the free ones (count, bytes).
static COUNTS: [AtomicU64; 6] = [const { AtomicU64::new(0) }; 6];

/// The most free bytes kept and for how long; `None`: off.
fn settings() -> Option<(u64, u64)> {
    static SET: OnceLock<Option<(u64, u64)>> = OnceLock::new();
    *SET.get_or_init(|| parse(std::env::var("VICTORY_BUFFER_REUSE").ok().as_deref()))
}

fn parse(set: Option<&str>) -> Option<(u64, u64)> {
    const DEFAULT: (u64, u64) = (256 << 20, 3000);
    let Some(set) = set.map(str::trim).filter(|s| !s.is_empty()) else {
        return Some(DEFAULT);
    };
    if set == "0" {
        return None;
    }
    let mut parts = set.split(',').map(|p| p.trim().parse::<u64>().ok());
    let mb = parts.next().flatten().filter(|&mb| mb > 0).map_or(DEFAULT.0, |mb| mb.min(4096) << 20);
    let ms = parts.next().flatten().filter(|&ms| ms > 0).map_or(DEFAULT.1, |ms| ms.min(60_000));
    Some((mb, ms))
}

fn with<T>(f: impl FnOnce(&mut Pool) -> T) -> T {
    f(POOL.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(Pool::default))
}

/// The references `buffer` has.
fn references(buffer: &ID3D12Resource) -> u32 {
    let raw = buffer.as_raw();
    // SAFETY: a live COM object: one more reference and that one let go
    // again; what the release answers is the count left.
    unsafe {
        let vtable = *(raw as *const *const windows::core::IUnknown_Vtbl);
        ((*vtable).AddRef)(raw);
        ((*vtable).Release)(raw)
    }
}

/// A free buffer of `kind` and `size`, zeroed, and where the CPU maps it.
pub(crate) fn take(kind: Kind, size: u64) -> Option<(ID3D12Resource, *mut u8)> {
    if size > LARGEST || settings().is_none() {
        return None;
    }
    let now = crate::host::coarse_ms();
    let kept = with(|pool| {
        let free = pool.free.get_mut(&(kind, size))?;
        if free.front().is_none_or(|k| now < k.since + SETTLE_MS) {
            return None;
        }
        let kept = free.pop_front()?;
        pool.free_bytes -= size;
        pool.free_count -= 1;
        Some(kept)
    })?;
    // SAFETY: the buffer's whole mapping, which nothing else names now.
    unsafe { std::ptr::write_bytes(kept.data as *mut u8, 0, size as usize) };
    let given = (kept.buffer.clone(), kept.data as *mut u8);
    with(|pool| {
        pool.held.insert(kept.buffer.as_raw() as usize);
        pool.out.entry((kind, size)).or_default().push(Kept { since: 0, ..kept });
    });
    COUNTS[0].fetch_add(1, Relaxed);
    COUNTS[1].fetch_add(size, Relaxed);
    Some(given)
}

/// Keeps `buffer`, just made for `kind` and `size` and mapped at `data`, to
/// be given out again once it is free.
pub(crate) fn made(kind: Kind, size: u64, buffer: &ID3D12Resource, data: *mut u8) {
    if size > LARGEST || settings().is_none() {
        return;
    }
    COUNTS[2].fetch_add(1, Relaxed);
    COUNTS[3].fetch_add(size, Relaxed);
    with(|pool| {
        pool.held.insert(buffer.as_raw() as usize);
        pool.out.entry((kind, size)).or_default().push(Kept { buffer: buffer.clone(), data: data as usize, since: 0 })
    });
}

/// The references `buffer` has, ours left out if the pool keeps it beside
/// the one it gave out. What lets go of a host copy when nothing else names
/// its buffer (`crate::shadow::let_go_idle`: "only the copy holds its host
/// buffer") counts with this: with the plain count every pooled buffer had
/// one reference more, and no window of title memory was let go any more
/// (each 4 MiB window is a pooled shared buffer; 1,127 windows, 5.0 GB, 8
/// minutes into run 135521, RTX 5080 Laptop, 2026-11-27).
pub(crate) fn references_outside(buffer: &ID3D12Resource) -> u32 {
    let raw = buffer.as_raw() as usize;
    // (Asked first: the pool moves a buffer out of `out` only once its own
    // reference is the last, and the caller holds one.)
    let ours = u32::from(with(|pool| pool.held.contains(&raw)));
    references(buffer).saturating_sub(ours)
}

/// Once a second: buffers whose only reference left is ours become free;
/// free ones past their time, or past the total, are let go.
pub(crate) fn scan() {
    let Some((most, keep_ms)) = settings() else { return };
    let most = if crate::residency::pressured() { most / 2 } else { most };
    let now = crate::host::coarse_ms().max(1);
    let mut gone: Vec<(Kept, u64)> = Vec::new();
    with(|pool| {
        let Pool { out, held: raws, free, free_bytes, free_count } = pool;
        for (key, held) in out.iter_mut() {
            let mut i = 0;
            while i < held.len() {
                if references(&held[i].buffer) == 1 {
                    let mut kept = held.swap_remove(i);
                    raws.remove(&(kept.buffer.as_raw() as usize));
                    kept.since = now;
                    *free_bytes += key.1;
                    *free_count += 1;
                    free.entry(*key).or_default().push_back(kept);
                } else {
                    i += 1;
                }
            }
        }
        out.retain(|_, held| !held.is_empty());
        for (key, list) in free.iter_mut() {
            while list.front().is_some_and(|k| now >= k.since + keep_ms) {
                gone.extend(list.pop_front().map(|k| (k, key.1)));
            }
        }
        let mut left: u64 = free.values().flatten().count() as u64;
        let mut bytes: u64 = free.iter().map(|(key, list)| key.1 * list.len() as u64).sum();
        if bytes > most {
            let mut all: Vec<(u64, (Kind, u64))> =
                free.iter().flat_map(|(key, list)| list.iter().map(move |k| (k.since, *key))).collect();
            all.sort_unstable_by_key(|a| a.0);
            for (_, key) in all {
                if bytes <= most {
                    break;
                }
                if let Some(k) = free.get_mut(&key).and_then(VecDeque::pop_front) {
                    bytes -= key.1;
                    left -= 1;
                    gone.push((k, key.1));
                }
            }
        }
        free.retain(|_, list| !list.is_empty());
        (*free_bytes, *free_count) = (bytes, left);
    });
    COUNTS[4].fetch_add(gone.len() as u64, Relaxed);
    COUNTS[5].fetch_add(gone.iter().map(|g| g.1).sum::<u64>(), Relaxed);
    // Let go outside the lock.
    drop(gone);
    log(now);
}

/// Every ten seconds: what was given out again and made new since the last line.
fn log(now: u64) {
    static LAST: Mutex<(u64, [u64; 6])> = Mutex::new((0, [0; 6]));
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    if now < last.0 + 10_000 {
        return;
    }
    let counts: [u64; 6] = std::array::from_fn(|i| COUNTS[i].load(Relaxed));
    let d: [u64; 6] = std::array::from_fn(|i| counts[i] - last.1[i]);
    let (free_bytes, free_count, out) =
        with(|pool| (pool.free_bytes, pool.free_count, pool.out.values().map(Vec::len).sum::<usize>()));
    let mb = |b: u64| b as f64 / (1 << 20) as f64;
    let ahead = crate::ahead::counts();
    tracing::info!(
        "buffers given out again (VICTORY_BUFFER_REUSE), last 10 s: {} given out again ({:.0} MB), {} made new \
         ({:.0} MB) of the kinds kept, {} free ones let go ({:.0} MB); now {} free kept ({:.0} MB), {} given out \
         and watched; since the start {} given out again ({:.0} MB), {} made new ({:.0} MB); made ahead \
         (VICTORY_BUFFERS_AHEAD) and taken since the start: {} the CPU maps, {} on the card; in store now {} and {}",
        d[0],
        mb(d[1]),
        d[2],
        mb(d[3]),
        d[4],
        mb(d[5]),
        free_count,
        mb(free_bytes),
        out,
        counts[0],
        mb(counts[1]),
        counts[2],
        mb(counts[3]),
        ahead[0],
        ahead[1],
        ahead[2],
        ahead[3]
    );
    #[cfg(feature = "diagnostics")]
    log_makers();
    *last = (now, counts);
}

/// Buffers made new by the code that asked for them, since the start
/// (diagnostics): count, bytes.
#[cfg(feature = "diagnostics")]
static MAKERS: Mutex<Option<HashMap<(&'static str, u32, &'static str), (u64, u64)>>> = Mutex::new(None);

/// A buffer of `what` and `size` was made new for `caller`.
#[cfg(feature = "diagnostics")]
pub(crate) fn note(what: &'static str, size: u64, caller: &'static std::panic::Location<'static>) {
    let mut makers = MAKERS.lock().unwrap_or_else(|e| e.into_inner());
    let e = makers.get_or_insert_with(HashMap::new).entry((caller.file(), caller.line(), what)).or_default();
    e.0 += 1;
    e.1 += size;
}

#[cfg(feature = "diagnostics")]
fn log_makers() {
    let mut all: Vec<((&'static str, u32, &'static str), (u64, u64))> = MAKERS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|m| m.iter().map(|(k, v)| (*k, *v)).collect())
        .unwrap_or_default();
    all.sort_unstable_by_key(|a| std::cmp::Reverse(a.1.0));
    let text: Vec<String> = all
        .iter()
        .take(16)
        .map(|((file, line, what), (n, bytes))| {
            let file = file.rsplit(['/', '\\']).next().unwrap_or(file);
            format!("{what} for {file}:{line} {n} ({:.0} MB)", *bytes as f64 / (1 << 20) as f64)
        })
        .collect();
    tracing::info!("buffers made new by the code that asked, since the start, most first: {}", text.join("; "));
}

#[cfg(test)]
mod tests {
    use super::parse;

    #[test]
    fn the_switch_reads_a_total_and_a_time() {
        assert_eq!(parse(None), Some((256 << 20, 3000)));
        assert_eq!(parse(Some("")), Some((256 << 20, 3000)));
        assert_eq!(parse(Some("0")), None);
        assert_eq!(parse(Some("512")), Some((512 << 20, 3000)));
        assert_eq!(parse(Some("128,1500")), Some((128 << 20, 1500)));
        assert_eq!(parse(Some("x,y")), Some((256 << 20, 3000)));
    }
}
