# GOLDEN PROTOCOL — Architecture

## 概念

```
single logical world + distributed infrastructure
```

ユーザーから見れば「一つの空間」。実装は分散したノードの群れ。
誰も所有しない。どのノードが落ちても空間は続く。

---

## 全体構造

```
[Browser Extension]   [Web Client]   [CLI / Any Client]
      |                    |                 |
      └──────── HTTPS ──────────────────────┘
                           |
               ┌─────────────────────┐
               │   Cloudflare Tunnel  │
               └──────────┬──────────┘
                           │
        ┌──────────────────┼──────────────────┐
        │                  │                  │
   [Node A]           [Node B]           [Node C]
  golden_core        golden_core        golden_core
  :7331              :7331              :7331
        │                  │                  │
        └──────── gossip ──┴──────── gossip ──┘
                           │
                    [SQLite DB (各ノード)]
               identities / entities / peers / feeds
```

どのノードに接続しても同じ空間にいる。
エンティティはgossipで全ノードに伝播する。

---

## ノードの役割

### Bootstrap Node
現在: `space.gold3112.online`

新しいノードが参加するときに最初に接続する既知の入口。
Bootstrapが落ちても、既に互いを知っているノード同士は
直接gossipし続けるため空間は継続する。

### 通常ノード
`GOLDEN_NODE_URL` を設定して起動したノード。
Bootstrapに自己登録し、ピアリストを受け取り、gossipに参加する。

### サイレントノード
`GOLDEN_NODE_URL` 未設定。
外部からは見えないが、bootstrapからエンティティを受け取って
ローカルで空間を動かすことができる。

---

## Federation Protocol

### ノード参加フロー

```
新ノード起動
    │
    ├─ POST /peer/register → bootstrap
    │      body: { node_id, url }
    │      response: { node_id, peers: [...] }
    │
    ├─ 受け取ったピアリストを保存
    │
    └─ GET /entities/export → bootstrap
           全エンティティをgossipで取り込む
```

### Gossipサイクル (10分ごと)

```
全ピアに対して:
    GET /entities/export → エンティティ差分を取り込む
    GET /peers           → 新しいピアを発見・追加
24時間応答なし → ピアを除去
```

### エンティティの競合解決

同じラベルのエンティティが複数ノードに存在する場合:
- `last_active` が新しい方を採用
- つまり「最も最近訪れたユーザーがいるノードの状態」が勝つ

### 共有しないもの

| 種別 | 理由 |
|------|------|
| `·convergence` エンティティ | ノード固有の一時的な発生物 |
| `wanderer_*` エンティティ | そのノードのSSE接続ユーザー (presence federation は別途) |
| Identity (ユーザーID) | プライバシー。ユーザーが明示的に同期する場合のみ |

---

## エンドポイント一覧

### 空間

| Method | Path | 説明 |
|--------|------|------|
| GET | `/` | ランディングページ (Canvas可視化) |
| GET | `/field` | 場の状態を返す |
| GET | `/field/stream` | SSEストリーム (2秒ごと) |
| GET | `/presence` | 接続中ユーザー一覧 |

### エンティティ

| Method | Path | 説明 |
|--------|------|------|
| GET | `/entities` | エンティティ一覧 (サマリー) |
| POST | `/entities` | エンティティを追加 |
| DELETE | `/entity/:id` | エンティティを削除 |
| GET | `/entities/export` | 全エンティティ (embedding込み、ピア同期用) |

### Identity

| Method | Path | 説明 |
|--------|------|------|
| GET | `/identity/new` | 新しいidentityを作成 |
| GET | `/identity/:id` | identity状態を取得 |
| POST | `/encounter` | encounterを記録・関心ベクトルを更新 |

### コネクタ

| Method | Path | 説明 |
|--------|------|------|
| POST | `/connect/rss` | RSSフィードをStreamエンティティとして取り込む |
| POST | `/connect/url` | URLのテキストをDataエンティティとして追加 |
| GET | `/feeds` | 登録済みRSSフィード一覧 |

### Federation

| Method | Path | 説明 |
|--------|------|------|
| GET | `/peers` | 既知ピアノード一覧 |
| POST | `/peer/register` | ピア登録 (他ノードが呼ぶ) |

---

## バックグラウンドタスク

| タスク | 間隔 | 内容 |
|--------|------|------|
| presence cleanup | 30秒 | 15秒以上last_seenが更新されないSSEユーザーを除去 |
| entity decay | 10分 | activity_vecをembeddingに向かって回帰 (α=0.995) |
| entity emergence | 5分 | ユーザー収束から新エンティティを自然発生 |
| gossip | 10分 | ピアからエンティティを同期、新ピアを発見 |
| RSS ingest | 1時間 | 登録フィードから新記事を取り込み、古い未訪問記事を除去 |

---

## データモデル

### Entity

```
id          : UUID (ノードをまたいで同一)
label       : String (一意。gossipの競合解決キー)
kind        : Human / AI / Service / Stream / Event / Data
embedding   : Vec<f32> (384次元、意味ベクトル。不変)
activity_vec: Vec<f32> (384次元、活動ベクトル。encounter/decayで変化)
last_active : DateTime<Utc>
```

`embedding` は「その存在の意味」。変わらない。
`activity_vec` は「今どんな関心が集まっているか」。ユーザーと空間が育てる。

### Distance (5成分)

```
distance(A, B, U) =
    0.25 × semantic   — embedding間のコサイン距離
  + 0.20 × relational — グラフ上の関係距離
  + 0.20 × activity   — activity_vec間の差異
  + 0.15 × temporal   — last_activeからの経過時間
  + 0.20 × attention  — ユーザーUの関心からBへの距離 (主観)
```

同じ空間でも、誰が見るかによって幾何学が変わる。

### Identity

```
id          : UUID
interest_vec: Vec<f32> (384次元。閲覧によって進化する)
history     : Vec<EncounterRecord> (最新200件)
position    : String (最後にいた場所)
```

---

## 新しいノードを立てるには

```bash
# 環境変数を設定して起動
GOLDEN_NODE_URL=https://yourserver.example.com \
GOLDEN_BOOTSTRAP=https://space.gold3112.online \
./golden_core

# または systemd で
Environment=GOLDEN_NODE_URL=https://yourserver.example.com
Environment=GOLDEN_BOOTSTRAP=https://space.gold3112.online
```

起動すると自動で:
1. bootstrapに自己登録
2. 既存エンティティをgossipで取り込む
3. 10分ごとに新しいエンティティが届く
4. ユーザーが空間の一部になる

---

## 設計原則

**1. プロトコルが返すのは場の状態。描き方はクライアントに委ねる。**
CLIでも、Canvasでも、3Dでも、同じAPIで繋がる。

**2. ユーザーは設定しない。空間がそこにある。**
HTTPSのように、存在を意識させない。

**3. 距離 = interaction cost。**
近さは意味の近さではなく、影響し合うまでのコスト。

**4. 空間は静的データではなく、動的プロセス。**
エンティティは育ち、decayし、人の動きから生まれる。

**5. Single logical world + distributed infrastructure.**
ユーザーから見れば一つの空間。実装は分散した群れ。
