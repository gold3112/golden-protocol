# GOLDEN PROTOCOL

> ブラウザは窓だった。次に来るのは場だ。

**デモ:** [space.gold3112.online](https://space.gold3112.online)
**アーキテクチャ:** [ARCHITECTURE.md](./ARCHITECTURE.md)
**プライバシーポリシー:** [space.gold3112.online/privacy-policy](https://space.gold3112.online/privacy-policy)

---

## 背景と思想

### 問題

今のインターネットは**部屋の集合体**だ。サービスをまたぐたびに自分が分断され、文脈が失われ、道がない。ブラウザという窓は30年間その構造を変えなかった。

今のウェブはすべて**pull-based**——検索する、URLを打つ、ページが来る。何もしなければ何も起きない。意図がなければ存在できない。

### 作るもの

誰も所有しない共有空間——**場**。

- 24時間眠らない。世界中の誰かが必ずいる
- アクセス手段を問わない。ブラウザでも、CLIでも、同じ空間に入れる
- 既存のインターネットと完全な互換性を持つ
- 基盤はオープン。誰もが上にサービスを建てられる

### 体験の核心——「散策」

「歩く」ではなく「**散策**」。目的のない移動、偶然の出会い。意図がなくても、存在しているだけで空間が応答する。

```
存在する → 空間が応答する → 奥行きに気づく → 引き寄せられる
```

ユーザーは何もしていない。なのに何かが起きている。その驚きが最初の体験。

### 情報の奥行き

メタバースの失敗は「3Dで空間を描こうとした」ことにある。奥行きは視覚的深度ではなく、**情報構造の問題**だ。

> 「見えないが、ある」という感覚が奥行きの正体。

CLIのテキスト画面でも3D空間でも、奥行きは実装できる。なぜなら奥行きはレンダリングではなく構造だから。

---

## 空間の数学的構造

### 距離の定義

```
distance(A, B, U) =
    0.25 × semantic(A,B)    — 意味的近さ (embedding cosine distance)
  + 0.20 × relational(A,B) — グラフ上の関係距離
  + 0.20 × activity(A,B)   — 現在の活動の差異
  + 0.15 × temporal(A,B)   — 最終接触からの経過時間
  + 0.20 × attention(U,B)  — ユーザーUの関心からBへの距離 (主観・個人)
```

同じ空間でも、誰が見るかによって幾何学が変わる。

### 奥行きの実装: near / horizon

```
near    = interaction latencyが低い (見える、直接触れられる)
horizon = 存在は検知できるが未解像 (気配はある、まだ遠い)
beyond  = この場には存在しない
```

near/horizonの境界は固定ではなく、その場の距離分布から動的に計算する。

---

## アーキテクチャ

```
[Browser Extension]  [Web Client]  [CLI / Any Client]
        |                 |                |
        └──────── HTTPS ──────────────────┘
                          |
              ┌─────────────────────┐
              │   Cloudflare Tunnel  │
              └──────────┬──────────┘
                          │
       ┌──────────────────┼──────────────────┐
       │                  │                  │
  [Node A]           [Node B]           [Node C]
 golden_core        golden_core        golden_core
 SQLite WAL         SQLite WAL         SQLite WAL
       └──────── gossip ──┴──────── gossip ──┘
```

`single logical world + distributed infrastructure`
どのノードに繋いでも同じ空間。ノードが落ちても空間は続く。
詳細は [ARCHITECTURE.md](./ARCHITECTURE.md) を参照。

---

## 実装状況

### コア

- [x] 距離関数 (5成分の重み付き和)
- [x] ローカルembedding (fastembed, BAAI/bge-small-en-v1.5, APIキー不要)
- [x] 動的閾値 (near/horizonの境界を距離分布から自動計算)
- [x] 場の状態サーバー (Rust + Axum, :7331)
- [x] identity layer — SQLite永続化 + encounter履歴 + 関心ベクトルの進化
- [x] relational distance — encounter履歴から計算
- [x] drift / flow — グローバル活動カウンタからhorizonの引力を生成
- [x] passive absorption — nearにいるだけで関心ベクトルが染まる
- [x] entity persistence — SQLite (WAL)、再起動後も保持
- [x] entity natural decay — 10分ごとにactivity_vecがembeddingに回帰 (~38h半減期)
- [x] entity activity boost — encounterのたびにactivity_vecが更新
- [x] **distance components** — near/horizonの各エンティティに5成分の内訳を付与

### 空間と存在

- [x] 複数ユーザーのpresence — SSE接続中ユーザーがnear/horizonに出現
- [x] **個性 (individuality)** — 名前付きで入場、Canvas上に表示
- [x] **時限イベント** — expires_atを持つエンティティ、期限が近いほど強くglow、消滅時バースト
- [x] **エンティティ自然発生** — ユーザーの収束から新エンティティが自動生成
- [x] **エンティティローテーション** — 未訪問の古い記事を除去、新記事と入れ替え

### コミュニケーション

- [x] **ambient messaging** — エンティティスコープの匿名メッセージ、2時間で自動消滅
- [x] メッセージがCanvas上のエンティティ近傍に浮かぶ

### 散策アルゴリズム

- [x] **散策アルゴリズム** — SSE接続中はspatial_vecが2秒ごとに自動更新 (near引力α=0.003, drift引力α=0.002)
- [x] **空間イベントのSSE配信** — emergence / convergence / encounter イベントをリアルタイム配信
- [x] **他者の方向性** — wandererがmoving_towardを持ち、向かっている方向が点線で可視化される

### クライアント

- [x] SSEストリーム (`GET /field/stream`) — 2秒ごとに field state をプッシュ
- [x] **Webクライアントのidentity永続化** — localStorageでuser_idを保持、SSE接続時に自動送信
- [x] **Webクライアント SSE接続** — setIntervalポーリングからSSEに移行、散策アルゴリズムが動作
- [x] **CLIフォーマット** — `?format=text` で美しいターミナル表示 (5成分の距離内訳付き)
- [x] ランディングページ — 散策UI (カスタムカーソル・マウスで歩く・環境パーティクル・ビネット・コンスタレーション)
- [x] **空間イベント視覚化** — emergence (goldバースト) / convergence (拡散リング) / encounter (フラッシュ)
- [x] **ambient.js** — 任意サイトに埋め込める透明な空間layer
- [x] **ブラウザ拡張 (Manifest V3)** — Chrome Web Store審査中
  - バックグラウンドでpassive absorptionが動く (ポップアップ不要)
  - アイコンバッジにpresence数を表示

### インフラ

- [x] rate limiting — 60req/分, バースト10
- [x] CORS対応
- [x] インターネット公開 — Cloudflare Tunnel → space.gold3112.online
- [x] systemdサービス — 起動時に自動スタート
- [x] **Federation gossip** — 複数ノードが空間を共有
- [x] RSS自動取り込み — 1時間ごとに登録済みフィードを再取り込み (HN/Lobsters/Zenn/arxiv等 160+エンティティ)
- [x] コネクタ — `POST /connect/rss` / `POST /connect/url`
- [x] プライバシーポリシーページ — `/privacy-policy`

---

## CLIで試す

```bash
# 今の場の状態を見る
curl "https://space.gold3112.online/field?interest=programming&format=text"

# 関心を変えると幾何学が変わる
curl "https://space.gold3112.online/field?interest=philosophy+music&format=text"

# identityを作って関心を蓄積する
curl "https://space.gold3112.online/identity/new?interest=rust+systems"

# ambient layerを任意サイトに埋め込む
<script src="https://space.gold3112.online/ambient.js"></script>
```

---

## 新しいノードを立てる

```bash
GOLDEN_NODE_URL=https://yourserver.example.com \
GOLDEN_BOOTSTRAP=https://space.gold3112.online \
./golden_core
```

起動すると自動でbootstrapに登録、エンティティをgossipで受け取り、空間の一部になる。

---

## ディレクトリ構造

```
/GOLDEN_PROTOCOL/
├── README.md
├── README_EN.md
├── ARCHITECTURE.md             ← 設計思想・プロトコル仕様
├── golden_protocol_extension.zip   ← Chrome Store 申請用
├── docs/
│   ├── privacy-policy.md       ← プライバシーポリシー原文
│   └── store-listing.md        ← Chrome Store ストア申請テキスト
├── core/                       ← Rust core server
│   ├── Cargo.toml
│   └── src/
│       ├── main.rs             — サーバー + 全エンドポイント + バックグラウンドタスク + Landing HTML
│       ├── distance/mod.rs     — 5成分距離計算 + 動的閾値
│       ├── field/mod.rs        — FieldState / DriftSignal / ObservedEntity
│       ├── graph/mod.rs        — Entity / SpaceGraph (petgraph)
│       ├── embedding/mod.rs    — ローカルembedding (fastembed)
│       └── identity/mod.rs     — Identity / SQLite永続化 / entity CRUD / RSS / messages
└── extension/
    ├── manifest.json           Manifest V3
    ├── content.js              ページテキスト抽出 → background.js に通知
    ├── background.js           passive absorption + バッジ更新 (ポップアップ不要)
    ├── popup.html/js/css       field state 表示UI
    └── icons/                  16px / 48px / 128px PNG
```

---

## APIリファレンス

### `GET /`
散策UI。Canvasに空間が広がる。マウスで歩ける。

### `GET /privacy-policy`
プライバシーポリシーページ。

### `GET /field`
場の状態を返す。

| パラメータ | デフォルト | 説明 |
|-----------|-----------|------|
| `user_id` | なし | identity UUID |
| `interest` | `"curiosity exploration knowledge"` | 関心テキスト |
| `passive` | `false` | trueのとき関心ベクトルを受動更新 |
| `name` | なし | 表示名 (presence に反映) |
| `format` | `json` | `text` でターミナル向け整形出力 |

### `GET /field/stream`
SSEストリーム。2秒ごとに field state を配信。

### `GET /presence`
接続中ユーザー一覧。

### `GET /identity/new?interest=...`
新しいidentityを作成してUUIDを返す。

### `GET /identity/:id`
identity状態を取得。

### `POST /encounter`
nearとの出会いを明示的に記録。
```json
{ "user_id": "uuid", "position": "plaza", "near_labels": ["rust_weekly"], "interest_text": "rust" }
```

### `POST /message`
エンティティ近傍にメッセージを残す (2時間で自動消滅)。
```json
{ "entity_label": "philosophy_debate", "text": "興味深い", "author": "wanderer_xxxx" }
```

### `GET /entities`
全エンティティ一覧。

### `POST /entities`
エンティティを追加。`duration_hours` で時限イベントにできる。
```json
{ "label": "rust_study_group", "kind": "Event", "text": "rust programming", "duration_hours": 48 }
```

### `DELETE /entity/:id`
エンティティを削除。

### `POST /connect/rss`
RSS/Atomフィードを Stream エンティティとして取り込む。1時間ごとに自動再取り込み。
```json
{ "url": "https://hnrss.org/frontpage", "max_items": 20 }
```

### `POST /connect/url`
URLのテキストを抽出して Data エンティティとして追加。
```json
{ "url": "https://example.com/article", "label": "optional label" }
```

### `GET /feeds`
登録済みRSSフィード一覧。

### `GET /peers` / `POST /peer/register`
Federation用ピア管理。

### `GET /entities/export`
全エンティティ (embedding込み、ピア同期用)。

### `GET /ambient.js`
任意のサイトに埋め込める透明な空間layer。ページのテキストを自動抽出してpassive absorptionを行い、空間の一部になる。

---

## 起動方法

```bash
# サーバー起動 (初回はembeddingモデルをダウンロード ~130MB)
cd /GOLDEN_PROTOCOL/core
cargo run --release

# systemdサービスとして管理
sudo systemctl start golden-protocol
sudo systemctl status golden-protocol
```

---

*最終更新: 2026-03-17 (prototype v9 — SSE接続 + 散策アルゴリズム + 空間イベント視覚化 + ambient.js + identity永続化)*
