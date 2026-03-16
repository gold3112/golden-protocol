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
}

#[derive(Deserialize)]
struct EntityRequest {
    label: String,
    kind:  Option<EntityKind>,
    /// embedding のシードテキスト。省略時は label をそのまま使う
    text:  Option<String>,
}

#[derive(Serialize)]
struct EntitySummary {
    id:          Uuid,
    label:       String,
    kind:        String,
    last_active: String,
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

    // position = near 上位3件のラベルから文脈を導く (最小実装: 最近傍のラベルを使用)
    if let Some(nearest) = field.near.first() {
        field.position = nearest.label.clone();
    }

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
  background: #07070f;
  font-family: 'SF Mono','Fira Code','Menlo',monospace;
  font-size: 12px;
  height: 100vh;
  overflow: hidden;
}
canvas {
  position: fixed;
  top: 0; left: 0;
  width: 100%; height: 100%;
}
#ui {
  position: fixed;
  top: 0; left: 0;
  width: 100%; height: 100%;
  pointer-events: none;
  display: flex;
  flex-direction: column;
  justify-content: space-between;
  padding: 28px 32px;
}
#top { display: flex; justify-content: space-between; align-items: flex-start; }
#tagline { font-size: 9px; letter-spacing: 0.22em; text-transform: uppercase; color: #2a2a44; margin-bottom: 10px; }
#position { font-size: 20px; color: #d4af37; letter-spacing: 0.04em; min-height: 28px; }
#meta-line { font-size: 10px; color: #303050; margin-top: 6px; }
#meta-line span { color: #484868; }
#presence-block { text-align: right; }
#presence-count { font-size: 28px; color: #50508a; letter-spacing: -0.02em; }
#presence-label { font-size: 9px; letter-spacing: 0.18em; text-transform: uppercase; color: #252540; margin-top: 4px; }
#bottom { display: flex; justify-content: space-between; align-items: flex-end; }
#drift { font-size: 11px; color: #906028; min-height: 18px; letter-spacing: 0.05em; }
#footer-right { text-align: right; }
#conn { font-size: 9px; letter-spacing: 0.15em; color: #202038; margin-bottom: 6px; }
#footer-link { pointer-events: auto; }
#footer-link a {
  font-size: 10px; color: #303055;
  text-decoration: none; border-bottom: 1px solid #252542;
  letter-spacing: 0.08em;
}
#footer-link a:hover { color: #7070b0; border-color: #5050a0; }
</style>
</head>
<body>
<canvas id="c"></canvas>
<div id="ui">
  <div id="top">
    <div>
      <div id="tagline">Golden Protocol · space.gold3112.online</div>
      <div id="position">—</div>
      <div id="meta-line">density <span id="density">—</span> &nbsp;·&nbsp; <span id="conn">connecting</span></div>
    </div>
    <div id="presence-block">
      <div id="presence-count">—</div>
      <div id="presence-label">present</div>
    </div>
  </div>
  <div id="bottom">
    <div id="drift"></div>
    <div id="footer-right">
      <div id="footer-link"><a href="https://github.com/gold3112/golden-protocol" target="_blank">source / extension →</a></div>
    </div>
  </div>
</div>

<script>
const canvas = document.getElementById('c');
const ctx    = canvas.getContext('2d');
let W, H, cx, cy;

function resize() {
  W = canvas.width  = window.innerWidth;
  H = canvas.height = window.innerHeight;
  cx = W / 2; cy = H / 2;
}
window.addEventListener('resize', resize);
resize();

// stable angle from label string
function labelAngle(label) {
  let h = 5381;
  for (let i = 0; i < label.length; i++) h = (((h << 5) + h) + label.charCodeAt(i)) >>> 0;
  return (h % 10000) / 10000 * Math.PI * 2;
}

function distToRadius(dist) {
  const near    = Math.min(W, H) * 0.18;
  const horizon = Math.min(W, H) * 0.36;
  const beyond  = Math.min(W, H) * 0.46;
  if (dist < 0.35) return near    + (dist / 0.35) * (horizon - near);
  if (dist < 0.70) return horizon + ((dist - 0.35) / 0.35) * (beyond - horizon);
  return beyond + ((dist - 0.70) / 0.30) * (Math.min(W, H) * 0.06);
}

// entity state: label -> {x, y, tx, ty, b, tb, kind}
const nodes = {};

const KIND_COLOR = {
  AI:      [170, 130, 255],
  Stream:  [70,  170, 255],
  Event:   [255, 150, 70 ],
  Data:    [80,  200, 130],
  Human:   [255, 210, 90 ],
  Service: [160, 160, 200],
};

function updateNodes(field, allEntities) {
  const distMap = {};
  (field.near    || []).forEach(e => distMap[e.label] = { d: e.distance, zone: 'near' });
  (field.horizon || []).forEach(e => distMap[e.label] = { d: e.distance, zone: 'horizon' });

  allEntities.forEach(e => {
    const angle = labelAngle(e.label);
    const info  = distMap[e.label];
    const dist  = info ? info.d : 0.88;
    const zone  = info ? info.zone : 'beyond';
    const r     = distToRadius(dist);
    const tx    = cx + Math.cos(angle) * r;
    const ty    = cy + Math.sin(angle) * r;
    const tb    = zone === 'near' ? 1.0 : zone === 'horizon' ? 0.35 : 0.08;

    if (!nodes[e.label]) {
      nodes[e.label] = { x: tx, y: ty, b: 0, kind: e.kind, label: e.label };
    }
    const n = nodes[e.label];
    n.tx = tx; n.ty = ty; n.tb = tb; n.kind = e.kind;
  });
}

function lerp(a, b, t) { return a + (b - a) * t; }

let clock = 0;
function animate() {
  requestAnimationFrame(animate);
  clock += 0.016;

  // soft trail
  ctx.fillStyle = 'rgba(7,7,15,0.18)';
  ctx.fillRect(0, 0, W, H);

  drawRings();

  Object.values(nodes).forEach(n => {
    if (n.tx !== undefined) { n.x = lerp(n.x, n.tx, 0.035); n.y = lerp(n.y, n.ty, 0.035); }
    n.b = lerp(n.b, n.tb || 0, 0.04);
    drawNode(n);
  });

  drawWanderers();
  drawSelf();
}

function drawNode(n) {
  const b = n.b;
  if (b < 0.02) return;
  const [r, g, bl] = KIND_COLOR[n.kind] || [140, 140, 190];
  const pulse  = 1 + 0.12 * Math.sin(clock * 1.4 + labelAngle(n.label));
  const radius = (2 + b * 5) * pulse;

  if (b > 0.25) {
    const gr = ctx.createRadialGradient(n.x, n.y, 0, n.x, n.y, radius * 7);
    gr.addColorStop(0, `rgba(${r},${g},${bl},${b * 0.25})`);
    gr.addColorStop(1, 'rgba(0,0,0,0)');
    ctx.fillStyle = gr;
    ctx.beginPath();
    ctx.arc(n.x, n.y, radius * 7, 0, Math.PI * 2);
    ctx.fill();
  }

  ctx.fillStyle = `rgba(${r},${g},${bl},${Math.min(1, b * 1.2)})`;
  ctx.beginPath();
  ctx.arc(n.x, n.y, radius, 0, Math.PI * 2);
  ctx.fill();

  if (b > 0.15) {
    ctx.fillStyle = `rgba(${r},${g},${bl},${b * 0.75})`;
    ctx.font = `${Math.round(9 + b * 3)}px "SF Mono",monospace`;
    ctx.fillText(n.label, n.x + radius + 5, n.y + 4);
  }
}

function drawSelf() {
  const pulse = 1 + 0.18 * Math.sin(clock * 2.2);
  const s = 9 * pulse;

  const gr = ctx.createRadialGradient(cx, cy, 0, cx, cy, 28 * pulse);
  gr.addColorStop(0,   'rgba(212,175,55,0.7)');
  gr.addColorStop(0.3, 'rgba(212,175,55,0.12)');
  gr.addColorStop(1,   'rgba(0,0,0,0)');
  ctx.fillStyle = gr;
  ctx.beginPath();
  ctx.arc(cx, cy, 28 * pulse, 0, Math.PI * 2);
  ctx.fill();

  ctx.strokeStyle = `rgba(212,175,55,${0.5 + 0.3 * Math.sin(clock * 2.2)})`;
  ctx.lineWidth = 1;
  ctx.beginPath();
  ctx.moveTo(cx - s, cy); ctx.lineTo(cx + s, cy);
  ctx.moveTo(cx, cy - s); ctx.lineTo(cx, cy + s);
  ctx.stroke();

  ctx.fillStyle = 'rgba(212,175,55,1)';
  ctx.beginPath();
  ctx.arc(cx, cy, 2.5, 0, Math.PI * 2);
  ctx.fill();
}

function drawRings() {
  [[0.18, 0.05], [0.42, 0.03]].forEach(([frac, alpha]) => {
    const r = Math.min(W, H) * frac;
    ctx.strokeStyle = `rgba(80,80,140,${alpha})`;
    ctx.lineWidth = 1;
    ctx.setLineDash([2, 10]);
    ctx.beginPath();
    ctx.arc(cx, cy, r, 0, Math.PI * 2);
    ctx.stroke();
    ctx.setLineDash([]);
  });
}

// --- wanderers (other users) ---
// id -> {x, y, tx, ty, phase}
const wanderers = {};

// stable small offset from entity center, seeded by user id
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
    const tx = node.x + ox;
    const ty = node.y + oy;
    if (!wanderers[u.id]) {
      wanderers[u.id] = { x: tx, y: ty, phase: Math.random() * Math.PI * 2 };
    }
    wanderers[u.id].tx = tx;
    wanderers[u.id].ty = ty;
  });
  // remove gone users
  Object.keys(wanderers).forEach(id => { if (!seen.has(id)) delete wanderers[id]; });
}

function drawWanderers() {
  Object.values(wanderers).forEach(w => {
    if (w.tx !== undefined) {
      w.x = lerp(w.x, w.tx, 0.06);
      w.y = lerp(w.y, w.ty, 0.06);
    }
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
  });
}

// --- data ---
let allEntities = [];

async function fetchEntities() {
  try { allEntities = await fetch('/entities').then(r => r.json()); } catch {}
}

async function fetchPresence() {
  try {
    const p = await fetch('/presence').then(r => r.json());
    updateWanderers(p.users || []);
  } catch {}
}

async function fetchField() {
  try {
    const f = await fetch('/field?interest=curiosity+exploration+encounter').then(r => r.json());
    updateNodes(f, allEntities);

    document.getElementById('position').textContent       = f.position || '—';
    document.getElementById('density').textContent        = ((f.density||0)*100).toFixed(0) + '%';
    document.getElementById('presence-count').textContent = f.presence || 0;
    document.getElementById('conn').textContent           = 'live';
    const drift = f.drift && f.drift.length ? '› drifting toward ' + f.drift[0].toward : '';
    document.getElementById('drift').textContent = drift;
  } catch {
    document.getElementById('conn').textContent = 'no signal';
  }
}

fetchEntities().then(() => {
  fetchField();
  fetchPresence();
  setInterval(fetchField,    4000);
  setInterval(fetchPresence, 4000);
  setInterval(fetchEntities, 30000);
});
animate();
</script>
</body>
</html>"##;

/// GET /field
async fn get_field(
    State(state): State<Arc<AppState>>,
    Query(q): Query<FieldQuery>,
) -> Json<FieldState> {
    Json(compute_field_state(&state, &q))
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
    let text = req.text.as_deref().unwrap_or(&req.label);
    let emb  = embedding::embed(text).map_err(|e| e.to_string())?;
    let kind = req.kind.unwrap_or(EntityKind::Data);

    let mut entity      = Entity::new(kind, &req.label);
    entity.activity_vec = Some(emb.clone());
    entity.embedding    = Some(emb);
    let summary = EntitySummary {
        id:          entity.id,
        label:       entity.label.clone(),
        kind:        format!("{:?}", entity.kind),
        last_active: entity.last_active.to_rfc3339(),
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
            "id":       u.id,
            "position": u.position,
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

/// POST /connect/rss — RSS/Atom フィードを Stream エンティティとして取り込む
async fn connect_rss(
    State(state): State<Arc<AppState>>,
    Json(req): Json<RssConnectRequest>,
) -> Result<Json<ConnectResult>, String> {
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
    let html = reqwest::get(&req.url).await
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
        let r = reqwest::get(url).await?;
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
                if let Ok(resp) = reqwest::Client::new()
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
                if let Ok(resp) = reqwest::get(format!("{}/peers", bootstrap)).await {
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
                    let entities: Vec<Entity> = match reqwest::get(
                        format!("{}/entities/export", peer_url)
                    ).await {
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
                    if let Ok(resp) = reqwest::get(format!("{}/peers", peer_url)).await {
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

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let app = Router::new()
        .route("/",              get(landing))
        .route("/field",         get(get_field))
        .route("/field/stream",  get(stream_field))
        .route("/presence",      get(get_presence))
        .route("/entities",      get(get_entities).post(post_entity))
        .route("/entity/:id",    axum::routing::delete(delete_entity))
        .route("/identity/new",  get(new_identity))
        .route("/identity/:id",  get(get_identity))
        .route("/encounter",     post(post_encounter))
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
