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
use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_stream::{Stream, wrappers::IntervalStream};
use futures_util::StreamExt as _;
use axum::{middleware::{self, Next}, http::StatusCode};
use governor::{clock::DefaultClock, state::keyed::DefaultKeyedStateStore, Quota, RateLimiter};
use std::num::NonZeroU32;
use std::net::IpAddr;
use tower_http::cors::{Any, CorsLayer};
use uuid::Uuid;

type IpLimiter = RateLimiter<IpAddr, DefaultKeyedStateStore<IpAddr>, DefaultClock>;

/// 空間で起きたイベント — SSEでwandererに届く
#[derive(Clone, Debug, Serialize)]
struct SpaceEvent {
    kind:   String,           // "emergence" | "convergence" | "encounter"
    label:  String,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
}

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
    /// 空間イベントのbroadcastチャンネル
    event_tx:        tokio::sync::broadcast::Sender<SpaceEvent>,
}

/// SSE接続中ユーザーの状態 (永続化しない)
#[derive(Clone)]
struct UserPresence {
    id:           Uuid,
    name:         String,
    interest_vec: Vec<f32>,
    spatial_vec:  Vec<f32>,           // 散策: 時間と共に動く位置ベクトル（interest_vecから独立）
    moving_toward: Option<String>,    // 今向かっている方向のエンティティラベル
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

    // SSE接続中ユーザーはspatial_vec（散策で動く位置）を優先する
    // これが「あなたが今どこにいるか」を決める
    let user_interest = {
        let spatial = q.user_id.and_then(|uid| {
            let users = state.connected_users.lock().unwrap();
            users.get(&uid)
                .map(|u| u.spatial_vec.clone())
                .filter(|v| v.iter().any(|&x| x != 0.0))
        });
        spatial
            .or_else(|| identity.as_ref().map(|i| i.interest_vec.clone()))
            .unwrap_or_else(|| fallback_interest(&q.interest))
    };

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
                    id:           entity.id,
                    label:        entity.label.clone(),
                    distance:     d,
                    visibility:   Default::default(),
                    components:   Some([sem, rel, act, tmp, att]),
                    moving_toward: None,
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
        // 名前が設定されていればそれを使う。空間に「誰か」として存在する
        let label = if !user.name.is_empty() {
            user.name.clone()
        } else {
            format!("wanderer_{}", short_id)
        };
        scored.push((
            ObservedEntity {
                id:           user.id,
                label,
                distance:     d,
                visibility:   Default::default(),
                components:   Some([sem, 0.5, 0.5, 0.0, att]),
                moving_toward: user.moving_toward.clone(),
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

/// GET / — 2D説明ランディング (HN向け)
async fn landing() -> Html<&'static str> {
    Html(ABOUT_HTML)
}

static ABOUT_HTML: &str = r###"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Golden Protocol</title>
<style>
* { box-sizing: border-box; margin: 0; padding: 0; }
:root { --gold: #d4af37; --dim: #555575; --bg: #06060f; --panel: #0c0c1a; --border: #181830; }
body { background: var(--bg); color: #b8b8d0; font-family: "SF Mono","Fira Code",monospace; font-size: 13px; line-height: 1.6; }
a { color: var(--gold); text-decoration: none; }
a:hover { text-decoration: underline; }

/* layout */
#app { display: flex; height: 100vh; overflow: hidden; }
#left  { flex: 1; min-width: 0; display: flex; flex-direction: column; overflow: hidden; }
#right { width: 320px; border-left: 1px solid var(--border); display: flex; flex-direction: column;
         transform: translateX(100%); transition: transform 0.22s ease; }
#right.open { transform: translateX(0); }

/* top bar */
#topbar {
  padding: 14px 20px 12px; border-bottom: 1px solid var(--border);
  display: flex; align-items: baseline; gap: 16px; flex-shrink: 0;
}
#topbar h1 { font-size: 13px; color: var(--gold); letter-spacing: 0.1em; font-weight: 400; }
#topbar .sub { font-size: 10px; color: var(--dim); }
#status-line { margin-left: auto; font-size: 10px; color: var(--dim); }

/* interest bar */
#interest-bar {
  padding: 10px 20px; border-bottom: 1px solid var(--border);
  display: flex; align-items: center; gap: 10px; flex-shrink: 0;
}
#interest-bar label { font-size: 10px; color: var(--dim); white-space: nowrap; }
#interest-input {
  flex: 1; background: transparent; border: none; border-bottom: 1px solid #282848;
  color: #d0d0f0; font-family: inherit; font-size: 12px; padding: 2px 0; outline: none;
}
#interest-input:focus { border-color: var(--gold); }
#interest-btn {
  background: none; border: 1px solid #282848; color: var(--dim);
  font-family: inherit; font-size: 10px; padding: 3px 10px; cursor: pointer; white-space: nowrap;
}
#interest-btn:hover { border-color: var(--gold); color: var(--gold); }

/* canvas */
#canvas-wrap { flex: 1; position: relative; }
canvas { display: block; width: 100%; height: 100%; }

/* bottom strip */
#bottom-strip {
  padding: 6px 20px; border-top: 1px solid var(--border); flex-shrink: 0;
  display: flex; gap: 20px; font-size: 10px; color: var(--dim);
  align-items: center;
}
#bottom-strip a { color: var(--dim); font-size: 10px; }
#bottom-strip a:hover { color: var(--gold); }
.sep { color: #282840; }

/* entity panel (right) */
#panel-header {
  padding: 14px 16px 10px; border-bottom: 1px solid var(--border);
  display: flex; align-items: flex-start; gap: 8px; flex-shrink: 0;
}
#panel-close {
  margin-left: auto; background: none; border: none; color: var(--dim);
  font-size: 16px; cursor: pointer; line-height: 1; padding: 0 2px;
}
#panel-close:hover { color: #d0d0f0; }
#panel-title { font-size: 12px; color: #d0d0f0; line-height: 1.4; flex: 1; }
#panel-kind  { font-size: 10px; color: var(--dim); margin-top: 2px; }

#panel-dist {
  padding: 12px 16px; border-bottom: 1px solid var(--border); flex-shrink: 0;
}
#panel-dist .dist-total { font-size: 18px; color: var(--gold); margin-bottom: 8px; }
.bar-row { display: flex; align-items: center; gap: 8px; margin-bottom: 4px; font-size: 10px; }
.bar-label { width: 64px; color: var(--dim); }
.bar-track { flex: 1; height: 3px; background: #181830; border-radius: 2px; }
.bar-fill  { height: 100%; border-radius: 2px; background: var(--gold); }
.bar-val   { width: 28px; text-align: right; color: #808098; }

#panel-search-link {
  display: block; padding: 8px 16px; font-size: 10px; color: var(--dim);
  border-bottom: 1px solid var(--border); flex-shrink: 0;
}
#panel-search-link:hover { color: var(--gold); }

#panel-messages { flex: 1; overflow-y: auto; padding: 12px 16px; }
.msg-item { margin-bottom: 12px; font-size: 11px; line-height: 1.5; }
.msg-author { color: var(--dim); font-size: 10px; margin-bottom: 2px; }
.msg-text   { color: #c0c0d8; }
#panel-empty { color: var(--dim); font-size: 11px; font-style: italic; }

#panel-compose {
  padding: 10px 12px; border-top: 1px solid var(--border); flex-shrink: 0;
  display: flex; gap: 8px; align-items: center;
}
#msg-input {
  flex: 1; background: transparent; border: none; border-bottom: 1px solid #282848;
  color: #d0d0f0; font-family: inherit; font-size: 11px; padding: 3px 0; outline: none;
}
#msg-input:focus { border-color: var(--gold); }
#msg-send {
  background: none; border: none; color: var(--dim); font-family: inherit;
  font-size: 14px; cursor: pointer; padding: 0 4px;
}
#msg-send:hover { color: var(--gold); }

/* info sidebar for concept */
#explain {
  padding: 16px 20px; font-size: 11px; color: var(--dim); line-height: 1.7;
  border-bottom: 1px solid var(--border); flex-shrink: 0;
}
#explain .row { display: flex; gap: 6px; margin-bottom: 3px; }
#explain .tag { color: #404060; width: 80px; flex-shrink: 0; }
#explain strong { color: #9090b0; font-weight: 400; }
</style>
</head>
<body>
<div id="app">

  <!-- left: field -->
  <div id="left">
    <div id="topbar">
      <h1>GOLDEN PROTOCOL</h1>
      <span class="sub">space.gold3112.online</span>
      <span id="status-line">—</span>
    </div>
    <div id="interest-bar">
      <label>interest</label>
      <input id="interest-input" type="text" value="programming" autocomplete="off" spellcheck="false" maxlength="120">
      <button id="interest-btn">→</button>
    </div>
    <div id="canvas-wrap">
      <canvas id="field-canvas"></canvas>
    </div>
    <div id="bottom-strip">
      <span id="info-counts">near — · horizon —</span>
      <span class="sep">|</span>
      <a href="https://github.com/gold3112/golden-protocol" target="_blank">GitHub</a>
      <span class="sep">|</span>
      <span style="color:#282840">click any dot to explore</span>
    </div>
  </div>

  <!-- right: entity panel -->
  <div id="right">
    <div id="panel-header">
      <div>
        <div id="panel-title">—</div>
        <div id="panel-kind"></div>
      </div>
      <button id="panel-close">×</button>
    </div>
    <div id="panel-dist">
      <div class="dist-total" id="dist-total">—</div>
      <div id="dist-bars"></div>
    </div>
    <a id="panel-search-link" href="#" target="_blank">search →</a>
    <div id="panel-messages"><div id="panel-empty">no messages yet</div></div>
    <div id="panel-compose">
      <input id="msg-input" type="text" placeholder="say something..." maxlength="280" autocomplete="off">
      <button id="msg-send">→</button>
    </div>
  </div>

</div>

<script>
const canvas = document.getElementById('field-canvas');
const ctx    = canvas.getContext('2d');
let W, H, cx, cy;

function resize() {
  const wrap = document.getElementById('canvas-wrap');
  W = canvas.width  = wrap.clientWidth;
  H = canvas.height = wrap.clientHeight;
  cx = W / 2; cy = H / 2;
  if (lastField) drawField(lastField);
}
const ro = new ResizeObserver(resize);
ro.observe(document.getElementById('canvas-wrap'));
requestAnimationFrame(()=>{ resize(); fetchField(currentInterest); });

function labelAngle(label) {
  let h = 5381;
  for (let i = 0; i < label.length; i++) h = (((h<<5)+h)+label.charCodeAt(i))>>>0;
  return (h%10000)/10000*Math.PI*2;
}
// components: [semantic, relational, activity, temporal, attention]
const COMP_NAMES  = ['semantic','relational','activity','temporal','attention'];
const COMP_COLORS = ['#a080e8','#50a8e8','#50c880','#e8a050','#a0a0e0'];

let lastField    = null;
let hoveredLabel = null;
let selectedLabel = null;
let allEntities  = [];  // {label, zone, distance, kind, _sx, _sy, ...}

function entityPos(e) {
  const s = Math.min(W, H);
  const NR = s * 0.26, HR = s * 0.43;
  const angle = labelAngle(e.label);
  let r;
  if (e.zone === 'near') {
    r = NR * 0.15 + e.distance * NR * 1.6;
    r = Math.min(r, NR * 0.93);
  } else {
    r = NR + 12 + (e.distance - 0.5) * (HR - NR) * 1.6;
    r = Math.max(NR + 8, Math.min(r, HR * 0.95));
  }
  return { x: cx + Math.cos(angle)*r, y: cy + Math.sin(angle)*r,
           angle, NR, HR, r };
}

function drawField(field) {
  lastField = field;
  allEntities = [
    ...(field.near||[]).map(e=>({...e,zone:'near'})),
    ...(field.horizon||[]).map(e=>({...e,zone:'horizon'})),
  ];

  ctx.clearRect(0,0,W,H);
  ctx.fillStyle='#0c0c1a'; ctx.fillRect(0,0,W,H);

  const s = Math.min(W,H);
  const NR = s*0.26, HR = s*0.43;

  // zone rings
  [[NR,'rgba(212,175,55,0.07)','rgba(212,175,55,0.16)'],
   [HR,'rgba(60,60,140,0.05)', 'rgba(60,60,140,0.12)']].forEach(([r,fill,stroke])=>{
    ctx.beginPath(); ctx.arc(cx,cy,r,0,Math.PI*2);
    ctx.fillStyle=fill; ctx.fill();
    ctx.strokeStyle=stroke; ctx.lineWidth=1;
    ctx.setLineDash([2,6]); ctx.stroke(); ctx.setLineDash([]);
  });

  // zone text
  ctx.font='9px "SF Mono",monospace'; ctx.textBaseline='middle';
  ctx.fillStyle='rgba(212,175,55,0.3)'; ctx.fillText('near',    cx+NR-28, cy-NR+6);
  ctx.fillStyle='rgba(60,60,140,0.5)';  ctx.fillText('horizon', cx+HR-46, cy-HR+6);

  // lines center→near (only non-selected, dim)
  allEntities.filter(e=>e.zone==='near').forEach(e=>{
    const {x,y} = entityPos(e);
    e._sx=x; e._sy=y;
    const isSel = selectedLabel === e.label;
    ctx.beginPath(); ctx.moveTo(cx,cy); ctx.lineTo(x,y);
    ctx.strokeStyle = isSel ? 'rgba(212,175,55,0.18)' : 'rgba(212,175,55,0.05)';
    ctx.lineWidth=1; ctx.stroke();
  });

  // horizon positions (no lines)
  allEntities.filter(e=>e.zone==='horizon').forEach(e=>{
    const {x,y} = entityPos(e); e._sx=x; e._sy=y;
  });

  // dots + labels
  // sort: horizon first, then near, selected last
  const sorted = [...allEntities].sort((a,b)=>{
    if (a.zone===b.zone) return 0;
    return a.zone==='horizon' ? -1 : 1;
  });
  if (selectedLabel) {
    const idx = sorted.findIndex(e=>e.label===selectedLabel);
    if (idx>=0) sorted.push(...sorted.splice(idx,1));
  }

  sorted.forEach(e=>{
    const {x,y,angle} = entityPos(e);
    e._sx=x; e._sy=y;
    const [cr,cg,cb] = [160,160,200];
    const isHov = hoveredLabel === e.label;
    const isSel = selectedLabel === e.label;
    const isNear = e.zone==='near';
    const alpha  = isNear ? (isSel||isHov?1:0.85) : (isSel||isHov?0.9:0.35);
    const radius = isNear ? (isSel?7:isHov?6:4) : (isHov||isSel?5:2.5);

    // glow for near or hovered
    if (isNear || isHov || isSel) {
      const gr = ctx.createRadialGradient(x,y,0,x,y,radius*5);
      gr.addColorStop(0,`rgba(${cr},${cg},${cb},${isSel?0.3:0.15})`);
      gr.addColorStop(1,'rgba(0,0,0,0)');
      ctx.fillStyle=gr; ctx.beginPath(); ctx.arc(x,y,radius*5,0,Math.PI*2); ctx.fill();
    }

    ctx.beginPath(); ctx.arc(x,y,radius,0,Math.PI*2);
    ctx.fillStyle=`rgba(${cr},${cg},${cb},${alpha})`; ctx.fill();

    // label: near always (max 8 by closeness), horizon only on hover/select
    const showLabel = isNear || isHov || isSel;
    if (showLabel) {
      const lbl = e.label.length>30 ? e.label.slice(0,29)+'…' : e.label;
      const tx = x + Math.cos(angle)*(radius+5);
      const ty = y + Math.sin(angle)*(radius+5);
      ctx.font = (isSel||isHov) ? 'bold 10px "SF Mono",monospace' : '10px "SF Mono",monospace';
      ctx.textAlign  = Math.cos(angle)>0 ? 'left' : 'right';
      ctx.textBaseline = 'middle';
      ctx.fillStyle = isSel ? `rgba(${cr},${cg},${cb},1)`
                    : isHov ? `rgba(${cr},${cg},${cb},0.95)`
                    :         `rgba(${cr},${cg},${cb},0.65)`;
      ctx.fillText(lbl, tx, ty);
    }
  });

  ctx.textAlign='left'; ctx.textBaseline='alphabetic';

  // self
  const sg = ctx.createRadialGradient(cx,cy,0,cx,cy,20);
  sg.addColorStop(0,'rgba(212,175,55,0.45)'); sg.addColorStop(1,'rgba(0,0,0,0)');
  ctx.fillStyle=sg; ctx.beginPath(); ctx.arc(cx,cy,20,0,Math.PI*2); ctx.fill();
  ctx.beginPath(); ctx.arc(cx,cy,4.5,0,Math.PI*2);
  ctx.fillStyle='rgba(212,175,55,0.95)'; ctx.fill();
  ctx.font='9px "SF Mono",monospace'; ctx.textAlign='center'; ctx.textBaseline='middle';
  ctx.fillStyle='rgba(212,175,55,0.4)'; ctx.fillText('you',cx,cy+17);
  ctx.textAlign='left'; ctx.textBaseline='alphabetic';
}

// hover
canvas.addEventListener('mousemove', e=>{
  const r = canvas.getBoundingClientRect();
  const mx=e.clientX-r.left, my=e.clientY-r.top;
  let found=null;
  allEntities.forEach(ent=>{
    if(ent._sx===undefined) return;
    const d=Math.hypot(ent._sx-mx,ent._sy-my);
    if(d<20) found=ent;
  });
  const lbl = found?found.label:null;
  if(lbl!==hoveredLabel){ hoveredLabel=lbl; if(lastField) drawField(lastField); }
  canvas.style.cursor = found?'pointer':'default';
});
canvas.addEventListener('mouseleave',()=>{
  hoveredLabel=null; if(lastField) drawField(lastField);
});

// click → open panel
canvas.addEventListener('click', e=>{
  const r = canvas.getBoundingClientRect();
  const mx=e.clientX-r.left, my=e.clientY-r.top;
  let found=null;
  allEntities.forEach(ent=>{
    if(ent._sx===undefined) return;
    if(Math.hypot(ent._sx-mx,ent._sy-my)<20) found=ent;
  });
  if(found) openPanel(found);
});

// panel
const rightEl = document.getElementById('right');
function openPanel(entity) {
  selectedLabel = entity.label;
  if(lastField) drawField(lastField);

  document.getElementById('panel-title').textContent = entity.label;
  document.getElementById('panel-kind').textContent  = `distance ${entity.distance ? entity.distance.toFixed(3) : '—'}`;
  document.getElementById('dist-total').textContent  = entity.distance ? entity.distance.toFixed(3) : '—';

  // distance bars — components is array [semantic, relational, activity, temporal, attention]
  const bars = document.getElementById('dist-bars');
  bars.innerHTML='';
  (entity.components||[]).forEach((val,i)=>{
    const pct = Math.round(val*100);
    bars.innerHTML+=`<div class="bar-row">
      <span class="bar-label">${COMP_NAMES[i]||i}</span>
      <div class="bar-track"><div class="bar-fill" style="width:${pct}%;background:${COMP_COLORS[i]}"></div></div>
      <span class="bar-val">${val.toFixed(2)}</span>
    </div>`;
  });

  // search link
  const sl = document.getElementById('panel-search-link');
  sl.href = 'https://www.google.com/search?q=' + encodeURIComponent(entity.label);
  sl.textContent = 'search "' + (entity.label.length>32?entity.label.slice(0,31)+'…':entity.label) + '" →';

  // messages from field state
  renderMessages(entity.label);

  rightEl.classList.add('open');
  document.getElementById('msg-input').dataset.entity = entity.label;
}

function renderMessages(entityLabel) {
  const msgs = (lastField&&lastField.messages||[]).filter(m=>m.entity_label===entityLabel);
  const el = document.getElementById('panel-messages');
  if(!msgs.length){
    el.innerHTML='<div id="panel-empty">no messages yet</div>'; return;
  }
  el.innerHTML = msgs.map(m=>`
    <div class="msg-item">
      <div class="msg-author">${escapeHtml(m.author||'wanderer')}</div>
      <div class="msg-text">${escapeHtml(m.content)}</div>
    </div>`).join('');
}

document.getElementById('panel-close').addEventListener('click',()=>{
  rightEl.classList.remove('open');
  selectedLabel=null;
  if(lastField) drawField(lastField);
});

// send message
async function sendMessage() {
  const input = document.getElementById('msg-input');
  const text  = input.value.trim();
  const entity= input.dataset.entity;
  if(!text||!entity) return;
  input.value='';
  try {
    await fetch('/message', {
      method:'POST', headers:{'Content-Type':'application/json'},
      body: JSON.stringify({ entity_label: entity, content: text, author: 'visitor' }),
    });
    await fetchField(currentInterest);  // refresh messages
    renderMessages(entity);
  } catch(_){}
}
document.getElementById('msg-send').addEventListener('click', sendMessage);
document.getElementById('msg-input').addEventListener('keydown', e=>{
  if(e.key==='Enter') sendMessage();
});

function escapeHtml(s){
  return String(s).replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;');
}

// fetch
let currentInterest = 'programming';
async function fetchField(interest) {
  document.getElementById('status-line').textContent = '...';
  try {
    const res = await fetch(`/field?interest=${encodeURIComponent(interest)}`);
    const f   = await res.json();
    document.getElementById('status-line').textContent =
      `${f.presence||0} here · ${new Date().toLocaleTimeString()}`;
    document.getElementById('info-counts').textContent =
      `near ${(f.near||[]).length} · horizon ${(f.horizon||[]).length}`;
    resize();
    drawField(f);
    // refresh panel if open
    if(selectedLabel) renderMessages(selectedLabel);
  } catch(_){
    document.getElementById('status-line').textContent = 'error';
  }
}

document.getElementById('interest-btn').addEventListener('click',()=>{
  currentInterest = document.getElementById('interest-input').value.trim()||'curiosity';
  fetchField(currentInterest);
});
document.getElementById('interest-input').addEventListener('keydown',e=>{
  if(e.key==='Enter'){
    currentInterest=e.target.value.trim()||'curiosity';
    fetchField(currentInterest);
  }
});

</script>
</body>
</html>
"###;

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
#bottom {
  display: flex; justify-content: space-between; align-items: flex-end;
  position: relative;
}
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

/* ─── wander bar ────────────────────────────────── */
#wander-bar {
  position: absolute;
  left: 50%; transform: translateX(-50%);
  display: flex; align-items: center; gap: 8px;
  pointer-events: auto;
  opacity: 0.55; transition: opacity 0.4s;
}
#wander-bar:hover, #wander-bar:focus-within { opacity: 1.0; }
#wander-label { font-size: 11px; color: #686890; }
#wander-input {
  background: transparent; border: none;
  border-bottom: 1px solid #383858;
  color: #9898c8; font-family: inherit; font-size: 11px;
  width: 200px; padding: 4px 0; outline: none;
  letter-spacing: 0.05em; text-align: center;
  transition: border-color 0.3s, width 0.3s;
}
#wander-input:focus { border-color: #6060a8; width: 260px; }
#wander-input::placeholder { color: #282848; }

/* ─── drift: clickable ──────────────────────────── */
.drift-link {
  cursor: pointer; transition: color 0.2s;
  pointer-events: auto;
}
.drift-link:hover { color: #c89840; }

/* ─── entity panel ──────────────────────────────── */
#panel {
  position: fixed;
  right: 28px; top: 50%; transform: translateY(-50%);
  left: auto;
  width: 240px;
  background: rgba(8,8,20,0.96);
  border: 1px solid #282848;
  padding: 20px;
  pointer-events: auto;
  opacity: 0; transition: opacity 0.25s ease, transform 0.25s ease;
  max-height: 70vh; overflow-y: auto;
  backdrop-filter: blur(12px);
  box-shadow: 0 8px 48px rgba(0,0,0,0.7);
}
#panel.visible { opacity: 1; }

/* ─── panel wander button ───────────────────────── */
#panel-wander {
  display: block; width: 100%;
  background: none; border: 1px solid #303060;
  color: #6060a8; font-family: inherit; font-size: 10px;
  letter-spacing: 0.12em; padding: 7px 0; cursor: pointer;
  margin-bottom: 14px; transition: all 0.2s;
}
#panel-wander:hover { border-color: #6060b0; color: #9898d8; }
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
    <div id="wander-bar">
      <span id="wander-label">›</span>
      <input id="wander-input" type="text" placeholder="what draws you now..." autocomplete="off" spellcheck="false" maxlength="120">
    </div>
    <div id="footer-right">
      <div id="footer-link"><a href="https://github.com/gold3112/golden-protocol" target="_blank">source →</a></div>
    </div>
  </div>
</div>

<!-- entity panel -->
<div id="panel">
  <button id="panel-close">×</button>
  <div id="panel-kind"></div>
  <div id="panel-label"></div>
  <a id="panel-search" href="#" target="_blank">search →</a>
  <button id="panel-wander" style="display:none">wander toward this →</button>
  <div id="panel-messages"></div>
  <div id="panel-msg-form">
    <input id="panel-msg-input" type="text" placeholder="say something..." autocomplete="off" maxlength="280">
    <button id="panel-msg-send">→</button>
  </div>
</div>

<div id="hint">click to enter · WASD · mouse to look</div>

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
const CAM = { fov: 520, d: 190 };
const CAM_HEIGHT   = 140;   // camera eye level in world space
const HORIZON_FRAC = 0.40;
let camZ = 0, camVZ = 0;    // forward/back
let camX = 0, camVX = 0;    // lateral
let camYaw   = 0, camYawV   = 0;   // horizontal rotation (drag left/right)
let camPitch = 0, camPitchV = 0;   // vertical tilt (drag up/down)

function project(wx, wy, wz) {
  // translate to camera-relative
  const dx = wx - camX;
  const dy = wy - CAM_HEIGHT;
  const dz = wz - camZ;

  // yaw rotation (turn left/right around Y axis)
  const cy = Math.cos(camYaw), sy = Math.sin(camYaw);
  const rx_y =  dx * cy - dz * sy;
  const rz_y =  dx * sy + dz * cy;

  // pitch rotation (look up/down around X axis)
  const cp = Math.cos(camPitch), sp = Math.sin(camPitch);
  const ry  =  dy * cp - rz_y * sp;
  const rz  =  dy * sp + rz_y * cp;

  const depth = rz + CAM.d;
  if (depth < 1) return null;
  const s = CAM.fov / depth;
  return {
    sx: cx + rx_y * s,
    sy: H * HORIZON_FRAC - ry * s,
    s,
    ps: Math.min(s * 380 / CAM.fov, 2.8),
  };
}

function entityWorldPos(angle, dist) {
  const wz = 55 + dist * 780;
  // spread is decoupled from depth: near entities get generous horizontal room
  const spread = 280 + dist * 320;
  const wx = Math.sin(angle) * spread;
  // more vertical range to avoid stacking
  const wy = Math.sin(angle * 2.7 + dist * 9.3) * 130;
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

// ─── FPS controls ─────────────────────────────────
const _keys = {};
let _locked = false;

// Pointer Lock: click canvas → capture mouse for FPS look
canvas.addEventListener('click', () => {
  if (!_locked) canvas.requestPointerLock();
});
document.addEventListener('pointerlockchange', () => {
  _locked = document.pointerLockElement === canvas;
  canvas.style.cursor = _locked ? 'none' : 'default';
  document.getElementById('hint').textContent =
    _locked ? 'WASD · mouse look · click to select · ESC to release'
            : 'click to enter · WASD · mouse to look';
});

// Mouse look (only when locked)
document.addEventListener('mousemove', e => {
  if (!_locked) return;
  camYaw   += e.movementX * 0.0022;
  camPitch -= e.movementY * 0.0022;
  camPitch  = Math.max(-0.70, Math.min(0.70, camPitch));
});

// Click while locked = select entity at crosshair center
canvas.addEventListener('mousedown', e => {
  if (!_locked || e.button !== 0) return;
  const hit = findNearestNode(cx, H * 0.48, 80);
  if (hit) showPanel(hit);
});

document.addEventListener('keydown', e => {
  _keys[e.key] = true;
  // '/' → wander bar
  const active = document.activeElement;
  const isInput = active && (active.tagName === 'INPUT' || active.tagName === 'TEXTAREA');
  if (e.key === '/' && !isInput) { e.preventDefault(); document.getElementById('wander-input')?.focus(); }
});
document.addEventListener('keyup', e => { _keys[e.key] = false; });

// Scroll: move forward/back (fallback when not locked)
canvas.addEventListener('wheel', e => {
  const fwdZ = Math.cos(camYaw), fwdX = Math.sin(camYaw);
  camZ += fwdZ * e.deltaY * 0.12;
  camX += fwdX * e.deltaY * 0.12;
  if (camZ < 0) camZ = 0;
}, { passive: true });

function updateCamera() {
  const isTyping = document.activeElement &&
    (document.activeElement.tagName === 'INPUT' || document.activeElement.tagName === 'TEXTAREA');
  if (isTyping) return;

  const speed  = 5.0;
  const fwdX   = Math.sin(camYaw), fwdZ = Math.cos(camYaw);
  const rtX    = Math.cos(camYaw), rtZ  = -Math.sin(camYaw);

  if (_keys['w'] || _keys['W'] || _keys['ArrowUp'])    { camX += fwdX*speed; camZ += fwdZ*speed; }
  if (_keys['s'] || _keys['S'] || _keys['ArrowDown'])  { camX -= fwdX*speed; camZ -= fwdZ*speed; }
  if (_keys['a'] || _keys['A'] || _keys['ArrowLeft'])  { camX -= rtX*speed;  camZ -= rtZ*speed;  }
  if (_keys['d'] || _keys['D'] || _keys['ArrowRight']) { camX += rtX*speed;  camZ += rtZ*speed;  }

  if (camZ < 0) camZ = 0;
}

// Crosshair drawn when locked
function drawCrosshair() {
  if (!_locked) return;
  const x = cx, y = H * 0.48;
  ctx.save();
  ctx.strokeStyle = 'rgba(255,255,255,0.55)';
  ctx.lineWidth = 1;
  ctx.setLineDash([]);
  ctx.beginPath();
  ctx.moveTo(x - 9, y); ctx.lineTo(x + 9, y);
  ctx.moveTo(x, y - 9); ctx.lineTo(x, y + 9);
  ctx.stroke();
  ctx.restore();
}

function lerp(a, b, t) { return a + (b - a) * t; }

let hoveredNode = null;

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

  document.getElementById('panel-kind').textContent  = node.kind;
  document.getElementById('panel-label').textContent = node.label;
  const q = encodeURIComponent(node.label);
  document.getElementById('panel-search').href = `https://www.google.com/search?q=${q}`;

  // wander toward ボタン (wanderer自身には表示しない)
  const wanderBtn = document.getElementById('panel-wander');
  if (wanderBtn) {
    if (node.kind && node.kind !== 'Human') {
      wanderBtn.style.display = 'block';
      wanderBtn.onclick = () => { updateInterest(node.label); closePanel(); };
    } else {
      wanderBtn.style.display = 'none';
    }
  }

  currentPanelEntity = node.label;
  renderPanelMessages(node.label);
  document.getElementById('panel-msg-input').value = '';
  panel.classList.add('visible');
  document.getElementById('hint').classList.add('hidden');
}

function closePanel() {
  document.getElementById('panel').classList.remove('visible');
  currentPanelEntity = null;
}

document.getElementById('panel-close').addEventListener('click', closePanel);

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


const convergencePulses = [];

function drawConvergencePulses() {
  for (let i = convergencePulses.length - 1; i >= 0; i--) {
    const cp = convergencePulses[i];
    cp.t += 0.008;
    if (cp.t > 1) { convergencePulses.splice(i, 1); continue; }
    const n = nodes[cp.label];
    if (!n || !n.rx) continue;
    const radius = cp.t * 120 * (n.projScale || 1);
    const alpha = (1 - cp.t) * 0.4;
    ctx.strokeStyle = `rgba(180,140,50,${alpha})`;
    ctx.lineWidth = 1.5;
    ctx.setLineDash([6, 10]);
    ctx.beginPath(); ctx.arc(n.rx, n.ry, radius, 0, Math.PI * 2); ctx.stroke();
    ctx.setLineDash([]);
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

  drawConvergencePulses();
  drawConstellation();
  drawWanderers();
  drawSelf();
  drawVignette();   // fog over everything
  drawCrosshair();
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
  if (isHovered || b > 0.52) {
    const alpha = isHovered ? 0.90 : Math.min(0.78, (b - 0.52) * 3.5);
    const fs = Math.max(8, Math.round((isHovered ? 12 : 8 + b * 4) * Math.min(1.3, proj.ps)));
    ctx.fillStyle = `rgba(${r},${g},${bl},${alpha})`;
    ctx.font = `${fs}px "SF Mono",monospace`;
    ctx.fillText(n.label, n.rx + radius + 4, n.ry + 4);
  }

  // most recent message floating near entity (near zone only)
  if (b > 0.62) {
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
  const sx = cx;
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
    if (!wanderers[u.id]) wanderers[u.id] = { x: tx, y: ty, phase: Math.random() * Math.PI * 2, name: u.name || null, serverId: u.id, encounterFlash: 0 };
    wanderers[u.id].tx = tx;
    wanderers[u.id].ty = ty;
    wanderers[u.id].name = u.name || null;
    wanderers[u.id].serverId = u.id;
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
    // moving_toward: 向かっている方向を点線で示す
    if (w.moving_toward) {
      const target = nodes[w.moving_toward];
      if (target && target.rx) {
        const dx = target.rx - w.x, dy = target.ry - w.y;
        const dist = Math.sqrt(dx*dx + dy*dy);
        if (dist > 20 && dist < 600) {
          const alpha = 0.12 * (1 + 0.3 * Math.sin(clock * 1.5 + w.phase));
          ctx.strokeStyle = `rgba(80,200,200,${alpha})`;
          ctx.lineWidth = 0.5;
          ctx.setLineDash([3, 9]);
          ctx.beginPath(); ctx.moveTo(w.x, w.y); ctx.lineTo(target.rx, target.ry); ctx.stroke();
          ctx.setLineDash([]);
        }
      }
    }
    // encounter flash
    if (w.encounterFlash > 0) {
      const ef = w.encounterFlash;
      w.encounterFlash = Math.max(0, ef - 0.02);
      const gr2 = ctx.createRadialGradient(w.x, w.y, 0, w.x, w.y, r * 12 * ef);
      gr2.addColorStop(0, `rgba(200,180,80,${ef * 0.5})`);
      gr2.addColorStop(1, 'rgba(0,0,0,0)');
      ctx.fillStyle = gr2;
      ctx.beginPath(); ctx.arc(w.x, w.y, r * 12 * ef, 0, Math.PI * 2); ctx.fill();
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

// identity — localStorage で永続化 (webクライアント用)
const WEB_ID_KEY = 'gp_web_id';
function getOrCreateWebId(interestHint) {
  const stored = localStorage.getItem(WEB_ID_KEY);
  if (stored) return Promise.resolve(JSON.parse(stored).id);
  return fetch('/identity/new?interest=' + encodeURIComponent(interestHint || 'curiosity exploration'))
    .then(r => r.json())
    .then(d => { localStorage.setItem(WEB_ID_KEY, JSON.stringify({id: d.id})); return d.id; })
    .catch(() => null);
}

async function updateInterest(newText) {
  const t = newText.trim();
  if (!t) return;
  interest = t;
  sessionStorage.setItem('gp_interest', interest);

  // 方向変更をencounterとして記録 → interest_vecが更新される
  const stored = localStorage.getItem(WEB_ID_KEY);
  if (stored) {
    try {
      const userId = JSON.parse(stored).id;
      await fetch('/encounter', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          user_id: userId,
          position: currentPosition,
          near_labels: [],
          interest_text: interest,
        }),
      });
    } catch(_) {}
  }

  // wanderバーのplaceholderを更新
  const wi = document.getElementById('wander-input');
  if (wi) { wi.value = ''; wi.placeholder = interest.slice(0, 40); }

  startSSE(); // 新しい方向でSSE再接続
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
  // request pointer lock so player can start moving immediately
  setTimeout(() => canvas.requestPointerLock(), 400);
  // hint fades out after 8s
  setTimeout(() => document.getElementById('hint').classList.add('hidden'), 8000);
  startSSE();
}

// wander bar
const _wi = document.getElementById('wander-input');
if (_wi) {
  _wi.addEventListener('keydown', e => {
    if (e.key === 'Enter') { updateInterest(_wi.value); _wi.blur(); }
    if (e.key === 'Escape') _wi.blur();
  });
}

// '/' key: wanderバーにフォーカス

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
let allEntities = [];
let currentPosition = 'plaza';

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

function handleFieldState(f) {
  updateNodes(f, allEntities);
  updateMessages(f);

  // wandererのmoving_toward情報をマージ
  const directions = {};
  [...(f.near || []), ...(f.horizon || [])].forEach(e => {
    if (e.moving_toward) directions[e.id] = e.moving_toward;
  });
  Object.values(wanderers).forEach(w => {
    if (directions[w.serverId]) w.moving_toward = directions[w.serverId];
  });

  const pos = f.position || '—';
  document.getElementById('position').textContent = pos.length > 48 ? pos.slice(0, 46) + '…' : pos;
  document.getElementById('presence-count').textContent = f.presence || 0;
  document.getElementById('conn').textContent = 'live';
  currentPosition = f.position || 'plaza';
  const driftEl = document.getElementById('drift');
  if (f.drift && f.drift.length) {
    driftEl.innerHTML = f.drift.slice(0, 2).map(d =>
      `<span class="drift-link" data-toward="${escapeHtml(d.toward)}">› ${escapeHtml(d.toward)}</span>`
    ).join('  ');
    driftEl.querySelectorAll('.drift-link').forEach(el => {
      el.addEventListener('click', () => updateInterest(el.dataset.toward));
    });
  } else {
    driftEl.textContent = '';
  }
}

function handleSpaceEvent(evt) {
  if (evt.kind === 'emergence') {
    // 新エンティティ誕生: 特別なrippleを発生
    const n = nodes[evt.label];
    if (n && n.wx !== undefined) {
      const [cr, cg, cb] = [200, 168, 64]; // gold
      for (let i = 0; i < 3; i++) {
        births.push({ wx: n.wx, wy: n.wy + 8, wz: n.wz, t: i * 0.3, r: cr, g: cg, b: cb });
      }
    }
    // UIに一時表示
    flashEvent('✦ ' + evt.label + ' emerged');

  } else if (evt.kind === 'convergence') {
    // 収束: 対象エンティティにpulseリングを追加
    const n = nodes[evt.label];
    if (n && n.wx !== undefined) {
      convergencePulses.push({ label: evt.label, t: 0, detail: evt.detail || '' });
    }
    flashEvent('⟳ gathering near ' + evt.label);

  } else if (evt.kind === 'encounter') {
    // 出会い: wandererを一時的に強調
    const w = Object.values(wanderers).find(w => w.name === evt.label || w.rawLabel === evt.label);
    if (w) w.encounterFlash = 1.0;
    flashEvent('◎ ' + evt.label + ' is near');
  }
}

function flashEvent(text) {
  const el = document.getElementById('conn');
  const prev = el.textContent;
  el.textContent = text;
  el.style.color = '#a08030';
  setTimeout(() => {
    el.textContent = prev;
    el.style.color = '';
  }, 4000);
}

let sseConn = null;

function startSSE() {
  if (sseConn) sseConn.close();

  getOrCreateWebId(interest).then(userId => {
    const params = new URLSearchParams({ interest: interest || 'curiosity exploration wandering', passive: 'true' });
    if (userId)    params.set('user_id', userId);
    if (authorName) params.set('name', authorName);

    sseConn = new EventSource('/field/stream?' + params.toString());

    sseConn.addEventListener('field', e => {
      try { handleFieldState(JSON.parse(e.data)); } catch(_) {}
    });

    sseConn.addEventListener('space', e => {
      try { handleSpaceEvent(JSON.parse(e.data)); } catch(_) {}
    });

    sseConn.onerror = () => {
      document.getElementById('conn').textContent = 'no signal';
    };
  });

  // entities と presence は引き続きポーリング (SSEに含まれないため)
  fetchEntities();
  fetchPresence();
  setInterval(fetchEntities, 30000);
  setInterval(fetchPresence, 6000);
}

// returning visitor: skip entry screen
if (interest) {
  document.getElementById('entry').style.display = 'none';
  setTimeout(() => document.getElementById('hint').classList.add('hidden'), 6000);
  const _wii = document.getElementById('wander-input');
  if (_wii && interest) _wii.placeholder = interest.slice(0, 40);
  startSSE();
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

/// GET /field/stream — SSEストリーム
/// 接続中はspatial_vecが散策アルゴリズムで動き続ける
/// 空間イベント (emergence, convergence, encounter) もここに届く
async fn stream_field(
    State(state): State<Arc<AppState>>,
    Query(q): Query<FieldQuery>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let user_id = q.user_id.unwrap_or_else(Uuid::new_v4);

    let interest_vec = q.user_id
        .and_then(|uid| Identity::load(&uid).ok())
        .map(|i| i.interest_vec.clone())
        .unwrap_or_else(|| fallback_interest(&q.interest));

    // spatial_vec: stored interest_vecから出発し、散策で動く
    let spatial_vec = interest_vec.clone();

    {
        let mut users = state.connected_users.lock().unwrap();
        users.insert(user_id, UserPresence {
            id:            user_id,
            name:          q.name.clone().unwrap_or_default(),
            interest_vec:  interest_vec.clone(),
            spatial_vec,
            moving_toward: None,
            position:      "plaza".to_string(),
            last_seen:     Utc::now(),
        });
        tracing::info!("user {} connected ({} total)", user_id, users.len());
    }

    let state_c = Arc::clone(&state);
    let q_c     = q.clone();
    let mut event_rx = state.event_tx.subscribe();
    let mut prev_near_ids: HashSet<Uuid> = HashSet::new();

    let stream = IntervalStream::new(tokio::time::interval(Duration::from_secs(2)))
        .flat_map(move |_| {
            // last_seen 更新
            {
                let mut users = state_c.connected_users.lock().unwrap();
                if let Some(u) = users.get_mut(&user_id) {
                    u.last_seen = Utc::now();
                }
            }

            let field = compute_field_state(&state_c, &q_c);

            // ── 散策アルゴリズム ──────────────────────────────────────────────
            // position(t+1) = position(t) + flow(drift) + interaction(near)
            // spatial_vecをnearとdriftの方向へゆっくり引っ張る
            // これが「移動」。テレポートではなく連続的な漂流。
            {
                // nearエンティティのembeddingを取得
                let near_embs: Vec<Vec<f32>> = {
                    let graph = state_c.graph.lock().unwrap();
                    field.near.iter()
                        .filter(|e| !e.label.starts_with("wanderer_") && !e.moving_toward.is_some())
                        .filter_map(|e| {
                            graph.graph.node_indices()
                                .find(|&i| graph.graph[i].label == e.label)
                                .and_then(|i| graph.graph[i].embedding.clone())
                        })
                        .collect()
                };
                // driftエンティティのembeddingを取得
                let drift_embs: Vec<Vec<f32>> = {
                    let graph = state_c.graph.lock().unwrap();
                    field.drift.iter()
                        .filter_map(|d| {
                            graph.graph.node_indices()
                                .find(|&i| graph.graph[i].label == d.toward)
                                .and_then(|i| graph.graph[i].embedding.clone())
                        })
                        .collect()
                };

                let mut users = state_c.connected_users.lock().unwrap();
                if let Some(user) = users.get_mut(&user_id) {
                    let dim = user.spatial_vec.len();
                    if dim > 0 {
                        // near引力: alpha=0.003 (非常にゆっくり)
                        if !near_embs.is_empty() {
                            let mut avg = vec![0.0f32; dim];
                            for emb in &near_embs {
                                if emb.len() == dim {
                                    for (a, v) in avg.iter_mut().zip(emb.iter()) { *a += v; }
                                }
                            }
                            avg.iter_mut().for_each(|v| *v /= near_embs.len() as f32);
                            let alpha = 0.003f32;
                            for (s, a) in user.spatial_vec.iter_mut().zip(avg.iter()) {
                                *s = *s * (1.0 - alpha) + a * alpha;
                            }
                        }
                        // drift引力: alpha=0.002
                        if !drift_embs.is_empty() {
                            let mut avg = vec![0.0f32; dim];
                            for emb in &drift_embs {
                                if emb.len() == dim {
                                    for (a, v) in avg.iter_mut().zip(emb.iter()) { *a += v; }
                                }
                            }
                            avg.iter_mut().for_each(|v| *v /= drift_embs.len() as f32);
                            let alpha = 0.002f32;
                            for (s, a) in user.spatial_vec.iter_mut().zip(avg.iter()) {
                                *s = *s * (1.0 - alpha) + a * alpha;
                            }
                        }
                        // 正規化 (単位ベクトルを維持)
                        let norm: f32 = user.spatial_vec.iter().map(|v| v * v).sum::<f32>().sqrt();
                        if norm > 0.0 {
                            user.spatial_vec.iter_mut().for_each(|v| *v /= norm);
                        }
                    }
                    // moving_toward: driftの最上位エンティティが「向かっている方向」
                    user.moving_toward = field.drift.first().map(|d| d.toward.clone());
                    user.position = field.position.clone();
                }
            }

            // ── 空間イベントの収集 ────────────────────────────────────────────
            let mut items: Vec<Result<Event, Infallible>> = Vec::new();

            // broadcastチャンネルから溜まっているイベントを全部受け取る
            loop {
                match event_rx.try_recv() {
                    Ok(evt) => {
                        if let Ok(json) = serde_json::to_string(&evt) {
                            items.push(Ok(Event::default().event("space").data(json)));
                        }
                    }
                    Err(_) => break,
                }
            }

            // ── encounter検知 ─────────────────────────────────────────────────
            // 別のwandererが自分のnear zoneに入ってきた瞬間を検知する
            let current_near_ids: HashSet<Uuid> = field.near.iter()
                .filter(|e| {
                    // wanderer_* または named wanderer (moving_towardを持つもの)
                    e.label.starts_with("wanderer_") || e.moving_toward.is_some()
                })
                .map(|e| e.id)
                .collect();

            for &id in &current_near_ids {
                if !prev_near_ids.contains(&id) {
                    // 新しいwandererがnearに現れた
                    if let Some(e) = field.near.iter().find(|e| e.id == id) {
                        let evt = SpaceEvent {
                            kind:   "encounter".to_string(),
                            label:  e.label.clone(),
                            detail: e.moving_toward.clone(),
                        };
                        if let Ok(json) = serde_json::to_string(&evt) {
                            items.push(Ok(Event::default().event("space").data(json)));
                        }
                    }
                }
            }
            prev_near_ids = current_near_ids;

            // field イベント (常に末尾)
            let json = serde_json::to_string(&field).unwrap_or_default();
            items.push(Ok(Event::default().event("field").data(json)));

            tokio_stream::iter(items)
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

/// GET /ambient.js — どのウェブサイトにも埋め込める ambient layer
/// <script src="https://space.gold3112.online/ambient.js"></script>
/// ユーザーは何も設定しない。空間がそこにある。HTTPSのように。
async fn ambient_js() -> impl axum::response::IntoResponse {
    let js = r#"/* Golden Protocol — ambient layer
 * https://space.gold3112.online/ambient.js
 * Place this script anywhere. Users configure nothing.
 * The space is simply there. */
(function(){
  var SERVER='https://space.gold3112.online';
  var KEY='gp_id';
  function getId(){try{var s=localStorage.getItem(KEY);if(s)return JSON.parse(s).id;}catch(_){}return null;}
  function saveId(id){try{localStorage.setItem(KEY,JSON.stringify({id:id}));}catch(_){}}
  function context(){
    var t=document.title||'';
    var m=document.querySelector('meta[name="description"]');
    var d=m?m.getAttribute('content')||'':'';
    return encodeURIComponent((t+' '+d).slice(0,200));
  }
  function absorb(id){
    var url=SERVER+'/field?passive=true&user_id='+id+'&interest='+context();
    if(typeof fetch!=='undefined'){
      fetch(url,{method:'GET',keepalive:true}).catch(function(){});
    }
  }
  function init(){
    var id=getId();
    if(id){absorb(id);return;}
    fetch(SERVER+'/identity/new?interest=curiosity+exploration+discovery')
      .then(function(r){return r.json();})
      .then(function(d){saveId(d.id);absorb(d.id);})
      .catch(function(){});
  }
  if(document.readyState==='loading'){
    document.addEventListener('DOMContentLoaded',init);
  }else{
    setTimeout(init,0);
  }
})();
"#;
    (
        [
            ("Content-Type", "application/javascript; charset=utf-8"),
            ("Cache-Control", "public, max-age=3600"),
            ("Access-Control-Allow-Origin", "*"),
        ],
        js,
    )
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

    let (event_tx, _) = tokio::sync::broadcast::channel::<SpaceEvent>(64);
    let state = Arc::new(AppState {
        graph:           Mutex::new(space),
        weights:         DistanceWeights::default(),
        activity:        Mutex::new(HashMap::new()),
        connected_users: Mutex::new(HashMap::new()),
        rate_limiter:    IpLimiter::keyed(quota),
        node_id,
        peers:           Mutex::new(saved_peers),
        event_tx,
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
                    let _ = state_c.event_tx.send(SpaceEvent {
                        kind:   "emergence".to_string(),
                        label:  new_label.clone(),
                        detail: Some(format!("{} converging", cluster.len())),
                    });
                    tracing::info!(
                        "emergence: '{}' spawned from {} converging users",
                        new_label, cluster.len()
                    );
                }
            }
        });
    }

    // Convergence検知タスク (60秒ごと)
    // 複数のSSE接続ユーザーが同じエンティティのnear圏にいる = 場の収束
    {
        let state_c = Arc::clone(&state);
        let tx_c    = state.event_tx.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(60));
            let mut prev_convergences: HashSet<String> = HashSet::new();
            loop {
                interval.tick().await;

                let users: Vec<Vec<f32>> = {
                    let u = state_c.connected_users.lock().unwrap();
                    u.values().map(|u| u.spatial_vec.clone()).collect()
                };
                if users.len() < 2 { prev_convergences.clear(); continue; }

                let entities: Vec<(String, Vec<f32>)> = {
                    let graph = state_c.graph.lock().unwrap();
                    graph.graph.node_indices()
                        .filter_map(|i| {
                            let e = &graph.graph[i];
                            e.embedding.as_ref().map(|emb| (e.label.clone(), emb.clone()))
                        })
                        .collect()
                };

                let mut current_convergences: HashSet<String> = HashSet::new();

                for (label, emb) in &entities {
                    let count = users.iter()
                        .filter(|sv| {
                            if sv.len() != emb.len() { return false; }
                            let sim = cosine_sim(sv, emb);
                            (1.0 - sim) < 0.30 // near閾値相当
                        })
                        .count();

                    if count >= 2 {
                        current_convergences.insert(label.clone());
                        // 新しいconvergenceだけ通知 (毎回送らない)
                        if !prev_convergences.contains(label) {
                            let _ = tx_c.send(SpaceEvent {
                                kind:   "convergence".to_string(),
                                label:  label.clone(),
                                detail: Some(format!("{} present", count)),
                            });
                            tracing::info!("convergence: {} users near '{}'", count, label);
                        }
                    }
                }
                prev_convergences = current_convergences;
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
        .route("/ambient.js",    get(ambient_js))
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
