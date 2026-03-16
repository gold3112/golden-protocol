# GOLDEN PROTOCOL

> Browsers were windows. What comes next is a place.

**Live:** [space.gold3112.online](https://space.gold3112.online)
**Architecture:** [ARCHITECTURE.md](./ARCHITECTURE.md)
**Privacy Policy:** [space.gold3112.online/privacy-policy](https://space.gold3112.online/privacy-policy)

---

## Try it now

```bash
# See the field state as a human-readable terminal output
curl "https://space.gold3112.online/field?interest=programming&format=text"

# Change your interest — the geometry shifts
curl "https://space.gold3112.online/field?interest=philosophy+music&format=text"
```

Or open [space.gold3112.online](https://space.gold3112.online) in a browser. Move your mouse — that's you walking.

---

## Background

### The Problem

Today's internet is a collection of isolated rooms. Every time you cross a service boundary, your identity fragments, context is lost, and there is no road between them. The browser as a "window" has not changed this structure in thirty years.

The web is entirely **pull-based** — you search, type a URL, a page arrives. Nothing happens without explicit intent. You cannot exist without wanting something.

### What This Builds

A shared space owned by no one — **a field**.

- Never sleeps. Someone is always present, 24/7, globally
- Access method is irrelevant. Browser, CLI, extension — all enter the same space
- Full compatibility with the existing internet
- Open foundation. Anyone can run a node and join

### The Core Experience — Wandering (散策)

Not "navigating to a destination" but **wandering** — purposeless movement, accidental encounters. Even without intent, simply being present causes the space to respond.

```
exist → space responds → notice depth → drawn in
```

The user does nothing deliberate. Yet something happens. That surprise is the first experience.

### Information Depth

The metaverse failed because it tried to render space in 3D. But depth is not a visual problem — it is a problem of **information structure**.

> The feeling of "unseen, yet present" — that is what depth actually is.

A CLI terminal and a 3D world can both implement depth. Because depth is not rendering — it is structure.

---

## Mathematical Structure

### Distance Definition

```
distance(A, B, U) =
    0.25 × semantic(A,B)    — meaning similarity (embedding cosine distance)
  + 0.20 × relational(A,B) — graph relationship distance
  + 0.20 × activity(A,B)   — difference in current activity
  + 0.15 × temporal(A,B)   — time elapsed since last contact
  + 0.20 × attention(U,B)  — distance from user U's interest to B (subjective)
```

The same space has a different geometry depending on who is looking.

### Depth: near / horizon

```
near    = low interaction latency (visible, directly reachable)
horizon = detectable but unresolved (sensed, not yet in focus)
beyond  = does not exist in this field
```

The near/horizon boundary is not fixed — it is computed dynamically from the distance distribution of the current field.

---

## Architecture

```
[Browser Extension]  [Web Client]  [CLI / Any Client]
        |                 |                |
        └──────── HTTPS ──────────────────┘
                          |
              ┌─────────────────────┐
              │   Cloudflare Tunnel  │
              └──────────┬──────────┘
                          │
       ┌──────────────────┼──────────────────┐
       │                  │                  │
  [Node A]           [Node B]           [Node C]
 golden_core        golden_core        golden_core
 SQLite WAL         SQLite WAL         SQLite WAL
       └──────── gossip ──┴──────── gossip ──┘
```

`single logical world + distributed infrastructure`

Any node can be reached and you are in the same space. If a node goes down, the space continues.

---

## Implementation Status

### Core

- [x] Distance function (5-component weighted sum)
- [x] Local embedding (fastembed, BAAI/bge-small-en-v1.5, no API key)
- [x] Dynamic thresholds (near/horizon boundaries from distance distribution)
- [x] Field state server (Rust + Axum, :7331)
- [x] Identity layer — SQLite persistence + encounter history + evolving interest vector
- [x] Relational distance — computed from encounter history
- [x] Drift / flow — gravitational pull from global activity counters
- [x] Passive absorption — simply being near entities slowly shapes interest vector
- [x] Entity persistence — SQLite WAL, survives restarts
- [x] Entity natural decay — activity_vec regresses toward embedding (~38h half-life)
- [x] **Distance components** — 5-component breakdown per entity in field response

### Space & Presence

- [x] Multi-user presence — SSE-connected users appear in near/horizon
- [x] **Individuality** — named presence, shown on canvas near cyan dot
- [x] **Time-limited events** — entities with expiry, urgency glow, burst on removal
- [x] **Entity emergence** — new entities auto-generated from user convergence
- [x] **Entity rotation** — stale unvisited articles removed, replaced with fresh ones

### Communication

- [x] **Ambient messaging** — anonymous entity-scoped messages, auto-expire after 2 hours
- [x] Messages float near entities on canvas

### Clients

- [x] SSE stream (`GET /field/stream`) — field state pushed every 2 seconds
- [x] **CLI format** — `?format=text` returns beautiful terminal output with distance breakdown
- [x] Landing page — immersive wandering UI (custom cursor, mouse-walk, particles, vignette, constellation lines)
- [x] **Browser extension (Manifest V3)** — Chrome Web Store review pending
  - Passive absorption runs in background (no popup needed)
  - Badge shows live presence count

### Infrastructure

- [x] Rate limiting — 60 req/min, burst 10
- [x] CORS
- [x] Public — Cloudflare Tunnel → space.gold3112.online
- [x] systemd service — auto-starts on boot
- [x] **Federation gossip** — multiple nodes share the space
- [x] RSS auto-ingest — hourly refresh of registered feeds (HN/Lobsters/Zenn/arxiv etc., 160+ entities)
- [x] Connectors — `POST /connect/rss` / `POST /connect/url`
- [x] Privacy policy page — `/privacy-policy`

---

## Running a Node

```bash
# Join the existing space
GOLDEN_NODE_URL=https://yourserver.example.com \
GOLDEN_BOOTSTRAP=https://space.gold3112.online \
./golden_core

# Or build from source
cd core && cargo build --release
```

On startup, the node automatically registers with the bootstrap, pulls all entities via gossip, and becomes part of the space.

---

## Directory Structure

```
/GOLDEN_PROTOCOL/
├── README.md                   ← Japanese docs
├── README_EN.md                ← This file
├── ARCHITECTURE.md             ← Design & protocol spec
├── golden_protocol_extension.zip   ← Chrome Store package
├── docs/
│   ├── privacy-policy.md       ← Privacy policy source
│   └── store-listing.md        ← Chrome Store listing draft
├── core/                       ← Rust core server
│   ├── Cargo.toml
│   └── src/
│       ├── main.rs             — server + all endpoints + background tasks + landing HTML
│       ├── distance/mod.rs     — 5-component distance + dynamic thresholds
│       ├── field/mod.rs        — FieldState / DriftSignal / ObservedEntity
│       ├── graph/mod.rs        — Entity / SpaceGraph (petgraph)
│       ├── embedding/mod.rs    — local embedding (fastembed)
│       └── identity/mod.rs     — Identity / SQLite / entity CRUD / RSS / messages
└── extension/
    ├── manifest.json           Manifest V3
    ├── content.js              page text extraction → notifies background.js
    ├── background.js           passive absorption + badge update (no popup needed)
    ├── popup.html/js/css       field state UI
    └── icons/                  16px / 48px / 128px PNG
```

---

## API Reference

### `GET /field`

Returns current field state as JSON or terminal text.

| Parameter | Default | Description |
|-----------|---------|-------------|
| `user_id` | — | Identity UUID |
| `interest` | `"curiosity exploration knowledge"` | Interest text |
| `passive` | `false` | If true, passively update interest vector |
| `name` | — | Display name (shown to other users) |
| `format` | `json` | `text` for terminal-formatted output |

### `GET /field/stream`
SSE stream. Field state pushed every 2 seconds. While connected, you appear in other users' near/horizon.

### `GET /presence`
List of currently connected users.

### `GET /identity/new?interest=...`
Create a new identity, returns UUID.

### `GET /identity/:id`
Get identity state.

### `POST /encounter`
Explicitly record an encounter with near entities.
```json
{ "user_id": "uuid", "position": "plaza", "near_labels": ["rust_weekly"], "interest_text": "rust" }
```

### `POST /message`
Leave a message near an entity (auto-deleted after 2 hours).
```json
{ "entity_label": "philosophy_debate", "text": "interesting thread", "author": "wanderer_xxxx" }
```

### `GET /entities`
All entities (summary).

### `POST /entities`
Add an entity. Optionally time-limited with `duration_hours`.
```json
{ "label": "rust_study_group", "kind": "Event", "text": "rust programming", "duration_hours": 48 }
```

### `DELETE /entity/:id`
Remove an entity.

### `POST /connect/rss`
Ingest RSS/Atom feed as Stream entities. Auto-refreshed hourly.
```json
{ "url": "https://hnrss.org/frontpage", "max_items": 20 }
```

### `POST /connect/url`
Extract URL text and add as Data entity.

### `GET /feeds`
List registered RSS feeds.

### Federation

| Method | Path | Description |
|--------|------|-------------|
| GET | `/peers` | Known peer nodes |
| POST | `/peer/register` | Register as peer |
| GET | `/entities/export` | All entities with embeddings (gossip sync) |

---

*Last updated: 2026-03-17 (prototype v8 — wandering UI + ambient messaging + Chrome extension + federation)*
