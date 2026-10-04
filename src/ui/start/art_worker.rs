//! Single-flight Start art decode worker (latest-wins mailbox).
//!
//! The UI posts [`ArtJob`]s; one OS thread decodes one path at a time and drains
//! the mailbox between items so fast nav never piles decode threads.

use crate::ui::start::icon_cache::{self, ArtRetainSet};
use iced::futures::channel::mpsc as iced_mpsc;
use iced::futures::{SinkExt, StreamExt};
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread;

/// Events delivered to the iced UI when decode milestones complete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtEventKind {
    /// Selected immersive cover is in the cache (or was already).
    SelectedReady,
    /// Visible ±NEIGHBORS strip art pass finished (or skipped).
    NearReady,
    /// Prefetch ring finished for this generation.
    JobDone,
}

#[derive(Debug, Clone)]
pub struct ArtEvent {
    pub generation: u64,
    pub kind: ArtEventKind,
}

/// One latest-wins warm request from the UI.
#[derive(Debug, Clone)]
pub struct ArtJob {
    pub generation: u64,
    pub selected_backdrop: Option<PathBuf>,
    pub selected_hero: Option<PathBuf>,
    pub selected_shell: Option<PathBuf>,
    pub near: ArtRetainSet,
    pub prefetch: ArtRetainSet,
}

enum Cmd {
    Job(Box<ArtJob>),
    Shutdown,
}

struct EventSink {
    /// Monotonic id so a stale subscription Drop cannot clear a newer bind.
    bind_id: u64,
    tx: iced_mpsc::UnboundedSender<ArtEvent>,
}

static MAILBOX: Mutex<Option<Sender<Cmd>>> = Mutex::new(None);
static JOIN: Mutex<Option<thread::JoinHandle<()>>> = Mutex::new(None);
static EVENT_TX: Mutex<Option<EventSink>> = Mutex::new(None);
static BIND_SEQ: AtomicU64 = AtomicU64::new(0);

/// Ensure the worker thread is running (idempotent).
pub fn ensure_started() {
    let mut slot = MAILBOX.lock().unwrap_or_else(|e| e.into_inner());
    if slot.is_some() {
        return;
    }
    let (tx, rx) = mpsc::channel::<Cmd>();
    let handle = thread::Builder::new()
        .name("start-art-worker".into())
        .spawn(move || worker_loop(rx))
        .expect("spawn start-art-worker");
    *slot = Some(tx);
    drop(slot);
    let mut join = JOIN.lock().unwrap_or_else(|e| e.into_inner());
    *join = Some(handle);
}

/// Bind the iced event sink. Returns a bind id for [`clear_event_sender`].
pub fn bind_event_sender(tx: iced_mpsc::UnboundedSender<ArtEvent>) -> u64 {
    let bind_id = BIND_SEQ.fetch_add(1, Ordering::Relaxed) + 1;
    let mut slot = EVENT_TX.lock().unwrap_or_else(|e| e.into_inner());
    *slot = Some(EventSink { bind_id, tx });
    bind_id
}

/// Clear the iced event sink only if `bind_id` is still the active bind.
///
/// Prevents a remount race where the old subscription Drop wipes the new sender.
pub fn clear_event_sender(bind_id: u64) {
    let mut slot = EVENT_TX.lock().unwrap_or_else(|e| e.into_inner());
    if slot.as_ref().is_some_and(|s| s.bind_id == bind_id) {
        *slot = None;
    }
}

/// Stop in-flight warm work without tearing the thread down (Start close).
///
/// Posts an empty job at `generation` so the worker abandons the current order
/// after the decode already in flight. Does not change pin/LRU state — reopen
/// can still hit warm cache.
pub fn cancel_jobs(generation: u64) {
    let guard = MAILBOX.lock().unwrap_or_else(|e| e.into_inner());
    let Some(tx) = guard.as_ref() else {
        return;
    };
    crate::controller::hid::diag::diag_info(format!("ui-diag: art worker cancel gen={generation}"));
    let _ = tx.send(Cmd::Job(Box::new(ArtJob {
        generation,
        selected_backdrop: None,
        selected_hero: None,
        selected_shell: None,
        near: ArtRetainSet::default(),
        prefetch: ArtRetainSet::default(),
    })));
}

/// Stop the worker thread and wipe every Start image cache (app Exit).
///
/// Wipes caches **before** joining so a slow final `image::open` cannot leave
/// handles in the process cache, and so quit stays responsive.
pub fn shutdown() {
    crate::controller::hid::diag::diag_info("ui-diag: art worker shutdown begin");
    let tx = {
        let mut slot = MAILBOX.lock().unwrap_or_else(|e| e.into_inner());
        slot.take()
    };
    if let Some(tx) = tx {
        let _ = tx.send(Cmd::Shutdown);
    }
    {
        let mut ev = EVENT_TX.lock().unwrap_or_else(|e| e.into_inner());
        *ev = None;
    }
    // Blank slate immediately — in-flight decode inserts are rejected via clear_epoch.
    super::wipe_art_caches();
    if let Some(handle) = JOIN.lock().unwrap_or_else(|e| e.into_inner()).take() {
        let _ = handle.join();
    }
    // Second wipe after join: catch anything inserted in the shutdown race window.
    super::wipe_art_caches();
    crate::controller::hid::diag::diag_info("ui-diag: art worker shutdown complete");
}

fn pin_job(job: &ArtJob) {
    let mut pins = job.prefetch.merged_with(&job.near);
    if let Some(p) = &job.selected_backdrop
        && !pins.backdrops.contains(p)
    {
        pins.backdrops.push(p.clone());
    }
    if let Some(p) = &job.selected_hero {
        if !pins.heroes.contains(p) {
            pins.heroes.push(p.clone());
        }
        if !pins.list_files.contains(p) {
            pins.list_files.push(p.clone());
        }
    }
    if let Some(p) = &job.selected_shell {
        if !pins.hero_shells.contains(p) {
            pins.hero_shells.push(p.clone());
        }
        if !pins.list_shells.contains(p) {
            pins.list_shells.push(p.clone());
        }
    }
    icon_cache::pin_art_window(pins);
}

/// Replace any pending job with `job` (latest-wins; worker drains backlog).
pub fn submit(job: ArtJob) {
    ensure_started();
    pin_job(&job);
    crate::controller::hid::diag::diag_info(format!(
        "ui-diag: art worker job gen={} sel_bd={} near_bd={} prefetch_bd={}",
        job.generation,
        u8::from(job.selected_backdrop.is_some()),
        job.near.backdrops.len(),
        job.prefetch.backdrops.len(),
    ));
    let guard = MAILBOX.lock().unwrap_or_else(|e| e.into_inner());
    let Some(tx) = guard.as_ref() else {
        return;
    };
    let _ = tx.send(Cmd::Job(Box::new(job)));
}

fn emit(event: ArtEvent) {
    let guard = EVENT_TX.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(sink) = guard.as_ref() {
        let _ = sink.tx.unbounded_send(event);
    }
}

fn drain_latest(rx: &Receiver<Cmd>, mut current: ArtJob) -> Result<ArtJob, ()> {
    loop {
        match rx.try_recv() {
            Ok(Cmd::Job(job)) => {
                if job.generation != current.generation {
                    crate::controller::hid::diag::diag_info(format!(
                        "ui-diag: art worker switch gen={}→{}",
                        current.generation, job.generation
                    ));
                }
                current = *job;
            }
            Ok(Cmd::Shutdown) => return Err(()),
            Err(TryRecvError::Empty) => break,
            Err(TryRecvError::Disconnected) => return Err(()),
        }
    }
    Ok(current)
}

fn worker_loop(rx: Receiver<Cmd>) {
    let mut pending: Option<ArtJob> = None;
    loop {
        let job = match pending.take() {
            Some(j) => match drain_latest(&rx, j) {
                Ok(j) => j,
                Err(()) => break,
            },
            None => match rx.recv() {
                Ok(Cmd::Job(j)) => match drain_latest(&rx, *j) {
                    Ok(j) => j,
                    Err(()) => break,
                },
                Ok(Cmd::Shutdown) | Err(_) => break,
            },
        };

        match run_job(&rx, job) {
            RunOutcome::Done => {}
            // Newer job was already drained out of the mailbox — must keep it.
            RunOutcome::Superseded(next) => pending = Some(*next),
            RunOutcome::Shutdown => break,
        }
    }
    crate::controller::hid::diag::diag_info("ui-diag: art worker shutdown");
}

#[derive(Debug)]
enum RunOutcome {
    Done,
    /// Mailbox delivered a newer generation; carry it so the loop can run it.
    Superseded(Box<ArtJob>),
    Shutdown,
}

/// Drain mailbox; on generation change return the newer job (do not drop it).
fn continue_or_switch(
    rx: &Receiver<Cmd>,
    job: ArtJob,
    generation: u64,
) -> Result<ArtJob, RunOutcome> {
    match drain_latest(rx, job) {
        Ok(j) if j.generation != generation => Err(RunOutcome::Superseded(Box::new(j))),
        Ok(j) => Ok(j),
        Err(()) => Err(RunOutcome::Shutdown),
    }
}

fn run_job(rx: &Receiver<Cmd>, mut job: ArtJob) -> RunOutcome {
    let generation = job.generation;

    // --- Selected strip portrait first (visible immediately; don't wait on cover) ---
    if let Some(path) = job.selected_hero.clone() {
        if icon_cache::hero_cached(&path).is_none() {
            let _ = icon_cache::hero_for_path(&path);
        }
        if icon_cache::icon_cached(&path).is_none() {
            let _ = icon_cache::handle_for_path(&path);
        }
    }
    if let Some(path) = job.selected_shell.clone() {
        if icon_cache::hero_shell_cached(&path).is_none() {
            let _ = icon_cache::hero_for_shell(&path);
        }
        if icon_cache::icon_shell_cached(&path).is_none() {
            let _ = icon_cache::handle_for_shell(&path);
        }
    }
    job = match continue_or_switch(rx, job, generation) {
        Ok(j) => j,
        Err(outcome) => return outcome,
    };

    // --- Selected cover (splash / ambient) ---
    if let Some(path) = job.selected_backdrop.clone()
        && icon_cache::backdrop_cached(&path).is_none()
    {
        let _ = icon_cache::backdrop_for_path(&path);
    }
    emit(ArtEvent {
        generation,
        kind: ArtEventKind::SelectedReady,
    });
    job = match continue_or_switch(rx, job, generation) {
        Ok(j) => j,
        Err(outcome) => return outcome,
    };

    // --- Visible strip portraits (do not wait on neighbor backdrops) ---
    for path in job.near.heroes.clone() {
        job = match continue_or_switch(rx, job, generation) {
            Ok(j) => j,
            Err(outcome) => return outcome,
        };
        if icon_cache::hero_cached(&path).is_none() {
            let _ = icon_cache::hero_for_path(&path);
        }
    }
    for path in job.near.hero_shells.clone() {
        job = match continue_or_switch(rx, job, generation) {
            Ok(j) => j,
            Err(outcome) => return outcome,
        };
        if icon_cache::hero_shell_cached(&path).is_none() {
            let _ = icon_cache::hero_for_shell(&path);
        }
    }
    for path in job.near.list_files.clone() {
        job = match continue_or_switch(rx, job, generation) {
            Ok(j) => j,
            Err(outcome) => return outcome,
        };
        if icon_cache::icon_cached(&path).is_none() {
            let _ = icon_cache::handle_for_path(&path);
        }
    }
    for path in job.near.list_shells.clone() {
        job = match continue_or_switch(rx, job, generation) {
            Ok(j) => j,
            Err(outcome) => return outcome,
        };
        if icon_cache::icon_shell_cached(&path).is_none() {
            let _ = icon_cache::handle_for_shell(&path);
        }
    }
    emit(ArtEvent {
        generation,
        kind: ArtEventKind::NearReady,
    });
    job = match continue_or_switch(rx, job, generation) {
        Ok(j) => j,
        Err(outcome) => return outcome,
    };

    // --- Near covers (title-change splash prep) after strip heroes ---
    let (_sel, near_bds) = icon_cache::split_selected_backdrop(
        job.selected_backdrop.clone(),
        job.near.backdrops.clone(),
    );
    for path in near_bds {
        job = match continue_or_switch(rx, job, generation) {
            Ok(j) => j,
            Err(outcome) => return outcome,
        };
        if icon_cache::backdrop_cached(&path).is_none() {
            let _ = icon_cache::backdrop_for_path(&path);
        }
    }

    // --- Prefetch ring: heroes before backdrops (cheaper + visible sooner if scrolled) ---
    for path in job.prefetch.heroes.clone() {
        job = match continue_or_switch(rx, job, generation) {
            Ok(j) => j,
            Err(outcome) => return outcome,
        };
        if icon_cache::hero_cached(&path).is_none() {
            let _ = icon_cache::hero_for_path(&path);
        }
    }
    for path in job.prefetch.hero_shells.clone() {
        job = match continue_or_switch(rx, job, generation) {
            Ok(j) => j,
            Err(outcome) => return outcome,
        };
        if icon_cache::hero_shell_cached(&path).is_none() {
            let _ = icon_cache::hero_for_shell(&path);
        }
    }
    for path in job.prefetch.list_files.clone() {
        job = match continue_or_switch(rx, job, generation) {
            Ok(j) => j,
            Err(outcome) => return outcome,
        };
        if icon_cache::icon_cached(&path).is_none() {
            let _ = icon_cache::handle_for_path(&path);
        }
    }
    for path in job.prefetch.list_shells.clone() {
        job = match continue_or_switch(rx, job, generation) {
            Ok(j) => j,
            Err(outcome) => return outcome,
        };
        if icon_cache::icon_shell_cached(&path).is_none() {
            let _ = icon_cache::handle_for_shell(&path);
        }
    }
    let (_sel, prefetch_bds) = icon_cache::split_selected_backdrop(
        job.selected_backdrop.clone(),
        job.prefetch.backdrops.clone(),
    );
    for path in prefetch_bds {
        job = match continue_or_switch(rx, job, generation) {
            Ok(j) => j,
            Err(outcome) => return outcome,
        };
        if icon_cache::backdrop_cached(&path).is_none() {
            let _ = icon_cache::backdrop_for_path(&path);
        }
    }

    emit(ArtEvent {
        generation,
        kind: ArtEventKind::JobDone,
    });
    crate::controller::hid::diag::diag_info(format!("ui-diag: art worker done gen={generation}"));
    RunOutcome::Done
}

/// iced subscription stream of [`ArtEvent`]s.
pub fn event_stream() -> impl iced::futures::Stream<Item = ArtEvent> {
    iced::stream::channel(32, async move |mut output| {
        let (tx, mut rx) = iced_mpsc::unbounded();
        let bind_id = bind_event_sender(tx);
        ensure_started();
        let _guard = EventBindGuard(bind_id);
        while let Some(ev) = rx.next().await {
            if output.send(ev).await.is_err() {
                break;
            }
        }
    })
}

struct EventBindGuard(u64);
impl Drop for EventBindGuard {
    fn drop(&mut self) {
        clear_event_sender(self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_job(generation: u64) -> ArtJob {
        ArtJob {
            generation,
            selected_backdrop: None,
            selected_hero: None,
            selected_shell: None,
            near: ArtRetainSet::default(),
            prefetch: ArtRetainSet::default(),
        }
    }

    #[test]
    fn drain_latest_keeps_newest_generation() {
        let (tx, rx) = mpsc::channel();
        let first = empty_job(1);
        tx.send(Cmd::Job(Box::new(empty_job(2)))).unwrap();
        tx.send(Cmd::Job(Box::new(empty_job(3)))).unwrap();
        let got = drain_latest(&rx, first).expect("no shutdown");
        assert_eq!(got.generation, 3);
    }

    #[test]
    fn continue_or_switch_preserves_newer_job() {
        // Close→reopen race: reopen job is drained mid-run; must not be dropped.
        let (tx, rx) = mpsc::channel();
        tx.send(Cmd::Job(Box::new(empty_job(9)))).unwrap();
        match continue_or_switch(&rx, empty_job(1), 1) {
            Err(RunOutcome::Superseded(next)) => assert_eq!(next.generation, 9),
            other => panic!("expected Superseded(9), got {other:?}"),
        }
    }

    #[test]
    fn shutdown_joins_and_allows_restart() {
        ensure_started();
        assert!(MAILBOX.lock().unwrap().is_some());
        shutdown();
        assert!(MAILBOX.lock().unwrap().is_none());
        assert!(JOIN.lock().unwrap().is_none());
        ensure_started();
        assert!(MAILBOX.lock().unwrap().is_some());
        shutdown();
    }

    #[test]
    fn clear_event_sender_ignores_stale_bind_id() {
        let (tx_a, _rx_a) = iced_mpsc::unbounded();
        let (tx_b, _rx_b) = iced_mpsc::unbounded();
        let id_a = bind_event_sender(tx_a);
        let id_b = bind_event_sender(tx_b);
        assert_ne!(id_a, id_b);
        clear_event_sender(id_a);
        {
            let guard = EVENT_TX.lock().unwrap();
            assert!(guard.is_some(), "newer bind must survive stale clear");
            assert_eq!(guard.as_ref().unwrap().bind_id, id_b);
        }
        clear_event_sender(id_b);
        assert!(EVENT_TX.lock().unwrap().is_none());
    }
}
