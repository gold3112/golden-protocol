use anyhow::Result;
use chrono::{DateTime, Utc};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use std::sync::{Mutex, OnceLock};
use uuid::Uuid;

const DB_PATH:   &str = "/GOLDEN_PROTOCOL/identities.db";
const STORE_DIR: &str = "/GOLDEN_PROTOCOL/identities"; // 移行元

static DB: OnceLock<Mutex<rusqlite::Connection>> = OnceLock::new();

// --- DB 初期化 ---

pub fn init_db() -> Result<()> {
    let conn = rusqlite::Connection::open(DB_PATH)?;
    conn.execute_batch("
        PRAGMA journal_mode=WAL;
        CREATE TABLE IF NOT EXISTS identities (
            id   TEXT PRIMARY KEY,
            data TEXT NOT NULL
        );
    ")?;
    DB.set(Mutex::new(conn)).ok();
    migrate_json_files();
    Ok(())
}

/// 既存の JSON ファイルを SQLite に移行して削除する
fn migrate_json_files() {
    let dir = std::path::Path::new(STORE_DIR);
    if !dir.exists() { return; }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut count = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") { continue; }
        let Ok(json) = std::fs::read_to_string(&path) else { continue };
        let Ok(identity) = serde_json::from_str::<Identity>(&json) else { continue };
        if identity.save().is_ok() {
            let _ = std::fs::remove_file(&path);
            count += 1;
        }
    }
    if count > 0 {
        tracing::info!("migrated {} identity files to SQLite", count);
    }
}

fn db() -> std::sync::MutexGuard<'static, rusqlite::Connection> {
    DB.get().expect("DB not initialized").lock().unwrap()
}

// --- データ型 ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncounterRecord {
    pub timestamp:     DateTime<Utc>,
    pub position:      String,
    pub near_labels:   Vec<String>,
    pub interest_text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Identity {
    pub id:           Uuid,
    pub created_at:   DateTime<Utc>,
    pub last_seen:    DateTime<Utc>,
    pub interest_vec: Vec<f32>,
    pub history:      Vec<EncounterRecord>,
    pub position:     String,
}

impl Identity {
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

    pub fn save(&self) -> Result<()> {
        let json = serde_json::to_string(self)?;
        db().execute(
            "INSERT OR REPLACE INTO identities (id, data) VALUES (?1, ?2)",
            params![self.id.to_string(), json],
        )?;
        Ok(())
    }

    pub fn load(id: &Uuid) -> Result<Self> {
        let data: String = db().query_row(
            "SELECT data FROM identities WHERE id = ?1",
            params![id.to_string()],
            |row| row.get(0),
        )?;
        Ok(serde_json::from_str(&data)?)
    }

    pub fn encounter(
        &mut self,
        position: &str,
        near_labels: Vec<String>,
        near_embeddings: Vec<Vec<f32>>,
        interest_text: String,
        alpha: f32,
    ) {
        self.history.push(EncounterRecord {
            timestamp:    Utc::now(),
            position:     position.to_string(),
            near_labels:  near_labels.clone(),
            interest_text,
        });
        if self.history.len() > 200 {
            self.history.remove(0);
        }
        if !near_embeddings.is_empty() {
            let dim = self.interest_vec.len();
            let mut avg = vec![0.0f32; dim];
            let mut count = 0;
            for emb in &near_embeddings {
                if emb.len() == dim {
                    for (i, v) in emb.iter().enumerate() { avg[i] += v; }
                    count += 1;
                }
            }
            if count > 0 {
                avg.iter_mut().for_each(|v| *v /= count as f32);
                self.interest_vec = self.interest_vec.iter()
                    .zip(avg.iter())
                    .map(|(c, a)| alpha * c + (1.0 - alpha) * a)
                    .collect();
            }
        }
        self.position  = position.to_string();
        self.last_seen = Utc::now();
    }

    pub fn encounter_count(&self) -> usize { self.history.len() }

    pub fn relational_dist(&self, label: &str) -> f32 {
        let count = self.history.iter()
            .filter(|r| r.near_labels.iter().any(|l| l == label))
            .count();
        1.0 / (1.0 + count as f32)
    }

    pub fn passive_absorb(&mut self, near_embeddings: Vec<Vec<f32>>, position: &str) {
        if near_embeddings.is_empty() { return; }
        let dim = self.interest_vec.len();
        let mut avg = vec![0.0f32; dim];
        let mut count = 0;
        for emb in &near_embeddings {
            if emb.len() == dim {
                for (i, v) in emb.iter().enumerate() { avg[i] += v; }
                count += 1;
            }
        }
        if count == 0 { return; }
        avg.iter_mut().for_each(|v| *v /= count as f32);
        self.interest_vec = self.interest_vec.iter()
            .zip(avg.iter())
            .map(|(c, a)| 0.98 * c + 0.02 * a)
            .collect();
        self.position  = position.to_string();
        self.last_seen = Utc::now();
    }
}
