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
    pgrep keel-agentd | head -1
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
