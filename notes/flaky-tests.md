# Flaky tests: shared global state vs. test threads

How to diagnose a test that fails intermittently in CI but never locally, and the
one real instance of it in this repo.

## Symptom

`cargo test` reports `494 passed; 1 failed` with a *different* slow time than
usual (0.72s vs 0.15s — the loaded box widened the race window). Re-running
passes. `git stash` + rerun changes *which* test fails, or makes it vanish.

**That last part is the tell.** A genuinely order-dependent test does not care
about your diff; adding or removing a test perturbs thread scheduling and moves
the collision around. If a change "fixes" a flake, suspect the change.

## Diagnosing

Default `cargo test` runs tests on N threads sharing one process. Anything
process-global — `static`, `OnceLock`, `LazyLock`, `AtomicU64` — is shared
across tests running concurrently.

Reproduce by turning threads up rather than waiting for luck:

```bash
cargo test --lib                                   # build once
BIN=$(ls -t target/debug/deps/sdsc_utils-*.exe | head -1)
for i in $(seq 1 400); do
  $BIN --test-threads=16 2>&1 | grep -E "^failures:" -A 5
done
```

`--test-threads=16` on a loaded box reproduced a ~1-in-150 flake that 40
sequential `cargo test` runs missed entirely. Grep `-E "^failures:" -A 5` so you
capture the **name**; `test result: FAILED` alone tells you nothing.

Then, in the failing test's neighbourhood, look for a global the test reads or
mutates, and ask: *who else touches it, and are they serialized?*

## The instance in this repo

`ui::start::icon_cache` holds a process-wide decode cache (`CACHE`), and its
tests guard it with `with_isolated_cache` → `with_cache_lock`. Ten tests in
`ui::start::view` take the same lock for the same reason.

`art_worker::shutdown()` calls `wipe_art_caches()` → `icon_cache::clear_all()`,
which wipes that same shared cache — and `art_worker`'s own test
`shutdown_joins_and_allows_restart` called it **without the lock**. So a
concurrent `icon_cache` test could decode a PNG, have the cache wiped
mid-assertion, and fail a decode that had succeeded:

```
---- ui::start::icon_cache::tests::filter_uncached_skips_warm_hits stdout
assertion failed: backdrop_for_path(&warm).is_some()
```

Fixed by having that test take `icon_cache::with_cache_lock`. Its siblings
(`drain_latest`, `continue_or_switch`, `clear_event_sender`) use local channels
and never reach the cache, so they needed nothing.

### Rules that fall out

- **Any test that mutates a shared global takes that global's test lock**, even
  if the assertion it makes looks unrelated. `shutdown()` wiping a cache is the
  canonical surprise.
- **Grep for the lock's name** (`with_cache_lock`) to find every participant,
  then check each one actually takes it. `with_isolated_cache`'s own comment
  already admitted the hazard: *"do not bump clear_epoch (that is Exit-only and
  races parallel view tests that decode into the shared process cache)"*.
- **Suspicious slowness is evidence.** A run that takes 4× longer is a wider
  race window, not a slower test.

## Not the cause (ruled out)

Checked while chasing this, so they need not be re-checked:

- **Wall-clock assertions in `ui::start`/`ui::toast`.** These use relative
  clocks (`let now = Instant::now(); ... now + Duration::from_millis(n)`),
  which are immune to scheduling. `motion`, `backdrop` and `layout` advance
  fixed `Duration`s explicitly.
- **`domain::color`'s `SPECTRUM`.** `active_spectrum_is_used` does mutate a
  process-global and reset it — a genuine smell — but no *test* other than
  itself reads `color_for_battery_percent`, so it cannot currently fail another
  test. Left alone; revisit if a color-reading test is added.
- **`HashMap`/`HashSet` iteration order.** SipHash randomizes per process, so an
  order-dependent test would fail on *some* fraction of 400 runs, not 1-in-150.
- **Pure-load stress.** 8 spinners on 16 cores changed nothing; the failure
  needs the *specific* interleaving, not just CPU contention.