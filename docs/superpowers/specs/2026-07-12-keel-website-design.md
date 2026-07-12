# Keel project website

Status: Approved
Date: 2026-07-12

## Context

Keel currently has no public-facing presentation beyond its README: no
place to point someone who wants a quick "what is this and why" without
reading the full milestone history, no rendered documentation for the
`JailSpec` YAML format, `keelctl`, or the two HTTP APIs, and no
standalone quickstart separate from `scripts/smoke-test.sh`. This
project adds a static website, built directly from content already
established in the README and the crates themselves, and deployed via
GitHub Pages.

The site is plain HTML/CSS with no build tooling or JavaScript
framework, consistent with the project's own dependency-light ethos
(hand-rolled HTTP parsing, no async runtime, no logging framework). A
single shared stylesheet and a small include-free templating approach
(each page is a complete, self-contained HTML file with a repeated nav)
keeps it simple to maintain by hand at the project's current size (9
pages).

## Goals

- A `site/` directory at the repo root containing a complete static
  website: home, why, quickstart, journey, and a four-page documentation
  section (architecture, JailSpec reference, CLI reference, HTTP API
  reference).
- A GitHub Actions workflow that deploys `site/` to GitHub Pages on every
  push to `main`, with no build/compile step (the HTML is served as-is).
- Visual design consistent with the existing Keel logo (deep red/orange
  gradient, ship's-wheel-and-keel mark): white background, dark slate
  body text, deep red as the primary accent, orange as a secondary
  accent, system sans-serif font stack (no external font fetches), card-
  based feature grid on the homepage, monospace code blocks throughout.
- Shared navigation across all pages: `Keel [logo] | Why | Docs (with
  sub-nav) | Quickstart | Journey | GitHub (external link)`.
- Documentation content that is technically accurate against the current
  code, not aspirational: the JailSpec reference reflects the actual
  fields in `keel-spec/src/types.rs` (`apiVersion`, `kind`, `metadata.name`,
  `spec.image`, `spec.command`, `spec.network.{vnet,bridge,address}`,
  `spec.resources.{cpu,memory}`, `spec.restartPolicy`); the CLI reference
  reflects `keelctl`'s actual three subcommands (`apply -f FILE`, `get
  [name]`, `delete NAME`, plus the shared `--socket PATH` flag); the HTTP
  API reference reflects `keel-agentd`'s actual routes (`PUT /jails/:name`,
  `GET /jails`, `GET /jails/:name`, `DELETE /jails/:name` over a Unix
  socket, with 400/404/409 error cases) and `keel-controlplane`'s actual
  routes (`POST /nodes/register`, `POST /nodes/:id/heartbeat`, `GET /nodes`
  over TCP).
- The quickstart walks through building from source and running a real
  local demo (FreeBSD VM + Rust toolchain prerequisites, clone and
  `cargo build`, write a `jail.yaml`, run `keel-agentd`, `keelctl apply`,
  `keelctl get`, confirm with `jls`), and points to
  `scripts/smoke-test.sh` as the fully automated version of the same
  flow.
- The journey page carries the Milestone 1-7 narrative from the README
  at its current level of detail (including the specific bugs found on
  the FreeBSD VM), since it's a meaningful signal of the project's
  verification discipline and doesn't belong buried at the bottom of a
  README once a dedicated site exists.

## Non-Goals

- No static site generator (mdBook, Zola, Docusaurus, etc.) and no
  client-side JavaScript. Plain hand-written HTML/CSS only.
- No search, no versioned docs, no dark mode toggle. The site describes
  the project as it exists today; these can be added later if the
  project's audience grows enough to need them.
- No changes to the README itself. The website content is derived from
  it but the README stays the canonical entry point for anyone landing
  directly on the GitHub repo.
- No custom domain configuration. The site deploys to the default
  `<org>.github.io/<repo>` GitHub Pages URL.
- No CLI/API reference beyond what's listed above (e.g. no OpenAPI spec,
  no generated rustdoc integration). Both are hand-written reference
  pages, kept in sync manually as the CLI/API surface grows.

## Site structure

```
site/
  index.html            Home
  why.html               Why Keel / Why FreeBSD / vs. Kubernetes
  quickstart.html         Build-from-source walkthrough
  journey.html            Milestone-by-milestone history
  docs/
    index.html            Docs landing, links to the four pages below
    architecture.html      Crate map + reconciliation flow
    jailspec.html           JailSpec YAML reference
    cli.html                 keelctl command reference
    api.html                  HTTP API reference (agentd + controlplane)
  assets/
    keel-logo.png          copied from docs/assets/keel-logo.png
    style.css               shared stylesheet
.github/workflows/pages.yml  deploy site/ to GitHub Pages on push to main
```

Every page repeats the same nav bar by hand (no templating layer); this
is acceptable at 9 pages and matches the project's preference for
explicit, dependency-free code over abstraction machinery introduced
before it's needed.

## Page content

**Home** - hero section with the logo and the tagline "Declarative,
self-healing orchestration for FreeBSD jails"; a 2x2 feature card grid
(Declarative jails, Self-healing, Safe by construction, Fast
provisioning) drawn from the README's "Why you'd use it" section; a
compact Keel-vs-Kubernetes comparison table; a one-line current-status
callout (sub-project 1 complete, sub-project 2 in progress: node
registry done, spec routing not yet designed); CTA buttons to
Quickstart, Docs, and the GitHub repo.

**Why** - the Motivation, Why FreeBSD, and full "How this differs from
Kubernetes" sections adapted from the README, plus the "Why you'd use
it" bullets in full (the homepage only teases these as cards).

**Quickstart** - prerequisites (FreeBSD 15.1 VM, Rust toolchain); clone
and `cargo build --release`; a sample `jail.yaml` using the exact schema
from `keel-spec`'s own test fixture; running `keel-agentd`; `keelctl
apply -f jail.yaml`; `keelctl get`; confirming the running jail with
`jls`; a closing pointer to `scripts/smoke-test.sh` for the complete
automated lifecycle (crash/restart/state-recovery included).

**Journey** - one section per milestone (1 through 7), ported from the
README's "The journey so far" with the same level of narrative detail,
including the specific FreeBSD-VM-discovered bugs and how each was
diagnosed and fixed.

**Docs landing** - one short paragraph plus four link cards to the pages
below.

**Architecture** - the crate map (`keel-spec`, `keel-jail`, `keel-zfs`,
`keel-net`, `keel-agentd`, `keel-controlplane`, `keelctl`) and how they
compose: the `Trait` + `Fake` + real-implementation pattern used
throughout, `Reconciler<J, Z, N>` as the generic core, and the
single-worker-thread-owns-state pattern shared by both `keel-agentd` and
`keel-controlplane` (HTTP/timer threads only reach state through an
`mpsc` command channel).

**JailSpec reference** - a field table (`apiVersion`, `kind`,
`metadata.name`, `spec.image`, `spec.command`, `spec.network.vnet`,
`spec.network.bridge`, `spec.network.address`, `spec.resources.cpu`,
`spec.resources.memory`, `spec.restartPolicy`) with type, meaning, and
which fields are immutable after first apply, plus the full example YAML
from `keel-spec/src/types.rs`'s own test fixture.

**CLI reference** - `keelctl apply -f FILE`, `keelctl get [name]`,
`keelctl delete NAME`, the shared `--socket PATH` flag (default
`/var/run/keel-agentd.sock`), example invocations and example output for
each.

**HTTP API reference** - two sub-sections:
- `keel-agentd` (Unix socket, YAML request/response bodies): `PUT
  /jails/:name`, `GET /jails`, `GET /jails/:name`, `DELETE /jails/:name`;
  documented error cases (400 invalid YAML or name/body mismatch, 404
  unknown jail, 409 attempted change to an immutable field).
- `keel-controlplane` (TCP, YAML bodies): `POST /nodes/register` (body:
  `id`, `addr`), `POST /nodes/:id/heartbeat`, `GET /nodes` (returns each
  node's `id`, `addr`, computed `status` of `Alive`/`Dead`, and
  `last_seen_secs`); documented error case (404 heartbeat from an
  unknown node).

## Visual design

- Palette sampled from the existing logo: deep red (primary accent, used
  for headings, links, buttons) and orange (secondary accent, used for
  highlights/hover states), on a white/near-white background with dark
  slate body text. No blue/teal.
- Typography: system sans-serif font stack (`-apple-system, "Segoe UI",
  Roboto, ...`) for body and headings, system monospace stack for code
  blocks and the CLI/API examples. No external font fetches, keeping the
  site dependency-free like the rest of the project.
- Layout: bold centered hero on the homepage, card grid for features,
  generous whitespace and soft box-shadows on cards, a responsive layout
  that collapses the nav and card grid on narrow viewports (plain CSS
  flexbox/grid + media queries, no JS).

## Deployment

`.github/workflows/pages.yml` triggers on push to `main` (and manual
dispatch), uses `actions/upload-pages-artifact` to upload `site/` as-is
and `actions/deploy-pages` to publish it, no build step. This requires
enabling GitHub Pages "GitHub Actions" source in the repo settings
once, a one-time manual step outside this codebase.

## Testing / verification

Since this is a static site with no application logic, verification is:
- Every internal link resolves (manual click-through or a simple `grep`
  for `href="` targets confirming each referenced file exists under
  `site/`).
- The site renders correctly and responsively in a browser at both
  desktop and narrow-viewport widths (checked via the `run` skill /
  local `python3 -m http.server` before pushing).
- The JailSpec/CLI/API reference content is cross-checked line-by-line
  against the current `keel-spec`, `keelctl`, `keel-agentd`, and
  `keel-controlplane` source at the time of writing (already done above
  in this spec).
- After the GitHub Actions workflow first runs, confirm the Pages URL
  actually serves the site.

## Open questions

None outstanding; all prior open questions were resolved during
brainstorming (hosting/build approach, visual style, quickstart depth,
docs depth, and journey-page treatment).
