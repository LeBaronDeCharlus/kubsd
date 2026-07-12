# Keel Website Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a static, dependency-free website for Keel (home, why, quickstart, journey, and a four-page docs section) and deploy it to GitHub Pages.

**Architecture:** Plain hand-written HTML files under `site/`, one shared stylesheet, no JavaScript and no build step. A GitHub Actions workflow uploads `site/` as-is to GitHub Pages on every push to `main`.

**Tech Stack:** HTML5, CSS3 (flexbox/grid, no preprocessor), GitHub Actions (`actions/upload-pages-artifact`, `actions/deploy-pages`).

## Global Constraints

- No static site generator, no client-side JavaScript, no build tooling. Every page is a complete, self-contained `.html` file.
- Visual palette is sampled from the existing Keel logo: deep red `#a3221f` (primary accent) and orange `#e08a3c` (secondary accent) on a white background with dark slate (`#1c1e21`) body text. No blue/teal.
- System font stack only (no external font fetches): `-apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, Helvetica, Arial, sans-serif` for body text, `ui-monospace, SFMono-Regular, Menlo, Consolas, monospace` for code.
- No emoji anywhere in site content.
- Every page repeats the same nav bar and footer by hand (no templating layer).
- GitHub repo: `https://github.com/lebarondecharlus/keel`. The remote's default branch (where GitHub Pages deploys from) is `main` — the local branch tracking it is named `master`, so `git push` targets `main` even though the local branch is called `master`.
- Root pages live at `site/*.html` and reference assets as `assets/...` and docs pages as `docs/....html`. Docs pages live at `site/docs/*.html` and reference assets as `../assets/...`, root pages as `../*.html`, and sibling docs pages as `....html` (no prefix).
- Documentation content (JailSpec fields, CLI commands, HTTP routes) must match the current code exactly: `keel-spec/src/types.rs`, `keelctl/src/main.rs`, `keel-agentd/src/http.rs` + `main.rs` + `record.rs`, `keel-controlplane/src/wire.rs` + `http.rs`.

---

## File structure

```
site/
  assets/
    style.css
    keel-logo.png
  index.html
  why.html
  quickstart.html
  journey.html
  docs/
    index.html
    architecture.html
    jailspec.html
    cli.html
    api.html
.github/
  workflows/
    pages.yml
```

---

### Task 1: Shared stylesheet and logo asset

**Files:**
- Create: `site/assets/style.css`
- Create: `site/assets/keel-logo.png` (copy of `docs/assets/keel-logo.png`)

**Interfaces:**
- Produces: every CSS class every later page task relies on: `.site-header`, `.nav`, `.brand`, `.brand-logo`, `.nav-links`, `.nav-dropdown`, `.nav-dropdown-menu`, `.nav-github`, `.container`, `.hero`, `.hero-logo`, `.tagline`, `.hero-ctas`, `.btn`, `.btn-primary`, `.btn-secondary`, `.status-banner`, `section` + `.lead`, `.card-grid`, `.card`, `.link-card-grid`, `.link-card`, `table` + `.vs-table`, `pre`/inline `code`, `.feature-list`, `.milestone` + `.badge`, `.page-header`, `footer.site-footer`.

- [ ] **Step 1: Create the assets directory and copy the logo**

```bash
mkdir -p site/assets site/docs
cp docs/assets/keel-logo.png site/assets/keel-logo.png
```

- [ ] **Step 2: Write the shared stylesheet**

Create `site/assets/style.css`:

```css
:root {
  --red-900: #6b1414;
  --red-700: #a3221f;
  --orange-500: #e08a3c;
  --slate-900: #1c1e21;
  --slate-600: #52565c;
  --slate-300: #d8dadd;
  --slate-100: #f6f5f4;
  --white: #ffffff;
  --radius: 10px;
  --shadow: 0 2px 10px rgba(28, 30, 33, 0.08);
  --max-width: 1080px;
}

* {
  box-sizing: border-box;
}

html,
body {
  margin: 0;
  padding: 0;
}

body {
  font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, Helvetica, Arial, sans-serif;
  color: var(--slate-900);
  background: var(--white);
  line-height: 1.6;
}

code,
pre {
  font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
}

a {
  color: var(--red-700);
}

.container {
  max-width: var(--max-width);
  margin: 0 auto;
  padding: 0 24px;
}

/* header / nav */
.site-header {
  border-bottom: 1px solid var(--slate-300);
  position: sticky;
  top: 0;
  background: rgba(255, 255, 255, 0.92);
  backdrop-filter: blur(6px);
  z-index: 10;
}

.nav {
  display: flex;
  align-items: center;
  justify-content: space-between;
  height: 64px;
}

.brand {
  display: flex;
  align-items: center;
  gap: 10px;
  font-weight: 700;
  font-size: 1.15rem;
  color: var(--red-700);
  text-decoration: none;
}

.brand-logo {
  height: 32px;
  width: auto;
}

.nav-links {
  display: flex;
  align-items: center;
  gap: 28px;
}

.nav-links a {
  color: var(--slate-900);
  text-decoration: none;
  font-weight: 500;
}

.nav-links a:hover {
  color: var(--red-700);
}

.nav-github {
  color: var(--red-700) !important;
}

.nav-dropdown {
  position: relative;
}

.nav-dropdown-menu {
  display: none;
  position: absolute;
  top: 100%;
  left: 0;
  background: var(--white);
  border: 1px solid var(--slate-300);
  border-radius: var(--radius);
  box-shadow: var(--shadow);
  padding: 8px;
  min-width: 200px;
}

.nav-dropdown:hover .nav-dropdown-menu,
.nav-dropdown:focus-within .nav-dropdown-menu {
  display: block;
}

.nav-dropdown-menu a {
  display: block;
  padding: 8px 10px;
  border-radius: 6px;
}

.nav-dropdown-menu a:hover {
  background: var(--slate-100);
}

/* hero */
.hero {
  padding: 80px 0 56px;
  text-align: center;
}

.hero-logo {
  height: 110px;
  margin-bottom: 24px;
}

.hero h1 {
  font-size: 2.6rem;
  margin: 0 0 12px;
}

.hero .tagline {
  font-size: 1.25rem;
  color: var(--slate-600);
  max-width: 640px;
  margin: 0 auto 32px;
}

.hero-ctas {
  display: flex;
  gap: 16px;
  justify-content: center;
  flex-wrap: wrap;
}

.btn {
  display: inline-block;
  padding: 12px 24px;
  border-radius: 8px;
  text-decoration: none;
  font-weight: 600;
}

.btn-primary {
  background: var(--red-700);
  color: var(--white);
}

.btn-primary:hover {
  background: var(--red-900);
}

.btn-secondary {
  background: var(--slate-100);
  color: var(--slate-900);
  border: 1px solid var(--slate-300);
}

.btn-secondary:hover {
  border-color: var(--red-700);
  color: var(--red-700);
}

/* status banner */
.status-banner {
  background: var(--slate-100);
  border: 1px solid var(--slate-300);
  border-radius: var(--radius);
  padding: 14px 20px;
  margin: 0 auto 48px;
  max-width: 720px;
  text-align: center;
  font-size: 0.95rem;
}

.status-banner strong {
  color: var(--red-700);
}

/* page header (non-home pages) */
.page-header {
  padding: 56px 0 24px;
}

.page-header h1 {
  font-size: 2.2rem;
  margin: 0 0 8px;
}

/* sections */
section {
  padding: 40px 0;
}

section h2 {
  font-size: 1.7rem;
  margin-bottom: 8px;
}

section p.lead {
  color: var(--slate-600);
  margin-top: 0;
  margin-bottom: 28px;
  max-width: 720px;
}

/* feature grid */
.card-grid {
  display: grid;
  grid-template-columns: repeat(2, 1fr);
  gap: 24px;
}

.card {
  background: var(--white);
  border: 1px solid var(--slate-300);
  border-radius: var(--radius);
  box-shadow: var(--shadow);
  padding: 24px;
}

.card h3 {
  margin-top: 0;
  color: var(--red-700);
}

.link-card-grid {
  display: grid;
  grid-template-columns: repeat(2, 1fr);
  gap: 20px;
}

.link-card {
  display: block;
  border: 1px solid var(--slate-300);
  border-radius: var(--radius);
  padding: 20px;
  text-decoration: none;
  color: inherit;
  box-shadow: var(--shadow);
}

.link-card:hover {
  border-color: var(--red-700);
}

.link-card h3 {
  color: var(--red-700);
  margin: 0 0 8px;
}

.link-card p {
  margin: 0;
  color: var(--slate-600);
}

/* feature list (why.html) */
ul.feature-list {
  padding-left: 20px;
}

ul.feature-list li {
  margin-bottom: 16px;
}

/* tables */
table {
  width: 100%;
  border-collapse: collapse;
  margin: 24px 0;
}

th,
td {
  text-align: left;
  padding: 10px 12px;
  border-bottom: 1px solid var(--slate-300);
  vertical-align: top;
}

th {
  background: var(--slate-100);
}

/* code blocks */
pre {
  background: var(--slate-900);
  color: var(--slate-100);
  padding: 18px 20px;
  border-radius: var(--radius);
  overflow-x: auto;
  font-size: 0.9rem;
}

p code,
li code,
td code,
th code {
  background: var(--slate-100);
  padding: 2px 6px;
  border-radius: 4px;
  font-size: 0.9em;
}

/* milestone / timeline (journey page) */
.milestone {
  border-left: 3px solid var(--orange-500);
  padding-left: 24px;
  margin-bottom: 40px;
}

.milestone h3 {
  color: var(--red-700);
  margin-bottom: 4px;
}

.milestone .badge {
  display: inline-block;
  background: var(--slate-100);
  border-radius: 999px;
  padding: 2px 10px;
  font-size: 0.8rem;
  color: var(--slate-600);
  margin-left: 8px;
  font-weight: normal;
}

footer.site-footer {
  border-top: 1px solid var(--slate-300);
  padding: 32px 0;
  margin-top: 48px;
  text-align: center;
  color: var(--slate-600);
  font-size: 0.9rem;
}

/* responsive */
@media (max-width: 720px) {
  .nav-links {
    gap: 16px;
    font-size: 0.9rem;
  }
  .brand-logo {
    height: 26px;
  }
  .card-grid,
  .link-card-grid {
    grid-template-columns: 1fr;
  }
  .hero h1 {
    font-size: 2rem;
  }
}
```

- [ ] **Step 3: Verify the files exist**

Run:

```bash
test -f site/assets/style.css && test -f site/assets/keel-logo.png && echo OK
```

Expected: `OK`

- [ ] **Step 4: Commit**

```bash
git add site/assets/style.css site/assets/keel-logo.png
git commit -m "Add shared stylesheet and logo asset for the Keel website"
```

---

### Task 2: Home page

**Files:**
- Create: `site/index.html`

**Interfaces:**
- Consumes: CSS classes from Task 1 (`site/assets/style.css`), `site/assets/keel-logo.png`.
- Produces: the root nav/footer HTML block later root-level pages (`why.html`, `quickstart.html`, `journey.html`) reuse verbatim, and the `#vs-kubernetes` anchor on `why.html` that this page links to.

- [ ] **Step 1: Write `site/index.html`**

```html
<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Keel · Declarative, self-healing orchestration for FreeBSD jails</title>
<link rel="icon" href="assets/keel-logo.png">
<link rel="stylesheet" href="assets/style.css">
</head>
<body>
<header class="site-header">
  <div class="container nav">
    <a class="brand" href="index.html"><img src="assets/keel-logo.png" alt="Keel logo" class="brand-logo">Keel</a>
    <nav class="nav-links">
      <a href="why.html">Why</a>
      <div class="nav-dropdown">
        <a href="docs/index.html">Docs</a>
        <div class="nav-dropdown-menu">
          <a href="docs/architecture.html">Architecture</a>
          <a href="docs/jailspec.html">JailSpec Reference</a>
          <a href="docs/cli.html">CLI Reference</a>
          <a href="docs/api.html">HTTP API Reference</a>
        </div>
      </div>
      <a href="quickstart.html">Quickstart</a>
      <a href="journey.html">Journey</a>
      <a class="nav-github" href="https://github.com/lebarondecharlus/keel" target="_blank" rel="noopener">GitHub &#8599;</a>
    </nav>
  </div>
</header>
<main>
  <section class="hero">
    <div class="container">
      <img src="assets/keel-logo.png" alt="Keel logo" class="hero-logo">
      <h1>Keel</h1>
      <p class="tagline">Declarative, self-healing orchestration for FreeBSD jails.</p>
      <div class="hero-ctas">
        <a class="btn btn-primary" href="quickstart.html">Get started</a>
        <a class="btn btn-secondary" href="docs/index.html">Read the docs</a>
        <a class="btn btn-secondary" href="https://github.com/lebarondecharlus/keel" target="_blank" rel="noopener">View on GitHub</a>
      </div>
    </div>
  </section>

  <section class="status">
    <div class="container">
      <div class="status-banner">
        <strong>Sub-project 1</strong> (single-node jail reconciliation daemon) is complete.
        <strong>Sub-project 2</strong> (multi-node control plane) is in progress: the node
        registry is done, spec routing is not yet designed.
        <a href="journey.html">See the full journey &rarr;</a>
      </div>
    </div>
  </section>

  <section class="features">
    <div class="container">
      <h2>Why you'd use it</h2>
      <p class="lead">Keel ties FreeBSD's own isolation and resource-control primitives together the way Kubernetes ties together Linux namespaces, cgroups, and overlay networking.</p>
      <div class="card-grid">
        <div class="card">
          <h3>Declarative jails</h3>
          <p>Describe a jail (image, command, resources, network, restart policy) as a spec; apply it; the daemon makes reality match it, continuously, not a one-shot script.</p>
        </div>
        <div class="card">
          <h3>Self-healing</h3>
          <p>Crashed jails restart automatically per policy, with crash-loop backoff so a persistently broken jail doesn't spin forever.</p>
        </div>
        <div class="card">
          <h3>Safe by construction</h3>
          <p>The daemon only ever touches jails it created (name-prefixed and tracked in its own state), so it can share a host with other jails or tooling without stepping on them.</p>
        </div>
        <div class="card">
          <h3>Fast provisioning</h3>
          <p>Jail root filesystems are ZFS clones of a base image, so creating a new jail is close to instant and cheap on disk.</p>
        </div>
      </div>
    </div>
  </section>

  <section class="comparison">
    <div class="container">
      <h2>How this differs from Kubernetes</h2>
      <p class="lead">Keel borrows Kubernetes' shape, not its scope. Today it's the FreeBSD analog of a kubelet, not a full cluster.</p>
      <table class="vs-table">
        <thead><tr><th></th><th>Kubernetes</th><th>Keel (current)</th></tr></thead>
        <tbody>
          <tr><td>Workload unit</td><td>Pod (Linux containers)</td><td>Jail</td></tr>
          <tr><td>Filesystem</td><td>overlay/container images</td><td>ZFS clones of a base dataset</td></tr>
          <tr><td>Scope</td><td>multi-node cluster, scheduler</td><td>single node (kubelet-equivalent only)</td></tr>
        </tbody>
      </table>
      <p><a href="why.html#vs-kubernetes">Read the full comparison &rarr;</a></p>
    </div>
  </section>
</main>
<footer class="site-footer">
  <div class="container">
    <p>Keel is open source on <a href="https://github.com/lebarondecharlus/keel" target="_blank" rel="noopener">GitHub</a>.</p>
  </div>
</footer>
</body>
</html>
```

- [ ] **Step 2: Verify structure and content**

```bash
grep -q "<title>Keel" site/index.html \
  && grep -q 'href="assets/style.css"' site/index.html \
  && grep -q 'href="quickstart.html"' site/index.html \
  && grep -q 'href="docs/index.html"' site/index.html \
  && grep -q 'href="why.html#vs-kubernetes"' site/index.html \
  && grep -q "Declarative jails" site/index.html \
  && grep -q "</html>" site/index.html \
  && echo OK
```

Expected: `OK`

- [ ] **Step 3: Commit**

```bash
git add site/index.html
git commit -m "Add Keel website home page"
```

---

### Task 3: Why page

**Files:**
- Create: `site/why.html`

**Interfaces:**
- Consumes: root nav/footer block established in Task 2.
- Produces: the `id="vs-kubernetes"` anchor linked from `index.html`.

- [ ] **Step 1: Write `site/why.html`**

```html
<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Why Keel · Keel</title>
<link rel="icon" href="assets/keel-logo.png">
<link rel="stylesheet" href="assets/style.css">
</head>
<body>
<header class="site-header">
  <div class="container nav">
    <a class="brand" href="index.html"><img src="assets/keel-logo.png" alt="Keel logo" class="brand-logo">Keel</a>
    <nav class="nav-links">
      <a href="why.html">Why</a>
      <div class="nav-dropdown">
        <a href="docs/index.html">Docs</a>
        <div class="nav-dropdown-menu">
          <a href="docs/architecture.html">Architecture</a>
          <a href="docs/jailspec.html">JailSpec Reference</a>
          <a href="docs/cli.html">CLI Reference</a>
          <a href="docs/api.html">HTTP API Reference</a>
        </div>
      </div>
      <a href="quickstart.html">Quickstart</a>
      <a href="journey.html">Journey</a>
      <a class="nav-github" href="https://github.com/lebarondecharlus/keel" target="_blank" rel="noopener">GitHub &#8599;</a>
    </nav>
  </div>
</header>
<main>
  <section class="page-header">
    <div class="container">
      <h1>Why Keel</h1>
      <p class="lead">FreeBSD has the primitives. Keel is the piece that ties them together.</p>
    </div>
  </section>

  <section id="motivation">
    <div class="container">
      <h2>Motivation</h2>
      <p>FreeBSD has mature, battle-tested primitives for isolation and resource control: jails, ZFS snapshots/clones, <code>rctl(8)</code> resource limits, VNET per-jail networking. What it lacks is something that ties them together the way Kubernetes ties together Linux namespaces, cgroups, and overlay networking.</p>
      <p>Existing FreeBSD jail tools (<code>bastille</code>, <code>ezjail</code>, <code>cbsd</code>, plain <code>jail.conf</code>) are good at <em>creating</em> jails, but none of them are <em>reconciliation-based</em>: none continuously watch a declarative spec and drive the live system to match it, restart what crashed, clean up what was removed, or survive their own restarts without losing track of what they manage. That gap, declarative, self-healing orchestration for FreeBSD, is what Keel is for.</p>
    </div>
  </section>

  <section id="why-freebsd">
    <div class="container">
      <h2>Why FreeBSD</h2>
      <p>Jails, ZFS, and VNET are not bolted-on features; they're core, long-lived parts of the base system. That means Keel can be a comparatively thin layer: most of what a container orchestrator normally has to build (copy-on-write filesystem layers, resource accounting, network namespace isolation) is already correct and well-tested at the OS level.</p>
    </div>
  </section>

  <section id="vs-kubernetes">
    <div class="container">
      <h2>How this differs from Kubernetes</h2>
      <p class="lead">Keel borrows Kubernetes' shape (declarative specs, a reconciliation loop, a CLI that will mirror <code>kubectl</code>) but it is not trying to be a drop-in replacement, and today it is far smaller in scope:</p>
      <table class="vs-table">
        <thead><tr><th></th><th>Kubernetes</th><th>Keel (current)</th></tr></thead>
        <tbody>
          <tr><td>Workload unit</td><td>Pod (Linux containers)</td><td>Jail</td></tr>
          <tr><td>Isolation</td><td>namespaces + cgroups</td><td>jails + <code>rctl(8)</code></td></tr>
          <tr><td>Filesystem</td><td>overlay/container images</td><td>ZFS clones of a base dataset</td></tr>
          <tr><td>Networking</td><td>CNI, overlay networks, Services</td><td>VNET + <code>epair(4)</code> + <code>bridge(4)</code>, static IPs</td></tr>
          <tr><td>Scope</td><td>multi-node cluster, scheduler</td><td>single node (kubelet-equivalent only)</td></tr>
          <tr><td>Control plane</td><td>API server + etcd + scheduler</td><td>none yet, one local daemon per host</td></tr>
        </tbody>
      </table>
      <p>In other words: what exists today is the FreeBSD analog of a <strong>kubelet</strong>, not a full cluster. There is no scheduler, no multi-node API server, and no cluster networking yet; see the <a href="journey.html">journey</a> for how the project got here and what's next.</p>
    </div>
  </section>

  <section id="why-youd-use-it">
    <div class="container">
      <h2>Why you'd use it</h2>
      <ul class="feature-list">
        <li><strong>Declarative jails.</strong> Describe a jail (image, command, resources, network, restart policy) as a spec; apply it; the daemon makes reality match it, continuously, not a one-shot script.</li>
        <li><strong>Self-healing.</strong> Crashed jails restart automatically per policy, with crash-loop backoff so a persistently broken jail doesn't spin forever.</li>
        <li><strong>Safe by construction.</strong> The daemon only ever touches jails it created (name-prefixed and tracked in its own state), so it can share a host with other jails or tooling without stepping on them. State is crash-safe: killing the daemon or the whole VM never leaves it confused about what it manages.</li>
        <li><strong>Fast provisioning.</strong> Jail root filesystems are ZFS clones of a base image, so creating a new jail is close to instant and cheap on disk.</li>
      </ul>
    </div>
  </section>
</main>
<footer class="site-footer">
  <div class="container">
    <p>Keel is open source on <a href="https://github.com/lebarondecharlus/keel" target="_blank" rel="noopener">GitHub</a>.</p>
  </div>
</footer>
</body>
</html>
```

- [ ] **Step 2: Verify structure and content**

```bash
grep -q "<title>Why Keel" site/why.html \
  && grep -q 'id="vs-kubernetes"' site/why.html \
  && grep -q "kubelet" site/why.html \
  && grep -q "</html>" site/why.html \
  && echo OK
```

Expected: `OK`

- [ ] **Step 3: Commit**

```bash
git add site/why.html
git commit -m "Add Keel website Why page"
```

---

### Task 4: Quickstart page

**Files:**
- Create: `site/quickstart.html`

**Interfaces:**
- Consumes: root nav/footer block established in Task 2.

- [ ] **Step 1: Write `site/quickstart.html`**

```html
<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Quickstart · Keel</title>
<link rel="icon" href="assets/keel-logo.png">
<link rel="stylesheet" href="assets/style.css">
</head>
<body>
<header class="site-header">
  <div class="container nav">
    <a class="brand" href="index.html"><img src="assets/keel-logo.png" alt="Keel logo" class="brand-logo">Keel</a>
    <nav class="nav-links">
      <a href="why.html">Why</a>
      <div class="nav-dropdown">
        <a href="docs/index.html">Docs</a>
        <div class="nav-dropdown-menu">
          <a href="docs/architecture.html">Architecture</a>
          <a href="docs/jailspec.html">JailSpec Reference</a>
          <a href="docs/cli.html">CLI Reference</a>
          <a href="docs/api.html">HTTP API Reference</a>
        </div>
      </div>
      <a href="quickstart.html">Quickstart</a>
      <a href="journey.html">Journey</a>
      <a class="nav-github" href="https://github.com/lebarondecharlus/keel" target="_blank" rel="noopener">GitHub &#8599;</a>
    </nav>
  </div>
</header>
<main>
  <section class="page-header">
    <div class="container">
      <h1>Quickstart</h1>
      <p class="lead">Build Keel from source and apply your first jail spec against a real FreeBSD host.</p>
    </div>
  </section>

  <section id="prerequisites">
    <div class="container">
      <h2>Prerequisites</h2>
      <ul>
        <li>A FreeBSD 15.1 host or VM with jails, ZFS, and VNET already usable (a ZFS pool such as <code>zroot</code>, and <code>vnet</code>/<code>if_bridge</code>/<code>if_epair</code> kernel support).</li>
        <li>Root access on that host (jail/ZFS/<code>rctl(8)</code> management requires it).</li>
        <li>The Rust toolchain (<a href="https://rustup.rs" target="_blank" rel="noopener">rustup</a>) installed on the host, or cross-compiled binaries copied over.</li>
        <li>A populated base dataset for at least one jail image, e.g. <code>zroot/keel/base/14.2-web</code>, containing a FreeBSD userland and your application.</li>
      </ul>
    </div>
  </section>

  <section id="build">
    <div class="container">
      <h2>1. Clone and build</h2>
      <pre>git clone https://github.com/lebarondecharlus/keel.git
cd keel
cargo build --release --workspace</pre>
      <p>This produces <code>target/release/keel-agentd</code> and <code>target/release/keelctl</code>.</p>
    </div>
  </section>

  <section id="spec">
    <div class="container">
      <h2>2. Write a jail spec</h2>
      <p>Save the following as <code>jail.yaml</code>. This is the same example used in <code>keel-spec</code>'s own tests:</p>
      <pre>apiVersion: keel/v1
kind: Jail
metadata:
  name: web-1
spec:
  image: base/14.2-web
  command: ["/usr/local/bin/myapp"]
  network:
    vnet: true
    bridge: keel0
    address: 10.0.0.5/24
  resources:
    cpu: "2"
    memory: "512M"
  restartPolicy: Always</pre>
      <p><code>image</code> must match an existing base dataset: with the default <code>--pool zroot</code>, <code>image: base/14.2-web</code> resolves to <code>zroot/keel/base/14.2-web</code>.</p>
    </div>
  </section>

  <section id="run">
    <div class="container">
      <h2>3. Run keel-agentd</h2>
      <pre># as root
./target/release/keel-agentd</pre>
      <p>By default this reconciles against pool <code>zroot</code>, keeps its state under <code>/var/db/keel</code>, and serves its API on the Unix socket <code>/var/run/keel-agentd.sock</code>. Override any of these with <code>--pool</code>, <code>--state-dir</code>, or <code>--socket</code>.</p>
    </div>
  </section>

  <section id="apply">
    <div class="container">
      <h2>4. Apply and inspect the jail</h2>
      <pre># from another shell, as root
./target/release/keelctl apply -f jail.yaml
./target/release/keelctl get
./target/release/keelctl get web-1</pre>
      <p>Confirm it's really running:</p>
      <pre>jls -j keel-web-1</pre>
      <p>Keel prefixes every jail it manages with <code>keel-</code>, so the spec named <code>web-1</code> becomes the jail <code>keel-web-1</code>.</p>
    </div>
  </section>

  <section id="cleanup">
    <div class="container">
      <h2>5. Clean up</h2>
      <pre>./target/release/keelctl delete web-1</pre>
    </div>
  </section>

  <section id="automated">
    <div class="container">
      <h2>The automated version</h2>
      <p><code>scripts/smoke-test.sh</code> in the repository runs this entire lifecycle end-to-end against a real <code>rc.d</code>-managed <code>keel-agentd</code> service, including killing the daemon mid-flight to prove the automatic restart and state recovery both work, then a clean teardown. See that script and <a href="docs/architecture.html">Architecture</a> for how the pieces fit together.</p>
    </div>
  </section>
</main>
<footer class="site-footer">
  <div class="container">
    <p>Keel is open source on <a href="https://github.com/lebarondecharlus/keel" target="_blank" rel="noopener">GitHub</a>.</p>
  </div>
</footer>
</body>
</html>
```

- [ ] **Step 2: Verify structure and content**

```bash
grep -q "<title>Quickstart" site/quickstart.html \
  && grep -q "cargo build --release --workspace" site/quickstart.html \
  && grep -q "keelctl apply -f jail.yaml" site/quickstart.html \
  && grep -q "jls -j keel-web-1" site/quickstart.html \
  && grep -q "scripts/smoke-test.sh" site/quickstart.html \
  && grep -q "</html>" site/quickstart.html \
  && echo OK
```

Expected: `OK`

- [ ] **Step 3: Commit**

```bash
git add site/quickstart.html
git commit -m "Add Keel website Quickstart page"
```

---

### Task 5: Journey page

**Files:**
- Create: `site/journey.html`

**Interfaces:**
- Consumes: root nav/footer block established in Task 2.

- [ ] **Step 1: Write `site/journey.html`**

```html
<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>The Journey So Far · Keel</title>
<link rel="icon" href="assets/keel-logo.png">
<link rel="stylesheet" href="assets/style.css">
</head>
<body>
<header class="site-header">
  <div class="container nav">
    <a class="brand" href="index.html"><img src="assets/keel-logo.png" alt="Keel logo" class="brand-logo">Keel</a>
    <nav class="nav-links">
      <a href="why.html">Why</a>
      <div class="nav-dropdown">
        <a href="docs/index.html">Docs</a>
        <div class="nav-dropdown-menu">
          <a href="docs/architecture.html">Architecture</a>
          <a href="docs/jailspec.html">JailSpec Reference</a>
          <a href="docs/cli.html">CLI Reference</a>
          <a href="docs/api.html">HTTP API Reference</a>
        </div>
      </div>
      <a href="quickstart.html">Quickstart</a>
      <a href="journey.html">Journey</a>
      <a class="nav-github" href="https://github.com/lebarondecharlus/keel" target="_blank" rel="noopener">GitHub &#8599;</a>
    </nav>
  </div>
</header>
<main>
  <section class="page-header">
    <div class="container">
      <h1>The Journey So Far</h1>
      <p class="lead">Keel is being built one milestone at a time: design a spec, write an implementation plan, execute it task by task with a review after every task, then a whole-branch review before moving on. Every FreeBSD-specific behavior is verified on a real FreeBSD 15.1 VM, not assumed, before it's locked into a plan.</p>
    </div>
  </section>

  <section>
    <div class="container">

      <div class="milestone">
        <h3>Milestone 1 <span class="badge">keel-spec</span></h3>
        <p>The foundation: a YAML schema for describing a jail (image, command, network, resources, restart policy) plus the parsing and validation that turns YAML into a typed <code>JailSpec</code>. This is where the core invariants of the whole system were decided: what counts as a valid jail name, how <code>cpu</code>/<code>memory</code> strings get parsed into concrete limits, which fields are allowed to change on a re-apply and which are immutable for the life of the jail, and how CIDR addresses are validated. Thirteen unit tests plus four end-to-end tests, all running on any OS, no FreeBSD required.</p>
      </div>

      <div class="milestone">
        <h3>Milestone 2 <span class="badge">keel-jail, keel-zfs</span></h3>
        <p>The first milestone that actually touches FreeBSD. Two crates, each built around the same pattern that carries through the rest of the project: a trait describing what the crate does (<code>JailRuntime</code>, <code>ZfsManager</code>), an in-memory <code>Fake</code> implementation for fast tests on any machine, and a real implementation that shells out to <code>jail(8)</code>, <code>jexec(8)</code>, <code>rctl(8)</code>, and <code>zfs(8)</code> on FreeBSD.</p>
        <p>Getting the real implementations right took real hardware: <code>is_running</code> first miscounted zombie processes as "running" until tested against an actual jail; <code>ps</code> invocation syntax that looked right on paper didn't parse the way FreeBSD expected; a <code>zfs snapshot</code> race under parallel tests had to be made tolerant of losing that race rather than erroring.</p>
      </div>

      <div class="milestone">
        <h3>Milestone 3 <span class="badge">keel-net</span></h3>
        <p>Adds <code>keel-net</code> and its <code>NetManager</code> trait: creating bridges, attaching a jail to one over an <code>epair(4)</code> pair with a static address, and tearing that down again. This milestone is also where <code>keel-jail::create</code> gained a <code>vnet</code> parameter, since VNET-enabled jails need to be created differently from the start, an early breaking change caught before it could compound. By this point the "verify on the real VM before writing the plan" discipline was fully in place, and Milestone 3 shipped with zero fix rounds across all five of its tasks.</p>
      </div>

      <div class="milestone">
        <h3>Milestone 4 <span class="badge">keel-agentd, the reconciliation core</span></h3>
        <p>The milestone that ties everything together. <code>Reconciler&lt;J, Z, N&gt;</code> is generic over the three runtime traits from Milestones 1 through 3, so it can be instantiated against the <code>Fake*</code> implementations for fast, FreeBSD-free testing today, and against the real <code>Process*</code>/<code>Cli*</code> implementations once a later milestone wires it up to an actual daemon.</p>
        <p>Its public API is small on purpose: <code>new</code>, <code>apply</code>, <code>delete</code>, <code>reconcile</code>. Underneath, seven tasks built it up in layers: a <code>JailRecord</code> with the naming and path derivation rules (jail names, dataset paths, epair names), a crash-safe state store that writes to a temp file and renames rather than risking a torn write, a per-jail <code>BackoffState</code> (starts at one second, doubles up to a five-minute cap, resets after sixty seconds of stable uptime), the provisioning path with automatic rollback on partial failure, and finally the public <code>reconcile()</code> that runs the whole desired-versus-observed diff for every jail in one pass, returning a list of per-jail failures so one broken jail never blocks the others from being reconciled.</p>
        <p>This was also the milestone where the review discipline paid for itself most visibly. Three real bugs were caught, not by the first pass of tests, but by treating every review (per-task, then a final whole-branch pass on top) as a genuine adversarial check rather than a formality:</p>
        <ul>
          <li><code>delete()</code> assumed that tearing down a jail, its dataset, and its resource limits were all safe to call on something that was never actually created, matching how network detach already behaved. Only the network side turned out to be built that way; the others needed the same tolerance added explicitly, for the real case of deleting a jail that was applied but never got as far as being provisioned.</li>
          <li>A test for crash-loop restart asserted that a jail would restart with zero elapsed time, but the backoff cooldown from the initial provisioning was, correctly, still armed at that instant. The bug was in the test's timing, not the reconciler.</li>
          <li>The final whole-branch review caught a real one: a failed restart attempt never armed the backoff cooldown at all, because the code returned early on error before reaching the line that would have armed it. Successful restarts were protected; failing ones, exactly the crash-loop case backoff exists for, were not. Fixed with a regression test that injects a restart failure and proves the cooldown now engages.</li>
        </ul>
        <p>Milestone 4 finished at 71 tests, all passing, all still running without touching FreeBSD.</p>
      </div>

      <div class="milestone">
        <h3>Milestone 5 <span class="badge">local HTTP API + keelctl</span></h3>
        <p>The first milestone where <code>keel-agentd</code> actually runs: a binary that wires the real <code>ProcessJailRuntime</code>/<code>CliZfsManager</code>/<code>ProcessNetManager</code> implementations into a <code>Reconciler</code>, drives it on a 5-second timer, and serves <code>apply</code>/<code>get</code>/<code>delete</code> over a local HTTP API on a <code>0600</code> Unix socket, plus <code>keelctl</code>, the companion CLI. A single worker thread owns the <code>Reconciler</code> exclusively; the HTTP server and timer only ever reach it through a command channel, so apply/delete take effect immediately (reconciled before the caller's response) without introducing a second concurrent owner. Everything is still hand-rolled and dependency-light, in keeping with the rest of the project: a small HTTP/1.1 parser (<code>httparse</code>) over a blocking <code>UnixListener</code>, YAML wire bodies reusing the spec's existing <code>serde_yaml</code>, no async runtime.</p>
        <p>Milestones 1-4 were tested entirely against fakes; Milestone 5's FreeBSD VM verification was the first time the whole stack ran against the real system end-to-end, and it promptly found two real bugs that no fake-backed test could have caught, both in code shipped since Milestone 2:</p>
        <ul>
          <li><code>keel-jail</code>'s <code>destroy</code> only ran <code>jail -r</code>, never reaping the process <code>start_command</code> had spawned into the jail. The kill left a zombie holding a reference into the jail's rootfs mount, so an immediately following dataset teardown failed with "device busy", exactly the sequence <code>Reconciler::delete</code> runs on every deletion of a jail with a running command. Fixed by tracking spawned children per jail name and blocking-waiting on only the destroyed jail's own children.</li>
          <li>Independently, even with that fixed, <code>zfs destroy</code> could still fail with "dataset is busy" for a brief window after <code>jail -r</code> returns, a real kernel-level mount-release timing gap, reproducible from a plain shell with no Rust involved. Fixed with a short bounded retry, the same pattern already used for <code>clone_from_base</code>'s snapshot race.</li>
        </ul>
        <p>Milestone 5 finished at 96 tests on macOS, plus a clean 5-for-5 real apply &rarr; running &rarr; delete cycle against actual jails, ZFS clones, VNET networking, and <code>rctl</code> limits on the FreeBSD VM.</p>
      </div>

      <div class="milestone">
        <h3>Milestone 6 <span class="badge">rc.d service + smoke test</span></h3>
        <p>The milestone that turns <code>keel-agentd</code> from "a binary you run in a terminal" into a real FreeBSD system service, closing out sub-project 1. No new code was needed for daemonization itself: an <code>rc.d</code> script wraps the unchanged Milestone 5 binary with the base system's <code>daemon(8)</code> utility, using <code>-r</code> for restart-on-crash and <code>-S</code> to route its output to syslog. The only Rust changes were two <code>eprintln!</code> call sites (daemon startup, per-jail reconciliation failures), no logging framework, no <code>daemonize</code>/<code>signal-hook</code> dependency, no custom <code>SIGTERM</code> handler, since the existing "jails outlive the daemon" design already makes the default terminate-on-signal behavior correct. A committed smoke test script (<code>scripts/smoke-test.sh</code>) proves the whole lifecycle end-to-end: install, start, apply a real spec, kill <code>keel-agentd</code> directly to simulate a crash, confirm both the automatic restart and correct state recovery with no duplicate jail, stop the service and confirm the jail keeps running, clean teardown.</p>
        <p>One design subtlety mattered before any code was written: <code>daemon(8)</code> takes two different pidfile flags, and combining <code>-r</code> with the wrong one (<code>-p</code>, the child's pid) means <code>service ... stop</code> kills the child directly while <code>daemon(8)</code> is still watching and immediately restarts it, the stop command would silently do nothing. Verified directly on the VM before committing to the design; the rc.d script uses <code>-P</code> (the supervisor's own pid) throughout.</p>
        <p>Full VM verification surfaced two more real bugs, neither reachable by any fakes-backed test, since both are genuine OS/supervisor-interaction characteristics that only exist under a real <code>daemon(8)</code>-supervised run:</p>
        <ul>
          <li><code>keel-jail</code>'s <code>start_command</code> spawned the jailed process with Rust's default stdio inheritance, so a long-running jail held <code>keel-agentd</code>'s own stdout/stderr open. Under <code>daemon(8) -S</code>, those are the write end of a pipe <code>daemon(8)</code> reads to detect (via EOF) that <code>keel-agentd</code> itself died and needs restarting, an inherited pipe held open by the jailed workload meant <code>daemon(8)</code> silently never restarted a killed <code>keel-agentd</code> whenever any jail with a running command was active. Root-caused on the VM with <code>procstat -f</code>, showing the jailed process's fd 0/1/2 as a pipe shared with <code>daemon(8)</code>'s own relay pipe; fixed by giving the jailed process its own <code>/dev/null</code> stdio.</li>
          <li>The smoke test's own "jails outlive the daemon" check was a permanent false negative: <code>jls</code>'s default columns never include the jail's name, only its dataset path, so <code>grep</code>-ing for the name-prefixed string could never match regardless of whether the jail was actually still running. Fixed with a direct <code>jls -j &lt;name&gt;</code> lookup instead.</li>
        </ul>
        <p>Milestone 6 finished at 97 tests on macOS (96 plus one new regression test pinned to the exact stdio-leak mechanism via <code>procstat</code>), plus two consecutive clean end-to-end smoke test runs on the FreeBSD VM.</p>
      </div>

      <div class="milestone">
        <h3>Milestone 7 <span class="badge">keel-controlplane, the node registry</span></h3>
        <p>The first milestone of sub-project 2 (the multi-node control plane), deliberately scoped to answer only "which nodes exist, and are they alive", no scheduling, no placement, no spec-forwarding, all of which stay separate future sub-projects. A new crate, <code>keel-controlplane</code>, follows the exact same shape <code>keel-agentd</code> already established: a single worker thread exclusively owns an in-memory <code>Registry</code>, reachable only through an <code>mpsc</code> command channel, fronted by the same hand-rolled <code>httparse</code>-based HTTP server <code>keel-agentd</code> uses for its local API, just over a <code>TcpListener</code> instead of a Unix socket, since nodes live on separate hosts. Three endpoints: register a node, heartbeat a node, list all nodes with their liveness computed on read (<code>Alive</code> within 15 seconds of the last heartbeat, <code>Dead</code> after) rather than tracked by a background sweep.</p>
        <p><code>keel-agentd</code> gained three new, entirely optional CLI flags (<code>--node-id</code>, <code>--control-plane-addr</code>, <code>--advertise-addr</code>) and a <code>registration.rs</code> background thread: register once at startup, heartbeat every 5 seconds, and treat <em>any</em> heartbeat failure, a rejected/unknown node or a connection error, identically, by simply re-registering on the next tick. There is deliberately no persistence anywhere in this milestone and no backoff: a control plane that restarts forgets every node, and every node notices and re-registers within one heartbeat interval, the same self-healing-over-durability bet the reconciler already made in Milestone 4, just applied to cluster membership instead of jail state.</p>
        <p>One constraint surfaced only once tests were being written, not before: the design assumed an in-process test could simulate "the control plane restarts" by dropping a <code>TcpListener</code> and rebinding a fresh one to the same address. Verified directly that this doesn't work, <code>std::net::TcpListener::bind</code> fails with "Address already in use" as long as the original socket is still alive and listening in another thread, regardless of <code>SO_REUSEADDR</code>. That specific behavior, the one genuinely OS-level part of this milestone, was verified for real instead, against three separate FreeBSD VMs: all three nodes registered and showed <code>Alive</code> within one heartbeat interval; killing <code>keel-agentd</code> on one node flipped only that node to <code>Dead</code> after the 15-second threshold, the other two unaffected; restarting <code>keel-controlplane</code> emptied the registry (<code>[]</code>), and both surviving nodes' own logs showed the expected <code>heartbeat failed: control plane returned status 404</code> &rarr; re-register cycle, landing back at all-<code>Alive</code> within about one heartbeat interval with neither node process ever restarted.</p>
        <p>Milestone 7 finished at 122 tests on macOS (96 inherited, 26 new across <code>keel-controlplane</code>'s registry/worker/http layers and <code>keel-agentd</code>'s new CLI flags and registration client), zero warnings, final whole-branch review clean with no Critical or Important findings.</p>
      </div>

      <div class="milestone">
        <h3>Roadmap <span class="badge">what's next</span></h3>
        <p><strong>Sub-project 1: single-node jail reconciliation daemon &mdash; complete.</strong> Jail spec language, jail lifecycle + ZFS clone provisioning, VNET networking, the reconciliation core, the local HTTP API + CLI, and <code>rc.d</code> service integration with an end-to-end smoke test are all done.</p>
        <p><strong>Sub-project 2: multi-node control plane &mdash; in progress.</strong> The node registry (register/heartbeat/list over TCP, self-healing membership) is done. Routing jail specs to a specific node through the control plane is not yet designed.</p>
        <p><strong>Not yet designed</strong> (future sub-projects, each will get its own spec): a scheduler for bin-packing jails across nodes, cluster networking (cross-node overlay, service discovery/load balancing), storage orchestration beyond a single host's ZFS pool, and bhyve VM workloads alongside jails.</p>
      </div>

    </div>
  </section>
</main>
<footer class="site-footer">
  <div class="container">
    <p>Keel is open source on <a href="https://github.com/lebarondecharlus/keel" target="_blank" rel="noopener">GitHub</a>.</p>
  </div>
</footer>
</body>
</html>
```

- [ ] **Step 2: Verify structure and content**

```bash
grep -q "<title>The Journey So Far" site/journey.html \
  && grep -q "Milestone 1" site/journey.html \
  && grep -q "Milestone 7" site/journey.html \
  && grep -q "122 tests on macOS" site/journey.html \
  && grep -q "</html>" site/journey.html \
  && echo OK
```

Expected: `OK`

- [ ] **Step 3: Commit**

```bash
git add site/journey.html
git commit -m "Add Keel website Journey page"
```

---

### Task 6: Docs landing page

**Files:**
- Create: `site/docs/index.html`

**Interfaces:**
- Consumes: CSS classes/assets from Task 1.
- Produces: the docs-level nav/footer HTML block (with `../` prefixes) that Tasks 7-10 reuse verbatim.

- [ ] **Step 1: Write `site/docs/index.html`**

```html
<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Documentation · Keel</title>
<link rel="icon" href="../assets/keel-logo.png">
<link rel="stylesheet" href="../assets/style.css">
</head>
<body>
<header class="site-header">
  <div class="container nav">
    <a class="brand" href="../index.html"><img src="../assets/keel-logo.png" alt="Keel logo" class="brand-logo">Keel</a>
    <nav class="nav-links">
      <a href="../why.html">Why</a>
      <div class="nav-dropdown">
        <a href="index.html">Docs</a>
        <div class="nav-dropdown-menu">
          <a href="architecture.html">Architecture</a>
          <a href="jailspec.html">JailSpec Reference</a>
          <a href="cli.html">CLI Reference</a>
          <a href="api.html">HTTP API Reference</a>
        </div>
      </div>
      <a href="../quickstart.html">Quickstart</a>
      <a href="../journey.html">Journey</a>
      <a class="nav-github" href="https://github.com/lebarondecharlus/keel" target="_blank" rel="noopener">GitHub &#8599;</a>
    </nav>
  </div>
</header>
<main>
  <section class="page-header">
    <div class="container">
      <h1>Documentation</h1>
      <p class="lead">Reference material for the JailSpec YAML format, the keelctl CLI, and the keel-agentd / keel-controlplane HTTP APIs, plus an overview of how the crates fit together.</p>
    </div>
  </section>

  <section>
    <div class="container">
      <div class="link-card-grid">
        <a class="link-card" href="architecture.html">
          <h3>Architecture</h3>
          <p>The crate map and how keel-agentd's Reconciler, keel-spec, keel-jail, keel-zfs, and keel-net compose, plus the single-worker-thread pattern shared with keel-controlplane.</p>
        </a>
        <a class="link-card" href="jailspec.html">
          <h3>JailSpec reference</h3>
          <p>Every field in the JailSpec YAML format: types, meaning, and which fields are immutable after the first apply.</p>
        </a>
        <a class="link-card" href="cli.html">
          <h3>keelctl CLI reference</h3>
          <p>apply, get, and delete: flags, example invocations, and example output.</p>
        </a>
        <a class="link-card" href="api.html">
          <h3>HTTP API reference</h3>
          <p>keel-agentd's Unix-socket API and keel-controlplane's TCP API: routes, request/response bodies, and error cases.</p>
        </a>
      </div>
    </div>
  </section>
</main>
<footer class="site-footer">
  <div class="container">
    <p>Keel is open source on <a href="https://github.com/lebarondecharlus/keel" target="_blank" rel="noopener">GitHub</a>.</p>
  </div>
</footer>
</body>
</html>
```

- [ ] **Step 2: Verify structure and content**

```bash
grep -q "<title>Documentation" site/docs/index.html \
  && grep -q 'href="../assets/style.css"' site/docs/index.html \
  && grep -q 'href="architecture.html"' site/docs/index.html \
  && grep -q 'href="jailspec.html"' site/docs/index.html \
  && grep -q 'href="cli.html"' site/docs/index.html \
  && grep -q 'href="api.html"' site/docs/index.html \
  && grep -q "</html>" site/docs/index.html \
  && echo OK
```

Expected: `OK`

- [ ] **Step 3: Commit**

```bash
git add site/docs/index.html
git commit -m "Add Keel website docs landing page"
```

---

### Task 7: Architecture page

**Files:**
- Create: `site/docs/architecture.html`

**Interfaces:**
- Consumes: docs-level nav/footer block established in Task 6.

- [ ] **Step 1: Write `site/docs/architecture.html`**

```html
<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Architecture · Keel Docs</title>
<link rel="icon" href="../assets/keel-logo.png">
<link rel="stylesheet" href="../assets/style.css">
</head>
<body>
<header class="site-header">
  <div class="container nav">
    <a class="brand" href="../index.html"><img src="../assets/keel-logo.png" alt="Keel logo" class="brand-logo">Keel</a>
    <nav class="nav-links">
      <a href="../why.html">Why</a>
      <div class="nav-dropdown">
        <a href="index.html">Docs</a>
        <div class="nav-dropdown-menu">
          <a href="architecture.html">Architecture</a>
          <a href="jailspec.html">JailSpec Reference</a>
          <a href="cli.html">CLI Reference</a>
          <a href="api.html">HTTP API Reference</a>
        </div>
      </div>
      <a href="../quickstart.html">Quickstart</a>
      <a href="../journey.html">Journey</a>
      <a class="nav-github" href="https://github.com/lebarondecharlus/keel" target="_blank" rel="noopener">GitHub &#8599;</a>
    </nav>
  </div>
</header>
<main>
  <section class="page-header">
    <div class="container">
      <h1>Architecture</h1>
      <p class="lead">How Keel's crates fit together, today.</p>
    </div>
  </section>

  <section id="crates">
    <div class="container">
      <h2>Crate map</h2>
      <table>
        <thead><tr><th>Crate</th><th>Responsibility</th></tr></thead>
        <tbody>
          <tr><td><code>keel-spec</code></td><td>The <code>JailSpec</code> YAML schema, parsing, and validation. No FreeBSD dependency; runs on any OS.</td></tr>
          <tr><td><code>keel-jail</code></td><td>The <code>JailRuntime</code> trait: create/start/stop/destroy a jail, check if it's running. A fake in-memory implementation plus a real one that shells out to <code>jail(8)</code>, <code>jexec(8)</code>, and <code>rctl(8)</code>.</td></tr>
          <tr><td><code>keel-zfs</code></td><td>The <code>ZfsManager</code> trait: clone a jail's root filesystem from a base dataset, destroy it again. Fake plus a real implementation over <code>zfs(8)</code>.</td></tr>
          <tr><td><code>keel-net</code></td><td>The <code>NetManager</code> trait: create a bridge, attach a jail to it over an <code>epair(4)</code> pair with a static address, tear it down. Fake plus a real implementation over FreeBSD networking commands.</td></tr>
          <tr><td><code>keel-agentd</code></td><td>The reconciliation daemon: a generic <code>Reconciler&lt;J, Z, N&gt;</code> over the three traits above, a crash-safe on-disk state store, crash-loop backoff, and a local HTTP API over a Unix socket.</td></tr>
          <tr><td><code>keelctl</code></td><td>The CLI: <code>apply</code>, <code>get</code>, <code>delete</code>, talking to <code>keel-agentd</code>'s Unix socket.</td></tr>
          <tr><td><code>keel-controlplane</code></td><td>The node registry: register/heartbeat/list nodes over TCP, with liveness computed on read rather than tracked by a background sweep.</td></tr>
        </tbody>
      </table>
    </div>
  </section>

  <section id="trait-pattern">
    <div class="container">
      <h2>The trait + fake + real pattern</h2>
      <p>Every FreeBSD-facing crate follows the same shape: a trait describing what the crate does (<code>JailRuntime</code>, <code>ZfsManager</code>, <code>NetManager</code>), an in-memory <code>Fake</code> implementation for fast tests on any machine, and a real implementation that shells out to the corresponding FreeBSD command. <code>keel-agentd</code>'s <code>Reconciler&lt;J, Z, N&gt;</code> is generic over all three, so the exact same reconciliation logic runs against <code>Fake*</code> implementations in CI and against <code>Process*</code>/<code>Cli*</code> implementations on a real host.</p>
    </div>
  </section>

  <section id="reconciliation-flow">
    <div class="container">
      <h2>Reconciliation flow</h2>
      <p><code>Reconciler</code>'s public API is small on purpose: <code>new</code>, <code>apply</code>, <code>delete</code>, <code>reconcile</code>. <code>apply</code> and <code>delete</code> update the desired state immediately (a caller's request is reconciled before the HTTP response returns); <code>reconcile</code> runs on a 5-second timer and diffs desired versus observed state for every jail in one pass, provisioning what's missing, restarting what crashed (with per-jail backoff starting at one second, doubling to a five-minute cap, and resetting after sixty seconds of stable uptime), and cleaning up what was removed. A failure reconciling one jail is returned in a per-jail failure list rather than aborting the whole pass, so one broken jail never blocks the others.</p>
    </div>
  </section>

  <section id="worker-thread">
    <div class="container">
      <h2>Single-worker-thread ownership</h2>
      <p>Both <code>keel-agentd</code> and <code>keel-controlplane</code> use the same concurrency pattern: a single worker thread exclusively owns the mutable state (the <code>Reconciler</code> in one case, an in-memory <code>Registry</code> in the other), reachable only through an <code>mpsc</code> command channel. The HTTP server (a small hand-rolled parser over <code>httparse</code>, no async runtime) and, for <code>keel-agentd</code>, the reconciliation timer, only ever reach that state through the channel, so there is never a second concurrent owner to synchronize against.</p>
    </div>
  </section>
</main>
<footer class="site-footer">
  <div class="container">
    <p>Keel is open source on <a href="https://github.com/lebarondecharlus/keel" target="_blank" rel="noopener">GitHub</a>.</p>
  </div>
</footer>
</body>
</html>
```

- [ ] **Step 2: Verify structure and content**

```bash
grep -q "<title>Architecture" site/docs/architecture.html \
  && grep -q "keel-controlplane" site/docs/architecture.html \
  && grep -q "Reconciler&lt;J, Z, N&gt;" site/docs/architecture.html \
  && grep -q "</html>" site/docs/architecture.html \
  && echo OK
```

Expected: `OK`

- [ ] **Step 3: Commit**

```bash
git add site/docs/architecture.html
git commit -m "Add Keel website Architecture doc page"
```

---

### Task 8: JailSpec reference page

**Files:**
- Create: `site/docs/jailspec.html`

**Interfaces:**
- Consumes: docs-level nav/footer block established in Task 6.

- [ ] **Step 1: Write `site/docs/jailspec.html`**

```html
<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>JailSpec Reference · Keel Docs</title>
<link rel="icon" href="../assets/keel-logo.png">
<link rel="stylesheet" href="../assets/style.css">
</head>
<body>
<header class="site-header">
  <div class="container nav">
    <a class="brand" href="../index.html"><img src="../assets/keel-logo.png" alt="Keel logo" class="brand-logo">Keel</a>
    <nav class="nav-links">
      <a href="../why.html">Why</a>
      <div class="nav-dropdown">
        <a href="index.html">Docs</a>
        <div class="nav-dropdown-menu">
          <a href="architecture.html">Architecture</a>
          <a href="jailspec.html">JailSpec Reference</a>
          <a href="cli.html">CLI Reference</a>
          <a href="api.html">HTTP API Reference</a>
        </div>
      </div>
      <a href="../quickstart.html">Quickstart</a>
      <a href="../journey.html">Journey</a>
      <a class="nav-github" href="https://github.com/lebarondecharlus/keel" target="_blank" rel="noopener">GitHub &#8599;</a>
    </nav>
  </div>
</header>
<main>
  <section class="page-header">
    <div class="container">
      <h1>JailSpec reference</h1>
      <p class="lead">The YAML schema for a Keel jail, defined in <code>keel-spec</code>.</p>
    </div>
  </section>

  <section id="example">
    <div class="container">
      <h2>Example</h2>
      <pre>apiVersion: keel/v1
kind: Jail
metadata:
  name: web-1
spec:
  image: base/14.2-web
  command: ["/usr/local/bin/myapp"]
  network:
    vnet: true
    bridge: keel0
    address: 10.0.0.5/24
  resources:
    cpu: "2"
    memory: "512M"
  restartPolicy: Always</pre>
    </div>
  </section>

  <section id="fields">
    <div class="container">
      <h2>Fields</h2>
      <table>
        <thead><tr><th>Field</th><th>Type</th><th>Meaning</th></tr></thead>
        <tbody>
          <tr><td><code>apiVersion</code></td><td>string</td><td>Schema version, currently always <code>keel/v1</code>.</td></tr>
          <tr><td><code>kind</code></td><td>string</td><td>Resource kind, currently always <code>Jail</code>.</td></tr>
          <tr><td><code>metadata.name</code></td><td>string</td><td>The jail's spec name. Must pass Keel's name validation. Immutable: it identifies the jail for its whole lifetime; changing it means applying a new jail, not renaming this one. The real FreeBSD jail is named <code>keel-&lt;name&gt;</code>.</td></tr>
          <tr><td><code>spec.image</code></td><td>string</td><td>Which base ZFS dataset to clone the jail's root filesystem from. With the default pool <code>zroot</code>, <code>image: base/14.2-web</code> resolves to the dataset <code>zroot/keel/base/14.2-web</code>, which must already exist and be populated. Immutable after first apply; changing it means destroying and recreating the jail.</td></tr>
          <tr><td><code>spec.command</code></td><td>list of strings</td><td>The command (and its arguments) to run inside the jail.</td></tr>
          <tr><td><code>spec.network.vnet</code></td><td>boolean</td><td>Whether the jail gets its own VNET network stack. VNET-enabled jails are created differently from the start, so this cannot be changed after first apply.</td></tr>
          <tr><td><code>spec.network.bridge</code></td><td>string</td><td>The <code>bridge(4)</code> interface the jail's <code>epair(4)</code> pair attaches to.</td></tr>
          <tr><td><code>spec.network.address</code></td><td>string (CIDR)</td><td>The static address assigned to the jail's side of the <code>epair(4)</code> pair, e.g. <code>10.0.0.5/24</code>. Validated as a CIDR address.</td></tr>
          <tr><td><code>spec.resources.cpu</code></td><td>string</td><td>CPU limit, parsed into an <code>rctl(8)</code> <code>pcpu</code> percentage (e.g. <code>"2"</code> means 2 cores' worth).</td></tr>
          <tr><td><code>spec.resources.memory</code></td><td>string</td><td>Memory limit, parsed into bytes for <code>rctl(8)</code> (e.g. <code>"512M"</code>).</td></tr>
          <tr><td><code>spec.restartPolicy</code></td><td><code>Always</code> | <code>OnFailure</code> | <code>Never</code></td><td>Whether the reconciler restarts the jail's command after it exits, and under what conditions.</td></tr>
        </tbody>
      </table>
      <p>Fields not listed as immutable above can change on a re-apply of the same <code>metadata.name</code>; the reconciler diffs the new spec against the running jail's state and reconciles the difference.</p>
    </div>
  </section>
</main>
<footer class="site-footer">
  <div class="container">
    <p>Keel is open source on <a href="https://github.com/lebarondecharlus/keel" target="_blank" rel="noopener">GitHub</a>.</p>
  </div>
</footer>
</body>
</html>
```

- [ ] **Step 2: Verify structure and content**

```bash
grep -q "<title>JailSpec Reference" site/docs/jailspec.html \
  && grep -q "restartPolicy" site/docs/jailspec.html \
  && grep -q "zroot/keel/base/14.2-web" site/docs/jailspec.html \
  && grep -q "</html>" site/docs/jailspec.html \
  && echo OK
```

Expected: `OK`

- [ ] **Step 3: Commit**

```bash
git add site/docs/jailspec.html
git commit -m "Add Keel website JailSpec reference doc page"
```

---

### Task 9: CLI reference page

**Files:**
- Create: `site/docs/cli.html`

**Interfaces:**
- Consumes: docs-level nav/footer block established in Task 6.

- [ ] **Step 1: Write `site/docs/cli.html`**

```html
<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>keelctl CLI Reference · Keel Docs</title>
<link rel="icon" href="../assets/keel-logo.png">
<link rel="stylesheet" href="../assets/style.css">
</head>
<body>
<header class="site-header">
  <div class="container nav">
    <a class="brand" href="../index.html"><img src="../assets/keel-logo.png" alt="Keel logo" class="brand-logo">Keel</a>
    <nav class="nav-links">
      <a href="../why.html">Why</a>
      <div class="nav-dropdown">
        <a href="index.html">Docs</a>
        <div class="nav-dropdown-menu">
          <a href="architecture.html">Architecture</a>
          <a href="jailspec.html">JailSpec Reference</a>
          <a href="cli.html">CLI Reference</a>
          <a href="api.html">HTTP API Reference</a>
        </div>
      </div>
      <a href="../quickstart.html">Quickstart</a>
      <a href="../journey.html">Journey</a>
      <a class="nav-github" href="https://github.com/lebarondecharlus/keel" target="_blank" rel="noopener">GitHub &#8599;</a>
    </nav>
  </div>
</header>
<main>
  <section class="page-header">
    <div class="container">
      <h1>keelctl CLI reference</h1>
      <p class="lead">keelctl talks to keel-agentd's local HTTP API over its Unix socket.</p>
    </div>
  </section>

  <section id="socket-flag">
    <div class="container">
      <h2>Global flag</h2>
      <table>
        <thead><tr><th>Flag</th><th>Default</th><th>Meaning</th></tr></thead>
        <tbody>
          <tr><td><code>--socket PATH</code></td><td><code>/var/run/keel-agentd.sock</code></td><td>Path to the keel-agentd Unix socket to talk to.</td></tr>
        </tbody>
      </table>
    </div>
  </section>

  <section id="apply">
    <div class="container">
      <h2>keelctl apply -f FILE</h2>
      <p>Reads a <code>JailSpec</code> YAML file, validates it locally, and sends it as a <code>PUT /jails/&lt;name&gt;</code> request (the name comes from the spec's own <code>metadata.name</code>).</p>
      <pre>keelctl apply -f jail.yaml</pre>
    </div>
  </section>

  <section id="get">
    <div class="container">
      <h2>keelctl get [name]</h2>
      <p>With no argument, lists every jail keel-agentd is tracking (<code>GET /jails</code>). With a name, fetches just that one (<code>GET /jails/&lt;name&gt;</code>).</p>
      <pre>keelctl get
keelctl get web-1</pre>
    </div>
  </section>

  <section id="delete">
    <div class="container">
      <h2>keelctl delete NAME</h2>
      <p>Tears down the named jail, its ZFS dataset, its network attachment, and its resource limits (<code>DELETE /jails/&lt;name&gt;</code>).</p>
      <pre>keelctl delete web-1</pre>
    </div>
  </section>

  <section id="errors">
    <div class="container">
      <h2>Errors</h2>
      <p>On any non-2xx response, keelctl prints the server's error message to stderr and exits non-zero. See the <a href="api.html">HTTP API reference</a> for the specific error cases each endpoint can return.</p>
    </div>
  </section>
</main>
<footer class="site-footer">
  <div class="container">
    <p>Keel is open source on <a href="https://github.com/lebarondecharlus/keel" target="_blank" rel="noopener">GitHub</a>.</p>
  </div>
</footer>
</body>
</html>
```

- [ ] **Step 2: Verify structure and content**

```bash
grep -q "<title>keelctl CLI Reference" site/docs/cli.html \
  && grep -q "keelctl apply -f jail.yaml" site/docs/cli.html \
  && grep -q "keelctl delete web-1" site/docs/cli.html \
  && grep -q 'href="api.html"' site/docs/cli.html \
  && grep -q "</html>" site/docs/cli.html \
  && echo OK
```

Expected: `OK`

- [ ] **Step 3: Commit**

```bash
git add site/docs/cli.html
git commit -m "Add Keel website CLI reference doc page"
```

---

### Task 10: HTTP API reference page

**Files:**
- Create: `site/docs/api.html`

**Interfaces:**
- Consumes: docs-level nav/footer block established in Task 6.

- [ ] **Step 1: Write `site/docs/api.html`**

```html
<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>HTTP API Reference · Keel Docs</title>
<link rel="icon" href="../assets/keel-logo.png">
<link rel="stylesheet" href="../assets/style.css">
</head>
<body>
<header class="site-header">
  <div class="container nav">
    <a class="brand" href="../index.html"><img src="../assets/keel-logo.png" alt="Keel logo" class="brand-logo">Keel</a>
    <nav class="nav-links">
      <a href="../why.html">Why</a>
      <div class="nav-dropdown">
        <a href="index.html">Docs</a>
        <div class="nav-dropdown-menu">
          <a href="architecture.html">Architecture</a>
          <a href="jailspec.html">JailSpec Reference</a>
          <a href="cli.html">CLI Reference</a>
          <a href="api.html">HTTP API Reference</a>
        </div>
      </div>
      <a href="../quickstart.html">Quickstart</a>
      <a href="../journey.html">Journey</a>
      <a class="nav-github" href="https://github.com/lebarondecharlus/keel" target="_blank" rel="noopener">GitHub &#8599;</a>
    </nav>
  </div>
</header>
<main>
  <section class="page-header">
    <div class="container">
      <h1>HTTP API reference</h1>
      <p class="lead">Both APIs speak YAML request/response bodies over HTTP/1.1, parsed with a small hand-rolled parser (<code>httparse</code>), no async runtime.</p>
    </div>
  </section>

  <section id="agentd">
    <div class="container">
      <h2>keel-agentd (Unix socket)</h2>
      <p>Default socket: <code>/var/run/keel-agentd.sock</code>.</p>
      <table>
        <thead><tr><th>Method &amp; path</th><th>Body</th><th>Success</th><th>Errors</th></tr></thead>
        <tbody>
          <tr><td><code>PUT /jails/&lt;name&gt;</code></td><td>JailSpec YAML; <code>metadata.name</code> must match <code>&lt;name&gt;</code></td><td>200, reconciled immediately before responding</td><td>400 invalid YAML or name/body mismatch, 409 attempted change to an immutable field</td></tr>
          <tr><td><code>GET /jails</code></td><td>none</td><td>200, YAML list of every tracked jail</td><td>&mdash;</td></tr>
          <tr><td><code>GET /jails/&lt;name&gt;</code></td><td>none</td><td>200, YAML for that jail</td><td>404 unknown jail</td></tr>
          <tr><td><code>DELETE /jails/&lt;name&gt;</code></td><td>none</td><td>200, jail/dataset/network/limits torn down before responding</td><td>404 unknown jail</td></tr>
        </tbody>
      </table>
    </div>
  </section>

  <section id="controlplane">
    <div class="container">
      <h2>keel-controlplane (TCP)</h2>
      <p>Nodes live on separate hosts, so this API is served over a <code>TcpListener</code> instead of a Unix socket.</p>
      <table>
        <thead><tr><th>Method &amp; path</th><th>Body</th><th>Success</th><th>Errors</th></tr></thead>
        <tbody>
          <tr><td><code>POST /nodes/register</code></td><td>YAML: <code>id</code>, <code>addr</code></td><td>200, node registered or re-registered</td><td>400 invalid YAML</td></tr>
          <tr><td><code>POST /nodes/&lt;id&gt;/heartbeat</code></td><td>none</td><td>200, node's last-seen time updated</td><td>404 unknown node (the node should re-register)</td></tr>
          <tr><td><code>GET /nodes</code></td><td>none</td><td>200, YAML list of every node: <code>id</code>, <code>addr</code>, computed <code>status</code> (<code>Alive</code> within 15 seconds of the last heartbeat, otherwise <code>Dead</code>), and <code>last_seen_secs</code></td><td>&mdash;</td></tr>
        </tbody>
      </table>
      <p>Liveness is computed on every read rather than tracked by a background sweep. There is no persistence: a control-plane restart forgets every node, and each node notices on its next heartbeat and re-registers automatically.</p>
    </div>
  </section>
</main>
<footer class="site-footer">
  <div class="container">
    <p>Keel is open source on <a href="https://github.com/lebarondecharlus/keel" target="_blank" rel="noopener">GitHub</a>.</p>
  </div>
</footer>
</body>
</html>
```

- [ ] **Step 2: Verify structure and content**

```bash
grep -q "<title>HTTP API Reference" site/docs/api.html \
  && grep -q "PUT /jails/" site/docs/api.html \
  && grep -q "POST /nodes/register" site/docs/api.html \
  && grep -q "last_seen_secs" site/docs/api.html \
  && grep -q "</html>" site/docs/api.html \
  && echo OK
```

Expected: `OK`

- [ ] **Step 3: Commit**

```bash
git add site/docs/api.html
git commit -m "Add Keel website HTTP API reference doc page"
```

---

### Task 11: GitHub Pages deploy workflow

**Files:**
- Create: `.github/workflows/pages.yml`

**Interfaces:**
- Consumes: `site/` directory (all prior tasks).

- [ ] **Step 1: Write `.github/workflows/pages.yml`**

```yaml
name: Deploy website to GitHub Pages

on:
  push:
    branches: [main]
    paths:
      - "site/**"
      - ".github/workflows/pages.yml"
  workflow_dispatch:

permissions:
  contents: read
  pages: write
  id-token: write

concurrency:
  group: pages
  cancel-in-progress: true

jobs:
  deploy:
    runs-on: ubuntu-latest
    environment:
      name: github-pages
      url: ${{ steps.deployment.outputs.page_url }}
    steps:
      - name: Checkout
        uses: actions/checkout@v4
      - name: Configure Pages
        uses: actions/configure-pages@v5
      - name: Upload site as Pages artifact
        uses: actions/upload-pages-artifact@v3
        with:
          path: site
      - name: Deploy to GitHub Pages
        id: deployment
        uses: actions/deploy-pages@v4
```

- [ ] **Step 2: Verify the workflow file is valid YAML**

```bash
python3 -c "import yaml, sys; yaml.safe_load(open('.github/workflows/pages.yml')); print('OK')"
```

Expected: `OK`

- [ ] **Step 3: Commit**

```bash
git add .github/workflows/pages.yml
git commit -m "Add GitHub Actions workflow to deploy the website to GitHub Pages"
```

---

### Task 12: Full-site link check and local render verification

**Files:**
- None created; this task only verifies Tasks 1-11's output.

**Interfaces:**
- Consumes: every file under `site/` from Tasks 1-11.

- [ ] **Step 1: Check every internal relative link resolves to a real file**

```bash
cd site
missing=0
for f in $(find . -name "*.html"); do
  dir=$(dirname "$f")
  for href in $(grep -oE 'href="[^"]+"' "$f" | sed -E 's/href="([^"]+)"/\1/'); do
    case "$href" in
      http*|"#"*) continue ;;
    esac
    target="$dir/${href%%#*}"
    if [ ! -f "$target" ]; then
      echo "BROKEN LINK in $f: $href (resolved to $target)"
      missing=1
    fi
  done
done
[ "$missing" -eq 0 ] && echo "ALL LINKS OK"
cd ..
```

Expected: `ALL LINKS OK` with no `BROKEN LINK` lines above it.

- [ ] **Step 2: Check every page references the stylesheet and closes its tags**

```bash
for f in $(find site -name "*.html"); do
  grep -q "style.css" "$f" || echo "MISSING STYLESHEET: $f"
  grep -q "</html>" "$f" || echo "UNCLOSED HTML: $f"
done
echo "DONE"
```

Expected: `DONE` with no `MISSING STYLESHEET` or `UNCLOSED HTML` lines above it.

- [ ] **Step 3: Serve the site locally and render the homepage and one docs page**

```bash
cd site && python3 -m http.server 8123 >/tmp/keel-site-server.log 2>&1 &
echo $! > /tmp/keel-site-server.pid
sleep 1
curl -sf http://localhost:8123/index.html >/dev/null && echo "index OK"
curl -sf http://localhost:8123/docs/jailspec.html >/dev/null && echo "jailspec OK"
kill "$(cat /tmp/keel-site-server.pid)"
cd ..
```

Expected: `index OK` and `jailspec OK`, then the server process is killed cleanly.

Use the `run` skill (or open `http://localhost:8123/index.html` in a browser while the server above is running, before killing it) to visually confirm: the nav dropdown under "Docs" opens on hover, the homepage feature-card grid and comparison table render correctly at both desktop and narrow-viewport widths, and the Journey page's milestone list renders with the orange left-border timeline styling.

- [ ] **Step 4: Note the one manual repo-settings step**

No file change for this step. After these commits are pushed, enable GitHub Pages once in the repo settings: **Settings → Pages → Build and deployment → Source: GitHub Actions**. This is a one-time manual action outside this codebase; the workflow from Task 11 will not deploy anywhere until it's done.

- [ ] **Step 5: Final commit if any fixes were needed**

If Steps 1-3 found any broken links, missing stylesheet references, or unclosed tags, fix them in the relevant page file(s) from Tasks 2-10, then:

```bash
git add site
git commit -m "Fix broken links / structural issues found in full-site verification"
```

If no fixes were needed, skip this step (nothing to commit).
