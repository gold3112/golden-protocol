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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_space_graph_basic() {
        let mut sg = SpaceGraph::new();
        let e1 = Entity::new(EntityKind::Human, "Alice");
        let e2 = Entity::new(EntityKind::AI, "Bot");
        let id1 = e1.id;
        let id2 = e2.id;

        sg.add_entity(e1);
        sg.add_entity(e2);

        assert!(sg.get_entity(&id1).is_some());
        assert_eq!(sg.get_entity(&id1).unwrap().label, "Alice");

        // 初期状態では繋がっていない
        assert_eq!(sg.shortest_path(id1, id2), None);

        // 関係を追加
        sg.add_relation(id1, id2, 0.8);
        assert_eq!(sg.shortest_path(id1, id2), Some(1));
    }

    #[test]
    fn test_shortest_path_multi_step() {
        let mut sg = SpaceGraph::new();
        let a = Entity::new(EntityKind::Human, "A");
        let b = Entity::new(EntityKind::Human, "B");
        let c = Entity::new(EntityKind::Human, "C");
        let id_a = a.id;
        let id_b = b.id;
        let id_c = c.id;

        sg.add_entity(a);
        sg.add_entity(b);
        sg.add_entity(c);

        sg.add_relation(id_a, id_b, 1.0);
        sg.add_relation(id_b, id_c, 1.0);

        assert_eq!(sg.shortest_path(id_a, id_c), Some(2));
    }
}
