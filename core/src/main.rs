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
    near_pct:    Option<f32>,
    horizon_pct: Option<f32>,
    passive:     Option<bool>,
    name:        Option<String>,
    format:      Option<String>,
}

#[derive(Deserialize)]
struct ArriveRequest {
    identity: Option<Uuid>,
    name:     Option<String>,
}

#[derive(Serialize)]
struct ArriveResponse {
    identity: Uuid,
    field:    FieldState,
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
            .unwrap_or_else(|| space_centroid(state))
    };

    let tau = 14400.0_f64;  // 4時間 — RSSコンテンツ(毎時更新)に適切な分散

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
    // activity の最大値を先に取得（正規化用）
    let max_activity_count = activity.values().copied().max().unwrap_or(1) as f32;

    let mut scored: Vec<(ObservedEntity, f32, Option<Vec<f32>>)> = graph.graph
        .node_indices()
        .filter(|idx| {
            let e = &graph.graph[*idx];
            e.expires_at.map_or(true, |exp| exp > Utc::now())
        })
        .map(|idx| {
            let entity = &graph.graph[idx];

            // semantic: エンティティ埋め込み ↔ ユーザー関心
            let sem = entity.embedding.as_deref()
                .map(|emb| semantic_dist(emb, &user_interest))
                .unwrap_or(0.5);

            // relational: グラフ接続次数
            // エッジなし = 不明(0.5中立)、接続あり = 近くなる
            let degree = graph.graph.edges(idx).count();
            let rel = if degree == 0 {
                0.5
            } else {
                1.0 - (degree as f32 / 4.0_f32).min(1.0)
            };

            // activity: セッション内アクセス数（semanticと無相関）
            // 未訪問 = 0.5(中立)、よくアクセスされるほど0に近づく
            let act_count = activity.get(&entity.label).copied().unwrap_or(0);
            let act = if act_count == 0 {
                0.5
            } else {
                1.0 - (act_count as f32 / (max_activity_count + 1.0)).min(1.0)
            };

            let tmp = temporal_dist(entity, tau);

            // attention: このユーザー個人の履歴（訪問経験）
            // 未訪問 = 0.5（中立）、訪問済み = 経験に応じて近くなる
            let att = identity.as_ref()
                .map(|id| id.relational_dist(&entity.label))
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

static ABOUT_HTML: &str = include_str!("../static/about.html");

static LANDING_HTML: &str = include_str!("../static/landing.html");

/// GET /privacy-policy
async fn privacy_policy() -> Html<&'static str> {
    Html(PRIVACY_HTML)
}

static PRIVACY_HTML: &str = include_str!("../static/privacy.html");

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
        .unwrap_or_else(|| space_centroid(&state));

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
/// POST /arrive — 空間への唯一の入口。
/// identity があれば継続、なければ新規作成。
/// 入力は「存在」だけ。interest もクエリも不要。
async fn arrive(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ArriveRequest>,
) -> Json<ArriveResponse> {
    let (uid, interest_vec) = if let Some(id) = req.identity {
        match Identity::load(&id) {
            Ok(identity) => (identity.id, identity.interest_vec.clone()),
            Err(_) => {
                // 未知UUID → 新規identity (centroidから出発)
                let centroid = space_centroid(&state);
                let identity = Identity::new(centroid.clone());
                let _ = identity.save();
                (identity.id, centroid)
            }
        }
    } else {
        // 初回到着 → 空間の重心から出発
        let centroid = space_centroid(&state);
        let identity = Identity::new(centroid.clone());
        let uid = identity.id;
        let _ = identity.save();
        (uid, centroid)
    };

    // presence に登録 (SSE接続前でも空間にいることを示す)
    {
        let mut users = state.connected_users.lock().unwrap();
        users.entry(uid).or_insert_with(|| UserPresence {
            id:           uid,
            name:         req.name.clone().unwrap_or_default(),
            interest_vec: interest_vec.clone(),
            spatial_vec:  interest_vec.clone(),
            moving_toward: None,
            position:     "plaza".to_string(),
            last_seen:    Utc::now(),
        });
    }

    let q = FieldQuery {
        user_id:     Some(uid),
        near_pct:    None,
        horizon_pct: None,
        passive:     None,
        name:        req.name.clone(),
        format:      None,
    };
    let field = compute_field_state(&state, &q);
    Json(ArriveResponse { identity: uid, field })
}

async fn new_identity() -> Json<IdentitySummary> {
    // legacy endpoint — /arrive を使うべき
    let identity = Identity::new(vec![0.0; 384]);
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
    let kind = req.kind.unwrap_or(EntityKind::Service);

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

// --- ネットワークユーティリティ (Federation 用) ---

fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap_or_default()
}

fn validate_external_url(url: &str) -> Result<(), String> {
    let parsed = url.parse::<reqwest::Url>()
        .map_err(|_| "invalid URL".to_string())?;
    match parsed.scheme() {
        "http" | "https" => {}
        s => return Err(format!("scheme '{}' not allowed", s)),
    }
    let host = parsed.host_str().unwrap_or("").to_lowercase();
    if host == "localhost" || host == "ip6-localhost" {
        return Err("internal host not allowed".to_string());
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        let blocked = match ip {
            IpAddr::V4(v4) => v4.is_loopback() || v4.is_private() || v4.is_link_local() || v4.is_unspecified(),
            IpAddr::V6(v6) => v6.is_loopback() || v6.is_unspecified(),
        };
        if blocked { return Err("private/internal IP not allowed".to_string()); }
    }
    Ok(())
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
    let js = include_str!("../static/ambient.js");
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

/// 空間の重心 — 全エンティティ embedding の平均・正規化ベクトル。
/// 新規到着者が「どこでもない場所」ではなく空間の中心から始まるようにする。
fn space_centroid(state: &AppState) -> Vec<f32> {
    let graph = state.graph.lock().unwrap();
    let embeddings: Vec<&Vec<f32>> = graph.graph.node_indices()
        .filter_map(|idx| graph.graph[idx].embedding.as_ref())
        .collect();
    if embeddings.is_empty() {
        return vec![0.0f32; 384];
    }
    let mut centroid = vec![0.0f32; 384];
    for emb in &embeddings {
        if emb.len() == 384 {
            for (c, e) in centroid.iter_mut().zip(emb.iter()) { *c += e; }
        }
    }
    let n = embeddings.len() as f32;
    centroid.iter_mut().for_each(|v| *v /= n);
    let norm: f32 = centroid.iter().map(|v| v * v).sum::<f32>().sqrt();
    if norm > 1e-8 { centroid.iter_mut().for_each(|v| *v /= norm); }
    centroid
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
            ("gathering",           EntityKind::Event),
            ("philosophy_debate",   EntityKind::Event),
            ("live_coding_session", EntityKind::Event),
            ("open_research",       EntityKind::Service),
            ("ambient_signal",      EntityKind::Service),
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
        .route("/arrive",        post(arrive))
        .route("/field",         get(get_field))
        .route("/field/stream",  get(stream_field))
        .route("/presence",      get(get_presence))
        .route("/entities",      get(get_entities).post(post_entity))
        .route("/entity/:id",    axum::routing::delete(delete_entity))
        .route("/identity/new",  get(new_identity))
        .route("/identity/:id",  get(get_identity))
        .route("/encounter",     post(post_encounter))
        .route("/message",        post(post_message))
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
