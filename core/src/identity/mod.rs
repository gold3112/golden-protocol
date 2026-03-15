use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use uuid::Uuid;

const STORE_DIR: &str = "/GOLDEN_PROTOCOL/identities";

/// 一回の散策で出会ったものの記録
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncounterRecord {
    pub timestamp:    DateTime<Utc>,
    pub position:     String,
    pub near_labels:  Vec<String>,  // nearにいた存在のラベル
    pub interest_text: String,      // そのときの関心テキスト
}

/// 散策者のidentity — 空間をまたいで持続する自分
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Identity {
    pub id:           Uuid,
    pub created_at:   DateTime<Utc>,
    pub last_seen:    DateTime<Utc>,

    /// 進化する関心ベクトル — 散策履歴から形成される
    pub interest_vec: Vec<f32>,

    /// 散策履歴
    pub history:      Vec<EncounterRecord>,

    /// 現在地
    pub position:     String,
}

impl Identity {
    /// 新しいidentityを作成
    pub fn new(seed_vec: Vec<f32>) -> Self {
        let now = Utc::now();
        Self {
            id:           Uuid::new_v4(),
            created_at:   now,
            last_seen:    now,
            interest_vec: seed_vec,
            history:      vec![],
            position:     "plaza".to_string(),
        }
    }

    /// 保存パス
    fn path(id: &Uuid) -> PathBuf {
        Path::new(STORE_DIR).join(format!("{}.json", id))
    }

    /// ディスクに保存
    pub fn save(&self) -> Result<()> {
        let path = Self::path(&self.id);
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(path, json)?;
        Ok(())
    }

    /// ディスクから読み込み
    pub fn load(id: &Uuid) -> Result<Self> {
        let path = Self::path(id);
        let json = std::fs::read_to_string(path)?;
        Ok(serde_json::from_str(&json)?)
    }

    /// encounter記録 → 関心ベクトルを更新
    ///
    /// alpha: 既存ベクトルの保持率 (0.85 = ゆっくり変化)
    /// nearにいた存在のembeddingを平均してblendする
    pub fn encounter(
        &mut self,
        position: &str,
        near_labels: Vec<String>,
        near_embeddings: Vec<Vec<f32>>,
        interest_text: String,
        alpha: f32,
    ) {
        // 履歴に記録
        self.history.push(EncounterRecord {
            timestamp:    Utc::now(),
            position:     position.to_string(),
            near_labels:  near_labels.clone(),
            interest_text,
        });

        // 履歴は最大200件
        if self.history.len() > 200 {
            self.history.remove(0);
        }

        // near embeddingを平均
        if !near_embeddings.is_empty() {
            let dim = self.interest_vec.len();
            let mut avg = vec![0.0f32; dim];
            let mut count = 0;

            for emb in &near_embeddings {
                if emb.len() == dim {
                    for (i, v) in emb.iter().enumerate() {
                        avg[i] += v;
                    }
                    count += 1;
                }
            }

            if count > 0 {
                avg.iter_mut().for_each(|v| *v /= count as f32);

                // blend: alpha × 既存 + (1-alpha) × 今回の出会い
                self.interest_vec = self.interest_vec.iter()
                    .zip(avg.iter())
                    .map(|(c, a)| alpha * c + (1.0 - alpha) * a)
                    .collect();
            }
        }

        self.position  = position.to_string();
        self.last_seen = Utc::now();
    }

    /// 散策回数
    pub fn encounter_count(&self) -> usize {
        self.history.len()
    }
}
