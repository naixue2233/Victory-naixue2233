//! Host pipelines are made by workers, not by the title thread that first
//! records a draw with them. Translating a shader and the host driver's
//! compile take from milliseconds to seconds each (structurisation of the
//! biggest compute programs ~0.4 s, some driver compiles 3.4 s), and
//! the title's first world frame wants over 1,200 new pipelines: made
//! on its recording threads, that frame took more than 30 s and the
//! title's GPU watchdog ended the run (decision 0007, "Pipelines made off
//! the title's threads").
//!
//! A first use queues the pipeline and waits for it within a budget per
//! frame, so the cheap ones (menus, one-off passes such as a colour-grading
//! bake) are there at once as before; past the budget a draw or dispatch
//! whose pipeline is not ready is dropped, as an untranslated one is.
//!
//! Passes over the screen (no vertex input, no depth or stencil) have two
//! workers and a longer per-frame budget of their own: the title fills its
//! lighting tables with such draws once, at the world's first frame, and
//! behind a world's worth of pipelines they would otherwise wait behind
//! queued draws; a run that lost them lacked indirect/lighting setup.
//!
//! shortcut: the budgets (half a second of waiting per title frame, 20 s
//! for screen passes) are guesses; another pass the title runs only once
//! and whose pipeline takes longer is lost.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::Shader;
use super::host::{GraphicsState, HostPipeline, TargetFormats};

/// What a host pipeline is made from, and the host pipeline once made.
pub(crate) struct Pending {
    shaders: Vec<Arc<Shader>>,
    pub(crate) graphics: Option<Box<GraphicsState>>,
    /// A pass over the screen ([`GraphicsState::screen_pass`]).
    screen: bool,
    queue_state: AtomicU8,
    made: OnceLock<Option<HostPipeline>>,
    /// What identifies it across runs (shader containers and state), and
    /// whether a draw or dispatch has requested it yet.
    key: String,
    used: AtomicBool,
    /// The title frame in which a thread first waited for it (`u64::MAX`:
    /// none yet). A pipeline is waited for in one frame only: one the
    /// worker has not made within that frame's budget (a slow or stuck
    /// driver compile) is dropped in later frames until it is made, instead
    /// of taking the whole budget again in every frame that draws with it
    /// (#1181: 500 ms waited in every world frame, felt as a freeze while
    /// the player moves). `VICTORY_PIPELINE_WAIT_EVERY_FRAME=1` waits in
    /// every frame as before.
    waited_frame: AtomicU64,
    /// The formats its state declares for what a draw draws into (zeros
    /// without a graphics state).
    pub(crate) formats: TargetFormats,
    /// The same shaders and state for other formats, bound at a draw
    /// ([`for_formats`]): a chain from the title's pipeline, one link per
    /// set of formats met, never shortened.
    variant: OnceLock<Arc<Pending>>,
}

// SAFETY: D3D12 objects are free-threaded; the graphics state (whose input
// elements point into its own names) is only read after creation.
unsafe impl Send for Pending {}
// SAFETY: as above.
unsafe impl Sync for Pending {}

impl Pending {
    pub(crate) fn new(shaders: Vec<Arc<Shader>>, graphics: Option<Box<GraphicsState>>) -> Pending {
        let mut key = blake3::Hasher::new();
        for s in &shaders {
            key.update(s.stage.as_bytes());
            key.update(s.hash.as_bytes());
        }
        key.update(graphics.as_ref().map(|g| g.describe()).unwrap_or_default().as_bytes());
        Pending {
            screen: graphics.as_ref().is_some_and(|g| g.screen_pass()),
            formats: graphics.as_ref().map_or([0; 9], |g| g.formats()),
            variant: OnceLock::new(),
            shaders,
            graphics,
            queue_state: AtomicU8::new(NOT_QUEUED),
            made: OnceLock::new(),
            key: key.finalize().to_hex()[..32].to_owned(),
            used: AtomicBool::new(false),
            waited_frame: AtomicU64::new(u64::MAX),
        }
    }

    fn make(&self) {
        self.made
            .get_or_init(|| HostPipeline::new(&self.shaders, self.graphics.as_deref()));
        let (lock, done) = &DONE;
        let _held = lock.lock().unwrap_or_else(|e| e.into_inner());
        done.notify_all();
    }
}

/// Why a pipeline has no host pipeline (diagnostics): its shaders that do
/// not translate, else "refused" or "being made"; `None` if it has one.
pub(crate) fn missing(p: &Pending) -> Option<String> {
    match p.made.get() {
        Some(Some(_)) => None,
        None => Some("being made".into()),
        Some(None) => {
            let failed: Vec<String> = p
                .shaders
                .iter()
                .filter(|s| matches!(s.translated.get(), Some(None)))
                .map(|s| format!("{} {}", s.stage, s.hash))
                .collect();
            Some(if failed.is_empty() {
                format!("refused {:?}", p.shaders)
            } else {
                failed.join(" + ")
            })
        }
    }
}

/// Signalled whenever a worker has made a pipeline.
static DONE: (Mutex<()>, Condvar) = (Mutex::new(()), Condvar::new());

/// Waiting allowed per title frame, over all threads.
fn wait_budget() -> Duration {
    static BUDGET: OnceLock<Duration> = OnceLock::new();
    *BUDGET.get_or_init(|| {
        let millis = std::env::var("VICTORY_PIPELINE_WAIT_MS")
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .unwrap_or(500);
        tracing::info!(millis, "host pipeline wait budget per frame");
        Duration::from_millis(millis)
    })
}

/// Waiting allowed per title frame for passes over the screen
/// (`VICTORY_SCREEN_PIPELINE_WAIT_MS`, default 20 s; 0 when `VICTORY_PIPELINE_WAIT_MS=0`).
fn screen_wait_budget() -> Duration {
    static BUDGET: OnceLock<Duration> = OnceLock::new();
    *BUDGET.get_or_init(|| {
        let millis = std::env::var("VICTORY_SCREEN_PIPELINE_WAIT_MS")
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .unwrap_or_else(|| if wait_budget().is_zero() { 0 } else { 20_000 });
        Duration::from_millis(millis)
    })
}

/// Pipelines queued and not made yet.
static WAITING: AtomicU64 = AtomicU64::new(0);

/// Pipelines queued and not made yet (for the frame line).
pub(crate) fn waiting() -> u64 {
    WAITING.load(Ordering::Relaxed)
}

/// The host pipeline if it is made (or is made within the frame's budget);
/// queues it on first use.
pub(crate) fn ready(p: &Arc<Pending>) -> Option<&HostPipeline> {
    // A still-pending pipeline was requested by the title too. Prewarm it
    // on the next launch, even if this run never got to use it.
    if !p.used.load(Ordering::Relaxed) && !p.used.swap(true, Ordering::Relaxed) {
        super::library::note_used(&p.key);
    }
    if p.made.get().is_none() {
        start(p, REQUESTED);
        wait(p, crate::frame::current());
    }
    p.made.get()?.as_ref()
}

/// Pipelines made again for the formats bound at a draw, since the start.
static VARIANTS: AtomicU64 = AtomicU64::new(0);

/// Whether a draw into views of other formats than its pipeline declares
/// gets a host pipeline for those formats ([`for_formats`]). On by
/// default; `VICTORY_TARGET_VARIANTS=0` sends such a draw to the host with
/// the title's pipeline, as before.
fn variants_on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_TARGET_VARIANTS").is_none_or(|v| v != "0"))
}

/// The host pipeline for a draw with `p` into views of the formats
/// `bound`, which `p` does not declare (`host::formats_differ`): that of
/// the same shaders and state for those formats. It is a pipeline like the
/// title's own: queued at its first draw, made by a worker, waited for
/// within the frame's budget, and the draw is dropped while it is not
/// there ([`ready`]).
#[cold]
pub(crate) fn for_formats<'a>(p: &'a Arc<Pending>, bound: &TargetFormats) -> Option<&'a HostPipeline> {
    match p.graphics.as_deref() {
        Some(g) if variants_on() => ready(variant(p, g, &super::host::formats_for(&p.formats, bound))),
        _ => ready(p),
    }
}

/// The pipeline of `p`'s shaders and its state `g` for the formats
/// `bound`: found in the chain that starts at `p`, or added to it (logged
/// then: once per pipeline and set of formats).
fn variant<'a>(p: &'a Arc<Pending>, g: &GraphicsState, bound: &TargetFormats) -> &'a Arc<Pending> {
    let mut at = p;
    // (`bound` is what `host::formats_for` gives, and a state made for it
    // declares exactly it, so the walk ends at the link made for it; the
    // count is a bound all the same: past it the title's own pipeline.)
    for _ in 0..64 {
        let next = at.variant.get_or_init(|| {
            let so_far = VARIANTS.fetch_add(1, Ordering::Relaxed) + 1;
            tracing::info!(
                pipeline = ?p.shaders,
                declared = ?p.formats,
                bound = ?bound,
                so_far,
                "a pipeline is made again for the render target formats bound at its draw (eight render targets, then depth; VICTORY_TARGET_VARIANTS=0: the draw goes out with the title's pipeline)"
            );
            Arc::new(Pending::new(p.shaders.clone(), Some(Box::new(g.with_formats(bound)))))
        });
        // (A state made for `bound` declares exactly `bound`.)
        if next.formats == *bound {
            return next;
        }
        at = next;
    }
    p
}

/// Makes `p` on this thread if it is not made yet (waiting for a worker
/// that is making it); whether it has a host pipeline.
pub(crate) fn now(p: &Arc<Pending>) -> bool {
    p.make();
    p.made.get().is_some_and(|made| made.is_some())
}

/// At the title's creation of a pipeline: starts it at once if an earlier
/// run drew with it (during loading, so the world's first frames find it),
/// unless memory commit headroom is too tight for speculative prewarming.
pub(crate) fn early(p: &Arc<Pending>) {
    // Turning off speculative starts keeps first-use scheduling and the disk caches available.
    static PREWARM: OnceLock<bool> = OnceLock::new();
    let enabled = *PREWARM.get_or_init(|| std::env::var_os("VICTORY_EARLY_PIPELINES").is_none_or(|v| v != "0"));
    if enabled
        && !p.shaders.is_empty()
        && super::library::wanted_early(&p.key)
        && current_commit_budget(1).1
    {
        super::library::EARLY.fetch_add(1, Ordering::Relaxed);
        start(p, EARLY);
    }
}

/// Whether the frame's budget is one for all threads together: a thread
/// that waits takes what is left of it and gives back what it did not
/// use. Counted only when a wait had ended, the budget was one per thread:
/// each of the title's five recording threads began its first wait of a
/// frame with nothing counted yet and waited the whole half second, 2.5 s
/// of waiting in every frame in which pipelines were being made (runs
/// 34-49: "2504 ms waiting for pipelines" in each such frame).
/// On by default; `VICTORY_PIPELINE_WAIT_SHARED=0` counts as before.
fn shared_budget() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_PIPELINE_WAIT_SHARED").is_none_or(|v| v != "0"))
}

/// Whether a pipeline is waited for in every frame that draws with it (the
/// old behaviour), not only in the first one that had budget for it.
fn wait_every_frame() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var_os("VICTORY_PIPELINE_WAIT_EVERY_FRAME").is_some_and(|v| v == "1")
    })
}

/// Claims frame `frame` as the one in which `p` is waited for; whether
/// `frame` is (or already was) that frame.
fn claim_wait(p: &Pending, frame: u64) -> bool {
    if wait_every_frame() {
        return true;
    }
    match p.waited_frame.compare_exchange(u64::MAX, frame, Ordering::AcqRel, Ordering::Acquire) {
        Ok(_) => true,
        Err(first) => first == frame,
    }
}

/// Waits for `p` while frame `frame` has budget of its kind left.
fn wait(p: &Pending, frame: u64) {
    let kind = usize::from(p.screen);
    let budget = if p.screen { screen_wait_budget() } else { wait_budget() };
    if budget.is_zero() {
        return;
    }
    // The frame the budget belongs to, and what was waited of it (or is
    // taken by a thread waiting now), kept separately for ordinary and
    // screen passes.
    static SPENT: [Mutex<(u64, Duration)>; 2] = [const { Mutex::new((u64::MAX, Duration::ZERO)) }; 2];
    let shared = shared_budget();
    let left = {
        let mut spent = SPENT[kind].lock().unwrap_or_else(|e| e.into_inner());
        if spent.0 != frame {
            *spent = (frame, Duration::ZERO);
        }
        let left = budget.saturating_sub(spent.1);
        // Claimed only with time left, so a pipeline first bound when the
        // frame's budget is spent keeps its one wait for a later frame.
        if left.is_zero() || !claim_wait(p, frame) {
            return;
        }
        if shared {
            spent.1 += left;
        }
        left
    };
    let started = Instant::now();
    let (lock, done) = &DONE;
    let mut held = lock.lock().unwrap_or_else(|e| e.into_inner());
    while p.made.get().is_none() {
        let rest = left.saturating_sub(started.elapsed());
        if rest.is_zero() {
            break;
        }
        held = done.wait_timeout(held, rest).unwrap_or_else(|e| e.into_inner()).0;
    }
    drop(held);
    let waited = started.elapsed().min(left);
    {
        let mut spent = SPENT[kind].lock().unwrap_or_else(|e| e.into_inner());
        // A frame that began meanwhile has its own budget.
        if spent.0 == frame {
            if shared {
                spent.1 = spent.1.saturating_sub(left - waited);
            } else {
                spent.1 += waited;
            }
        }
    }
    crate::framelog::add(crate::framelog::T::PipelineWaitUs, started.elapsed().as_micros() as u64);
    #[cfg(feature = "diagnostics")]
    crate::copystat::add(
        crate::copystat::C::PipelineWaitUs,
        started.elapsed().as_micros() as u64,
    );
}

const NOT_QUEUED: u8 = 0;
const EARLY: u8 = 1;
const REQUESTED: u8 = 2;
const MAKING: u8 = 3;

/// Minimum remaining commit charge (in MiB) before `pipeline::make` stops
/// speculative `EARLY` prewarming and throttles concurrent compiler workers.
/// `VICTORY_PIPELINE_COMMIT_MB=0` turns commit throttling off.
const DEFAULT_COMMIT_FLOOR_MB: u64 = 2048;

fn commit_floor_mb() -> u64 {
    static FLOOR: OnceLock<u64> = OnceLock::new();
    *FLOOR.get_or_init(|| {
        std::env::var("VICTORY_PIPELINE_COMMIT_MB")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(DEFAULT_COMMIT_FLOOR_MB)
    })
}

/// Returns `(max_concurrent_workers, allow_early_prewarm)` for the current
/// host memory headroom. Never drops `max_concurrent_workers` below 1.
fn commit_budget(
    base_workers: usize,
    avail_commit_bytes: Option<u64>,
    avail_phys_bytes: Option<u64>,
    floor_mb: u64,
) -> (usize, bool) {
    let base = base_workers.max(1);
    if floor_mb == 0 {
        return (base, true);
    }
    let floor = floor_mb.saturating_mul(1024 * 1024);
    let caution = floor.saturating_mul(2);
    if avail_commit_bytes.is_some_and(|c| c < floor) {
        return (1, false);
    }
    if avail_commit_bytes.is_some_and(|c| c < caution)
        || avail_phys_bytes.is_some_and(|p| p < floor)
    {
        return (base.min(2), false);
    }
    (base, true)
}

fn current_commit_budget(base_workers: usize) -> (usize, bool) {
    #[cfg(windows)]
    let (avail_commit, avail_phys) = (
        crate::host::available_commit_ram(),
        crate::host::available_physical_ram(),
    );
    #[cfg(not(windows))]
    let (avail_commit, avail_phys) = (None, None);
    let budget = commit_budget(base_workers, avail_commit, avail_phys, commit_floor_mb());
    if !budget.1 {
        static WARNED: std::sync::Once = std::sync::Once::new();
        WARNED.call_once(|| {
            tracing::info!(
                allowed_workers = budget.0,
                avail_commit_mb = avail_commit.map(|b| b >> 20),
                avail_phys_mb = avail_phys.map(|b| b >> 20),
                "throttling pipeline compilation under low memory headroom"
            );
        });
    }
    budget
}

struct Queue {
    early: VecDeque<Arc<Pending>>,
    requested: VecDeque<Arc<Pending>>,
    active: usize,
}

impl Queue {
    const fn new() -> Self {
        Self {
            early: VecDeque::new(),
            requested: VecDeque::new(),
            active: 0,
        }
    }

    /// Queues an unrequested pipeline or promotes one queued at creation.
    /// Returns whether it is new to the queue, or `None` if nothing changed.
    fn push(&mut self, p: &Arc<Pending>, priority: u8) -> Option<bool> {
        let prev = p.queue_state.load(Ordering::Relaxed);
        if prev >= priority {
            return None;
        }
        p.queue_state.store(priority, Ordering::Relaxed);
        if priority == REQUESTED {
            self.requested.push_back(p.clone());
        } else {
            self.early.push_back(p.clone());
        }
        Some(prev == NOT_QUEUED)
    }

    fn pop_with(&mut self, allow_early: bool) -> Option<Arc<Pending>> {
        let p = self.requested.pop_front().or_else(|| {
            if !allow_early {
                return None;
            }
            // An early entry can have been promoted and claimed already.
            std::iter::from_fn(|| self.early.pop_front())
                .find(|p| p.queue_state.load(Ordering::Relaxed) == EARLY)
        })?;
        p.queue_state.store(MAKING, Ordering::Relaxed);
        Some(p)
    }

    #[cfg(test)]
    fn pop(&mut self) -> Option<Arc<Pending>> {
        self.pop_with(true)
    }
}

/// Ordinary pipelines (0) and screen passes (1) use separate queues so
/// screen passes are not starved behind queued draws.
static QUEUES: [Mutex<Queue>; 2] = [const { Mutex::new(Queue::new()) }; 2];
static AVAILABLE: [Condvar; 2] = [const { Condvar::new() }; 2];

/// Start or promote work for a draw without waiting behind the entire early
/// list. Queue changes and worker claims happen under the same lock.
fn start(p: &Arc<Pending>, priority: u8) {
    if p.queue_state.load(Ordering::Relaxed) >= priority {
        return;
    }
    if !workers() {
        if p.queue_state.swap(MAKING, Ordering::Relaxed) == NOT_QUEUED {
            p.make();
        }
        return;
    }
    let kind = usize::from(p.screen);
    let mut queue = QUEUES[kind].lock().unwrap_or_else(|e| e.into_inner());
    if let Some(new) = queue.push(p, priority) {
        if new {
            PEAK.fetch_max(WAITING.fetch_add(1, Ordering::Relaxed) + 1, Ordering::Relaxed);
        }
        AVAILABLE[kind].notify_one();
    }
}

/// Lets the queue know a worker has finished, and wakes a waiter if any work
/// is queued behind it.
struct ActiveGuard {
    kind: usize,
}

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        let mut queue = QUEUES[self.kind].lock().unwrap_or_else(|e| e.into_inner());
        queue.active = queue.active.saturating_sub(1);
        if !queue.requested.is_empty() || !queue.early.is_empty() {
            AVAILABLE[self.kind].notify_all();
        }
    }
}

fn next(kind: usize, workers: usize) -> (Arc<Pending>, ActiveGuard) {
    let mut queue = QUEUES[kind].lock().unwrap_or_else(|e| e.into_inner());
    loop {
        let (allowed, allow_early) = current_commit_budget(workers);
        if queue.active < allowed
            && let Some(p) = queue.pop_with(allow_early)
        {
            queue.active += 1;
            return (p, ActiveGuard { kind });
        }
        queue = AVAILABLE[kind].wait(queue).unwrap_or_else(|e| e.into_inner());
    }
}

/// Starts the ordinary workers (half the logical processors, 2 to 8) and two
/// dedicated screen-pass workers on first use.
fn workers() -> bool {
    static RUNNING: OnceLock<bool> = OnceLock::new();
    *RUNNING.get_or_init(|| {
        let mut running = false;
        let ordinary = std::thread::available_parallelism().map_or(2, |n| (n.get() / 2).clamp(2, 8));
        for (kind, count, prefix) in [
            (0usize, ordinary, "victory-pipelines"),
            (1usize, 2, "victory-screen-pipelines"),
        ] {
            for i in 0..count {
                let spawned = std::thread::Builder::new()
                    .name(format!("{prefix}-{i}"))
                    .spawn(move || {
                        // Below the title's own threads (loading, recording).
                        // SAFETY: a plain call on our own thread.
                        unsafe {
                            use windows::Win32::System::Threading::*;
                            let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
                        }
                        loop {
                            let (job, guard) = next(kind, ordinary);
                            job.make();
                            drop(guard);
                            if WAITING.fetch_sub(1, Ordering::Relaxed) == 1 {
                                drained();
                            } else {
                                super::library::save(false);
                            }
                        }
                    });
                if let Err(e) = spawned {
                    tracing::warn!(error = %e, "no pipeline worker");
                } else {
                    running = true;
                }
            }
        }
        running
    })
}

/// The most pipelines waiting at once since the workers last ran out.
static PEAK: AtomicU64 = AtomicU64::new(0);

/// The workers ran out of work: logged after a batch of 10 or more, and
/// the cache saved (at most every 20 s).
fn drained() {
    use super::library::{EARLY, LOADED, MADE};
    if PEAK.swap(0, Ordering::Relaxed) >= 10 {
        tracing::info!(
            made = MADE.load(Ordering::Relaxed),
            loaded = LOADED.load(Ordering::Relaxed),
            started_early = EARLY.load(Ordering::Relaxed),
            "host pipelines all made"
        );
    }
    super::library::save(false);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A first use queues the pipeline and, within the budget, gets the
    /// worker's answer (here: no host pipeline for no shaders).
    #[test]
    fn first_use_is_made_by_a_worker() {
        let p = Arc::new(Pending::new(Vec::new(), None));
        assert!(ready(&p).is_none());
        assert_ne!(p.queue_state.load(Ordering::Relaxed), NOT_QUEUED);
        let started = Instant::now();
        while p.made.get().is_none() && started.elapsed() < Duration::from_secs(10) {
            std::thread::yield_now();
        }
        assert!(matches!(p.made.get(), Some(None)));
    }

    /// A draw into other formats than its pipeline declares finds the
    /// pipeline for them, which exists once per set of formats and is
    /// queued by its first draw like any other.
    #[test]
    fn a_pipeline_for_bound_formats_is_made_once() {
        // The title's pipeline declares one render target, format 24.
        let p = Arc::new(Pending::new(Vec::new(), Some(Box::new(GraphicsState::declaring(24)))));
        assert_eq!(p.formats, [24, 0, 0, 0, 0, 0, 0, 0, 0]);
        let g = p.graphics.as_deref().unwrap();
        let (a, b) = ([26, 0, 0, 0, 0, 0, 0, 0, 0], [28, 0, 0, 0, 0, 0, 0, 0, 0]);
        let first = variant(&p, g, &a);
        assert_eq!(first.formats, a);
        assert_ne!(first.key, p.key);
        assert!(Arc::ptr_eq(first, variant(&p, g, &a)));
        let second = variant(&p, g, &b);
        assert_eq!(second.formats, b);
        assert!(!Arc::ptr_eq(first, second));
        assert_ne!(second.key, first.key);
        assert!(Arc::ptr_eq(first, variant(&p, g, &a)));
        assert!(Arc::ptr_eq(second, variant(&p, g, &b)));
        assert_eq!(first.queue_state.load(Ordering::Relaxed), NOT_QUEUED);
        assert_eq!(p.queue_state.load(Ordering::Relaxed), NOT_QUEUED);
    }

    #[test]
    fn pending_pipeline_is_recorded_for_the_next_launch() {
        let mut p = Pending::new(Vec::new(), None);
        p.key = "pending-pipeline-test".to_owned();
        // Simulate a worker that has taken the job but cannot finish it yet.
        p.queue_state.store(MAKING, Ordering::Relaxed);
        let p = Arc::new(p);

        assert!(!super::super::library::wanted_early(&p.key));
        assert!(ready(&p).is_none());
        assert!(p.made.get().is_none());
        assert!(super::super::library::wanted_early(&p.key));
    }

    #[test]
    fn draws_promote_early_pipelines_without_claiming_them_twice() {
        let earlier = Arc::new(Pending::new(Vec::new(), None));
        let promoted = Arc::new(Pending::new(Vec::new(), None));
        let requested = Arc::new(Pending::new(Vec::new(), None));
        let mut queue = Queue::new();

        assert_eq!(queue.push(&earlier, EARLY), Some(true));
        assert_eq!(queue.push(&promoted, EARLY), Some(true));
        assert_eq!(queue.push(&requested, REQUESTED), Some(true));
        assert_eq!(queue.push(&promoted, REQUESTED), Some(false));
        assert_eq!(queue.push(&promoted, REQUESTED), None);

        assert!(Arc::ptr_eq(&queue.pop().unwrap(), &requested));
        assert!(Arc::ptr_eq(&queue.pop().unwrap(), &promoted));
        assert_eq!(queue.push(&promoted, REQUESTED), None);
        assert!(Arc::ptr_eq(&queue.pop().unwrap(), &earlier));
        assert!(queue.pop().is_none());
    }

    #[test]
    fn screen_pass_uses_dedicated_queue_and_worker() {
        let mut state = GraphicsState::default();
        state.topology = windows::Win32::Graphics::Direct3D12::D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE;
        let p = Arc::new(Pending::new(Vec::new(), Some(Box::new(state))));
        assert!(p.screen);
        assert!(ready(&p).is_none());
        assert_ne!(p.queue_state.load(Ordering::Relaxed), NOT_QUEUED);
        let started = Instant::now();
        while p.made.get().is_none() && started.elapsed() < Duration::from_secs(10) {
            std::thread::yield_now();
        }
        assert!(matches!(p.made.get(), Some(None)));
    }

    #[test]
    fn commit_budget_throttles_workers_and_early_under_pressure() {
        const MIB: u64 = 1024 * 1024;
        const GIB: u64 = 1024 * MIB;

        // Escape hatch disables throttling.
        assert_eq!(commit_budget(6, Some(256 * MIB), Some(256 * MIB), 0), (6, true));
        // Unknown headroom keeps full concurrency; low physical RAM still throttles.
        assert_eq!(commit_budget(6, None, None, 2048), (6, true));
        assert_eq!(commit_budget(6, None, Some(512 * MIB), 2048), (2, false));
        // Plenty of headroom: all workers + early prewarm allowed.
        assert_eq!(commit_budget(6, Some(8 * GIB), Some(8 * GIB), 2048), (6, true));
        assert_eq!(commit_budget(6, Some(4 * GIB), Some(2 * GIB), 2048), (6, true));
        // Tight commit (< 2x floor = 4 GiB) or low physical RAM (< floor): max 2 workers, no early.
        assert_eq!(commit_budget(6, Some(4 * GIB - 1), Some(8 * GIB), 2048), (2, false));
        assert_eq!(commit_budget(6, Some(8 * GIB), Some(2 * GIB - 1), 2048), (2, false));
        // Critical commit (< floor = 2 GiB): serialize to 1 worker, no early.
        assert_eq!(commit_budget(6, Some(2 * GIB - 1), Some(8 * GIB), 2048), (1, false));
        assert_eq!(commit_budget(0, Some(256 * MIB), Some(256 * MIB), 2048), (1, false));
    }

    #[test]
    fn early_pipelines_are_deferred_when_only_requested_allowed() {
        let early_only = Arc::new(Pending::new(Vec::new(), None));
        let promoted = Arc::new(Pending::new(Vec::new(), None));
        let requested = Arc::new(Pending::new(Vec::new(), None));
        let mut queue = Queue::new();

        assert_eq!(queue.push(&early_only, EARLY), Some(true));
        assert_eq!(queue.push(&promoted, EARLY), Some(true));
        assert_eq!(queue.push(&requested, REQUESTED), Some(true));

        assert!(Arc::ptr_eq(&queue.pop_with(false).unwrap(), &requested));
        assert!(queue.pop_with(false).is_none());

        // Promoted early work still runs even when speculative early is paused.
        assert_eq!(queue.push(&promoted, REQUESTED), Some(false));
        assert!(Arc::ptr_eq(&queue.pop_with(false).unwrap(), &promoted));
        assert!(queue.pop_with(false).is_none());

        // Once early is allowed again, the deferred item is drained.
        assert!(Arc::ptr_eq(&queue.pop_with(true).unwrap(), &early_only));
        assert!(queue.pop_with(true).is_none());
    }

    /// A pipeline still being made that was waited for in an earlier frame
    /// is not waited for again (#1181).
    #[test]
    fn a_pipeline_is_waited_for_in_one_frame_only() {
        let p = Pending::new(Vec::new(), None);
        // Claimed by a worker that has not finished.
        p.queue_state.store(MAKING, Ordering::Relaxed);
        let frame = crate::frame::current();
        // Another frame than this one, and never `u64::MAX` (none yet): with
        // the frame counter still at 0, `0 - 1` would be that.
        p.waited_frame.store(frame.wrapping_add(1), Ordering::Relaxed);
        let p = Arc::new(p);
        let started = Instant::now();
        assert!(ready(&p).is_none());
        assert!(
            started.elapsed() < Duration::from_millis(50),
            "waited {:?}",
            started.elapsed()
        );
    }

    /// The claim: the frame that takes it waits, the same frame may wait
    /// again, a later one may not.
    #[test]
    fn the_first_frame_with_budget_claims_the_wait() {
        let p = Pending::new(Vec::new(), None);
        p.queue_state.store(MAKING, Ordering::Relaxed);
        assert!(claim_wait(&p, 7));
        assert!(claim_wait(&p, 7));
        assert!(!claim_wait(&p, 8));
        assert!(!claim_wait(&p, 9));
    }
}
