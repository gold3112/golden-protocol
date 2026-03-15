mod distance;
mod embedding;
mod field;
mod graph;
mod identity;

use axum::{
    extract::{Path, Query, State},
    routing::{get, post},
    Json, Router,
};
use distance::{compute_distance, semantic_dist, temporal_dist, DistanceWeights, DynamicThresholds};
use field::{DriftSignal, FieldState, ObservedEntity};
use graph::{Entity, EntityKind, SpaceGraph};
use identity::Identity;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

// --- 共有状態 ---

struct AppState {
    graph:    Mutex<SpaceGraph>,
    weights:  DistanceWeights,
    /// グローバル活動カウンタ: label -> 累計encounter数 (drift計算用)
    activity: Mutex<HashMap<String, u32>>,
}

// --- リクエスト / レスポンス型 ---

#[derive(Deserialize)]
struct FieldQuery {
    user_id:     Option<Uuid>,
    interest:    Option<String>,
    near_pct:    Option<f32>,
    horizon_pct: Option<f32>,
    /// trueのとき、nearに存在するだけで関心ベクトルが受動的に更新される
    passive:     Option<bool>,
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

// --- ハンドラ ---

/// GET /field
async fn get_field(
    State(state): State<Arc<AppState>>,
    Query(q): Query<FieldQuery>,
) -> Json<FieldState> {
    let near_pct    = q.near_pct.unwrap_or(0.30);
    let horizon_pct = q.horizon_pct.unwrap_or(0.70);
    let passive     = q.passive.unwrap_or(false);

    // identityロード
    let mut identity = q.user_id.and_then(|uid| Identity::load(&uid).ok());

    // 関心ベクトル
    let user_interest = identity.as_ref()
        .map(|i| i.interest_vec.clone())
        .unwrap_or_else(|| fallback_interest(&q.interest));

    let tau = 86400.0_f64;
    let graph    = state.graph.lock().unwrap();
    let weights  = &state.weights;
    let activity = state.activity.lock().unwrap();

    // Pass 1: 全エンティティの距離を計算
    let mut scored: Vec<(ObservedEntity, f32, Option<Vec<f32>>)> = graph.graph
        .node_indices()
        .map(|idx| {
            let entity = &graph.graph[idx];

            let sem = entity.embedding.as_deref()
                .map(|emb| semantic_dist(emb, &user_interest))
                .unwrap_or(0.5);

            // relational: identity の encounter 履歴から計算
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
            id.passive_absorb(near_embeddings, "plaza");
            let _ = id.save();
        }
    }

    Json(field)
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
        graph:    Mutex::new(space),
        weights:  DistanceWeights::default(),
        activity: Mutex::new(HashMap::new()),
    });

    let app = Router::new()
        .route("/field",        get(get_field))
        .route("/identity/new", get(new_identity))
        .route("/identity/:id", get(get_identity))
        .route("/encounter",    post(post_encounter))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("0.0.0.0:7331").await.unwrap();
    println!("golden_core listening on :7331");
    axum::serve(listener, app).await.unwrap();
}
