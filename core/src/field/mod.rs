use serde::{Deserialize, Serialize};
use uuid::Uuid;
use chrono::{DateTime, Utc};
use crate::distance::Visibility;
use crate::identity::MessageRecord;

/// drift signal — 空間の流れが向かう先
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DriftSignal {
    pub toward:   String,   // 引き寄せられている存在のラベル
    pub strength: f32,      // 引力の強さ [0,1]
}

/// 場にいる存在の観測情報
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObservedEntity {
    pub id: Uuid,
    pub label: String,
    pub distance: f32,
    pub visibility: Visibility,
    /// 距離の5成分 [semantic, relational, activity, temporal, attention]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub components: Option<[f32; 5]>,
    /// 今向かっている方向のエンティティラベル (wandererのみ)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub moving_toward: Option<String>,
}

impl Default for ObservedEntity {
    fn default() -> Self {
        Self {
            id: Uuid::new_v4(),
            label: String::new(),
            distance: 1.0,
            visibility: Visibility::Beyond,
            components: None,
            moving_toward: None,
        }
    }
}

/// サーバーが返す「場の状態」— ページではなく状態
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FieldState {
    /// あなたが今いる場所 (座標ではなく文脈)
    pub position: String,

    /// この場所の情報密度 [0,1]
    pub density: f32,

    /// 今ここにいる気配の数
    pub presence: usize,

    /// 見える存在 (near)
    pub near: Vec<ObservedEntity>,

    /// 気配はあるが未解像の存在 (horizon) — 奥行きの実装
    pub horizon: Vec<ObservedEntity>,

    /// 空間の流れ — 活動が高まっている方向への引力
    pub drift: Vec<DriftSignal>,

    /// 場の時刻
    pub timestamp: DateTime<Utc>,

    /// 動的閾値 [near_threshold, horizon_threshold]
    pub thresholds: [f32; 2],

    /// near にいる entity に残された最近のメッセージ
    pub messages: Vec<MessageRecord>,
}

impl FieldState {
    pub fn new(position: &str) -> Self {
        Self {
            position: position.to_string(),
            density: 0.0,
            presence: 0,
            near: vec![],
            horizon: vec![],
            drift: Vec::new(),
            timestamp: Utc::now(),
            thresholds: [0.35, 0.65],
            messages: vec![],
        }
    }

    /// nearとhorizonからdensityを計算
    pub fn compute_density(&mut self) {
        let total = (self.near.len() + self.horizon.len()) as f32;
        if total == 0.0 {
            self.density = 0.0;
            return;
        }
        // nearは重み1.0、horizonは重み0.4 (遠いので薄い)
        let weighted = self.near.len() as f32 * 1.0
                     + self.horizon.len() as f32 * 0.4;
        self.density = (weighted / (total + 10.0)).clamp(0.0, 1.0);
    }
}
