mod distance;
mod embedding;
mod field;
mod graph;
mod identity;

use axum::{
    extract::{Path, Query, State},
    response::sse::{Event, KeepAlive, Sse},
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
use uuid::Uuid;

// --- 共有状態 ---

struct AppState {
    graph:           Mutex<SpaceGraph>,
    weights:         DistanceWeights,
    /// グローバル活動カウンタ: label -> 累計encounter数 (drift計算用)
    activity:        Mutex<HashMap<String, u32>>,
    /// SSE接続中ユーザーのin-memory状態
    connected_users: Mutex<HashMap<Uuid, UserPresence>>,
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

    let mut entity    = Entity::new(kind, &req.label);
    entity.embedding  = Some(emb);
    let summary = EntitySummary {
        id:          entity.id,
        label:       entity.label.clone(),
        kind:        format!("{:?}", entity.kind),
        last_active: entity.last_active.to_rfc3339(),
    };

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

    let graph = state.graph.lock().unwrap();
    let near_embeddings: Vec<Vec<f32>> = graph.graph.node_indices()
        .filter_map(|idx| {
            let e = &graph.graph[idx];
            if req.near_labels.contains(&e.label) { e.embedding.clone() } else { None }
        })
        .collect();
    drop(graph);

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

// --- ユーティリティ ---

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

    println!("initializing embedding model...");
    embedding::init_model().expect("failed to init embedding model");
    println!("embedding model ready.");

    let mut space = SpaceGraph::new();
    let labels = vec![
        ("wandering_ai",        EntityKind::AI),
        ("knowledge_stream",    EntityKind::Stream),
        ("gathering",           EntityKind::Event),
        ("deep_archive",        EntityKind::Data),
        ("distant_signal",      EntityKind::Stream),
        ("philosophy_debate",   EntityKind::Event),
        ("music_history",       EntityKind::Data),
        ("live_coding_session", EntityKind::Event),
    ];
    let texts: Vec<&str> = labels.iter().map(|(l, _)| *l).collect();
    let embeddings = embedding::embed_batch(texts).expect("embed failed");
    for ((label, kind), emb) in labels.into_iter().zip(embeddings) {
        let mut entity = Entity::new(kind, label);
        entity.embedding = Some(emb);
        space.add_entity(entity);
    }

    let state = Arc::new(AppState {
        graph:           Mutex::new(space),
        weights:         DistanceWeights::default(),
        activity:        Mutex::new(HashMap::new()),
        connected_users: Mutex::new(HashMap::new()),
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

    let app = Router::new()
        .route("/field",         get(get_field))
        .route("/field/stream",  get(stream_field))
        .route("/presence",      get(get_presence))
        .route("/entities",      get(get_entities).post(post_entity))
        .route("/entity/:id",    axum::routing::delete(delete_entity))
        .route("/identity/new",  get(new_identity))
        .route("/identity/:id",  get(get_identity))
        .route("/encounter",     post(post_encounter))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("0.0.0.0:7331").await.unwrap();
    println!("golden_core listening on :7331");
    axum::serve(listener, app).await.unwrap();
}
