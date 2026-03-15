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
use distance::{
    compute_distance, relational_dist, semantic_dist, temporal_dist,
    DistanceWeights, DynamicThresholds,
};
use field::{FieldState, ObservedEntity};
use graph::{Entity, EntityKind, SpaceGraph};
use identity::Identity;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use uuid::Uuid;

struct AppState {
    graph:   Mutex<SpaceGraph>,
    weights: DistanceWeights,
}

// --- クエリ / リクエスト型 ---

#[derive(Deserialize)]
struct FieldQuery {
    user_id:     Option<Uuid>,
    interest:    Option<String>,
    near_pct:    Option<f32>,
    horizon_pct: Option<f32>,
}

#[derive(Deserialize)]
struct EncounterRequest {
    user_id:      Uuid,
    position:     String,
    near_labels:  Vec<String>,
    interest_text: String,
}

#[derive(Serialize)]
struct IdentitySummary {
    id:               Uuid,
    created_at:       String,
    last_seen:        String,
    position:         String,
    encounter_count:  usize,
    interest_preview: Vec<f32>, // 最初の8次元だけ表示
}

// --- ハンドラ ---

/// GET /field — 場の状態を返す
async fn get_field(
    State(state): State<Arc<AppState>>,
    Query(q): Query<FieldQuery>,
) -> Json<FieldState> {
    let graph    = state.graph.lock().unwrap();
    let weights  = &state.weights;
    let near_pct    = q.near_pct.unwrap_or(0.30);
    let horizon_pct = q.horizon_pct.unwrap_or(0.70);

    // identity が指定されていればそのベクトルを使う
    let user_interest = if let Some(uid) = q.user_id {
        match Identity::load(&uid) {
            Ok(identity) => identity.interest_vec,
            Err(_) => fallback_interest(&q.interest),
        }
    } else {
        fallback_interest(&q.interest)
    };

    let tau = 86400.0_f64;
    let mut field = FieldState::new("plaza");

    let mut scored: Vec<(ObservedEntity, f32, Option<Vec<f32>>)> = graph.graph
        .node_indices()
        .map(|idx| {
            let entity = &graph.graph[idx];

            let sem = entity.embedding.as_deref()
                .map(|emb| semantic_dist(emb, &user_interest))
                .unwrap_or(0.5);
            let rel = relational_dist(None);
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
            let emb_clone = entity.embedding.clone();
            (
                ObservedEntity {
                    id: entity.id,
                    label: entity.label.clone(),
                    distance: d,
                    visibility: Default::default(),
                },
                d,
                emb_clone,
            )
        })
        .collect();

    let all_distances: Vec<f32> = scored.iter().map(|(_, d, _)| *d).collect();
    let thresholds = DynamicThresholds::from_distances(&all_distances, near_pct, horizon_pct);
    field.thresholds = [thresholds.near, thresholds.horizon];

    for (obs, d, _) in &scored {
        let mut obs = obs.clone();
        obs.visibility = thresholds.classify(*d);
        match obs.visibility {
            distance::Visibility::Near    => field.near.push(obs),
            distance::Visibility::Horizon => field.horizon.push(obs),
            distance::Visibility::Beyond  => {}
        }
    }

    field.near.sort_by(|a, b| a.distance.partial_cmp(&b.distance).unwrap());
    field.horizon.sort_by(|a, b| a.distance.partial_cmp(&b.distance).unwrap());
    field.presence = field.near.len() + field.horizon.len();
    field.compute_density();

    Json(field)
}

/// POST /identity/new — 新しいidentityを作成
async fn new_identity(
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Json<IdentitySummary> {
    let seed_text = q.get("interest")
        .map(|s| s.as_str())
        .unwrap_or("curiosity exploration knowledge");

    let seed_vec = embedding::embed(seed_text)
        .unwrap_or_else(|_| vec![0.0; 384]);

    let identity = Identity::new(seed_vec);
    identity.save().expect("failed to save identity");

    Json(to_summary(&identity))
}

/// GET /identity/:id — identity状態を取得
async fn get_identity(
    Path(id): Path<Uuid>,
) -> Result<Json<IdentitySummary>, String> {
    Identity::load(&id)
        .map(|i| Json(to_summary(&i)))
        .map_err(|_| format!("identity {} not found", id))
}

/// POST /encounter — encounter記録 → identity更新
async fn post_encounter(
    State(state): State<Arc<AppState>>,
    Json(req): Json<EncounterRequest>,
) -> Result<Json<IdentitySummary>, String> {
    let mut identity = Identity::load(&req.user_id)
        .map_err(|_| format!("identity {} not found", req.user_id))?;

    // nearラベルに対応するembeddingをグラフから取得
    let graph = state.graph.lock().unwrap();
    let near_embeddings: Vec<Vec<f32>> = graph.graph
        .node_indices()
        .filter_map(|idx| {
            let e = &graph.graph[idx];
            if req.near_labels.contains(&e.label) {
                e.embedding.clone()
            } else {
                None
            }
        })
        .collect();

    identity.encounter(
        &req.position,
        req.near_labels,
        near_embeddings,
        req.interest_text,
        0.85, // alpha: ゆっくり変化
    );

    identity.save().map_err(|e| e.to_string())?;
    Ok(Json(to_summary(&identity)))
}

// --- ユーティリティ ---

fn fallback_interest(interest: &Option<String>) -> Vec<f32> {
    let text = interest.as_deref()
        .unwrap_or("curiosity exploration knowledge");
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
        graph:   Mutex::new(space),
        weights: DistanceWeights::default(),
    });

    let app = Router::new()
        .route("/field",           get(get_field))
        .route("/identity/new",    get(new_identity))
        .route("/identity/:id",    get(get_identity))
        .route("/encounter",       post(post_encounter))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("0.0.0.0:7331").await.unwrap();
    println!("golden_core listening on :7331");
    axum::serve(listener, app).await.unwrap();
}
