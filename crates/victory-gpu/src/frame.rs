//! Frame pacing: the title sets a frame interval and waits for each frame
//! to begin. Ours is a clock started by the first interval set, ticking
//! once per interval; the token is the frame number. Each event kind
//! (origin, vblank, flip) happens once per tick, so two waits for the same
//! kind never end on the same tick: the title polls the origin
//! with "token on time-out" (starting its next frame at once) and is paced
//! by a blocking wait for the flip before each present.
//!
//! shortcut: event offsets are ignored (every event is at the tick); tie
//! the clock to presentation if a title paces by them.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::object::{HRESULT, S_FALSE, S_OK, This};

/// What a wait returns when its time is up first.
const TIMEOUT: HRESULT = S_FALSE;
/// The frame-origin event: the one that starts frames and makes tokens.
const ORIGIN: u32 = 0;
/// Make the frame's token even if the wait times out (the title starts the
/// frame early).
const TOKEN_ON_TIMEOUT: u32 = 2;
const SIXTIETH: Duration = Duration::from_micros(16_667);

struct Clock {
    start: Instant,
    interval: Duration,
    /// The tick number at `start` (ticks go on across interval changes).
    first: u64,
}

static CLOCK: Mutex<Option<Clock>> = Mutex::new(None);
static FRAME: AtomicU64 = AtomicU64::new(0);

fn clock() -> std::sync::MutexGuard<'static, Option<Clock>> {
    CLOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Device method: a frame every `length` microseconds. The period is how
/// many intervals a frame may take to reach the display (pipeline depth),
/// not part of the rate.
pub(crate) extern "system" fn set_interval(
    _: This,
    _output: This,
    length: u32,
    _period: u32,
    _flags: u32,
) -> HRESULT {
    let interval = match length {
        0 => SIXTIETH,
        us => Duration::from_micros(u64::from(us)),
    };
    tracing::info!(?interval, "frame interval");
    // A new interval starts from now, after the last tick handed out.
    let first = LAST
        .iter()
        .map(|t| t.load(Ordering::Relaxed))
        .max()
        .unwrap_or(0);
    *clock() = Some(Clock {
        start: Instant::now(),
        interval,
        first,
    });
    S_OK
}

/// Device method: events are scheduled at every tick.
pub(crate) extern "system" fn schedule(
    _: This,
    _kind: u32,
    _offset: u32,
    _list: *const u8,
    _flags: u32,
) -> HRESULT {
    S_OK
}

/// Diagnostics: what the presented frames did against the display's own
/// refresh grid. A frame shown for two refreshes and the next for four is
/// judder the title cannot see: it paced by our clock, not by the screen.
#[cfg(feature = "diagnostics")]
pub(crate) fn presented(sync_qpc: i64, sync_refresh: u32) {
    use std::sync::Mutex;
    #[derive(Default)]
    struct Pace {
        since: Option<Instant>,
        presents: u32,
        mean: u64,
        min: u64,
        max: u64,
        shown: [u32; 6],
        refresh_mean: u64,
        refresh_min: u64,
        refresh_max: u64,
        refreshes: u32,
        last: Option<(Instant, i64, u32)>,
        buckets: [u32; 7],
        after_wake_sum: u64,
        after_wake_max: u64,
        after_wake_n: u64,
        skipped: u32,
        last_tick: Option<u64>,
    }
    static PACE: Mutex<Pace> = Mutex::new(Pace {
        since: None,
        presents: 0,
        mean: 0,
        min: u64::MAX,
        max: 0,
        shown: [0; 6],
        refresh_mean: 0,
        refresh_min: u64::MAX,
        refresh_max: 0,
        refreshes: 0,
        last: None,
        buckets: [0; 7],
        after_wake_sum: 0,
        after_wake_max: 0,
        after_wake_n: 0,
        skipped: 0,
        last_tick: None,
    });
    /// QPC ticks per microsecond, read once.
    fn qpc_per_us() -> i64 {
        use windows_sys::Win32::System::Performance::QueryPerformanceFrequency;
        static FREQ: std::sync::OnceLock<i64> = std::sync::OnceLock::new();
        *FREQ.get_or_init(|| {
            let mut f = 0i64;
            // SAFETY: a plain out-parameter.
            unsafe { QueryPerformanceFrequency(&mut f) };
            (f / 1_000_000).max(1)
        })
    }
    let now = Instant::now();
    let mut pace = PACE.lock().unwrap_or_else(|e| e.into_inner());
    let since = *pace.since.get_or_insert(now);
    let woke = *WAKE_AT.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((at, tick)) = woke {
        let after = now.duration_since(at).as_micros() as u64;
        if after < 5_000 {
            pace.after_wake_sum += after;
            pace.after_wake_max = pace.after_wake_max.max(after);
            pace.after_wake_n += 1;
            // A quarter of the new sample, so a single slow frame does not
            // move the lead far.
            let mean = AFTER.load(Ordering::Relaxed);
            AFTER.store((mean * 3 + after) / 4, Ordering::Relaxed);
        }
        if let Some(before) = pace.last_tick {
            let step = tick.saturating_sub(before);
            if step > 1 {
                pace.skipped += (step - 1) as u32;
            }
        }
        pace.last_tick = Some(tick);
    }
    if let Some((then, then_qpc, then_refresh)) = pace.last {
        let us = now.duration_since(then).as_micros() as u64;
        pace.mean += us;
        pace.min = pace.min.min(us);
        pace.max = pace.max.max(us);
        let bucket = (us / 1000).clamp(28, 34) as usize - 28;
        pace.buckets[bucket] += 1;
        let shown = sync_refresh.saturating_sub(then_refresh).clamp(1, 6) as usize;
        pace.shown[shown - 1] += 1;
        let refresh_us = ((sync_qpc - then_qpc) / qpc_per_us()).max(0) as u64;
        if refresh_us > 0 && shown >= 1 {
            let each = refresh_us / shown as u64;
            pace.refresh_mean += each;
            pace.refresh_min = pace.refresh_min.min(each);
            pace.refresh_max = pace.refresh_max.max(each);
            pace.refreshes += 1;
        }
    }
    pace.presents += 1;
    pace.last = Some((now, sync_qpc, sync_refresh));
    let span = now.duration_since(since);
    if pace.presents < 30 || span < Duration::from_secs(1) {
        return;
    }
    let n = u64::from(pace.presents - 1).max(1);
    let refreshes = u64::from(pace.refreshes).max(1);
    let late_n = LATE_N.swap(0, Ordering::Relaxed).max(1);
    let woke_n = pace.after_wake_n.max(1);
    tracing::info!(
        wake_late_us_mean = LATE_SUM.swap(0, Ordering::Relaxed) / late_n,
        after_wake_us_mean = pace.after_wake_sum / woke_n,
        after_wake_us_max = pace.after_wake_max,
        ticks_skipped = pace.skipped,
        wake_late_us_max = LATE_MAX.swap(0, Ordering::Relaxed),
        presents = pace.presents - 1,
        interval_ms = (pace.mean as f64 / n as f64) / 1000.0,
        interval_ms_min = pace.min as f64 / 1000.0,
        interval_ms_max = pace.max as f64 / 1000.0,
        shown_1 = pace.shown[0],
        shown_2 = pace.shown[1],
        shown_3 = pace.shown[2],
        shown_4 = pace.shown[3],
        shown_more = pace.shown[4] + pace.shown[5],
        refresh_us = pace.refresh_mean / refreshes,
        refresh_us_min = pace.refresh_min,
        refresh_us_max = pace.refresh_max,
        "pacing: what the presented frames did on the display"
    );
    let buckets = pace.buckets;
    *pace = Pace {
        since: Some(now),
        last: pace.last,
        ..Pace::default()
    };
    tracing::info!(
        ms28 = buckets[0],
        ms29 = buckets[1],
        ms30 = buckets[2],
        ms31 = buckets[3],
        ms32 = buckets[4],
        ms33 = buckets[5],
        ms34 = buckets[6],
        "pacing: interval histogram (buckets are 28..34+ ms)"
    );
}

/// Frames begun so far (frame waits that ended).
pub(crate) fn current() -> u64 {
    FRAME.load(Ordering::Relaxed)
}

/// Sleeps for `d`, accurately. `thread::sleep` rounds up to the system's
/// timer granularity: a 33.3 ms wait woke at 34.4 ms, every frame, so the
/// title's 33.333 ms cadence drifted against the display's refreshes and
/// frames fell out of step one every few seconds (the judder). A
/// high-resolution waitable timer holds the wait to within a millisecond
/// and a short spin covers the last stretch.
fn pause(d: Duration) {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Threading::{
            CreateWaitableTimerExW, SetWaitableTimer, WaitForSingleObject,
        };
        /// The last stretch is spun, not slept: the timer is not exact.
        const SPIN: Duration = Duration::from_micros(400);
        /// `CREATE_WAITABLE_TIMER_HIGH_RESOLUTION` (Windows 10 1803 and later).
        const HIGH_RESOLUTION: u32 = 0x0000_0002;
        /// `TIMER_ALL_ACCESS`.
        const ALL_ACCESS: u32 = 0x1F_0003;
        static TIMER: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
        let timer = *TIMER.get_or_init(|| {
            // SAFETY: plain object creation, no name, no security descriptor.
            let handle = unsafe {
                CreateWaitableTimerExW(std::ptr::null(), std::ptr::null(), HIGH_RESOLUTION, ALL_ACCESS)
            };
            handle as usize
        });
        let target = Instant::now() + d;
        // A negative due time is relative, in 100 ns units.
        if timer != 0 && d > SPIN {
            let due = -((d - SPIN).as_nanos() as i64 / 100).max(1);
            // SAFETY: the handle is ours and alive for the process.
            let set = unsafe { SetWaitableTimer(timer as _, &due, 0, None, std::ptr::null(), 0) };
            if set != 0 {
                // SAFETY: waiting on our own timer; the timeout is a backstop.
                unsafe { WaitForSingleObject(timer as _, SPIN.as_millis() as u32 + 100) };
            } else {
                std::thread::sleep(d - SPIN);
            }
        }
        while Instant::now() < target {
            std::hint::spin_loop();
        }
    }
    #[cfg(not(windows))]
    std::thread::sleep(d);
}

/// Per event kind (origin, vblank, flip and later kinds): the last tick a
/// wait for it ended on.
static LAST: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

fn last(kind: u32) -> &'static AtomicU64 {
    &LAST[(kind as usize).min(LAST.len() - 1)]
}

/// What a frame costs between its flip waking and its present returning, as
/// a rolling mean in microseconds: the waits lead by this much so the present
/// lands on the tick rather than that much after it.
static AFTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// When the last flip wait woke, and on which tick, for the pacing report.
#[cfg(feature = "diagnostics")]
static WAKE_AT: std::sync::Mutex<Option<(Instant, u64)>> = std::sync::Mutex::new(None);

/// How late the waits woke: `thread::sleep`'s granularity would show here.
#[cfg(feature = "diagnostics")]
static LATE_SUM: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "diagnostics")]
static LATE_MAX: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "diagnostics")]
static LATE_N: AtomicU64 = AtomicU64::new(0);

/// The next tick after now and after the last one a wait for `kind` ended
/// on (an event happens once per tick: a wait woken a little early must not
/// let the next one end on the same tick), and how long until it.
fn next_tick(kind: u32) -> (u64, Duration) {
    let mut clock = clock();
    let c = clock.get_or_insert_with(|| Clock {
        start: Instant::now(),
        interval: SIXTIETH,
        first: 0,
    });
    let elapsed = c.start.elapsed().as_nanos();
    let interval = c.interval.as_nanos().max(1);
    let tick =
        (c.first + (elapsed / interval + 1) as u64).max(last(kind).load(Ordering::Relaxed) + 1);
    let at = u128::from(tick - c.first) * interval;
    (
        tick,
        Duration::from_nanos(at.saturating_sub(elapsed) as u64),
    )
}

/// Device method: blocks until the next tick (or the time-out); an origin
/// wait then hands out the new frame's token. Other events keep the token
/// the title passes. Ticks are absolute (start plus whole intervals), so
/// lateness never accumulates.
pub(crate) extern "system" fn wait(
    _: This,
    kind: u32,
    timeout_ms: u32,
    _list: *const u8,
    flags: u32,
    token: *mut u64,
) -> HRESULT {
    let origin = kind == ORIGIN;
    let (mut tick, mut wait) = next_tick(kind);
    let limit = Duration::from_millis(u64::from(timeout_ms));
    // A blocking wait for an event that has happened since the last wait
    // for it ended (a frame took longer than a tick) ends at once, on the
    // tick that passed: sleeping on to the next one cost 17 ms of every
    // 730 ms frame (run 58: "2.0 frame waits sleeping 16.8 ms"). Polls (a
    // time-out shorter than a tick) answer as before.
    // `VICTORY_LATE_WAITS=1` sleeps to the next tick, as before.
    static SLEEP: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    // The frame's own work happens after the wake: lead the wait so that the
    // present lands on the tick instead of this much behind it. Without it a
    // 33.333 ms frame presents every 34.4 ms, which on a 180 Hz panel is 6.2
    // refreshes a frame: the picture then holds some frames seven refreshes
    // and judders (run 43: interval_ms 34.43, refresh_us 5554).
    if !origin {
        let lead = Duration::from_micros(AFTER.load(Ordering::Relaxed).min(4_000));
        if lead < wait {
            wait -= lead;
        }
    }
    if !*SLEEP.get_or_init(|| std::env::var_os("VICTORY_LATE_WAITS").is_some_and(|v| v == "1"))
        && limit >= wait
        && tick > last(kind).load(Ordering::Relaxed) + 1
    {
        tick -= 1;
        wait = Duration::ZERO;
    }
    let hr = if wait > limit {
        pause(limit);
        TIMEOUT
    } else {
        pause(wait);
        #[cfg(feature = "diagnostics")]
        {
            // How late the wait woke against the tick it was for.
            let clock = clock();
            if let Some(c) = clock.as_ref() {
                let at = u128::from(tick - c.first) * c.interval.as_nanos().max(1);
                let late = c.start.elapsed().as_nanos().saturating_sub(at) as u64;
                let us = late / 1000;
                LATE_SUM.fetch_add(us, Ordering::Relaxed);
                LATE_MAX.fetch_max(us, Ordering::Relaxed);
                LATE_N.fetch_add(1, Ordering::Relaxed);
            }
        }
        #[cfg(feature = "diagnostics")]
        if kind != ORIGIN {
            *WAKE_AT.lock().unwrap_or_else(|e| e.into_inner()) = Some((Instant::now(), tick));
        }
        last(kind).fetch_max(tick, Ordering::Relaxed);
        S_OK
    };
    #[cfg(all(windows, feature = "diagnostics"))]
    {
        use crate::copystat::{C, add};
        add(C::PaceWaits, 1);
        add(C::PaceUs, wait.min(limit).as_micros() as u64);
    }
    // A frame begun on a time-out takes no tick: its flip still does.
    if origin && (hr == S_OK || flags & TOKEN_ON_TIMEOUT != 0) {
        // Tokens count frames (consecutive), not ticks.
        let frame = FRAME.fetch_add(1, Ordering::Relaxed) + 1;
        if !token.is_null() {
            // SAFETY: checked non-null; the title passes its token.
            unsafe { token.write(frame) };
        }
    }
    #[cfg(feature = "diagnostics")]
    tracing::debug!(
        kind,
        timeout_ms,
        flags,
        hr,
        tick,
        wait_us = wait.as_micros() as u64,
        frame = current(),
        "frame wait"
    );
    hr
}

/// Device method: no statistics are kept, so none are returned.
pub(crate) extern "system" fn statistics(
    _: This,
    _token: u64,
    _kinds: u32,
    count: *mut u32,
    _out: *mut u8,
) -> HRESULT {
    if !count.is_null() {
        // SAFETY: checked non-null.
        unsafe { count.write(0) };
    }
    S_OK
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waits_end_on_ticks_and_count_frames() {
        let null = std::ptr::null();
        assert_eq!(set_interval(null, null, 2_000, 1, 0), S_OK);
        let mut first = 0;
        assert_eq!(wait(null, 0, 1_000, std::ptr::null(), 0, &mut first), S_OK);
        let before = Instant::now();
        let mut second = 0;
        assert_eq!(wait(null, 0, 1_000, std::ptr::null(), 0, &mut second), S_OK);
        assert!(second > first);
        assert!(before.elapsed() <= Duration::from_millis(50));
        assert_eq!(wait(null, 0, 0, std::ptr::null(), 0, &mut second), TIMEOUT);
        // Tokens count frames, a frame started early on a time-out too.
        let mut early = 0;
        let flags = TOKEN_ON_TIMEOUT;
        assert_eq!(
            wait(null, 0, 0, std::ptr::null(), flags, &mut early),
            TIMEOUT
        );
        let mut next = 0;
        assert_eq!(wait(null, 0, 1_000, std::ptr::null(), 0, &mut next), S_OK);
        assert!(early == second + 1 && next == early + 1);
        assert!(LAST[0].load(Ordering::Relaxed) >= 2);

        // The title's loop (the clock is global, so in the same test): an
        // origin poll that times out (and starts the frame), then a
        // blocking flip wait. Each flip takes its own tick.
        const FLIP: u32 = 2;
        assert_eq!(set_interval(null, null, 5_000, 3, 0), S_OK);
        let mut token = 0;
        let flips: Vec<u64> = (0..4)
            .map(|_| {
                let _ = wait(null, 0, 0, std::ptr::null(), TOKEN_ON_TIMEOUT, &mut token);
                assert_eq!(
                    wait(null, FLIP, u32::MAX, std::ptr::null(), 0, &mut token),
                    S_OK
                );
                LAST[2].load(Ordering::Relaxed)
            })
            .collect();
        assert!(flips.windows(2).all(|w| w[1] == w[0] + 1), "{flips:?}");
        // Even when a flip wait begins on the tick another just ended on.
        let (tick, _) = next_tick(FLIP);
        assert!(tick > flips[3]);
    }
}
