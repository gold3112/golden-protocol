use crate::graph::Entity;
use chrono::Utc;

/// distance(A, B, U) = weighted sum of 5 components
/// 全成分 [0,1] に正規化済み
#[derive(Debug, Clone)]
pub struct DistanceWeights {
    pub semantic:    f32,   // w1
    pub relational:  f32,   // w2
    pub activity:    f32,   // w3
    pub temporal:    f32,   // w4
    pub attention:   f32,   // w5
}

impl Default for DistanceWeights {
    fn default() -> Self {
        Self {
            semantic:   0.35,  // entity↔interest 意味距離（メイン）
            relational: 0.10,  // グラフ接続度（エッジ少ない環境では影響を抑える）
            activity:   0.25,  // 社会的シグナル（似た関心の人が来たか）
            temporal:   0.15,  // 鮮度
            attention:  0.15,  // 個人履歴（identity使用時のみ有効）
        }
    }
}

/// 1. 意味距離 — cosine distance between embeddings
pub fn semantic_dist(a: &[f32], b: &[f32]) -> f32 {
    1.0 - cosine_similarity(a, b)
}

/// 2. 関係距離 — 最短パスから正規化
pub fn relational_dist(shortest_path: Option<usize>) -> f32 {
    match shortest_path {
        None => 1.0,                              // 繋がっていない
        Some(0) => 0.0,                           // 同一存在
        Some(n) => 1.0 - 1.0 / (1.0 + n as f32), // 距離が遠いほど1に近づく
    }
}

/// 3. 活動距離 — 活動ベクトルのcosine distance
pub fn activity_dist(a: &[f32], b: &[f32]) -> f32 {
    1.0 - cosine_similarity(a, b)
}

/// 4. 時間距離 — 最終接触からの経過時間 (指数減衰)
/// τ = 時定数 (秒) — デフォルト1日
pub fn temporal_dist(entity: &Entity, tau_seconds: f64) -> f32 {
    let elapsed = Utc::now()
        .signed_duration_since(entity.last_active)
        .num_seconds() as f64;
    let elapsed = elapsed.max(0.0);
    (1.0 - (-elapsed / tau_seconds).exp()) as f32
}

/// 5. 注意距離 — ユーザーの関心ベクトルとtargetのembedding
pub fn attention_dist(user_interest: &[f32], target_embedding: &[f32]) -> f32 {
    1.0 - cosine_similarity(user_interest, target_embedding)
}

/// 統合距離計算
pub fn compute_distance(
    semantic:    f32,
    relational:  f32,
    activity:    f32,
    temporal:    f32,
    attention:   f32,
    weights:     &DistanceWeights,
) -> f32 {
    let d = weights.semantic   * semantic
          + weights.relational * relational
          + weights.activity   * activity
          + weights.temporal   * temporal
          + weights.attention  * attention;
    d.clamp(0.0, 1.0)
}

use serde::{Deserialize, Serialize};

/// near / horizon / beyond の判定
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub enum Visibility {
    Near,
    Horizon,
    #[default]
    Beyond,
}

/// 動的閾値 — 距離の分布から near/horizon の境界を計算
/// パーセンタイルベース: 常に意味ある near/horizon の分割を保証する
pub struct DynamicThresholds {
    pub near:    f32,   // near_pct パーセンタイル
    pub horizon: f32,   // horizon_pct パーセンタイル
}

impl DynamicThresholds {
    /// distances: 全エンティティの距離リスト
    /// near_pct:    nearに含める割合 (例: 0.3 = 下位30%)
    /// horizon_pct: horizonに含める割合 (例: 0.7 = 下位70%)
    pub fn from_distances(distances: &[f32], near_pct: f32, horizon_pct: f32) -> Self {
        if distances.is_empty() {
            return Self { near: 0.35, horizon: 0.65 };
        }
        let mut sorted = distances.to_vec();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

        let near    = percentile(&sorted, near_pct);
        let horizon = percentile(&sorted, horizon_pct);

        // nearとhorizonに最低限の幅を確保 (同じ値にならないように)
        let horizon = horizon.max(near + 0.05);

        Self { near, horizon }
    }

    pub fn classify(&self, d: f32) -> Visibility {
        if d <= self.near {
            Visibility::Near
        } else if d <= self.horizon {
            Visibility::Horizon
        } else {
            Visibility::Beyond
        }
    }
}

fn percentile(sorted: &[f32], p: f32) -> f32 {
    if sorted.is_empty() { return 0.0; }
    let idx = (p * (sorted.len() - 1) as f32).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

// --- 内部ユーティリティ ---

fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    (dot / (norm_a * norm_b)).clamp(-1.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cosine_similarity() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![1.0, 0.0, 0.0];
        assert!((cosine_similarity(&a, &b) - 1.0).abs() < 1e-6);

        let c = vec![0.0, 1.0, 0.0];
        assert!((cosine_similarity(&a, &c) - 0.0).abs() < 1e-6);

        let d = vec![-1.0, 0.0, 0.0];
        assert!((cosine_similarity(&a, &d) + 1.0).abs() < 1e-6);
    }

    #[test]
    fn test_relational_dist() {
        assert_eq!(relational_dist(None), 1.0);
        assert_eq!(relational_dist(Some(0)), 0.0);
        assert!(relational_dist(Some(1)) > 0.0);
        assert!(relational_dist(Some(10)) > relational_dist(Some(1)));
    }

    #[test]
    fn test_dynamic_thresholds() {
        let dists = vec![0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8, 0.9];
        let dt = DynamicThresholds::from_distances(&dists, 0.3, 0.7);
        
        // 0.3 percentile of [0.1...0.9] (9 elements)
        // index = round(0.3 * 8) = 2 -> 0.3
        assert!((dt.near - 0.3).abs() < 1e-6);
        // 0.7 percentile
        // index = round(0.7 * 8) = 6 -> 0.7
        assert!((dt.horizon - 0.7).abs() < 1e-6);

        assert_eq!(dt.classify(0.2), Visibility::Near);
        assert_eq!(dt.classify(0.5), Visibility::Horizon);
        assert_eq!(dt.classify(0.8), Visibility::Beyond);
    }

    #[test]
    fn test_compute_distance_clamping() {
        let weights = DistanceWeights::default();
        let d = compute_distance(2.0, 2.0, 2.0, 2.0, 2.0, &weights);
        assert!(d <= 1.0);
        let d2 = compute_distance(-1.0, -1.0, -1.0, -1.0, -1.0, &weights);
        assert!(d2 >= 0.0);
    }
}
