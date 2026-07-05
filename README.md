# kubsd

A Kubernetes-style orchestration platform for FreeBSD, built on jails, ZFS,
and VNET networking.

## Motivation

FreeBSD has mature, battle-tested primitives for isolation and resource
control — jails, ZFS snapshots/clones, `rctl(8)` resource limits, VNET
per-jail networking — but nothing that ties them together the way
Kubernetes ties together Linux namespaces, cgroups, and overlay networking.

Existing FreeBSD jail tools (`bastille`, `ezjail`, `cbsd`, plain
`jail.conf`) are good at *creating* jails, but none of them are
*reconciliation-based*: none continuously watch a declarative spec and
drive the live system to match it, restart what crashed, clean up what was
removed, or survive their own restarts without losing track of what they
manage. That gap — declarative, self-healing orchestration for FreeBSD — is
what kubsd is for.

## Why FreeBSD

Jails, ZFS, and VNET are not bolted-on features; they're core, long-lived
parts of the base system. That means kubsd can be a comparatively thin
layer: most of what a container orchestrator normally has to build
(copy-on-write filesystem layers, resource accounting, network namespace
isolation) is already correct and well-tested at the OS level.

## How this differs from Kubernetes

kubsd borrows Kubernetes' *shape* — declarative specs, a reconciliation
loop, a CLI that mirrors `kubectl` — but it is not trying to be a
drop-in replacement, and today it is far smaller in scope:

| | Kubernetes | kubsd (current) |
|---|---|---|
| Workload unit | Pod (Linux containers) | Jail |
| Isolation | namespaces + cgroups | jails + `rctl(8)` |
| Filesystem | overlay/container images | ZFS clones of a base dataset |
| Networking | CNI, overlay networks, Services | VNET + `epair(4)` + `bridge(4)`, static IPs |
| Scope | multi-node cluster, scheduler | single node (kubelet-equivalent only) |
| Control plane | API server + etcd + scheduler | none yet — one local daemon per host |

In other words: what exists today is the FreeBSD analog of a **kubelet**,
not a full cluster. There is no scheduler, no multi-node API server, and no
cluster networking yet — see Roadmap below.

## Why you'd use it

- **Declarative jails.** Describe a jail (image, command, resources,
  network, restart policy) as a spec; apply it; the daemon makes reality
  match it, continuously — not a one-shot script.
- **Self-healing.** Crashed jails restart automatically per policy, with
  crash-loop backoff so a persistently broken jail doesn't spin forever.
- **Safe by construction.** The daemon only ever touches jails it created
  (name-prefixed and tracked in its own state), so it can share a host with
  other jails or tooling without stepping on them. State is crash-safe:
  killing the daemon or the whole VM never leaves it confused about what it
  manages.
- **Fast provisioning.** Jail root filesystems are ZFS clones of a base
  image, so creating a new jail is close to instant and cheap on disk.

## Status

Early days — no working code yet. The design is written and approved; the
first implementation milestone (FreeBSD dev environment + the jail-spec
parser/validator crate) is about to start.

- Design spec: [`docs/superpowers/specs/2026-07-05-kubsd-agent-design.md`](docs/superpowers/specs/2026-07-05-kubsd-agent-design.md)
- Current implementation plan: [`docs/superpowers/plans/2026-07-05-kubsd-agent-milestone1-env-and-spec.md`](docs/superpowers/plans/2026-07-05-kubsd-agent-milestone1-env-and-spec.md)

## Roadmap

**Sub-project 1: kubsd-agent** (single-node jail reconciliation daemon — in progress)

1. FreeBSD dev environment + `kubsd-spec` (jail YAML schema, parsing, validation) — *current milestone*
2. `kubsd-jail` (jail lifecycle via `jail(8)`/`jls(8)`/`rctl(8)`) and `kubsd-zfs` (ZFS clone provisioning)
3. `kubsd-net` (VNET + `epair(4)` + `bridge(4)` wiring)
4. `kubsd-agentd` reconciliation core (desired vs. observed state, crash-loop backoff, crash-safe persistence) tested against fakes of the above
5. Local HTTP API + `kubsdctl` CLI, wired to the real jail/ZFS/net implementations
6. `rc.d` service integration and an end-to-end smoke test on the FreeBSD VM

**Not yet designed** (future sub-projects, each will get its own spec):

- Multi-node control plane (API server, cluster state store)
- Scheduler (bin-packing jails across nodes)
- Cluster networking (cross-node overlay, service discovery/load balancing)
- Storage orchestration beyond a single host's ZFS pool
- bhyve VM workloads alongside jails

## Platform support

FreeBSD only. No plans to support other BSDs (NetBSD, OpenBSD) or Linux —
the design leans directly on FreeBSD-specific primitives (jails, `rctl`,
VNET, ZFS) rather than abstracting over multiple OSes.
