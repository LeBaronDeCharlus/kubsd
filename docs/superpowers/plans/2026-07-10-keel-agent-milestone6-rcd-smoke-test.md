# keel-agentd Milestone 6: rc.d Service Integration + End-to-End Smoke Test — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.
>
> **Special note on Task 4:** it needs the real FreeBSD VM (`root@192.168.64.2`).
> The coordinating session has direct SSH access to this VM and should run
> this task itself rather than dispatching a subagent for it — this mirrors
> Milestone 5's Task 8. **Tasks 1-3 are pure file edits/creation, verified
> locally (macOS), and need no FreeBSD VM interaction at all.**

**Goal:** Make `keel-agentd` a real FreeBSD system service: an `rc.d`
script that starts/stops/restarts it via `daemon(8)` (with automatic
restart-on-crash), two new log lines reaching syslog, and a committed,
repeatable smoke test script that proves the whole lifecycle end-to-end
on the real VM.

**Architecture:** `keel-agentd` itself gets no new code for daemonization —
it stays exactly the foreground binary Milestone 5 built. A new `rc.d`
script wraps it with the base system's `daemon(8)` utility using `-r`
(restart-on-crash) and `-P` (supervisor pidfile, not `-p` — see Global
Constraints for why this distinction is load-bearing) for correct
start/stop/crash-restart semantics, and `-S` to route its stdout/stderr to
syslog. Two `eprintln!` call sites (daemon startup, per-jail reconcile
failures in the timer's `Tick` handling) are the only Rust changes.

**Tech Stack:** Rust (2021 edition, no new crate dependencies), POSIX
`/bin/sh` for the `rc.d` script and the smoke test script (FreeBSD's base
shell, not bash — no bashisms), FreeBSD's base-system `daemon(8)`/`rc.subr`.

## Global Constraints

- Design spec: `docs/superpowers/specs/2026-07-10-keel-agent-milestone6-rcd-smoke-test-design.md` (Approved). The `rc.d` script content, logging call sites, and smoke test sequence there must match exactly.
- **No new Rust dependencies.** No `daemonize`, no `signal-hook`, no `syslog`/`tracing` crate. Logging is plain `eprintln!`; daemonization is entirely `daemon(8)`'s job.
- **`-P` (supervisor pidfile), never `-p` (child pidfile), in the `rc.d` script's `daemon` invocation.** Verified directly on the FreeBSD VM (2026-07-10): with `-p` combined with `-r`, `service keel_agentd stop` would kill the child directly, and `daemon -r` (still running, still watching) would immediately restart it — the stop command would silently fail to stop anything. `-P` records `daemon(8)`'s own PID instead, so stopping it correctly forwards the signal to the child and does not restart, since `daemon(8)` itself is also exiting.
- No custom `SIGTERM` handler in `keel-agentd` — default terminate-on-signal behavior is correct and already relied upon (jails outlive the daemon).
- The smoke test script must never hang: every polling/retry loop has a bounded attempt count and a clear failure message on timeout, per the design's "must not hang forever" requirement.
- **The readiness race:** `service keel_agentd status` (and a freshly-restarted process's PID appearing) only proves the process is alive, not that it has reached `UnixListener::bind` yet (per `main.rs`'s actual startup order: reconciler init → worker/timer threads spawned → *then* stale-socket cleanup and bind). Every `keelctl` call that follows a `service ... onestart`/`onerestart` or a simulated crash must be wrapped in a retry loop that tolerates a connection failure, not just a `running: false` response — this is the exact bug the design spec's addendum calls out, and skipping it will make the smoke test flaky under `set -e`.
- No placeholders: Tasks 1-3 produce buildable/syntax-valid artifacts, verified with `cargo build`/`cargo test`/`sh -n` as appropriate; Task 4 is the only real functional verification (Milestones 1-5 already established fakes cannot exercise `daemon(8)`/`rc.subr`/real process supervision, so there is nothing meaningful for a unit test to assert here beyond what compiles).

---

### Task 1: Logging — daemon startup and per-jail reconcile failures

**Files:**
- Modify: `keel-agentd/src/main.rs`
- Modify: `keel-agentd/src/worker.rs`

**Interfaces:**
- Consumes: `Reconciler::reconcile(&mut self, now: Instant) -> Vec<(String, ReconcileError)>` (existing, unchanged), `ReconcileError`'s existing `Display` impl (from `thiserror`, already used elsewhere via `.to_string()`).
- Produces: no new public interface — both changes are `eprintln!` side effects only.

- [ ] **Step 1: Log daemon startup in `main.rs`**

Modify `keel-agentd/src/main.rs`'s `fn main()`:

```rust
fn main() {
    let config = parse_args();

    let reconciler = Reconciler::new(
        ProcessJailRuntime::new(),
        CliZfsManager::new(),
        ProcessNetManager::new(),
        config.pool.clone(),
        config.state_dir.clone(),
    )
    .expect("failed to initialize reconciler from on-disk state");

    eprintln!(
        "keel-agentd: starting (pool={}, state_dir={}, socket={})",
        config.pool,
        config.state_dir.display(),
        config.socket.display()
    );

    let (_worker_handle, commands) = worker::spawn(reconciler);
```

(The rest of `main()` — timer thread spawn, socket bind/permissions, `http::run` call — is unchanged. Only `config.pool`/`config.state_dir` gained `.clone()` calls, since the new `eprintln!` needs them after they're moved into `Reconciler::new`.)

- [ ] **Step 2: Log per-jail reconcile failures in `worker.rs`**

Modify `keel-agentd/src/worker.rs`'s `handle_command`, in the `Command::Tick` arm:

```rust
        Command::Tick => {
            for (name, error) in reconciler.reconcile(Instant::now()) {
                eprintln!("keel-agentd: reconcile error for jail '{name}': {error}");
            }
        }
```

(This replaces the previous `let _ = reconciler.reconcile(Instant::now());` — same call, but the returned per-jail failures are now logged instead of discarded.)

- [ ] **Step 3: Build and run the full test suite**

Run: `cargo build --workspace && cargo test --workspace`
Expected: builds cleanly, all 96 existing tests still pass (no test asserted on the previously-discarded return value of `reconcile` inside `Tick` handling, so behavior is unchanged from every test's point of view — only a new, unconditional side effect was added).

- [ ] **Step 4: Manually confirm the log lines print**

Run: `cargo run -p keel-agentd -- --pool zroot --state-dir /tmp/keel-m6-smoke --socket /tmp/keel-m6-smoke.sock 2>&1 | head -1 &`
then after a couple seconds: `kill %1` (or `pkill -f "keel-agentd.*keel-m6-smoke"`)
Expected: the first line of output is `keel-agentd: starting (pool=zroot, state_dir=/tmp/keel-m6-smoke, socket=/tmp/keel-m6-smoke.sock)`. (This is a real daemon run, so `ProcessJailRuntime`/`CliZfsManager`/`ProcessNetManager` will fail at actual jail/zfs/network operations on macOS — that's expected and irrelevant here, this step only confirms the startup log line itself prints correctly before any of that matters.)

Clean up: `rm -rf /tmp/keel-m6-smoke /tmp/keel-m6-smoke.sock`

- [ ] **Step 5: Commit**

```bash
git add keel-agentd/src/main.rs keel-agentd/src/worker.rs
git commit -m "Add startup and per-jail reconcile-failure logging to keel-agentd"
```

---

### Task 2: `rc.d` script

**Files:**
- Create: `keel-agentd/rc.d/keel_agentd`

**Interfaces:**
- Consumes: the `keel-agentd` binary (existing), FreeBSD's base-system `daemon(8)` and `rc.subr`.
- Produces: an installable `rc.d` script. Task 4 is its first real (VM) exercise.

- [ ] **Step 1: Write the script**

Create `keel-agentd/rc.d/keel_agentd`:

```sh
#!/bin/sh
#
# PROVIDE: keel_agentd
# REQUIRE: NETWORKING
# KEYWORD: shutdown

. /etc/rc.subr

name="keel_agentd"
rcvar="keel_agentd_enable"

load_rc_config "$name"

: ${keel_agentd_enable:="NO"}
: ${keel_agentd_bin:="/usr/local/bin/keel-agentd"}
: ${keel_agentd_pool:="zroot"}
: ${keel_agentd_state_dir:="/var/db/keel"}
: ${keel_agentd_socket:="/var/run/keel-agentd.sock"}

pidfile="/var/run/${name}.pid"
command="/usr/sbin/daemon"
command_args="-r -P ${pidfile} -S -T ${name} -- \
  ${keel_agentd_bin} --pool ${keel_agentd_pool} \
  --state-dir ${keel_agentd_state_dir} --socket ${keel_agentd_socket}"

run_rc_command "$1"
```

- [ ] **Step 2: Make it executable and syntax-check it**

```bash
chmod 555 keel-agentd/rc.d/keel_agentd
sh -n keel-agentd/rc.d/keel_agentd
echo "exit code: $?"
```

Expected: exit code `0` (a syntax-only check — `/etc/rc.subr` doesn't exist on macOS so the script can't actually *run* here, but `sh -n` parses it without executing `.`-sourced files, which is enough to catch typos/quoting mistakes before it reaches the VM).

- [ ] **Step 3: Commit**

```bash
git add keel-agentd/rc.d/keel_agentd
git commit -m "Add rc.d script for keel-agentd, wrapping daemon(8) for supervision"
```

---

### Task 3: Smoke test script

**Files:**
- Create: `scripts/smoke-test.sh`

**Interfaces:**
- Consumes: `keel-agentd`/`keelctl` release binaries (built by the script itself), the `rc.d` script from Task 2, `service(8)`, `daemon(8)`, `jls(8)`, `pgrep(1)`.
- Produces: an installable, repeatable smoke test. Task 4 is its first real (VM) run.

- [ ] **Step 1: Write the script**

Create `scripts/smoke-test.sh`:

```sh
#!/bin/sh
#
# End-to-end smoke test for keel-agentd running as a real rc.d-managed
# service. Must be run as root on a FreeBSD host with jails/ZFS/VNET
# already set up (see docs/superpowers/specs/2026-07-05-keel-agent-design.md's
# prerequisites) and a populated zroot/keel/base/test dataset (see
# docs/superpowers/plans/2026-07-09-keel-agent-milestone5-http-cli.md's
# Task 8 Step 2).
#
# Usage: ./scripts/smoke-test.sh

set -eu

REPO_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_DIR"

JAIL_NAME="smoke-test"
SPEC_FILE="/tmp/keel-smoke-test-spec.yaml"
MAX_ATTEMPTS=20
RETRY_DELAY=0.5

log() {
    echo "[smoke-test] $*"
}

fail() {
    echo "[smoke-test] FAILED: $*" >&2
    exit 1
}

wait_for_service_status() {
    attempt=0
    while [ "$attempt" -lt "$MAX_ATTEMPTS" ]; do
        if service keel_agentd status >/dev/null 2>&1; then
            return 0
        fi
        attempt=$((attempt + 1))
        sleep "$RETRY_DELAY"
    done
    fail "service keel_agentd did not report running after $MAX_ATTEMPTS attempts"
}

# Retries on ANY keelctl failure, not just a "running: false" response —
# right after (re)start, keelctl can hit a connection failure because the
# process is alive (service status already confirms that) but hasn't
# reached UnixListener::bind yet. See this plan's Global Constraints.
keelctl_retry() {
    attempt=0
    while [ "$attempt" -lt "$MAX_ATTEMPTS" ]; do
        if output=$(/usr/local/bin/keelctl "$@" 2>&1); then
            echo "$output"
            return 0
        fi
        attempt=$((attempt + 1))
        sleep "$RETRY_DELAY"
    done
    fail "keelctl $* did not succeed after $MAX_ATTEMPTS attempts (last output: $output)"
}

wait_for_running() {
    attempt=0
    while [ "$attempt" -lt "$MAX_ATTEMPTS" ]; do
        if /usr/local/bin/keelctl get "$JAIL_NAME" 2>/dev/null | grep -q "running: true"; then
            return 0
        fi
        attempt=$((attempt + 1))
        sleep "$RETRY_DELAY"
    done
    fail "jail '$JAIL_NAME' did not reach running: true after $MAX_ATTEMPTS attempts"
}

find_keel_agentd_pid() {
    pgrep -f "/usr/local/bin/keel-agentd" | head -1
}

log "building release binaries..."
cargo build --release --workspace

log "installing binaries to /usr/local/bin..."
install -m 755 target/release/keel-agentd target/release/keelctl /usr/local/bin/

log "installing rc.d script..."
install -m 555 keel-agentd/rc.d/keel_agentd /usr/local/etc/rc.d/keel_agentd

log "starting keel_agentd..."
service keel_agentd onestart
wait_for_service_status
log "service is running"

log "writing test spec..."
cat > "$SPEC_FILE" <<EOF
apiVersion: keel/v1
kind: Jail
metadata:
  name: ${JAIL_NAME}
spec:
  image: base/test
  command: ["/bin/sh", "-c", "while true; do :; done"]
  network:
    vnet: true
    bridge: keel0
    address: 10.0.0.20/24
  resources:
    cpu: "1"
    memory: 256M
  restartPolicy: Always
EOF

log "applying test spec..."
keelctl_retry apply -f "$SPEC_FILE"
wait_for_running
log "jail is running"

log "simulating a crash..."
old_pid=$(find_keel_agentd_pid)
[ -n "$old_pid" ] || fail "could not find a running keel-agentd process"
kill -9 "$old_pid"

attempt=0
new_pid=""
while [ "$attempt" -lt "$MAX_ATTEMPTS" ]; do
    new_pid=$(find_keel_agentd_pid)
    if [ -n "$new_pid" ] && [ "$new_pid" != "$old_pid" ]; then
        break
    fi
    attempt=$((attempt + 1))
    sleep "$RETRY_DELAY"
done
[ -n "$new_pid" ] && [ "$new_pid" != "$old_pid" ] || fail "keel-agentd did not restart after being killed (old pid $old_pid)"
log "keel-agentd restarted with new pid $new_pid (was $old_pid)"

jail_count=$(keelctl_retry get | grep -c "name: ${JAIL_NAME}" || true)
[ "$jail_count" -eq 1 ] || fail "expected exactly 1 jail named '$JAIL_NAME' after restart, found $jail_count"
wait_for_running
log "reconciler correctly recovered state after crash restart, no duplicate jail"

log "stopping keel_agentd..."
service keel_agentd onestop
sleep 1
if ! jls | grep -q "keel-${JAIL_NAME}"; then
    fail "jail was torn down when the daemon stopped (jails must outlive the daemon)"
fi
log "jail is still running after daemon stop (jails outlive the daemon, confirmed)"

log "restarting keel_agentd to clean up the test jail..."
service keel_agentd onestart
wait_for_service_status
keelctl_retry delete "$JAIL_NAME"
service keel_agentd onestop
rm -f "$SPEC_FILE"

log "SMOKE TEST PASSED"
```

- [ ] **Step 2: Make it executable and syntax-check it**

```bash
chmod 755 scripts/smoke-test.sh
sh -n scripts/smoke-test.sh
echo "exit code: $?"
```

Expected: exit code `0`.

- [ ] **Step 3: Commit**

```bash
git add scripts/smoke-test.sh
git commit -m "Add end-to-end smoke test script for the rc.d-managed keel-agentd service"
```

---

### Task 4: FreeBSD VM verification

**Files:** none expected (verification only), unless the VM run surfaces a real bug in Tasks 1-3's artifacts — if so, fix it there, following the same practice Milestone 5's Task 8 established (fix on macOS with a regression test/plan update where applicable, re-verify, then re-run the affected VM steps).

- [ ] **Step 1: Sync the repo on the VM**

```bash
ssh root@192.168.64.2 "cd kubsd && git pull && cargo build --release --workspace"
```

Expected: builds successfully. (`kubsd` is the VM's actual, never-renamed clone directory — see Milestone 5's Task 8 for why.)

- [ ] **Step 2: Confirm `zroot/keel/base/test` still has real content**

```bash
ssh root@192.168.64.2 "ls /zroot/keel/base/test/bin /zroot/keel/base/test/sbin 2>&1"
```

Expected: shows `sh` under `bin` (populated during Milestone 5's Task 8 — if somehow missing, re-populate per that task's Step 2 instructions before continuing).

- [ ] **Step 3: Run the smoke test script**

```bash
ssh root@192.168.64.2 "cd kubsd && sh scripts/smoke-test.sh"
```

Expected: prints each `[smoke-test] ...` progress line in order, ending with `[smoke-test] SMOKE TEST PASSED`, exit code 0. If any step fails, the script prints a `[smoke-test] FAILED: ...` message identifying exactly which check failed — use that to diagnose (real bug in the rc.d script, the smoke test script itself, or `keel-agentd`/`keelctl`) before re-running.

- [ ] **Step 4: Confirm syslog captured output under the right tag**

```bash
ssh root@192.168.64.2 "grep keel_agentd /var/log/messages | tail -20"
```

Expected: at least one line containing `keel-agentd: starting (pool=zroot, state_dir=/var/db/keel, socket=/var/run/keel-agentd.sock)`, from the daemon's (re)starts during the smoke test.

- [ ] **Step 5: Confirm no leftover state on the VM**

```bash
ssh root@192.168.64.2 "jls; zfs list -r zroot/keel; service keel_agentd status || true"
```

Expected: no `keel-smoke-test` jail in `jls`, no leftover `zroot/keel/jails/smoke-test` dataset (the smoke test's own Step 8 cleanup should have already removed both), and `service keel_agentd status` reports not running (the smoke test's last action is `service keel_agentd onestop`). The installed binaries (`/usr/local/bin/keel-agentd`, `/usr/local/bin/keelctl`) and rc.d script are expected to remain — per the design spec's resolved Open Question, the smoke test deliberately doesn't uninstall itself.

- [ ] **Step 6: Record the outcome**

If every step above passed with no code changes needed, note in a follow-up commit message (or the final review) that Milestone 6 was VM-verified on this date, including the syslog line observed in Step 4. If the VM surfaced a real bug, fix it, add/adjust the relevant regression coverage or plan text, re-verify with `cargo test --workspace` and a re-run of the smoke test, before considering the milestone done.

---

## Milestone Exit Criteria

- `keel-agentd` requires no new Rust dependencies and no change to its process model — it remains the plain foreground binary Milestone 5 built.
- `keel-agentd/rc.d/keel_agentd` correctly starts, stops, and restarts the service via `daemon(8)`, with `service keel_agentd stop` never triggering `daemon -r`'s restart (verified via `-P`, not `-p`).
- `scripts/smoke-test.sh` passes end-to-end on the real FreeBSD VM: apply → running, crash → automatic restart with correct state recovery and no duplicate jail, stop → jail keeps running, clean teardown.
- Syslog receives both the daemon startup line and (if triggered) per-jail reconcile failure lines, under the `keel_agentd` tag.
- `cargo test --workspace` still passes (96/96, unchanged from Milestone 5 — this milestone adds no new automated Rust tests, per the design spec's Testing Strategy).
