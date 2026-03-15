use serde::{Deserialize, Serialize};
use uuid::Uuid;
use chrono::{DateTime, Utc};
use petgraph::graph::{DiGraph, NodeIndex};
use std::collections::HashMap;

/// 存在の種別 — 人・AI・サービス・イベントすべて同じレイヤー
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum EntityKind {
    Human,
    AI,
    Service,
    Stream,
    Event,
    Data,
}

/// 存在 = 反応可能なもの
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entity {
    pub id: Uuid,
    pub kind: EntityKind,
    pub label: String,
    pub embedding: Option<Vec<f32>>,       // 意味ベクトル
    pub activity_vec: Option<Vec<f32>>,    // 活動ベクトル
    pub last_active: DateTime<Utc>,
}

impl Entity {
    pub fn new(kind: EntityKind, label: &str) -> Self {
        Self {
            id: Uuid::new_v4(),
            kind,
            label: label.to_string(),
            embedding: None,
            activity_vec: None,
            last_active: Utc::now(),
        }
    }
}

/// 関係の種別
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Relation {
    pub weight: f32,   // 関係の強さ [0,1]
}

/// 空間グラフ — 動的グラフ: node=存在, edge=関係
pub struct SpaceGraph {
    pub graph: DiGraph<Entity, Relation>,
    pub index_map: HashMap<Uuid, NodeIndex>,
}

impl SpaceGraph {
    pub fn new() -> Self {
        Self {
            graph: DiGraph::new(),
            index_map: HashMap::new(),
        }
    }

    pub fn add_entity(&mut self, entity: Entity) -> NodeIndex {
        let id = entity.id;
        let idx = self.graph.add_node(entity);
        self.index_map.insert(id, idx);
        idx
    }

    pub fn add_relation(&mut self, from: Uuid, to: Uuid, weight: f32) {
        if let (Some(&a), Some(&b)) = (self.index_map.get(&from), self.index_map.get(&to)) {
            self.graph.add_edge(a, b, Relation { weight });
        }
    }

    pub fn get_entity(&self, id: &Uuid) -> Option<&Entity> {
        self.index_map.get(id).map(|&idx| &self.graph[idx])
    }

    pub fn shortest_path(&self, from: Uuid, to: Uuid) -> Option<usize> {
        use petgraph::algo::astar;
        let start = *self.index_map.get(&from)?;
        let goal = *self.index_map.get(&to)?;
        astar(&self.graph, start, |n| n == goal, |_| 1, |_| 0)
            .map(|(cost, _)| cost)
    }
}
