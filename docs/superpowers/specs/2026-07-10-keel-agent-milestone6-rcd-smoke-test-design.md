# Milestone 6: `rc.d` Service Integration + End-to-End Smoke Test

Status: Approved
Date: 2026-07-10

## Context

Milestone 5 delivered a working `keel-agentd` binary and `keelctl` CLI,
verified end-to-end on the FreeBSD VM by hand: build, run in the
foreground, drive with `keelctl`, confirm real jails come up and tear
down correctly. What it explicitly deferred (per that milestone's
Non-Goals) was everything about *running keel-agentd as a proper system
service*: an `rc.d` script, daemonization, crash-restart supervision, and
any log output at all (`keel-agentd` currently has zero logging calls
anywhere in the codebase).

This is the last item on the README's roadmap for "sub-project 1: the
single-node jail reconciliation daemon." Per the top-level design spec's
Error Handling section, the intended shape was always "`keel-agentd` runs
under an `rc.d` script with restart-on-crash (`keep_alive`), and logs to
syslog" — this milestone makes that literally true, and proves it with an
automated smoke test on the real VM.

## Goals (Milestone 6)

- An `rc.d` script that starts, stops, and restarts `keel-agentd` using
  standard FreeBSD service-management commands, with configuration
  exposed through `/etc/rc.conf` variables.
- Crash-restart supervision: if `keel-agentd` exits unexpectedly (crashes),
  it is automatically restarted. A deliberate `service keel_agentd stop`
  must **not** trigger a restart.
- Log output reaching syslog: at minimum, a startup message and every
  per-jail reconciliation failure (currently silently discarded in
  `worker.rs`'s `Tick` handling — an open question explicitly left by
  Milestone 5's design spec, closed here).
- An automated, repeatable smoke test script, committed to the repo, that
  exercises the full lifecycle on the real FreeBSD VM: install, start,
  apply a real spec, confirm the jail runs, simulate a crash and confirm
  both process-level restart and correct reconciler state recovery,
  confirm a deliberate stop leaves running jails untouched, clean
  teardown.

## Non-Goals (Milestone 6)

- Any change to `keel-agentd`'s own process model: no self-daemonization,
  no double-fork, no `daemonize`/`signal-hook` crate. `keel-agentd` stays
  exactly the foreground binary Milestone 5 built; `daemon(8)` (already
  part of the base system) does all of detaching, pidfile management, and
  crash-restart supervision.
- A structured logging framework (`tracing`, the `log` facade, a `syslog`
  crate). Plain `eprintln!`/`println!` calls, captured by `daemon(8)`'s
  `-S` flag and forwarded to syslog — no new dependency.
- A custom `SIGTERM` handler. Default signal behavior (process exits,
  running jails are untouched) is already correct per the existing
  "jails outlive the daemon" design invariant; adding a handler here would
  be solving a problem the architecture doesn't have.
- Packaging as a real FreeBSD port/pkg (`Makefile`, `+MANIFEST`, etc.) —
  the smoke test installs the binary and rc.d script directly via `cargo
  build --release` + `install(1)`, matching this project's current
  build-from-source-only scope.
- Multi-node concerns, log rotation policy beyond what `newsyslog(8)`
  already provides system-wide, metrics/health-check endpoints.

## Architecture

### Why `daemon(8)`, not self-daemonization

FreeBSD's base system already ships `daemon(8)`, which detaches a
foreground program from its controlling terminal, manages a pidfile, and
(with `-r`) supervises and restarts it if it exits unexpectedly. Wrapping
`keel-agentd` with `daemon(8)` gets all of this for free, with zero new
code or dependencies in the binary itself — it stays the same simple,
synchronous foreground program Milestone 5 built and already verified.
The alternative (having `keel-agentd` fork/detach/write its own pidfile,
e.g. via the `daemonize` crate) would add real code and a new dependency
to reimplement something the base system already does correctly.

**A verified subtlety:** `daemon(8)` takes two different pidfile flags —
`-p child_pidfile` (records the *child's* PID) and `-P
supervisor_pidfile` (records `daemon(8)`'s *own* PID). Combining `-r`
(restart) with `-p` is a foot-gun explicitly called out in `daemon(8)`'s
own man page: `rc.subr`'s stop procedure signals whatever PID is in the
configured pidfile, so with `-p`, stopping the service kills the *child*
directly, and `daemon(8)` (still running, still watching) immediately
restarts it — `service keel_agentd stop` would appear to do nothing.
Verified directly on the FreeBSD VM (2026-07-10): with `-P
supervisor_pidfile`, killing the PID in that file cleanly stops both
`daemon(8)` and the child with no restart, and the pidfile is removed;
killing the child's PID directly (simulating a crash) is automatically
restarted with a new PID, the supervisor's PID and pidfile unchanged.
This milestone's rc.d script uses `-P`, never `-p`.

### `rc.d` script

New file: `keel-agentd/rc.d/keel_agentd` (underscore, not hyphen — shell
identifiers used by `rc.subr` can't contain hyphens; the script's `name`
and `rcvar` are `keel_agentd`, while `command` points at the
hyphenated `keel-agentd` binary path).

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

Standard `rc.subr` commands all work as usual: `start`/`stop`/`restart`/
`status` act on the enabled, `/etc/rc.conf`-configured service; `onestart`/
`onestop`/`onerestart` do the same without requiring `keel_agentd_enable`
to be set first — this is what the smoke test uses, so it never has to
edit the VM's system-wide `/etc/rc.conf`.

`REQUIRE: NETWORKING` ensures network interfaces are up before
`keel-agentd` starts (jails need `keel0`/VNET wiring); no `REQUIRE` on
ZFS is needed since the pool containing `/var/db/keel` and the jail
datasets is mounted as part of early boot, well before `rc.d` scripts run.

### Logging

Two log call sites, both plain `eprintln!` (no new dependency):

- `main.rs`, once at startup, after `Reconciler::new` succeeds: pool,
  state dir, and socket path, so an operator reading syslog can confirm
  which configuration is actually running.
- `worker.rs`'s `Command::Tick` handling: currently
  `let _ = reconciler.reconcile(Instant::now());`, silently discarding
  the `Vec<(String, ReconcileError)>` of per-jail failures. This becomes
  a loop over that vector, one `eprintln!` per `(jail_name, error)` pair.
  This is the exact gap Milestone 5's design spec left open
  ("Whether `keel-agentd` should log reconciliation failures from `Tick`
  handling anywhere... deferred until a milestone that actually needs
  operator-visible logs" — this milestone).

`Command::Apply`/`Command::Delete` handling is **not** additionally
logged: their outcome already reaches the caller synchronously via the
HTTP response, and logging every request would be scope beyond what this
milestone's goals call for (an operator-visible record of *background*
reconciliation failures, which otherwise have no other visibility at
all). `daemon(8)`'s `-S -T keel_agentd` flag captures everything written
to `keel-agentd`'s stdout/stderr and forwards it to syslog (facility
`daemon`, priority `notice` by default) tagged `keel_agentd` — confirmed
directly on the VM.

### Smoke test script

New file: `scripts/smoke-test.sh`, run manually on the FreeBSD VM (not
part of `cargo test`, since it needs root, real jails, and a real rc.d
install — matching how Milestone 5's own end-to-end VM verification was
never a `cargo test` target either). Sequence:

1. `cargo build --release --workspace`.
2. `install -m 755 target/release/keel-agentd target/release/keelctl
   /usr/local/bin/`.
3. `install -m 555 keel-agentd/rc.d/keel_agentd
   /usr/local/etc/rc.d/keel_agentd`.
4. `service keel_agentd onestart`; poll `service keel_agentd status`
   until it reports running (bounded retries, not an infinite loop).
5. `keelctl apply -f` a real spec; poll `keelctl get` until
   `running: true`.
6. **Crash simulation:** find the actual `keel-agentd` process's PID
   (distinct from `daemon(8)`'s own supervisor PID in the rc.d pidfile —
   `pgrep -f` or `ps` filtered on the binary path), `kill -9` it directly,
   confirm `daemon(8)` restarts it (a new PID appears within a few
   seconds), and confirm via `keelctl get` that the reconciler correctly
   rehydrated its on-disk state and the jail is still `running: true`
   with no duplicate jail created (crash-safety, demonstrated against the
   real system rather than only against fakes as in Milestone 4).
7. `service keel_agentd onestop`; confirm (via `jls`) the jail is **still
   running** — proving "jails outlive the daemon" end-to-end, not just as
   a documented invariant.
8. Clean up the test jail only, not the installation: start the service
   again (`onestart`, since it's needed to reach it via `keelctl`),
   `keelctl delete` the test jail, `onestop` the service. The installed
   binaries and rc.d script are deliberately left in place (see
   Open Questions) so a re-run doesn't need to rebuild/reinstall from
   scratch; `keel_agentd_enable` is never set in `/etc/rc.conf` by this
   script, so the service does not start on the VM's next boot.

The script exits non-zero on any step's failure (`set -e` plus explicit
checks for the polling loops, which must not hang forever — bounded
retry counts with a clear failure message on timeout).

**Readiness race, and why every post-start `keelctl` call must be
retried, not just the `running: true` checks:** `service ... status`'s
notion of "running" is `rc.subr`'s pidfile check — it only confirms the
PID recorded in the pidfile is alive, not that `keel-agentd` has reached
the point of binding its Unix socket. Per `main.rs`'s actual startup
order (reconciler init, then worker-thread spawn, then timer-thread
spawn, and only *after* that the stale-socket cleanup and `bind`), there
is a real window where `daemon(8)` reports the child alive but
`keelctl` still gets a hard connection failure (`failed to connect to
/var/run/keel-agentd.sock: ...`, `keelctl`'s own error path on a missing
or unbound socket). A single unguarded `keelctl` call right after
`onestart` (step 5) or right after the new post-crash PID appears
(step 6) can therefore fail the whole script under `set -e` even though
nothing is actually broken. Steps 5 and 6 must wrap their first
`keelctl` call in the same bounded-retry loop used for the `running:
true` polling — retrying on connection failure, not only on
`running: false` — so the script tolerates this startup window instead
of racing it.

## Error Handling

- If `keel-agentd` crashes repeatedly in a tight loop, `daemon -r`
  restarts it every second indefinitely (per `daemon(8)`'s default
  restart delay) — there is no backoff at the supervisor level. This is
  an accepted v1 behavior: `keel-agentd`'s own internal per-jail backoff
  (Milestone 4) is what prevents a single crash-looping *jail* from
  consuming resources; a crash-looping *daemon itself* is a much rarer,
  more serious failure mode (a real bug) that this milestone doesn't
  attempt to further rate-limit beyond what `daemon(8)` provides
  out of the box.
- Syslog is the only log destination for v1 — no log file, no rotation
  policy beyond whatever the system's existing `newsyslog.conf` already
  applies to `/var/log/messages` (or wherever the `daemon` facility
  routes on a given system's `syslog.conf`).

## Testing Strategy

- No new Rust unit tests are needed for the two `eprintln!` call sites —
  they're direct, unconditional side effects with no branching logic to
  verify beyond "did the loop iterate over the failures vector," which
  the existing `reconcile`/`Tick`-handling tests already exercise
  indirectly (the vector's contents are already asserted elsewhere).
- All verification for this milestone is the smoke test script, run on
  the real FreeBSD VM. This is consistent with Milestone 5's own
  approach: fakes cannot meaningfully exercise `daemon(8)`, `rc.subr`, or
  real process supervision.

## Open Questions / Deferred Decisions

- **Resolved:** the smoke test leaves the installed binaries and rc.d
  script in place after a run (only the test jail is deleted), and never
  sets `keel_agentd_enable` in `/etc/rc.conf`, so the service is present
  but does not start on the VM's next boot and a re-run doesn't need to
  rebuild/reinstall from scratch.
- Whether a future milestone should add supervisor-level backoff (e.g.
  `daemon -R <delay>` instead of `-r`) if a real crash-loop is ever
  observed in production; not needed to close this milestone.
- **Found during VM verification, not anticipated by this spec:**
  `keel-jail`'s `start_command` must give each jailed process its own
  isolated stdio (`/dev/null`) rather than inheriting `keel-agentd`'s own —
  otherwise a long-running jailed process holds open the pipe `daemon(8)
  -S` relies on seeing EOF from to detect `keel-agentd`'s own exit,
  silently breaking crash-restart for any jail with an active command.
  `/dev/null` is the right scope for this milestone (log aggregation is an
  explicit non-goal), but a future log-aggregation milestone will want
  per-jail log files instead — noted here so that isn't silently
  forgotten.
