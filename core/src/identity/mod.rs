use anyhow::Result;
use chrono::{DateTime, Utc};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use std::sync::{Mutex, OnceLock};
use uuid::Uuid;

// graph モジュールの Entity/EntityKind を参照
use crate::graph::{Entity, EntityKind};

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
        CREATE TABLE IF NOT EXISTS entities (
            id           TEXT PRIMARY KEY,
            label        TEXT NOT NULL UNIQUE,
            kind         TEXT NOT NULL,
            embedding    TEXT NOT NULL,
            activity_vec TEXT,
            last_active  TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS rss_feeds (
            url       TEXT PRIMARY KEY,
            max_items INTEGER NOT NULL DEFAULT 20,
            added_at  TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS peers (
            url       TEXT PRIMARY KEY,
            node_id   TEXT NOT NULL,
            last_seen TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS node_config (
            key   TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS messages (
            id           TEXT PRIMARY KEY,
            entity_label TEXT NOT NULL,
            text         TEXT NOT NULL,
            author       TEXT NOT NULL DEFAULT 'wanderer',
            created_at   TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS messages_label ON messages(entity_label);
        CREATE INDEX IF NOT EXISTS messages_time  ON messages(created_at);
    ")?;
    DB.set(Mutex::new(conn)).ok();
    // migration: add expires_at column if not present (ignore error if already exists)
    let _ = DB.get().expect("DB not initialized").lock().unwrap()
        .execute("ALTER TABLE entities ADD COLUMN expires_at TEXT", []);
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

// --- エンティティ永続化 ---

pub fn save_entity(entity: &Entity) -> Result<()> {
    let emb  = serde_json::to_string(&entity.embedding)?;
    let act  = entity.activity_vec.as_ref().map(|v| serde_json::to_string(v)).transpose()?;
    let kind = format!("{:?}", entity.kind);
    let exp  = entity.expires_at.map(|t| t.to_rfc3339());
    db().execute(
        "INSERT OR REPLACE INTO entities (id, label, kind, embedding, activity_vec, last_active, expires_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            entity.id.to_string(),
            entity.label,
            kind,
            emb,
            act,
            entity.last_active.to_rfc3339(),
            exp,
        ],
    )?;
    Ok(())
}

pub fn delete_entity_db(id: &Uuid) -> Result<()> {
    db().execute("DELETE FROM entities WHERE id = ?1", params![id.to_string()])?;
    Ok(())
}

pub fn load_all_entities() -> Vec<Entity> {
    let conn = db();
    let mut stmt = match conn.prepare(
        "SELECT id, label, kind, embedding, activity_vec, last_active, expires_at FROM entities"
    ) {
        Ok(s) => s,
        Err(_) => return vec![],
    };

    let rows = match stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, Option<String>>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, Option<String>>(6)?,
        ))
    }) {
        Ok(r) => r,
        Err(_) => return vec![],
    };
    rows.filter_map(|r| r.ok())
    .filter_map(|(id, label, kind, emb, act, last_active, exp)| {
        let id  = id.parse::<Uuid>().ok()?;
        let kind = match kind.as_str() {
            "Human"   => EntityKind::Human,
            "AI"      => EntityKind::AI,
            "Event"   => EntityKind::Event,
            _         => EntityKind::Service, // migration: Stream/Data → Service
        };
        let embedding:    Option<Vec<f32>> = serde_json::from_str(&emb).ok();
        let activity_vec: Option<Vec<f32>> = act.and_then(|a| serde_json::from_str(&a).ok());
        let last_active = last_active.parse::<DateTime<Utc>>().unwrap_or_else(|_| Utc::now());
        let expires_at: Option<DateTime<Utc>> = exp.and_then(|s| s.parse().ok());

        Some(Entity { id, kind, label, embedding, activity_vec, last_active, expires_at })
    })
    .collect()
}

// --- ピア管理 ---

pub fn save_peer(url: &str, node_id: Uuid) -> Result<()> {
    db().execute(
        "INSERT OR REPLACE INTO peers (url, node_id, last_seen) VALUES (?1, ?2, ?3)",
        params![url, node_id.to_string(), Utc::now().to_rfc3339()],
    )?;
    Ok(())
}

pub fn load_peers_from_db() -> Vec<(String, Uuid)> {
    let conn = db();
    let mut stmt = match conn.prepare("SELECT url, node_id FROM peers") {
        Ok(s) => s,
        Err(_) => return vec![],
    };
    let rows = match stmt.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))) {
        Ok(r) => r,
        Err(_) => return vec![],
    };
    rows.filter_map(|r| r.ok())
        .filter_map(|(url, id)| id.parse::<Uuid>().ok().map(|uid| (url, uid)))
        .collect()
}

pub fn prune_stale_peers(hours: i64) -> Result<usize> {
    let cutoff = (Utc::now() - chrono::Duration::hours(hours)).to_rfc3339();
    let n = db().execute("DELETE FROM peers WHERE last_seen < ?1", params![cutoff])?;
    Ok(n)
}

// --- ノード自身のID管理 ---

// --- メッセージ ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageRecord {
    pub id:           Uuid,
    pub entity_label: String,
    pub text:         String,
    pub author:       String,
    pub created_at:   DateTime<Utc>,
}

pub fn save_message(entity_label: &str, text: &str, author: &str) -> Result<()> {
    let id = Uuid::new_v4();
    db().execute(
        "INSERT INTO messages (id, entity_label, text, author, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![id.to_string(), entity_label, text, author, Utc::now().to_rfc3339()],
    )?;
    Ok(())
}

pub fn load_messages_for_labels(labels: &[String], per_label: usize) -> Vec<MessageRecord> {
    if labels.is_empty() { return vec![]; }
    let conn = db();
    let mut results = vec![];
    for label in labels {
        let mut stmt = match conn.prepare(
            "SELECT id, entity_label, text, author, created_at FROM messages \
             WHERE entity_label = ?1 ORDER BY created_at DESC LIMIT ?2"
        ) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let rows = match stmt.query_map(params![label, per_label as i64], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        }) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for (id, entity_label, text, author, created_at) in rows.flatten() {
            let id          = id.parse::<Uuid>().unwrap_or_else(|_| Uuid::new_v4());
            let created_at  = created_at.parse::<DateTime<Utc>>().unwrap_or_else(|_| Utc::now());
            results.push(MessageRecord { id, entity_label, text, author, created_at });
        }
    }
    // 時系列昇順で返す (古い→新しい)
    results.sort_by(|a, b| a.created_at.cmp(&b.created_at));
    results
}

pub fn cleanup_old_messages(hours: i64) -> Result<usize> {
    let cutoff = (Utc::now() - chrono::Duration::hours(hours)).to_rfc3339();
    let n = db().execute("DELETE FROM messages WHERE created_at < ?1", params![cutoff])?;
    Ok(n)
}

pub fn get_or_create_node_id() -> Uuid {
    let conn = db();
    if let Ok(id) = conn.query_row(
        "SELECT value FROM node_config WHERE key = 'node_id'",
        [],
        |row| row.get::<_, String>(0),
    ) {
        return id.parse().unwrap_or_else(|_| Uuid::new_v4());
    }
    let id = Uuid::new_v4();
    let _ = conn.execute(
        "INSERT INTO node_config (key, value) VALUES ('node_id', ?1)",
        params![id.to_string()],
    );
    id
}
