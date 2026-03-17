mod distance;
mod embedding;
mod field;
mod graph;
mod identity;

use axum::{
    extract::{Path, Query, State},
    response::{Html, sse::{Event, KeepAlive, Sse}},
    routing::{get, post},
    Json, Router,
};
use chrono::{DateTime, Utc};
use distance::{compute_distance, semantic_dist, temporal_dist, DistanceWeights, DynamicThresholds};
use field::{DriftSignal, FieldState, ObservedEntity};
use graph::{Entity, EntityKind, SpaceGraph};
use identity::Identity;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_stream::{Stream, StreamExt as _, wrappers::IntervalStream};
use axum::{middleware::{self, Next}, http::StatusCode};
use governor::{clock::DefaultClock, state::keyed::DefaultKeyedStateStore, Quota, RateLimiter};
use std::num::NonZeroU32;
use std::net::IpAddr;
use tower_http::cors::{Any, CorsLayer};
use uuid::Uuid;

type IpLimiter = RateLimiter<IpAddr, DefaultKeyedStateStore<IpAddr>, DefaultClock>;

// --- 共有状態 ---

struct AppState {
    graph:           Mutex<SpaceGraph>,
    weights:         DistanceWeights,
    /// グローバル活動カウンタ: label -> 累計encounter数 (drift計算用)
    activity:        Mutex<HashMap<String, u32>>,
    /// SSE接続中ユーザーのin-memory状態
    connected_users: Mutex<HashMap<Uuid, UserPresence>>,
    /// IPごとのレートリミッター
    rate_limiter:    IpLimiter,
    /// このノードの固有ID
    node_id:         Uuid,
    /// 接続中のピアノード: url -> node_id
    peers:           Mutex<HashMap<String, Uuid>>,
}

/// SSE接続中ユーザーの状態 (永続化しない)
#[derive(Clone)]
struct UserPresence {
    id:           Uuid,
    name:         String,
    interest_vec: Vec<f32>,
    position:     String,
    last_seen:    DateTime<Utc>,
}

// --- リクエスト / レスポンス型 ---

#[derive(Deserialize, Clone)]
struct FieldQuery {
    user_id:     Option<Uuid>,
    interest:    Option<String>,
    near_pct:    Option<f32>,
    horizon_pct: Option<f32>,
    passive:     Option<bool>,
    name:        Option<String>,
    format:      Option<String>,
}

#[derive(Deserialize)]
struct EntityRequest {
    label:          String,
    kind:           Option<EntityKind>,
    /// embedding のシードテキスト。省略時は label をそのまま使う
    text:           Option<String>,
    duration_hours: Option<f64>,
}

#[derive(Serialize)]
struct EntitySummary {
    id:          Uuid,
    label:       String,
    kind:        String,
    last_active: String,
    expires_at:  Option<String>,
}

#[derive(Deserialize)]
struct EncounterRequest {
    user_id:       Uuid,
    position:      String,
    near_labels:   Vec<String>,
    interest_text: String,
}

#[derive(Serialize)]
struct IdentitySummary {
    id:               Uuid,
    created_at:       String,
    last_seen:        String,
    position:         String,
    encounter_count:  usize,
    interest_preview: Vec<f32>,
}

// --- レート制限ミドルウェア ---

async fn rate_limit(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    req: axum::extract::Request,
    next: Next,
) -> Result<axum::response::Response, StatusCode> {
    let ip = extract_ip(&req);
    match state.rate_limiter.check_key(&ip) {
        Ok(_)  => Ok(next.run(req).await),
        Err(_) => Err(StatusCode::TOO_MANY_REQUESTS),
    }
}

/// IP アドレスを抽出する
/// Cloudflare 経由: CF-Connecting-IP → X-Forwarded-For → フォールバック
fn extract_ip(req: &axum::extract::Request) -> IpAddr {
    let headers = req.headers();
    headers.get("CF-Connecting-IP")
        .or_else(|| headers.get("X-Forwarded-For"))
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.split(',').next())
        .and_then(|s| s.trim().parse::<IpAddr>().ok())
        .unwrap_or(IpAddr::from([127, 0, 0, 1]))
}

// --- field state 計算コア (HTTP・SSE共通) ---

fn compute_field_state(state: &AppState, q: &FieldQuery) -> FieldState {
    let near_pct    = q.near_pct.unwrap_or(0.30);
    let horizon_pct = q.horizon_pct.unwrap_or(0.70);
    let passive     = q.passive.unwrap_or(false);

    let mut identity = q.user_id.and_then(|uid| Identity::load(&uid).ok());

    let user_interest = identity.as_ref()
        .map(|i| i.interest_vec.clone())
        .unwrap_or_else(|| fallback_interest(&q.interest));

    let tau = 86400.0_f64;

    // 接続中ユーザー (自分以外) を取得 — ロックはすぐ解放
    let other_users: Vec<UserPresence> = {
        let users = state.connected_users.lock().unwrap();
        users.values()
            .filter(|u| q.user_id.map_or(true, |id| u.id != id))
            .cloned()
            .collect()
    };

    let graph    = state.graph.lock().unwrap();
    let weights  = &state.weights;
    let activity = state.activity.lock().unwrap();

    // Pass 1: グラフエンティティの距離計算
    let mut scored: Vec<(ObservedEntity, f32, Option<Vec<f32>>)> = graph.graph
        .node_indices()
        .filter(|idx| {
            let e = &graph.graph[*idx];
            e.expires_at.map_or(true, |exp| exp > Utc::now())
        })
        .map(|idx| {
            let entity = &graph.graph[idx];

            let sem = entity.embedding.as_deref()
                .map(|emb| semantic_dist(emb, &user_interest))
                .unwrap_or(0.5);

            let rel = identity.as_ref()
                .map(|id| id.relational_dist(&entity.label))
                .unwrap_or(1.0);

            let act = entity.activity_vec.as_deref()
                .map(|av| {
                    let u = vec![1.0 / av.len() as f32; av.len()];
                    distance::activity_dist(av, &u)
                })
                .unwrap_or(0.5);

            let tmp = temporal_dist(entity, tau);

            let att = entity.embedding.as_deref()
                .map(|emb| distance::attention_dist(&user_interest, emb))
                .unwrap_or(0.5);

            let d = compute_distance(sem, rel, act, tmp, att, weights);
            (
                ObservedEntity {
                    id:         entity.id,
                    label:      entity.label.clone(),
                    distance:   d,
                    visibility: Default::default(),
                    components: Some([sem, rel, act, tmp, att]),
                },
                d,
                entity.embedding.clone(),
            )
        })
        .collect();

    // 接続中ユーザーを動的エンティティとして追加
    // temporal=0.0 (今まさにここにいる), relational=0.5 (未知)
    for user in &other_users {
        let sem = semantic_dist(&user.interest_vec, &user_interest);
        let att = distance::attention_dist(&user_interest, &user.interest_vec);
        let d   = compute_distance(sem, 0.5, 0.5, 0.0, att, weights);
        let short_id = &user.id.to_string()[..8];
        scored.push((
            ObservedEntity {
                id:         user.id,
                label:      format!("wanderer_{}", short_id),
                distance:   d,
                visibility: Default::default(),
                components: Some([sem, 0.5, 0.5, 0.0, att]),
            },
            d,
            Some(user.interest_vec.clone()),
        ));
    }

    // Pass 2: 動的閾値
    let all_distances: Vec<f32> = scored.iter().map(|(_, d, _)| *d).collect();
    let thresholds = DynamicThresholds::from_distances(&all_distances, near_pct, horizon_pct);

    let mut field = FieldState::new("plaza");
    field.thresholds = [thresholds.near, thresholds.horizon];

    let mut near_embeddings: Vec<Vec<f32>> = Vec::new();

    for (obs, d, emb) in &scored {
        let mut obs = obs.clone();
        obs.visibility = thresholds.classify(*d);
        match obs.visibility {
            distance::Visibility::Near => {
                if let Some(e) = emb { near_embeddings.push(e.clone()); }
                field.near.push(obs);
            }
            distance::Visibility::Horizon => field.horizon.push(obs),
            distance::Visibility::Beyond  => {}
        }
    }

    field.near.sort_by(|a, b| a.distance.partial_cmp(&b.distance).unwrap());
    field.horizon.sort_by(|a, b| a.distance.partial_cmp(&b.distance).unwrap());
    field.presence = field.near.len() + field.horizon.len();
    field.compute_density();

    // position = nearエンティティの文脈から生成する
    // 単一の点ではなく、意味の集まりとして場所を記述する
    // 0件: デフォルトの "plaza" のまま（FieldState::newで設定済み）
    // 1件: そのラベル
    // 2件以上: 上位2件を並べて「その周辺」を表現する
    field.position = match field.near.as_slice() {
        [] => field.position.clone(), // "plaza" を維持
        [one] => one.label.clone(),
        [first, second, ..] => format!("{}, {}", first.label, second.label),
    };

    // Pass 3: drift — horizonの中でグローバル活動が高いものを抽出
    let mut drift_candidates: Vec<(String, u32)> = field.horizon.iter()
        .map(|e| {
            let count = activity.get(&e.label).copied().unwrap_or(0);
            (e.label.clone(), count)
        })
        .filter(|(_, c)| *c > 0)
        .collect();
    drift_candidates.sort_by(|a, b| b.1.cmp(&a.1));

    let max_activity = drift_candidates.first().map(|(_, c)| *c).unwrap_or(1) as f32;
    field.drift = drift_candidates.into_iter()
        .take(3)
        .map(|(label, count)| DriftSignal {
            toward:   label,
            strength: (count as f32 / max_activity).clamp(0.0, 1.0),
        })
        .collect();

    // Pass 4 (optional): passive absorption
    drop(graph);
    drop(activity);

    if passive {
        if let Some(ref mut id) = identity {
            id.passive_absorb(near_embeddings, &field.position);
            let _ = id.save();
        }
    }

    // Pass 5: near entity のメッセージをロード (wanderer_ は除く)
    let near_labels: Vec<String> = field.near.iter()
        .filter(|e| !e.label.starts_with("wanderer_"))
        .map(|e| e.label.clone())
        .collect();
    field.messages = identity::load_messages_for_labels(&near_labels, 3);

    field
}

// --- ハンドラ ---

/// GET / — ランディングページ
async fn landing() -> Html<&'static str> {
    Html(LANDING_HTML)
}

static LANDING_HTML: &str = r##"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Golden Protocol</title>
<style>
* { box-sizing: border-box; margin: 0; padding: 0; }
body {
  background: #06060f;
  font-family: 'SF Mono','Fira Code','Menlo',monospace;
  font-size: 12px;
  height: 100vh;
  overflow: hidden;
}
canvas { position: fixed; top: 0; left: 0; width: 100%; height: 100%; }

/* ─── entry overlay ─────────────────────────────── */
#entry {
  position: fixed; inset: 0;
  display: flex; align-items: center; justify-content: center;
  background: rgba(6,6,15,0.62);
  z-index: 100;
  transition: opacity 1.6s ease;
  backdrop-filter: blur(4px);
}
#entry.hidden { opacity: 0; pointer-events: none; }

#entry-box {
  text-align: center; max-width: 380px; padding: 0 28px;
  animation: fadeUp 0.9s cubic-bezier(0.16,1,0.3,1) forwards;
}
@keyframes fadeUp {
  from { opacity: 0; transform: translateY(16px); }
  to   { opacity: 1; transform: translateY(0); }
}

#entry-title {
  font-size: 9px; letter-spacing: 0.5em; text-transform: uppercase;
  color: #c8a840; margin-bottom: 44px;
}

#entry-q {
  font-size: 24px; color: #c4c4ec; font-weight: normal;
  line-height: 1.5; margin-bottom: 14px;
  animation: breathe 5s ease-in-out infinite;
}
#entry-sub {
  font-size: 12px; color: #6464a0; margin-bottom: 40px; line-height: 2;
}

#entry-input {
  width: 100%; background: transparent;
  border: none; border-bottom: 1px solid #484880;
  color: #c8c8f0; font-family: inherit; font-size: 16px;
  padding: 12px 0; outline: none; text-align: center;
  letter-spacing: 0.06em; transition: border-color 0.3s;
}
#entry-input::placeholder { color: #383860; }
#entry-input:focus { border-color: #8888d0; }

#entry-name {
  width: 100%; background: transparent;
  border: none; border-bottom: 1px solid #252545;
  color: #7070a8; font-family: inherit; font-size: 13px;
  padding: 10px 0; outline: none; text-align: center;
  letter-spacing: 0.05em; margin-bottom: 0;
  transition: border-color 0.3s;
}
#entry-name::placeholder { color: #282848; }
#entry-name:focus { border-color: #484878; }

#entry-actions { margin-top: 28px; display: flex; gap: 20px; justify-content: center; align-items: center; }
#entry-enter {
  background: none; border: 1px solid #5858a8;
  color: #9090d8; font-family: inherit; font-size: 11px;
  letter-spacing: 0.22em; padding: 10px 28px; cursor: pointer;
  transition: all 0.25s;
}
#entry-enter:hover {
  border-color: #a0a0e8; color: #c8c8ff;
  box-shadow: 0 0 28px rgba(120,120,240,0.13);
}
#entry-skip {
  background: none; border: none; color: #3a3a5e;
  font-family: inherit; font-size: 11px; cursor: pointer;
  letter-spacing: 0.1em; padding: 8px 0; transition: color 0.2s;
}
#entry-skip:hover { color: #686890; }

/* ─── main ui ───────────────────────────────────── */
#ui {
  position: fixed; inset: 0;
  pointer-events: none;
  display: flex; flex-direction: column;
  justify-content: space-between;
  padding: 28px 32px;
}
#top { display: flex; justify-content: space-between; align-items: flex-start; }
#tagline {
  font-size: 9px; letter-spacing: 0.24em; text-transform: uppercase;
  color: #484868; margin-bottom: 10px;
}
#position {
  font-size: 18px; color: #d4af37; letter-spacing: 0.03em;
  min-height: 28px; max-width: 400px;
  overflow: hidden; text-overflow: ellipsis; white-space: nowrap;
}
#meta-line { font-size: 10px; color: #404060; margin-top: 6px; }
#meta-line span { color: #585880; }
#presence-block { text-align: right; }
#presence-count {
  font-size: 30px; color: #7878c0; letter-spacing: -0.02em;
  transition: color 0.5s;
}
#presence-label {
  font-size: 9px; letter-spacing: 0.2em; text-transform: uppercase;
  color: #404060; margin-top: 4px;
}
#bottom { display: flex; justify-content: space-between; align-items: flex-end; }
#drift { font-size: 11px; color: #a07830; min-height: 18px; letter-spacing: 0.06em; }
#footer-right { text-align: right; }
#conn {
  font-size: 9px; letter-spacing: 0.15em; color: #484868;
  margin-bottom: 6px; transition: color 0.3s;
}
#footer-link { pointer-events: auto; }
#footer-link a {
  font-size: 10px; color: #505080; text-decoration: none;
  border-bottom: 1px solid #2a2a50; letter-spacing: 0.08em;
  transition: all 0.2s;
}
#footer-link a:hover { color: #9090d0; border-color: #6060b0; }
#cli-hint {
  font-size: 9px; color: #303052; letter-spacing: 0.06em;
  margin-top: 5px; font-family: inherit;
}

/* ─── entity panel ──────────────────────────────── */
#panel {
  position: fixed;
  width: 240px;
  background: rgba(8,8,20,0.96);
  border: 1px solid #282848;
  padding: 20px;
  pointer-events: auto;
  opacity: 0; transition: opacity 0.25s ease;
  max-height: 70vh; overflow-y: auto;
  backdrop-filter: blur(12px);
  box-shadow: 0 8px 48px rgba(0,0,0,0.7);
}
#panel.visible { opacity: 1; }
#panel-kind {
  font-size: 9px; letter-spacing: 0.22em; text-transform: uppercase;
  color: #585890; margin-bottom: 10px;
}
#panel-label {
  font-size: 13px; color: #c8c8f0; line-height: 1.6;
  margin-bottom: 14px; word-break: break-word;
}
#panel-search {
  display: block; font-size: 10px; color: #6060a8;
  text-decoration: none; letter-spacing: 0.1em;
  border-bottom: 1px solid #202040; padding-bottom: 12px; margin-bottom: 14px;
  transition: color 0.2s, border-color 0.2s;
}
#panel-search:hover { color: #9898e0; border-color: #5050b0; }
#panel-close {
  position: absolute; top: 12px; right: 14px;
  background: none; border: none; color: #484870;
  font-family: inherit; font-size: 15px; cursor: pointer;
  line-height: 1; transition: color 0.2s;
}
#panel-close:hover { color: #9090d0; }
#panel-messages { margin-bottom: 12px; min-height: 20px; }
.panel-msg { margin-bottom: 10px; }
.panel-msg-author {
  font-size: 9px; color: #505080; letter-spacing: 0.1em;
  display: block; margin-bottom: 3px;
}
.panel-msg-text {
  font-size: 11px; color: #9898c8; line-height: 1.6;
  display: block; word-break: break-word;
}
.panel-no-msg {
  font-size: 10px; color: #343458; letter-spacing: 0.05em; font-style: italic;
}
#panel-msg-form {
  display: flex; gap: 6px; margin-top: 4px;
  border-top: 1px solid #1e1e3a; padding-top: 12px;
}
#panel-msg-input {
  flex: 1; background: transparent; border: none;
  border-bottom: 1px solid #303060; color: #9898c8;
  font-family: inherit; font-size: 11px; padding: 4px 0;
  outline: none; transition: border-color 0.2s;
}
#panel-msg-input::placeholder { color: #282850; }
#panel-msg-input:focus { border-color: #5858b0; }
#panel-msg-send {
  background: none; border: none; color: #505090;
  font-family: inherit; font-size: 13px; cursor: pointer;
  padding: 0 4px; transition: color 0.2s;
}
#panel-msg-send:hover { color: #9090d8; }

/* ─── hint ──────────────────────────────────────── */
#hint {
  position: fixed; bottom: 64px; left: 50%; transform: translateX(-50%);
  font-size: 10px; color: #505078; letter-spacing: 0.16em;
  pointer-events: none; transition: opacity 2s ease;
  white-space: nowrap;
}
#hint.hidden { opacity: 0; }

/* ─── animations ────────────────────────────────── */
@keyframes breathe {
  0%,100% { opacity: 0.72; } 50% { opacity: 1; }
}
.evt-time { font-size: 9px; color: rgba(255,150,70,0.7); letter-spacing: 0.08em; }
</style>
</head>
<body>
<canvas id="c"></canvas>

<!-- entry overlay -->
<div id="entry">
  <div id="entry-box">
    <div id="entry-title">Golden Protocol</div>
    <div id="entry-q">What are you thinking about?</div>
    <div id="entry-sub">The server returns distance, not pages.<br>Others are already wandering.</div>
    <input id="entry-input" type="text" placeholder="type anything..." autocomplete="off" spellcheck="false">
    <input id="entry-name" type="text" placeholder="your name  (optional)" autocomplete="off" spellcheck="false">
    <div id="entry-actions">
      <button id="entry-enter">enter →</button>
      <button id="entry-skip">just wander</button>
    </div>
  </div>
</div>

<!-- main ui -->
<div id="ui">
  <div id="top">
    <div>
      <div id="tagline">Golden Protocol · space.gold3112.online</div>
      <div id="position">—</div>
      <div id="meta-line"><span id="conn">connecting</span></div>
    </div>
    <div id="presence-block">
      <div id="presence-count">—</div>
      <div id="presence-label">here</div>
    </div>
  </div>
  <div id="bottom">
    <div id="drift"></div>
    <div id="footer-right">
      <div id="footer-link"><a href="https://github.com/gold3112/golden-protocol" target="_blank">source / extension →</a></div>
      <div id="cli-hint">curl space.gold3112.online/field?interest=...&amp;format=text</div>
    </div>
  </div>
</div>

<!-- entity panel -->
<div id="panel">
  <button id="panel-close">×</button>
  <div id="panel-kind"></div>
  <div id="panel-label"></div>
  <a id="panel-search" href="#" target="_blank">search →</a>
  <div id="panel-messages"></div>
  <div id="panel-msg-form">
    <input id="panel-msg-input" type="text" placeholder="say something..." autocomplete="off" maxlength="280">
    <button id="panel-msg-send">→</button>
  </div>
</div>

<div id="hint">move toward something. the space responds.</div>

<script>
const canvas = document.getElementById('c');
const ctx    = canvas.getContext('2d');
let W, H, cx, cy;
let selfX, selfY;
let mouseX = -999, mouseY = -999;

// ambient particles
let particles = [];
function initParticles() {
  particles = Array.from({length: 60}, () => ({
    x:  Math.random() * W,
    y:  Math.random() * H,
    vx: (Math.random() - 0.5) * 0.22,
    vy: (Math.random() - 0.5) * 0.22,
    r:  0.4 + Math.random() * 1.1,
    a:  0.018 + Math.random() * 0.05,
  }));
}

function resize() {
  W = canvas.width  = window.innerWidth;
  H = canvas.height = window.innerHeight;
  cx = W / 2; cy = H / 2;
  if (selfX === undefined) { selfX = cx; selfY = cy; }
  initParticles();
}
window.addEventListener('resize', resize);
resize();

function labelAngle(label) {
  let h = 5381;
  for (let i = 0; i < label.length; i++) h = (((h << 5) + h) + label.charCodeAt(i)) >>> 0;
  return (h % 10000) / 10000 * Math.PI * 2;
}

// ─── 3D perspective camera ────────────────────────
const CAM = { fov: 520, h: 140, d: 190 };
const HORIZON_FRAC = 0.40;
let camLookX = 0;

function project(wx, wy, wz) {
  const relZ = wz + CAM.d;
  if (relZ < 1) return null;
  const s = CAM.fov / relZ;
  const pan = camLookX * (1 - Math.min(1, wz / 900));
  return {
    sx: cx + pan + wx * s,
    sy: H * HORIZON_FRAC + (CAM.h - wy) * s,
    s,
    ps: s * 380 / CAM.fov,   // normalized scale (1.0 at ~200 units)
  };
}

function entityWorldPos(angle, dist) {
  const wz = 55 + dist * 780;
  const wx = Math.sin(angle) * wz * 0.70;
  const wy = 16 + Math.sin(angle * 2.7 + dist * 9.3) * 52;
  return { wx, wy, wz };
}

const nodes = {};
const KIND_COLOR = {
  AI:      [170, 130, 255],
  Stream:  [70,  170, 255],
  Event:   [255, 150, 70 ],
  Data:    [80,  200, 130],
  Human:   [255, 210, 90 ],
  Service: [160, 160, 200],
};

// birth ripple queue: { wx, wy, wz, t, r, g, b }
const births = [];
const bursts = []; // entity expiry burst: { wx, wy, wz, t, r, g, b }
let prevEntityLabels = new Set();

function updateNodes(field, allEntities) {
  const distMap = {};
  (field.near    || []).forEach(e => distMap[e.label] = { d: e.distance, zone: 'near' });
  (field.horizon || []).forEach(e => distMap[e.label] = { d: e.distance, zone: 'horizon' });

  allEntities.forEach(e => {
    const angle = labelAngle(e.label);
    const info  = distMap[e.label];
    const dist  = info ? info.d : 0.88;
    const zone  = info ? info.zone : 'beyond';
    const { wx, wy, wz } = entityWorldPos(angle, dist);
    const tb = zone === 'near' ? 1.0 : zone === 'horizon' ? 0.35 : 0.08;

    if (!nodes[e.label]) {
      nodes[e.label] = {
        wx, wy, wz, b: 0, kind: e.kind, label: e.label,
        expiresAt: e.expires_at || null,
        driftPhase: Math.random() * Math.PI * 2,
        driftFreq:  0.28 + Math.random() * 0.35,
        driftAmpX:  3  + Math.random() * 5,
        driftAmpY:  5  + Math.random() * 9,
        pendingBirth: zone !== 'beyond',
        birthKind: e.kind,
      };
    }
    const n = nodes[e.label];
    n.twx = wx; n.twy = wy; n.twz = wz; n.tb = tb; n.kind = e.kind;
    n.expiresAt = e.expires_at || null;
  });
}

// camera pan: mouse shifts the look-at point horizontally
function updateCamera() {
  const target = mouseX > 0 ? (mouseX - cx) * 0.11 : 0;
  camLookX = lerp(camLookX, target, 0.022);
}

function lerp(a, b, t) { return a + (b - a) * t; }

// --- hover / click ---
let hoveredNode = null;

canvas.addEventListener('mousemove', e => {
  mouseX = e.clientX;
  mouseY = e.clientY;
  const hit = findNearestNode(e.clientX, e.clientY, 44);
  if (hit !== hoveredNode) hoveredNode = hit;
});
canvas.addEventListener('mouseleave', () => { mouseX = -999; mouseY = -999; });

canvas.addEventListener('click', e => {
  const hit = findNearestNode(e.clientX, e.clientY, 44);
  if (hit) showPanel(hit);
});

function findNearestNode(mx, my, maxDist) {
  let best = null, bestD = maxDist;
  Object.values(nodes).forEach(n => {
    if (n.b < 0.1) return;
    const d = Math.hypot(n.rx - mx, n.ry - my); // use rendered pos
    if (d < bestD) { bestD = d; best = n; }
  });
  return best;
}

let entityMessages = {}; // label -> MessageRecord[]
let currentPanelEntity = null;

// ephemeral author name — persists for the session
let authorName = sessionStorage.getItem('gp_author');
if (!authorName) {
  authorName = 'wanderer_' + Math.random().toString(36).slice(2, 6);
  sessionStorage.setItem('gp_author', authorName);
}

function escapeHtml(s) {
  return String(s).replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;');
}

function updateMessages(field) {
  const fresh = {};
  (field.messages || []).forEach(m => {
    if (!fresh[m.entity_label]) fresh[m.entity_label] = [];
    fresh[m.entity_label].push(m);
  });
  entityMessages = fresh;
  // refresh panel if open
  if (currentPanelEntity) renderPanelMessages(currentPanelEntity);
}

function renderPanelMessages(label) {
  const msgs = entityMessages[label] || [];
  const el   = document.getElementById('panel-messages');
  if (msgs.length === 0) {
    el.innerHTML = '<div class="panel-no-msg">no voices here yet</div>';
  } else {
    el.innerHTML = msgs.map(m =>
      `<div class="panel-msg">` +
      `<span class="panel-msg-author">${escapeHtml(m.author)}</span>` +
      `<span class="panel-msg-text">${escapeHtml(m.text)}</span>` +
      `</div>`
    ).join('');
  }
}

function showPanel(node) {
  const panel = document.getElementById('panel');
  // position panel near the entity, clamped to screen
  const PW = 224, PH = 300;
  const ex = node.rx ?? node.x, ey = node.ry ?? node.y;
  let left = ex + 20, top = ey - 50;
  if (left + PW > W - 16) left = ex - PW - 20;
  left = Math.max(12, left);
  top  = Math.max(12, Math.min(H - PH - 12, top));
  panel.style.left = left + 'px';
  panel.style.top  = top  + 'px';

  document.getElementById('panel-kind').textContent  = node.kind;
  document.getElementById('panel-label').textContent = node.label;
  const q = encodeURIComponent(node.label);
  document.getElementById('panel-search').href = `https://www.google.com/search?q=${q}`;
  currentPanelEntity = node.label;
  renderPanelMessages(node.label);
  document.getElementById('panel-msg-input').value = '';
  panel.classList.add('visible');
  document.getElementById('hint').classList.add('hidden');
}

document.getElementById('panel-close').addEventListener('click', () => {
  document.getElementById('panel').classList.remove('visible');
  currentPanelEntity = null;
});

async function sendMessage() {
  const input = document.getElementById('panel-msg-input');
  const text  = input.value.trim();
  if (!text || !currentPanelEntity) return;
  try {
    await fetch('/message', {
      method:  'POST',
      headers: { 'Content-Type': 'application/json' },
      body:    JSON.stringify({ entity_label: currentPanelEntity, text, author: authorName }),
    });
    input.value = '';
    await fetchField(); // refresh messages
  } catch {}
}
document.getElementById('panel-msg-send').addEventListener('click', sendMessage);
document.getElementById('panel-msg-input').addEventListener('keydown', e => {
  if (e.key === 'Enter') sendMessage();
});

// --- ambient atmosphere ---
function drawParticles() {
  particles.forEach(p => {
    p.x += p.vx; p.y += p.vy;
    if (p.x < 0) p.x += W; if (p.x > W) p.x -= W;
    if (p.y < 0) p.y += H; if (p.y > H) p.y -= H;
    ctx.fillStyle = `rgba(180,155,70,${p.a})`;
    ctx.beginPath(); ctx.arc(p.x, p.y, p.r, 0, Math.PI * 2); ctx.fill();
  });
}

function drawVignette() {
  const grd = ctx.createRadialGradient(cx, cy, Math.min(W,H)*0.22, cx, cy, Math.max(W,H)*0.78);
  grd.addColorStop(0, 'rgba(0,0,0,0)');
  grd.addColorStop(1, 'rgba(4,4,12,0.80)');
  ctx.fillStyle = grd;
  ctx.fillRect(0, 0, W, H);
}

function drawConstellation() {
  const nearNodes = Object.values(nodes).filter(n => n.b > 0.45 && n.rx);
  for (let i = 0; i < nearNodes.length; i++) {
    for (let j = i + 1; j < nearNodes.length; j++) {
      const a = nearNodes[i], b = nearNodes[j];
      const d = Math.hypot(a.rx - b.rx, a.ry - b.ry);
      if (d > 220) continue;
      const alpha = (1 - d / 220) * 0.08 * Math.min(a.b, b.b);
      ctx.strokeStyle = `rgba(90,90,150,${alpha})`;
      ctx.lineWidth = 0.5;
      ctx.setLineDash([]);
      ctx.beginPath(); ctx.moveTo(a.rx, a.ry); ctx.lineTo(b.rx, b.ry); ctx.stroke();
    }
  }
}


// --- canvas animation ---
let clock = 0;
function animate() {
  requestAnimationFrame(animate);
  clock += 0.016;
  updateCamera();

  ctx.fillStyle = 'rgba(6,6,15,0.13)';
  ctx.fillRect(0, 0, W, H);

  drawSky();
  drawGrid();
  drawHorizon();
  drawParticles();
  drawWelcomeRipples();
  drawBirths();
  drawBursts();

  // draw far-to-near for correct depth ordering
  const nodeList = Object.values(nodes);
  nodeList.sort((a, b) => (b.wz || 0) - (a.wz || 0));
  nodeList.forEach(n => {
    if (n.twx !== undefined) {
      n.wx = lerp(n.wx, n.twx, 0.032);
      n.wy = lerp(n.wy, n.twy, 0.032);
      n.wz = lerp(n.wz, n.twz, 0.032);
    }
    n.b = lerp(n.b, n.tb || 0, 0.04);
    drawNode(n);
  });

  drawConstellation();
  drawWanderers();
  drawSelf();
  drawVignette();   // fog over everything
}

function drawNode(n) {
  const b = n.b;
  if (b < 0.02) return;

  // organic drift in world space — amplitude scales with brightness
  const ds = b * 1.4;
  const dwx = n.driftAmpX * ds * Math.sin(clock * n.driftFreq + n.driftPhase);
  const dwy = n.driftAmpY * ds * Math.cos(clock * n.driftFreq * 0.71 + n.driftPhase + 1.1);
  const dwz = n.driftAmpX * 0.4 * ds * Math.sin(clock * n.driftFreq * 0.53 + n.driftPhase + 2.3);

  const proj = project(n.wx + dwx, n.wy + dwy, n.wz + dwz);
  if (!proj) return;
  n.rx = proj.sx;
  n.ry = proj.sy;
  n.projScale = proj.ps;

  // spawn birth ripple after first successful projection
  if (n.pendingBirth) {
    n.pendingBirth = false;
    const [cr, cg, cb] = KIND_COLOR[n.birthKind] || [140, 140, 190];
    births.push({ wx: n.wx, wy: n.wy, wz: n.wz, t: 0, r: cr, g: cg, b: cb });
  }

  const [r, g, bl] = KIND_COLOR[n.kind] || [140, 140, 190];
  const isHovered  = hoveredNode === n;
  const pulse  = 1 + (isHovered ? 0.3 : 0.10) * Math.sin(clock * 1.4 + labelAngle(n.label));
  const radius = (3 + b * 6) * proj.ps * pulse * (isHovered ? 1.4 : 1);

  let urgency = 0;
  if (n.expiresAt) {
    const remaining = new Date(n.expiresAt) - Date.now();
    if (remaining <= 0) {
      urgency = 1;
    } else if (remaining < 7200000) { // 2 hours
      urgency = 1 - remaining / 7200000;
    }
  }
  const urgencyBoost = urgency * 0.6;

  if (b > 0.2 || isHovered) {
    const glowR = isHovered ? radius * 10 : radius * 7;
    const gr = ctx.createRadialGradient(n.rx, n.ry, 0, n.rx, n.ry, glowR);
    gr.addColorStop(0, `rgba(${r},${g},${bl},${(isHovered ? 0.4 : b * 0.25) * (1 + urgencyBoost)})`);
    gr.addColorStop(1, 'rgba(0,0,0,0)');
    ctx.fillStyle = gr;
    ctx.beginPath();
    ctx.arc(n.rx, n.ry, glowR, 0, Math.PI * 2);
    ctx.fill();
  }

  ctx.fillStyle = `rgba(${r},${g},${bl},${Math.min(1, (isHovered ? 1 : b) * 1.2)})`;
  ctx.beginPath();
  ctx.arc(n.rx, n.ry, radius, 0, Math.PI * 2);
  ctx.fill();

  // label reveals gradually as you approach — distant entities are anonymous dots
  if (isHovered || b > 0.35) {
    const alpha = isHovered ? 0.90 : Math.min(0.78, (b - 0.35) * 2.8);
    const fs = Math.max(8, Math.round((isHovered ? 12 : 8 + b * 4) * Math.min(1.3, proj.ps)));
    ctx.fillStyle = `rgba(${r},${g},${bl},${alpha})`;
    ctx.font = `${fs}px "SF Mono",monospace`;
    ctx.fillText(n.label, n.rx + radius + 4, n.ry + 4);
  }

  // most recent message floating near entity (near zone only)
  if (b > 0.45) {
    const msgs = entityMessages[n.label];
    if (msgs && msgs.length > 0) {
      const msg  = msgs[msgs.length - 1];
      const age  = (Date.now() - new Date(msg.created_at).getTime()) / (2 * 3600 * 1000);
      const msgA = Math.max(0, (1 - age) * 0.55 * (b - 0.4));
      if (msgA > 0.04) {
        const snippet = msg.text.length > 38 ? msg.text.slice(0, 38) + '…' : msg.text;
        ctx.fillStyle = `rgba(140,140,190,${msgA})`;
        ctx.font = `9px "SF Mono",monospace`;
        ctx.fillText('"' + snippet + '"', n.rx + radius + 5, n.ry + 18);
      }
    }
  }

  if (n.expiresAt && n.b > 0.3) {
    const remaining = new Date(n.expiresAt) - Date.now();
    if (remaining > 0 && remaining < 6 * 3600000) {
      const mins = Math.floor(remaining / 60000);
      const timeStr = mins >= 60 ? Math.floor(mins/60) + 'h ' + (mins%60) + 'm' : mins + 'm';
      const urgA = Math.max(0.3, urgency) * n.b;
      ctx.fillStyle = `rgba(255,150,70,${urgA * 0.8})`;
      ctx.font = '9px "SF Mono",monospace';
      ctx.fillText('◈ ' + timeStr, n.rx + radius + 5, n.ry + 30);
    }
  }
}

// birth ripple — expanding ring projected in 3D
function drawBirths() {
  for (let i = births.length - 1; i >= 0; i--) {
    const b = births[i];
    b.t += 0.016;
    const p = b.t / 2.2;
    if (p > 1) { births.splice(i, 1); continue; }
    const proj = project(b.wx, b.wy, b.wz);
    if (!proj) continue;
    const eased = 1 - (1 - p) * (1 - p);
    const radius = eased * 75 * proj.ps;
    const alpha  = (1 - p) * 0.55;
    ctx.strokeStyle = `rgba(${b.r},${b.g},${b.b},${alpha})`;
    ctx.lineWidth = 1.2;
    ctx.setLineDash([]);
    ctx.beginPath(); ctx.arc(proj.sx, proj.sy, radius, 0, Math.PI * 2); ctx.stroke();
  }
}

function drawBursts() {
  for (let i = bursts.length - 1; i >= 0; i--) {
    const b = bursts[i];
    b.t += 0.016;
    const p = b.t / 1.0;
    if (p > 1) { bursts.splice(i, 1); continue; }
    const proj = project(b.wx, b.wy, b.wz);
    if (!proj) continue;
    for (let ring = 0; ring < 3; ring++) {
      const rp = Math.min(1, p + ring * 0.18);
      const radius = rp * 90 * proj.ps;
      const alpha  = (1 - rp) * 0.65 / (ring + 1);
      ctx.strokeStyle = `rgba(${b.r},${b.g},${b.b},${alpha})`;
      ctx.lineWidth = 1.5 - ring * 0.4;
      ctx.setLineDash([]);
      ctx.beginPath(); ctx.arc(proj.sx, proj.sy, radius, 0, Math.PI * 2); ctx.stroke();
    }
  }
}

function drawSelf() {
  // viewer crosshair — fixed at ground level near camera
  const sx = cx + camLookX * 0.18;
  const sy = H * 0.78;
  const pulse = 0.14 * Math.sin(clock * 2.2);
  const r = 8 + pulse * 4;
  const gr = ctx.createRadialGradient(sx, sy, 0, sx, sy, 38);
  gr.addColorStop(0,   'rgba(212,175,55,0.38)');
  gr.addColorStop(0.4, 'rgba(212,175,55,0.06)');
  gr.addColorStop(1,   'rgba(0,0,0,0)');
  ctx.fillStyle = gr;
  ctx.beginPath(); ctx.arc(sx, sy, 38, 0, Math.PI * 2); ctx.fill();
  ctx.strokeStyle = `rgba(212,175,55,${0.42 + pulse * 2.2})`;
  ctx.lineWidth = 1;
  ctx.setLineDash([]);
  ctx.beginPath();
  ctx.moveTo(sx - r, sy); ctx.lineTo(sx + r, sy);
  ctx.moveTo(sx, sy - r); ctx.lineTo(sx, sy + r);
  ctx.stroke();
  ctx.fillStyle = 'rgba(212,175,55,0.9)';
  ctx.beginPath(); ctx.arc(sx, sy, 2, 0, Math.PI * 2); ctx.fill();
}

function drawSky() {
  // faint atmospheric gradient above horizon
  const hy = H * HORIZON_FRAC;
  const gr = ctx.createLinearGradient(0, 0, 0, hy);
  gr.addColorStop(0, 'rgba(4,4,22,0.6)');
  gr.addColorStop(1, 'rgba(0,0,0,0)');
  ctx.fillStyle = gr;
  ctx.fillRect(0, 0, W, hy);
}

function drawGrid() {
  const GS = 90;       // world-space grid spacing
  const EX = 1100;     // x extent
  const EZ = 1200;     // z extent (depth)
  ctx.setLineDash([]);

  // depth lines (recede to horizon)
  for (let wx = -EX; wx <= EX; wx += GS) {
    const near = project(wx, 0, 0);
    const far  = project(wx, 0, EZ);
    if (!near || !far) continue;
    if (near.sy < H * HORIZON_FRAC) continue;
    const fade = 1 - Math.abs(wx) / (EX * 1.1);
    ctx.strokeStyle = `rgba(38,52,130,${fade * 0.28})`;
    ctx.lineWidth = 0.5;
    ctx.beginPath(); ctx.moveTo(near.sx, near.sy); ctx.lineTo(far.sx, far.sy); ctx.stroke();
  }

  // cross lines (horizontal bands receding)
  for (let wz = 0; wz < EZ; wz += GS) {
    const left  = project(-EX, 0, wz);
    const right = project( EX, 0, wz);
    if (!left || !right) continue;
    if (left.sy < H * HORIZON_FRAC - 2) continue;
    const fade = 1 - wz / EZ;
    ctx.strokeStyle = `rgba(38,52,130,${fade * 0.22})`;
    ctx.lineWidth = 0.5;
    ctx.beginPath(); ctx.moveTo(left.sx, left.sy); ctx.lineTo(right.sx, right.sy); ctx.stroke();
  }
}

function drawHorizon() {
  const hy = H * HORIZON_FRAC;
  // glow band at horizon
  const gr = ctx.createLinearGradient(0, hy - 18, 0, hy + 18);
  gr.addColorStop(0,   'rgba(0,0,0,0)');
  gr.addColorStop(0.5, 'rgba(50,70,200,0.09)');
  gr.addColorStop(1,   'rgba(0,0,0,0)');
  ctx.fillStyle = gr;
  ctx.fillRect(0, hy - 18, W, 36);
  // hard horizon line
  ctx.strokeStyle = 'rgba(55,75,190,0.18)';
  ctx.lineWidth = 0.6;
  ctx.setLineDash([]);
  ctx.beginPath(); ctx.moveTo(0, hy); ctx.lineTo(W, hy); ctx.stroke();
}

// --- wanderers ---
const wanderers = {};

function idOffset(id) {
  let h = 0;
  for (let i = 0; i < id.length; i++) h = (((h << 5) + h) + id.charCodeAt(i)) >>> 0;
  const angle = (h % 1000) / 1000 * Math.PI * 2;
  const r = 18 + (h % 500) / 500 * 14;
  return [Math.cos(angle) * r, Math.sin(angle) * r];
}

function updateWanderers(users) {
  const seen = new Set();
  users.forEach(u => {
    seen.add(u.id);
    const node = nodes[u.position];
    if (!node) return;
    const [ox, oy] = idOffset(u.id);
    const tx = (node.rx || node.x) + ox, ty = (node.ry || node.y) + oy;
    if (!wanderers[u.id]) wanderers[u.id] = { x: tx, y: ty, phase: Math.random() * Math.PI * 2, name: u.name || null };
    wanderers[u.id].tx = tx;
    wanderers[u.id].ty = ty;
    wanderers[u.id].name = u.name || null;
  });
  Object.keys(wanderers).forEach(id => { if (!seen.has(id)) delete wanderers[id]; });
}

function drawWanderers() {
  Object.values(wanderers).forEach(w => {
    if (w.tx !== undefined) { w.x = lerp(w.x, w.tx, 0.06); w.y = lerp(w.y, w.ty, 0.06); }
    const pulse = 1 + 0.2 * Math.sin(clock * 1.8 + w.phase);
    const r = 3.5 * pulse;
    const gr = ctx.createRadialGradient(w.x, w.y, 0, w.x, w.y, r * 5);
    gr.addColorStop(0, 'rgba(80,200,200,0.3)');
    gr.addColorStop(1, 'rgba(0,0,0,0)');
    ctx.fillStyle = gr;
    ctx.beginPath();
    ctx.arc(w.x, w.y, r * 5, 0, Math.PI * 2);
    ctx.fill();
    ctx.fillStyle = `rgba(100,220,220,${0.7 + 0.3 * Math.sin(clock * 1.8 + w.phase)})`;
    ctx.beginPath();
    ctx.arc(w.x, w.y, r, 0, Math.PI * 2);
    ctx.fill();
    if (w.name) {
      ctx.fillStyle = `rgba(100,220,220,${0.5 + 0.2 * Math.sin(clock * 1.8 + w.phase)})`;
      ctx.font = '9px "SF Mono",monospace';
      ctx.fillText(w.name, w.x + r * 2 + 4, w.y + 3);
    }
  });
}

// --- entry & interest ---
let interest = sessionStorage.getItem('gp_interest') || '';

// welcome ripple burst on entry
const welcomeRipples = [];
function spawnWelcomeRipples() {
  for (let i = 0; i < 4; i++) {
    welcomeRipples.push({ t: i * 0.18, maxR: Math.min(W, H) * (0.22 + i * 0.14) });
  }
}
function drawWelcomeRipples() {
  const hy = H * HORIZON_FRAC;
  for (let i = welcomeRipples.length - 1; i >= 0; i--) {
    const rip = welcomeRipples[i];
    rip.t += 0.012;
    if (rip.t >= 1) { welcomeRipples.splice(i, 1); continue; }
    const r = rip.maxR * rip.t;
    const a = (1 - rip.t) * 0.18;
    ctx.strokeStyle = `rgba(200,168,64,${a})`;
    ctx.lineWidth = 0.8;
    ctx.setLineDash([]);
    ctx.beginPath(); ctx.arc(cx, hy, r, 0, Math.PI * 2); ctx.stroke();
  }
}

function enterSpace(text) {
  interest = text.trim() || 'curiosity exploration wandering';
  sessionStorage.setItem('gp_interest', interest);
  const nameInput = document.getElementById('entry-name');
  if (nameInput && nameInput.value.trim()) {
    authorName = nameInput.value.trim().slice(0, 24);
    sessionStorage.setItem('gp_author', authorName);
  }
  const el = document.getElementById('entry');
  el.classList.add('hidden');
  setTimeout(() => el.style.display = 'none', 1200);
  // welcome ripples
  spawnWelcomeRipples();
  // hint fades out after 6s
  setTimeout(() => document.getElementById('hint').classList.add('hidden'), 6000);
  startPolling();
}

document.getElementById('entry-enter').addEventListener('click', () => {
  enterSpace(document.getElementById('entry-input').value);
});
document.getElementById('entry-input').addEventListener('keydown', e => {
  if (e.key === 'Enter') enterSpace(e.target.value);
});
document.getElementById('entry-skip').addEventListener('click', () => {
  enterSpace('');
});

// --- data ---
let allEntities = [], pollStarted = false;

async function fetchEntities() {
  try {
    const newEntities = await fetch('/entities').then(r => r.json());
    const newLabels = new Set(newEntities.map(e => e.label));
    prevEntityLabels.forEach(label => {
      if (!newLabels.has(label) && nodes[label] && nodes[label].b > 0.1) {
        const [cr, cg, cb] = KIND_COLOR[nodes[label].kind] || [140, 140, 190];
        bursts.push({ wx: nodes[label].wx, wy: nodes[label].wy, wz: nodes[label].wz, t: 0, r: cr, g: cg, b: cb });
      }
    });
    prevEntityLabels = newLabels;
    allEntities = newEntities;
  } catch {}
}

async function fetchPresence() {
  try {
    const p = await fetch('/presence').then(r => r.json());
    updateWanderers(p.users || []);
  } catch {}
}

async function fetchField() {
  try {
    const enc = encodeURIComponent(interest || 'curiosity exploration wandering');
    const nameParam = authorName ? '&name=' + encodeURIComponent(authorName) : '';
    const f   = await fetch('/field?interest=' + enc + nameParam).then(r => r.json());
    updateNodes(f, allEntities);
    // updateCamera() is called each frame in animate()
    updateMessages(f);
    const pos = f.position || '—';
    document.getElementById('position').textContent = pos.length > 48 ? pos.slice(0, 46) + '…' : pos;
    document.getElementById('presence-count').textContent = f.presence || 0;
    document.getElementById('conn').textContent           = 'live';
    const drift = f.drift && f.drift.length ? '› drifting toward ' + f.drift[0].toward : '';
    document.getElementById('drift').textContent = drift;
  } catch {
    document.getElementById('conn').textContent = 'no signal';
  }
}

function startPolling() {
  if (pollStarted) return;
  pollStarted = true;
  fetchEntities().then(() => {
    fetchField();
    fetchPresence();
    setInterval(fetchField,    4000);
    setInterval(fetchPresence, 4000);
    setInterval(fetchEntities, 30000);
  });
}

// returning visitor: skip entry screen
if (interest) {
  document.getElementById('entry').style.display = 'none';
  setTimeout(() => document.getElementById('hint').classList.add('hidden'), 6000);
  startPolling();
}

animate();
</script>
</body>
</html>"##;

/// GET /privacy-policy
async fn privacy_policy() -> Html<&'static str> {
    Html(PRIVACY_HTML)
}

static PRIVACY_HTML: &str = r##"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<title>Privacy Policy — Golden Protocol</title>
<style>
  :root { --gold: #c8a840; --bg: #0a0a0c; --text: #d0c8b0; --muted: #6a6050; }
  * { box-sizing: border-box; margin: 0; padding: 0; }
  body { background: var(--bg); color: var(--text); font-family: 'Georgia', serif;
         line-height: 1.75; max-width: 720px; margin: 0 auto; padding: 3rem 1.5rem; }
  h1 { color: var(--gold); font-size: 1.6rem; margin-bottom: 0.3rem; }
  h2 { color: var(--gold); font-size: 1.05rem; margin: 2.2rem 0 0.6rem;
       text-transform: uppercase; letter-spacing: 0.08em; }
  p  { margin-bottom: 1rem; }
  a  { color: var(--gold); text-decoration: none; border-bottom: 1px solid var(--muted); }
  a:hover { border-color: var(--gold); }
  table { width: 100%; border-collapse: collapse; margin: 1rem 0 1.5rem; font-size: 0.9rem; }
  th, td { padding: 0.5rem 0.75rem; border: 1px solid #2a2820; text-align: left; }
  th { background: #14120e; color: var(--gold); }
  tr:nth-child(even) { background: #0e0d0a; }
  .meta { color: var(--muted); font-size: 0.85rem; margin-bottom: 2.5rem; }
  .back { display: inline-block; margin-top: 3rem; font-size: 0.9rem;
          color: var(--muted); border-bottom: none; }
  .back:hover { color: var(--gold); }
  hr { border: none; border-top: 1px solid #2a2820; margin: 2rem 0; }
</style>
</head>
<body>
<h1>Privacy Policy</h1>
<p class="meta">Golden Protocol Extension &nbsp;·&nbsp; Last updated: 2026-03-17</p>

<h2>Overview</h2>
<p>Golden Protocol is a browser extension that gives you a presence in a shared semantic space.
This policy explains what data the extension handles, how it is used, and what is never collected.</p>

<h2>What is stored on your device</h2>
<table>
  <tr><th>Data</th><th>Storage</th><th>Purpose</th></tr>
  <tr><td>Random UUID (userId)</td><td>chrome.storage.local</td><td>Identifies your presence across sessions</td></tr>
  <tr><td>Current page text excerpt (≤600 chars)</td><td>chrome.storage.session</td><td>Temporary; used only to compute your interest vector</td></tr>
</table>

<h2>What is sent to the server</h2>
<table>
  <tr><th>Data sent</th><th>What is NOT sent</th></tr>
  <tr><td>384-dimensional semantic vector (your interest embedding)</td><td>Raw page text or URLs</td></tr>
  <tr><td>Your random UUID</td><td>Any personally identifiable information</td></tr>
</table>
<p>The server receives a compressed semantic representation of what you are reading — not the actual content or address.
The vector cannot be reversed into readable text.</p>

<h2>What is never collected</h2>
<p>No browsing history is stored or transmitted. No personally identifiable information is collected.
No data is sold or shared with third parties. No analytics, crash reporting, or tracking SDKs are included.</p>

<h2>Server-side data</h2>
<p>The server stores your UUID and interest vector, a short encounter history (last 200 visits as position labels and timestamps — not URLs),
and any messages you voluntarily leave in the field (auto-deleted after 2 hours).
This data is not linked to any real-world identity. Your server-side data expires naturally within 30 days of inactivity.</p>

<h2>Permissions</h2>
<table>
  <tr><th>Permission</th><th>Why it is needed</th></tr>
  <tr><td>storage</td><td>Store your UUID locally so your identity persists across sessions</td></tr>
  <tr><td>activeTab</td><td>Read the current tab's text to compute your interest vector</td></tr>
  <tr><td>tabs</td><td>Detect navigation in single-page apps so the field updates as you browse</td></tr>
  <tr><td>host: space.gold3112.online</td><td>Send interest vectors to and receive field state from the Golden Protocol server</td></tr>
</table>

<h2>Data deletion</h2>
<p>Uninstalling the extension removes your UUID from chrome.storage.local.
Your server-side interest vector and encounter history expire automatically within 30 days.
To request immediate server-side deletion, open an issue at
<a href="https://github.com/gold3112/golden-protocol/issues">github.com/gold3112/golden-protocol/issues</a>.</p>

<hr>
<p style="color:var(--muted);font-size:0.85rem;">
Questions or concerns: <a href="https://github.com/gold3112/golden-protocol/issues">open an issue on GitHub</a>
</p>
<a class="back" href="/">← back to the field</a>
</body>
</html>"##;

/// GET /field
async fn get_field(
    State(state): State<Arc<AppState>>,
    Query(q): Query<FieldQuery>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let field = compute_field_state(&state, &q);
    if q.format.as_deref() == Some("text") {
        (
            [("Content-Type", "text/plain; charset=utf-8")],
            render_field_text(&field),
        ).into_response()
    } else {
        Json(field).into_response()
    }
}

fn render_field_text(field: &FieldState) -> String {
    let w = "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━";
    let d = "  ─────────────────────────────────────────";
    let mut o = String::new();

    o.push_str(&format!("{}\n  GOLDEN PROTOCOL  ·  field state\n{}\n\n", w, w));
    o.push_str(&format!("  position  ·  {}\n", field.position));
    o.push_str(&format!("  presence  ·  {}\n", field.presence));
    o.push_str(&format!("  density   ·  {:.2}\n", field.density));
    o.push_str(&format!("  time      ·  {}\n\n", field.timestamp.format("%Y-%m-%dT%H:%M:%SZ")));

    if !field.near.is_empty() {
        o.push_str("  NEAR\n");
        o.push_str(&format!("{}\n", d));
        for e in &field.near {
            let lbl = if e.label.chars().count() > 32 { format!("{}…", e.label.chars().take(31).collect::<String>()) } else { e.label.clone() };
            if let Some(c) = e.components {
                o.push_str(&format!("  ● {:<34} {:.2}  s:{:.2} r:{:.2} a:{:.2} t:{:.2} u:{:.2}\n",
                    lbl, e.distance, c[0], c[1], c[2], c[3], c[4]));
            } else {
                o.push_str(&format!("  ● {:<34} {:.2}\n", lbl, e.distance));
            }
        }
        o.push('\n');
    }

    if !field.horizon.is_empty() {
        o.push_str("  HORIZON\n");
        o.push_str(&format!("{}\n", d));
        for e in field.horizon.iter().take(6) {
            let lbl = if e.label.chars().count() > 32 { format!("{}…", e.label.chars().take(31).collect::<String>()) } else { e.label.clone() };
            o.push_str(&format!("  · {:<34} {:.2}\n", lbl, e.distance));
        }
        if field.horizon.len() > 6 {
            o.push_str(&format!("  … and {} more beyond\n", field.horizon.len() - 6));
        }
        o.push('\n');
    }

    if !field.drift.is_empty() {
        o.push_str(&format!("  drift  →  {} ({:.2})\n\n", field.drift[0].toward, field.drift[0].strength));
    }

    if !field.messages.is_empty() {
        o.push_str("  VOICES\n");
        o.push_str(&format!("{}\n", d));
        for m in &field.messages {
            let txt = if m.text.len() > 52 { format!("{}…", &m.text[..51]) } else { m.text.clone() };
            o.push_str(&format!("  {}  ·  \"{}\"\n", m.author, txt));
        }
        o.push('\n');
    }

    o.push_str(&format!("{}\n", w));
    o.push_str("  s=semantic r=relational a=activity t=temporal u=attention\n");
    o.push_str("  curl \"https://space.gold3112.online/field?interest=YOUR_THOUGHT&format=text\"\n");
    o.push_str(&format!("{}\n", w));
    o
}

/// GET /field/stream — SSE: field stateをリアルタイムにプッシュ
/// 接続中はこのユーザーが他ユーザーの near/horizon に現れる
async fn stream_field(
    State(state): State<Arc<AppState>>,
    Query(q): Query<FieldQuery>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let user_id = q.user_id.unwrap_or_else(Uuid::new_v4);

    // 接続時: ユーザーを登録
    let interest_vec = q.user_id
        .and_then(|uid| Identity::load(&uid).ok())
        .map(|i| i.interest_vec.clone())
        .unwrap_or_else(|| fallback_interest(&q.interest));

    {
        let mut users = state.connected_users.lock().unwrap();
        users.insert(user_id, UserPresence {
            id:           user_id,
            name:         q.name.clone().unwrap_or_default(),
            interest_vec: interest_vec.clone(),
            position:     "plaza".to_string(),
            last_seen:    Utc::now(),
        });
        tracing::info!("user {} connected ({} total)", user_id, users.len());
    }

    let state_c = Arc::clone(&state);
    let q_c     = q.clone();

    let stream = IntervalStream::new(tokio::time::interval(Duration::from_secs(2)))
        .map(move |_| {
            // last_seen を更新 (これが止まると cleanup タスクが除去する)
            {
                let mut users = state_c.connected_users.lock().unwrap();
                if let Some(u) = users.get_mut(&user_id) {
                    u.last_seen = Utc::now();
                }
            }

            let field = compute_field_state(&state_c, &q_c);

            // position を connected_users に反映 (他ユーザーから見える位置を更新)
            {
                let mut users = state_c.connected_users.lock().unwrap();
                if let Some(u) = users.get_mut(&user_id) {
                    u.position = field.position.clone();
                }
            }

            let json = serde_json::to_string(&field).unwrap_or_default();
            Ok::<Event, Infallible>(Event::default().event("field").data(json))
        });

    Sse::new(stream).keep_alive(KeepAlive::default())
}

/// GET /identity/new
async fn new_identity(
    Query(q): Query<HashMap<String, String>>,
) -> Json<IdentitySummary> {
    let seed_text = q.get("interest").map(|s| s.as_str())
        .unwrap_or("curiosity exploration knowledge");
    let seed_vec = embedding::embed(seed_text).unwrap_or_else(|_| vec![0.0; 384]);
    let identity = Identity::new(seed_vec);
    identity.save().expect("save failed");
    Json(to_summary(&identity))
}

/// GET /identity/:id
async fn get_identity(Path(id): Path<Uuid>) -> Result<Json<IdentitySummary>, String> {
    Identity::load(&id)
        .map(|i| Json(to_summary(&i)))
        .map_err(|_| format!("identity {} not found", id))
}

/// GET /entities — 現在の空間にいるエンティティ一覧
async fn get_entities(
    State(state): State<Arc<AppState>>,
) -> Json<Vec<EntitySummary>> {
    let graph = state.graph.lock().unwrap();
    let list = graph.graph.node_indices()
        .map(|idx| {
            let e = &graph.graph[idx];
            EntitySummary {
                id:          e.id,
                label:       e.label.clone(),
                kind:        format!("{:?}", e.kind),
                last_active: e.last_active.to_rfc3339(),
                expires_at:  e.expires_at.map(|t| t.to_rfc3339()),
            }
        })
        .collect();
    Json(list)
}

/// POST /entity — エンティティを動的に追加
async fn post_entity(
    State(state): State<Arc<AppState>>,
    Json(req): Json<EntityRequest>,
) -> Result<Json<EntitySummary>, String> {
    if req.label.chars().count() > 120 {
        return Err("label too long (max 120 chars)".to_string());
    }
    if req.text.as_deref().unwrap_or("").len() > 4000 {
        return Err("text too long (max 4000 bytes)".to_string());
    }
    let text = req.text.as_deref().unwrap_or(&req.label);
    let emb  = embedding::embed(text).map_err(|e| e.to_string())?;
    let kind = req.kind.unwrap_or(EntityKind::Data);

    let mut entity      = Entity::new(kind, &req.label);
    entity.activity_vec = Some(emb.clone());
    entity.embedding    = Some(emb);
    if let Some(h) = req.duration_hours {
        entity.expires_at = Some(Utc::now() + chrono::Duration::seconds((h * 3600.0) as i64));
    }
    let summary = EntitySummary {
        id:          entity.id,
        label:       entity.label.clone(),
        kind:        format!("{:?}", entity.kind),
        last_active: entity.last_active.to_rfc3339(),
        expires_at:  entity.expires_at.map(|t| t.to_rfc3339()),
    };

    let _ = identity::save_entity(&entity);
    state.graph.lock().unwrap().add_entity(entity);
    tracing::info!("entity added: {}", req.label);
    Ok(Json(summary))
}

/// DELETE /entity/:id — エンティティを削除
async fn delete_entity(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, String> {
    let removed = state.graph.lock().unwrap().remove_entity(&id);
    if removed {
        let _ = identity::delete_entity_db(&id);
        tracing::info!("entity removed: {}", id);
        Ok(Json(serde_json::json!({ "removed": id })))
    } else {
        Err(format!("entity {} not found", id))
    }
}

/// GET /presence — 現在の接続ユーザー一覧
async fn get_presence(
    State(state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    let users = state.connected_users.lock().unwrap();
    let list: Vec<serde_json::Value> = users.values().map(|u| {
        serde_json::json!({
            "id":        u.id,
            "name":      u.name,
            "position":  u.position,
            "last_seen": u.last_seen.to_rfc3339(),
        })
    }).collect();
    Json(serde_json::json!({ "count": list.len(), "users": list }))
}

/// POST /encounter — 明示的encounter記録 + グローバル活動更新
async fn post_encounter(
    State(state): State<Arc<AppState>>,
    Json(req): Json<EncounterRequest>,
) -> Result<Json<IdentitySummary>, String> {
    if req.near_labels.len() > 50 {
        return Err("too many near_labels (max 50)".to_string());
    }
    if req.interest_text.len() > 1000 {
        return Err("interest_text too long (max 1000 bytes)".to_string());
    }
    let mut identity = Identity::load(&req.user_id)
        .map_err(|_| format!("identity {} not found", req.user_id))?;

    // near エンティティの embedding を収集 + ID を記録
    let (near_embeddings, near_ids): (Vec<Vec<f32>>, Vec<uuid::Uuid>) = {
        let graph = state.graph.lock().unwrap();
        graph.graph.node_indices()
            .filter_map(|idx| {
                let e = &graph.graph[idx];
                if req.near_labels.contains(&e.label) {
                    e.embedding.clone().map(|emb| (emb, e.id))
                } else {
                    None
                }
            })
            .unzip()
    };

    // ユーザーの関心ベクトルを取得 (encounter後のblend前)
    let user_interest = identity.interest_vec.clone();

    // near エンティティの activity_vec をユーザーの関心で更新して DB に保存
    {
        let mut graph = state.graph.lock().unwrap();
        for id in &near_ids {
            graph.update_entity_activity(id, &user_interest, 0.90);
            if let Some(&idx) = graph.index_map.get(id) {
                let _ = identity::save_entity(&graph.graph[idx]);
            }
        }
    }

    // グローバル活動カウンタを更新 → drift に反映
    {
        let mut activity = state.activity.lock().unwrap();
        for label in &req.near_labels {
            *activity.entry(label.clone()).or_insert(0) += 1;
        }
    }

    identity.encounter(
        &req.position,
        req.near_labels,
        near_embeddings,
        req.interest_text,
        0.85,
    );
    identity.save().map_err(|e| e.to_string())?;
    Ok(Json(to_summary(&identity)))
}

// --- メッセージ ---

#[derive(Deserialize)]
struct MessageRequest {
    entity_label: String,
    text:         String,
    author:       Option<String>,
}

/// POST /message — entity に紐づくメッセージを投稿
async fn post_message(
    Json(req): Json<MessageRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let text = req.text.trim().to_string();
    if text.is_empty() || text.len() > 280 {
        return Err(StatusCode::BAD_REQUEST);
    }
    let author = req.author.as_deref().unwrap_or("wanderer");
    let author = author.trim().chars().take(24).collect::<String>();
    let author = if author.is_empty() { "wanderer".to_string() } else { author };

    identity::save_message(&req.entity_label, &text, &author)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

// --- コネクタ ---

#[derive(Deserialize)]
struct RssConnectRequest {
    url:       String,
    max_items: Option<usize>,
}

#[derive(Deserialize)]
struct UrlConnectRequest {
    url:   String,
    label: Option<String>,
}

#[derive(Serialize)]
struct ConnectResult {
    added:  usize,
    labels: Vec<String>,
}

/// タイムアウト付き HTTP クライアント (外部フェッチ共通)
fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap_or_default()
}

/// URL が外部公開サービスを指しているか検証する (SSRF 対策)
fn validate_external_url(url: &str) -> Result<(), String> {
    let parsed = url.parse::<reqwest::Url>()
        .map_err(|_| "invalid URL".to_string())?;

    // スキームは http / https のみ許可
    match parsed.scheme() {
        "http" | "https" => {}
        s => return Err(format!("scheme '{}' not allowed", s)),
    }

    let host = parsed.host_str().unwrap_or("").to_lowercase();

    // localhost 系
    if host == "localhost" || host == "ip6-localhost" {
        return Err("internal host not allowed".to_string());
    }

    // IP アドレスの場合はプライベート範囲をチェック
    if let Ok(ip) = host.parse::<IpAddr>() {
        let blocked = match ip {
            IpAddr::V4(v4) => {
                v4.is_loopback()        // 127.0.0.0/8
                || v4.is_private()      // 10/8, 172.16/12, 192.168/16
                || v4.is_link_local()   // 169.254/16 (AWSメタデータ等)
                || v4.is_broadcast()
                || v4.is_unspecified()
            }
            IpAddr::V6(v6) => {
                v6.is_loopback() || v6.is_unspecified()
            }
        };
        if blocked {
            return Err("private/internal IP not allowed".to_string());
        }
    }

    Ok(())
}

/// POST /connect/rss — RSS/Atom フィードを Stream エンティティとして取り込む
async fn connect_rss(
    State(state): State<Arc<AppState>>,
    Json(req): Json<RssConnectRequest>,
) -> Result<Json<ConnectResult>, String> {
    validate_external_url(&req.url)?;
    let max   = req.max_items.unwrap_or(20);
    let added = ingest_rss_feed(&req.url, max, &state).await;
    let _ = identity::save_feed(&req.url, max);
    tracing::info!("rss connect: added {} entities from {}", added, req.url);
    Ok(Json(ConnectResult { added, labels: vec![] }))
}

/// POST /connect/url — URL のテキストを抽出して Data エンティティとして追加
async fn connect_url(
    State(state): State<Arc<AppState>>,
    Json(req): Json<UrlConnectRequest>,
) -> Result<Json<ConnectResult>, String> {
    validate_external_url(&req.url)?;
    let html = http_client().get(&req.url).send().await
        .map_err(|e| e.to_string())?
        .text().await
        .map_err(|e| e.to_string())?;

    let doc = scraper::Html::parse_document(&html);

    // title タグ
    let title = scraper::Selector::parse("title").unwrap();
    let page_title = doc.select(&title)
        .next()
        .map(|e| e.text().collect::<String>())
        .unwrap_or_else(|| req.url.clone());
    let page_title = page_title.trim().chars().take(80).collect::<String>();

    // body テキスト抽出 (script/style を除く)
    let body_sel  = scraper::Selector::parse("body").unwrap();
    let skip_sel  = scraper::Selector::parse("script, style, nav, footer").unwrap();
    let body_text = doc.select(&body_sel)
        .next()
        .map(|body| {
            body.descendants()
                .filter(|n| n.value().is_text())
                .filter(|n| {
                    // script/style の子テキストを除外
                    !n.ancestors().any(|a| {
                        a.value().as_element()
                            .map(|_| skip_sel.matches(&scraper::ElementRef::wrap(a).unwrap()))
                            .unwrap_or(false)
                    })
                })
                .map(|n| n.value().as_text().unwrap().trim().to_string())
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();
    let body_text = body_text.trim().chars().take(800).collect::<String>();
    let seed_text = format!("{} {}", page_title, body_text);

    let label = req.label.unwrap_or(page_title.clone());

    let emb = embedding::embed(&seed_text).map_err(|e| e.to_string())?;
    let mut entity      = Entity::new(EntityKind::Data, &label);
    entity.activity_vec = Some(emb.clone());
    entity.embedding    = Some(emb);
    let _ = identity::save_entity(&entity);
    state.graph.lock().unwrap().add_entity(entity);

    tracing::info!("url connect: added '{}' from {}", label, req.url);
    Ok(Json(ConnectResult { added: 1, labels: vec![label] }))
}

/// RSS フィードを取り込む共通ロジック (handler + 自動取り込みタスクで共用)
async fn ingest_rss_feed(url: &str, max: usize, state: &Arc<AppState>) -> usize {
    let bytes = match async {
        let r = http_client().get(url).send().await?;
        r.bytes().await
    }.await {
        Ok(b) => b,
        Err(e) => { tracing::warn!("rss fetch failed {}: {}", url, e); return 0; }
    };
    let channel = match rss::Channel::read_from(&bytes[..]) {
        Ok(c) => c,
        Err(e) => { tracing::warn!("rss parse failed {}: {}", url, e); return 0; }
    };
    let mut added = 0;
    for item in channel.items().iter().take(max) {
        let title = item.title().unwrap_or("untitled").to_string();
        let desc  = item.description().unwrap_or("").to_string();
        let text  = scraper::Html::parse_fragment(&format!("{} {}", title, desc))
            .root_element().text().collect::<String>();
        let text  = text.trim().chars().take(500).collect::<String>();
        let label = title.chars().take(60).collect::<String>();
        {
            let graph = state.graph.lock().unwrap();
            if graph.graph.node_indices().any(|i| graph.graph[i].label == label) { continue; }
        }
        if let Ok(emb) = embedding::embed(&text) {
            let mut entity      = Entity::new(EntityKind::Stream, &label);
            entity.activity_vec = Some(emb.clone());
            entity.embedding    = Some(emb);
            let _ = identity::save_entity(&entity);
            state.graph.lock().unwrap().add_entity(entity);
            added += 1;
        }
    }
    added
}

/// GET /feeds — 登録済み RSS フィード一覧
async fn get_feeds() -> Json<serde_json::Value> {
    let feeds = identity::load_feeds();
    let list: Vec<serde_json::Value> = feeds.into_iter().map(|(url, max)| {
        serde_json::json!({ "url": url, "max_items": max })
    }).collect();
    Json(serde_json::json!({ "count": list.len(), "feeds": list }))
}

// --- Federation ---

#[derive(Serialize, Deserialize)]
struct PeerAnnounce {
    node_id: Uuid,
    url:     String,
}

/// GET /peers — 既知ピア一覧
async fn get_peers(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let peers = state.peers.lock().unwrap();
    let list: Vec<serde_json::Value> = peers.iter().map(|(url, id)| {
        serde_json::json!({ "url": url, "node_id": id })
    }).collect();
    Json(serde_json::json!({
        "node_id": state.node_id,
        "peers":   list,
    }))
}

/// POST /peer/register — 他ノードが自分を登録してくる
async fn register_peer(
    State(state): State<Arc<AppState>>,
    Json(req): Json<PeerAnnounce>,
) -> Json<serde_json::Value> {
    let url = req.url.trim_end_matches('/').to_string();
    if validate_external_url(&url).is_err() {
        return Json(serde_json::json!({ "error": "invalid peer url" }));
    }
    state.peers.lock().unwrap().insert(url.clone(), req.node_id);
    let _ = identity::save_peer(&url, req.node_id);
    tracing::info!("peer registered: {} ({})", url, req.node_id);
    // 自分のピアリストを返す (相手も知らないノードを知れる)
    let peers = state.peers.lock().unwrap();
    let list: Vec<serde_json::Value> = peers.iter()
        .filter(|(u, _)| **u != url)
        .map(|(u, id)| serde_json::json!({ "url": u, "node_id": id }))
        .collect();
    Json(serde_json::json!({
        "node_id": state.node_id,
        "peers":   list,
    }))
}

/// GET /entities/export — 全エンティティをembedding込みでエクスポート (ピア同期用)
async fn export_entities(State(state): State<Arc<AppState>>) -> Json<Vec<Entity>> {
    let graph = state.graph.lock().unwrap();
    let entities: Vec<Entity> = graph.graph.node_indices()
        .map(|idx| graph.graph[idx].clone())
        .filter(|e| !e.label.ends_with("·convergence")) // 発生エンティティは共有しない
        .filter(|e| !e.label.starts_with("wanderer_"))  // 他ノードのユーザーも共有しない
        .collect();
    Json(entities)
}

// --- ユーティリティ ---

fn cosine_sim(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() { return 0.0; }
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let na: f32  = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32  = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na > 0.0 && nb > 0.0 { dot / (na * nb) } else { 0.0 }
}

fn fallback_interest(interest: &Option<String>) -> Vec<f32> {
    let text = interest.as_deref().unwrap_or("curiosity exploration knowledge");
    embedding::embed(text).unwrap_or_else(|_| vec![0.0; 384])
}

fn to_summary(i: &Identity) -> IdentitySummary {
    IdentitySummary {
        id:               i.id,
        created_at:       i.created_at.to_rfc3339(),
        last_seen:        i.last_seen.to_rfc3339(),
        position:         i.position.clone(),
        encounter_count:  i.encounter_count(),
        interest_preview: i.interest_vec.iter().take(8).cloned().collect(),
    }
}

// --- main ---

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    identity::init_db().expect("failed to init identity DB");
    println!("initializing embedding model...");
    embedding::init_model().expect("failed to init embedding model");
    println!("embedding model ready.");

    let mut space = SpaceGraph::new();

    // DB からエンティティをロード。なければデフォルトを生成して保存
    let saved = identity::load_all_entities();
    if saved.is_empty() {
        let defaults = vec![
            ("wandering_ai",        EntityKind::AI),
            ("knowledge_stream",    EntityKind::Stream),
            ("gathering",           EntityKind::Event),
            ("deep_archive",        EntityKind::Data),
            ("distant_signal",      EntityKind::Stream),
            ("philosophy_debate",   EntityKind::Event),
            ("music_history",       EntityKind::Data),
            ("live_coding_session", EntityKind::Event),
        ];
        let texts: Vec<&str> = defaults.iter().map(|(l, _)| *l).collect();
        let embeddings = embedding::embed_batch(texts).expect("embed failed");
        for ((label, kind), emb) in defaults.into_iter().zip(embeddings) {
            let mut entity = Entity::new(kind, label);
            entity.activity_vec = Some(emb.clone());
            entity.embedding    = Some(emb);
            let _ = identity::save_entity(&entity);
            space.add_entity(entity);
        }
        tracing::info!("initialized default entities");
    } else {
        for entity in saved {
            space.add_entity(entity);
        }
        tracing::info!("loaded {} entities from DB", space.graph.node_count());
    }

    // 60 req/分、バースト最大 10
    let quota = Quota::per_minute(NonZeroU32::new(60).unwrap())
        .allow_burst(NonZeroU32::new(10).unwrap());

    let node_id      = identity::get_or_create_node_id();
    let saved_peers  = identity::load_peers_from_db()
        .into_iter().collect::<HashMap<String, Uuid>>();
    tracing::info!("node_id: {} ({} known peers)", node_id, saved_peers.len());

    let state = Arc::new(AppState {
        graph:           Mutex::new(space),
        weights:         DistanceWeights::default(),
        activity:        Mutex::new(HashMap::new()),
        connected_users: Mutex::new(HashMap::new()),
        rate_limiter:    IpLimiter::keyed(quota),
        node_id,
        peers:           Mutex::new(saved_peers),
    });

    // 切断ユーザーの cleanup タスク (15秒以上 last_seen が更新されなければ除去)
    {
        let state_c = Arc::clone(&state);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(30));
            loop {
                interval.tick().await;
                let cutoff = Utc::now() - chrono::Duration::seconds(15);
                let mut users = state_c.connected_users.lock().unwrap();
                let before = users.len();
                users.retain(|_, u| u.last_seen > cutoff);
                let removed = before - users.len();
                if removed > 0 {
                    tracing::info!("presence cleanup: removed {} stale users ({} remain)", removed, users.len());
                }
            }
        });
    }

    // エンティティ自然減衰タスク (10分ごと)
    // activity_vec を embedding（素の意味）に向かって静かに戻す
    // alpha=0.995: 約230回 (38時間) で半減
    {
        let state_c = Arc::clone(&state);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(600));
            loop {
                interval.tick().await;
                let mut graph = state_c.graph.lock().unwrap();
                let mut decayed = 0usize;
                for idx in graph.graph.node_indices() {
                    let entity = &mut graph.graph[idx];
                    // embedding が基準 — activity が離れているほど引き戻す力が強い
                    if let (Some(av), Some(emb)) = (entity.activity_vec.as_mut(), entity.embedding.as_ref()) {
                        if av.len() == emb.len() {
                            for (a, e) in av.iter_mut().zip(emb.iter()) {
                                *a = 0.995 * *a + 0.005 * e;
                            }
                            decayed += 1;
                        }
                    }
                }
                // 変化したエンティティをまとめて DB 保存
                let to_save: Vec<_> = graph.graph.node_indices()
                    .map(|i| graph.graph[i].clone())
                    .collect();
                drop(graph);
                for entity in &to_save {
                    let _ = identity::save_entity(entity);
                }
                if decayed > 0 {
                    tracing::info!("decay tick: {} entities drifted back toward embedding", decayed);
                }
            }
        });
    }

    // ユーザー収束からのエンティティ自然発生タスク (5分ごと)
    // 複数ユーザーの関心ベクトルが近い = 空間に新しい「場」が生まれる
    {
        let state_c = Arc::clone(&state);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(300));
            loop {
                interval.tick().await;

                // 接続中ユーザーの interest_vec を取得 (fallback vec は除外)
                let fallback = embedding::embed("curiosity exploration knowledge")
                    .unwrap_or_else(|_| vec![0.0; 384]);
                let users: Vec<(Uuid, Vec<f32>)> = {
                    let users = state_c.connected_users.lock().unwrap();
                    users.values()
                        .filter(|u| {
                            // fallbackと違うベクトルを持つユーザーのみ (拡張機能で閲覧済み)
                            let sim = cosine_sim(&u.interest_vec, &fallback);
                            sim < 0.98
                        })
                        .map(|u| (u.id, u.interest_vec.clone()))
                        .collect()
                };
                if users.len() < 2 { continue; }

                // 貪欲クラスタリング: sim > 0.75 のペアをまとめる
                let mut visited = vec![false; users.len()];
                let mut clusters: Vec<Vec<usize>> = vec![];
                for i in 0..users.len() {
                    if visited[i] { continue; }
                    let mut cluster = vec![i];
                    visited[i] = true;
                    for j in (i+1)..users.len() {
                        if !visited[j] && cosine_sim(&users[i].1, &users[j].1) > 0.75 {
                            cluster.push(j);
                            visited[j] = true;
                        }
                    }
                    if cluster.len() >= 2 { clusters.push(cluster); }
                }
                if clusters.is_empty() { continue; }

                for cluster in &clusters {
                    // 重心ベクトルを計算
                    let dim = users[0].1.len();
                    let mut centroid = vec![0.0f32; dim];
                    for &i in cluster {
                        for (c, v) in centroid.iter_mut().zip(users[i].1.iter()) {
                            *c += v;
                        }
                    }
                    centroid.iter_mut().for_each(|c| *c /= cluster.len() as f32);

                    // 既存エンティティとの最小距離 + 最近傍ラベルを取得
                    let (min_dist, nearest_label, emergence_count) = {
                        let graph = state_c.graph.lock().unwrap();
                        let mut min_d = f32::MAX;
                        let mut label = String::from("field");
                        let mut emg_count = 0usize;
                        for idx in graph.graph.node_indices() {
                            let e = &graph.graph[idx];
                            if e.label.ends_with("·convergence") { emg_count += 1; }
                            if let Some(emb) = &e.embedding {
                                let d = 1.0 - cosine_sim(&centroid, emb);
                                if d < min_d { min_d = d; label = e.label.clone(); }
                            }
                        }
                        (min_d, label, emg_count)
                    };

                    // 既存エンティティから十分離れていて、発生上限未満なら生成
                    if min_dist < 0.25 { continue; }
                    if emergence_count >= 5 { continue; }

                    let new_label = format!("{}·convergence", nearest_label.chars().take(40).collect::<String>());
                    // 重複チェック
                    {
                        let graph = state_c.graph.lock().unwrap();
                        if graph.graph.node_indices().any(|i| graph.graph[i].label == new_label) { continue; }
                    }

                    let mut entity      = Entity::new(EntityKind::Event, &new_label);
                    entity.embedding    = Some(centroid.clone());
                    entity.activity_vec = Some(centroid);
                    let _ = identity::save_entity(&entity);
                    state_c.graph.lock().unwrap().add_entity(entity);
                    tracing::info!(
                        "emergence: '{}' spawned from {} converging users",
                        new_label, cluster.len()
                    );
                }
            }
        });
    }

    // Federation gossipタスク
    // 10分ごとに全ピアからエンティティを同期
    // GOLDEN_NODE_URL が設定されていれば bootstrap node に自己登録
    {
        let state_c   = Arc::clone(&state);
        let node_id_c = state.node_id;
        tokio::spawn(async move {
            let node_url  = std::env::var("GOLDEN_NODE_URL").unwrap_or_default();
            let bootstrap = std::env::var("GOLDEN_BOOTSTRAP")
                .unwrap_or_else(|_| "https://space.gold3112.online".to_string());

            // 自分がbootstrapでない場合、bootstrapに自己登録
            if !node_url.is_empty() && !node_url.contains("space.gold3112.online") {
                let body = serde_json::json!({ "node_id": node_id_c, "url": node_url });
                if let Ok(resp) = http_client()
                    .post(format!("{}/peer/register", bootstrap))
                    .json(&body)
                    .send().await
                {
                    if let Ok(data) = resp.json::<serde_json::Value>().await {
                        // bootstrapが返すピアリストも取り込む
                        if let Some(peers) = data["peers"].as_array() {
                            for p in peers {
                                if let (Some(url), Some(id)) = (
                                    p["url"].as_str(),
                                    p["node_id"].as_str().and_then(|s| s.parse::<Uuid>().ok())
                                ) {
                                    state_c.peers.lock().unwrap().insert(url.to_string(), id);
                                    let _ = identity::save_peer(url, id);
                                }
                            }
                        }
                    }
                    tracing::info!("registered with bootstrap: {}", bootstrap);
                }
            }

            // bootstrapをピアとして追加 (自分でない場合)
            if !bootstrap.contains(node_url.trim_end_matches('/')) {
                if let Ok(resp) = http_client().get(format!("{}/peers", bootstrap)).send().await {
                    if let Ok(data) = resp.json::<serde_json::Value>().await {
                        if let Some(id) = data["node_id"].as_str().and_then(|s| s.parse::<Uuid>().ok()) {
                            state_c.peers.lock().unwrap().insert(bootstrap.clone(), id);
                            let _ = identity::save_peer(&bootstrap, id);
                        }
                    }
                }
            }

            // 10分ごとにgossip
            let mut interval = tokio::time::interval(Duration::from_secs(600));
            loop {
                interval.tick().await;
                let peers: Vec<String> = state_c.peers.lock().unwrap().keys().cloned().collect();
                for peer_url in peers {
                    // ピアのエンティティをfetch
                    let entities: Vec<Entity> = match http_client()
                        .get(format!("{}/entities/export", peer_url))
                        .send().await
                    {
                        Ok(r) => r.json().await.unwrap_or_default(),
                        Err(_) => continue,
                    };

                    let mut imported = 0usize;
                    for remote in entities {
                        let mut graph = state_c.graph.lock().unwrap();
                        // ラベルで検索: 存在しなければ追加、あればlast_activeが新しい方を採用
                        let existing = graph.graph.node_indices()
                            .find(|&i| graph.graph[i].label == remote.label);
                        match existing {
                            None => {
                                let _ = identity::save_entity(&remote);
                                graph.add_entity(remote);
                                imported += 1;
                            }
                            Some(idx) => {
                                if remote.last_active > graph.graph[idx].last_active {
                                    graph.graph[idx].activity_vec = remote.activity_vec;
                                    graph.graph[idx].last_active  = remote.last_active;
                                    let _ = identity::save_entity(&graph.graph[idx]);
                                }
                            }
                        }
                    }
                    if imported > 0 {
                        tracing::info!("gossip: +{} entities from {}", imported, peer_url);
                    }

                    // ピアのピアリストも取り込む (ネットワーク拡大)
                    if let Ok(resp) = http_client().get(format!("{}/peers", peer_url)).send().await {
                        if let Ok(data) = resp.json::<serde_json::Value>().await {
                            if let Some(list) = data["peers"].as_array() {
                                for p in list {
                                    if let (Some(url), Some(id)) = (
                                        p["url"].as_str(),
                                        p["node_id"].as_str().and_then(|s| s.parse::<Uuid>().ok())
                                    ) {
                                        let mut peers = state_c.peers.lock().unwrap();
                                        if !peers.contains_key(url) {
                                            peers.insert(url.to_string(), id);
                                            let _ = identity::save_peer(url, id);
                                            tracing::info!("discovered new peer via gossip: {}", url);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                // 24時間応答のないピアを削除
                let _ = identity::prune_stale_peers(24);
            }
        });
    }

    // RSS 自動取り込みタスク
    // 起動直後: デフォルトフィード登録 + 即時取り込み
    // 以降:     1時間ごとに全登録フィードを再取り込み
    {
        let state_c = Arc::clone(&state);
        tokio::spawn(async move {
            // デフォルトフィード (初回のみ登録)
            let default_feeds: &[(&str, usize)] = &[
                ("https://hnrss.org/frontpage",              10), // Hacker News
                ("https://feeds.bbci.co.uk/news/rss.xml",    8),  // BBC World
                ("https://rss.arxiv.org/rss/cs.AI",          8),  // arxiv AI
            ];
            for (url, max) in default_feeds {
                if identity::load_feeds().iter().any(|(u, _)| u == *url) { continue; }
                let _ = identity::save_feed(url, *max);
                let added = ingest_rss_feed(url, *max, &state_c).await;
                tracing::info!("default feed seeded: +{} from {}", added, url);
            }

            // 1時間ごとに再取り込み + 古い記事をローテーション
            let mut interval = tokio::time::interval(Duration::from_secs(3600));
            loop {
                interval.tick().await;

                // 古い未訪問 Stream エンティティを除去
                // 条件: kind=Stream かつ last_active が2時間以上前 かつ activity≈embedding (誰も来ていない)
                let stale_ids: Vec<Uuid> = {
                    let graph = state_c.graph.lock().unwrap();
                    let cutoff = Utc::now() - chrono::Duration::hours(2);
                    graph.graph.node_indices().filter_map(|idx| {
                        let e = &graph.graph[idx];
                        if e.kind != EntityKind::Stream { return None; }
                        if e.last_active > cutoff      { return None; }
                        // activity_vec と embedding のコサイン類似度を計算
                        // 0.99 以上 = ほぼ decay しきった = 誰も来ていない
                        let sim = match (e.activity_vec.as_ref(), e.embedding.as_ref()) {
                            (Some(a), Some(b)) => cosine_sim(a, b),
                            _ => 1.0,
                        };
                        if sim >= 0.99 { Some(e.id) } else { None }
                    }).collect()
                };
                if !stale_ids.is_empty() {
                    let mut graph = state_c.graph.lock().unwrap();
                    for id in &stale_ids {
                        graph.remove_entity(id);
                        let _ = identity::delete_entity_db(id);
                    }
                    tracing::info!("entity rotation: removed {} stale stream entities", stale_ids.len());
                }

                // 新しい記事を取り込む
                let feeds = identity::load_feeds();
                tracing::info!("rss auto-ingest: processing {} feeds", feeds.len());
                for (url, max) in feeds {
                    let added = ingest_rss_feed(&url, max, &state_c).await;
                    if added > 0 {
                        tracing::info!("rss auto-ingest: +{} from {}", added, url);
                    }
                }
            }
        });
    }

    // 期限切れエンティティの自動削除タスク (1分ごと)
    {
        let state_c = Arc::clone(&state);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(60));
            loop {
                interval.tick().await;
                let now = Utc::now();
                let expired: Vec<Uuid> = {
                    let graph = state_c.graph.lock().unwrap();
                    graph.graph.node_indices()
                        .filter_map(|idx| {
                            let e = &graph.graph[idx];
                            if e.expires_at.map_or(false, |exp| exp <= now) { Some(e.id) } else { None }
                        })
                        .collect()
                };
                for id in expired {
                    state_c.graph.lock().unwrap().remove_entity(&id);
                    let _ = identity::delete_entity_db(&id);
                    tracing::info!("event expired and removed: {}", id);
                }
            }
        });
    }

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    // メッセージ自動消去タスク (30分ごとに2時間以上前のメッセージを削除)
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(1800));
        loop {
            interval.tick().await;
            match identity::cleanup_old_messages(2) {
                Ok(n) if n > 0 => tracing::info!("message cleanup: removed {} old messages", n),
                _ => {}
            }
        }
    });

    let app = Router::new()
        .route("/",              get(landing))
        .route("/privacy-policy", get(privacy_policy))
        .route("/field",         get(get_field))
        .route("/field/stream",  get(stream_field))
        .route("/presence",      get(get_presence))
        .route("/entities",      get(get_entities).post(post_entity))
        .route("/entity/:id",    axum::routing::delete(delete_entity))
        .route("/identity/new",  get(new_identity))
        .route("/identity/:id",  get(get_identity))
        .route("/encounter",     post(post_encounter))
        .route("/message",        post(post_message))
        .route("/connect/rss",    post(connect_rss))
        .route("/connect/url",    post(connect_url))
        .route("/feeds",          get(get_feeds))
        .route("/peers",          get(get_peers))
        .route("/peer/register",  post(register_peer))
        .route("/entities/export", get(export_entities))
        .layer(middleware::from_fn_with_state(Arc::clone(&state), rate_limit))
        .layer(cors)
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("0.0.0.0:7331").await.unwrap();
    println!("golden_core listening on :7331");
    axum::serve(listener, app).await.unwrap();
}
