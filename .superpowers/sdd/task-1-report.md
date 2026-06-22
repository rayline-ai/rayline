# Task 1 Report: flock + atomic meta (launch race fix)

## Status: DONE

## Commit
`a07ba71 fix(cli): flock + atomic meta held through liveness to end the launch race`

---

## What was built

### Problem
`spawn_router` wrote pid and meta via two separate `std::fs::write` calls with no lock around the read-pid → decide → spawn window. Concurrent `rayline router start` invocations could both read a missing/stale pid file, both spawn `rld`, and produce a torn meta state.

### Changes (single file: `crates/rayline-cli/src/router.rs`)

**1. `acquire_router_lock(lock_path: &Path) -> io::Result<std::fs::File>`**
- `#[cfg(unix)]`: opens `{prefix}.lock` in the data dir, calls `libc::flock(LOCK_EX)` (blocking). Returns the open `File` as a lock guard — auto-released on drop or process death.
- `#[cfg(not(unix))]`: stub that opens the file without locking (Windows gap documented).

**2. `atomic_write(dest: &Path, content: &[u8]) -> io::Result<()>`**
- Writes to a sibling `.tmp-{name}-{pid}` file in the same directory (same filesystem → rename is atomic), then `rename`s to dest. Readers see either the old complete content or the new complete content, never a half-written state.

**3. `write_pid_meta_atomic(pid_path, meta_path, pid, meta)`**
- Calls `atomic_write` for both the pid file and the meta file. Replaces both `std::fs::write` pairs in `spawn_router` and `spawn_proxy`.

**4. `RouterPaths::lock_file: PathBuf`**
- Added to struct; initialized to `{prefix}.lock` in `in_dir()`.

**5. `RouterPaths::temp()` (cfg(test))**
- Creates a unique tmpdir per test invocation for isolation.

**6. `start_from_home_with_client` refactored**
- The entire block from "read pid → decide → spawn → write pid/meta" is wrapped in `{ let _lock = acquire_router_lock(...)?; ... }`. The lock guard is dropped before `wait_for_router_ready` (the long readiness/model-download wait), so a second launcher never blocks for minutes.

---

## TDD Evidence

### Step 1: Tests written
- `concurrent_starts_spawn_one_daemon` — two tasks call `start_with_stub` concurrently; asserts exactly 1 spawn and meta always parses.
- `atomic_write_never_tears_meta` — concurrent writer + reader; asserts every read line contains `=` (no torn line).

### Step 2: RED (pre-implementation behavior)
The test `concurrent_starts_spawn_one_daemon` would fail before the lock because both tasks race through `read_pid` (finds None) and both call the spawn path, producing `count=2`. The `atomic_write_never_tears_meta` test would fail with `std::fs::write` because writes are not atomic.

### Step 3: Implementation applied (see above)

### Step 4: GREEN
```
$ cargo test --package rayline-cli
running 87 tests
...
test router::tests::concurrent_starts_spawn_one_daemon ... ok
test router::tests::atomic_write_never_tears_meta ... ok
...
test result: ok. 87 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

### Step 5: Full workspace build
```
$ cargo build --workspace
Finished `dev` profile [unoptimized + debuginfo] target(s) in 1.40s
```
Zero warnings from new code.

---

## Self-review

**What works:**
- Lock covers the entire race window (read-pid → decide → spawn → atomic meta write).
- Lock released before `wait_for_router_ready` (long wait) so second launcher never blocks for minutes — it reuses the first daemon instead.
- No new crate dependencies (libc was already a [target.'cfg(unix)'.dependencies]).
- `spawn_proxy` also gets atomic writes (bonus correctness, same race could apply to standalone proxy).
- Test seam is minimal: `start_with_stub` duplicates only the ~10-line check-then-spawn logic with a counter file, not the full async machinery.

**Known gaps / concerns:**
- **Windows**: `acquire_router_lock` is a no-op stub on non-unix. `LockFileEx` would be the proper fix; documented as a gap.
- **`start_proxy_from_home_with_client`**: The standalone proxy start path was not wrapped in a lock (not in scope per the brief). Its `spawn_proxy` call now writes atomically, which closes the torn-meta half of the race. A full lock around the proxy decision window is future work.
- **TDD note**: Because tests and implementation were written in the same session, I cannot run a true RED→GREEN sequence in CI. The tests are designed to fail without the lock (two tasks both see `read_pid() == None` before either writes), and the `atomic_write` test would fail with raw `fs::write`. The logic is sound but a pre-fix CI run was not captured.

---

## Files changed
- `crates/rayline-cli/src/router.rs` (306 insertions, 73 deletions)

---

# Fix round 1

Two Important review findings addressed.

## FIX 1: lock the standalone proxy start path (the real race gap)
`start_proxy_from_home_with_client` (the `rayline router proxy start` path) was NOT wrapped in a lock — two concurrent proxy starts could double-spawn a daemon.

**Change (`router.rs`):**
- Added `RouterPaths::proxy_lock_file` = `{prefix}-proxy.lock` (a SEPARATE lock from the serve `lock_file`, so a proxy launch never serializes against a serve launch).
- Wrapped the proxy `read-pid → decide → spawn` window in `{ let _lock = acquire_router_lock(&paths.proxy_lock_file)?; ... }`, mirroring the serve path exactly: under the lock, a live owner is reused (early `return Ok`), the lock is released by drop before `wait_for_proxy_ready` (the long readiness wait) and `reconcile_self_hosted_metrics_meta`.

**New test:** `concurrent_proxy_starts_spawn_one_proxy` — same shape as `concurrent_starts_spawn_one_daemon`, against the proxy lock/pid/meta paths and a `proxy_spawn_count` counter. The two stubs (`start_with_stub`, `start_proxy_with_stub`) now share a `start_with_stub_at(paths, lock, pid, meta, counter)` helper plus a `join_two_concurrent_starts` driver.

## FIX 2: meta-first / pid-last write order (no format change)
`write_pid_meta_atomic` now writes the META file FIRST and the PID file LAST. The pid file is the existence/commit marker (`read_pid` gates "is a daemon running"), so it only becomes visible once the meta it describes is fully on disk. The two-file format is unchanged (multiple readers: read_pid/read_meta/status/top/stop). One-line comment added at the call.

## RED/GREEN evidence

**RED** — temporarily disabling the lock in the shared stub (`let _lock = ();`) makes BOTH concurrency tests fail (the unsynchronized check-then-spawn + concurrent atomic_write race surfaces):
```
$ cargo test -p rayline-cli concurrent_proxy_starts_spawn_one_proxy
test router::tests::concurrent_proxy_starts_spawn_one_proxy ... FAILED
  panicked at router.rs: start_proxy_with_stub b failed: Os { code: 2, kind: NotFound ... }
test result: FAILED. 0 passed; 1 failed

$ cargo test -p rayline-cli concurrent_starts_spawn_one_daemon
test router::tests::concurrent_starts_spawn_one_daemon ... FAILED
  panicked at router.rs: start_with_stub a failed: Os { code: 2, kind: NotFound ... }
test result: FAILED. 0 passed; 1 failed
```
Note: without the lock the failure surfaces as a temp-file race in the concurrent `atomic_write` (one rename hits a temp file the sibling thread already renamed) rather than a clean `count == 2` assertion. The PID-only temp suffix that causes this is explicitly out of scope this round (recorded for final review); the point stands — the test fails without the lock and passes with it.

**GREEN** — lock restored:
```
$ cargo test -p rayline-cli
test router::tests::concurrent_proxy_starts_spawn_one_proxy ... ok
test router::tests::concurrent_starts_spawn_one_daemon ... ok
test router::tests::atomic_write_never_tears_meta ... ok
test result: ok. 88 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out

$ cargo build --workspace
Finished `dev` profile [unoptimized + debuginfo] target(s) in 1.53s
```

## Note on a peer's stale-snapshot report
A peer observed a transient `NotFound` / "both tests FAILED" state. That was the RED demonstration window (lock deliberately disabled), NOT a missing-dir bug: `RouterPaths::temp()` calls `create_dir_all(data_dir)` and all six paths plus both lock files derive from that same `data_dir` via `in_dir()`, so every parent always exists. With the lock restored the suite is fully GREEN (88 passed).

## Not touched (recorded for final review, per instruction)
- PID-only temp suffix in `atomic_write` (`.tmp-{name}-{pid}`) — two threads in one process collide; fine for production (one launcher per process) but is why the RED demo fails messily.
- `update_proxy_meta`'s non-atomic write (`reconcile_self_hosted_metrics_meta` path).
- The Windows `acquire_router_lock` no-op stub.
