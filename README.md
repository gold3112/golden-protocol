# GOLDEN PROTOCOL

> ブラウザは窓だった。次に来るのは箱だ。

**デモ:** [space.gold3112.online](https://space.gold3112.online)

---

## 背景と思想

### 問題

今のインターネットは**部屋の集合体**だ。サービスをまたぐたびに自分が分断され、文脈が失われ、道がない。ブラウザという窓は30年間その構造を変えなかった。

今のウェブはすべて**pull-based**——検索する、URLを打つ、ページが来る。何もしなければ何も起きない。意図がなければ存在できない。

### 作るもの

誰も所有しない共有空間——**街**。

- 24時間眠らない。世界中の誰かが必ずいる
- アクセス手段を問わない。2Dでも3Dでも、CLIでも、同じ空間に入れる
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
horizon = 存在は検知できるが未解決 (気配はある、未解像)
beyond  = この場には存在しない
```

near/horizonの境界は固定ではなく、その場の距離分布から動的に計算する。

---

## アーキテクチャ

```
[Chrome Extension]          [Landing Page / Any Client]
 content.js                  Canvas visualization
 popup.js (polls /field)     polls /field, /entities
        |                            |
        └──────────── HTTPS ─────────┘
                          |
               [Cloudflare Tunnel]
                          |
             [Core Protocol Server]
              Rust + Axum :7331
              192.168.1.5 (LAN)
                          |
                    [SQLite DB]
                 identities.db (WAL)
                 entities / identities / rss_feeds
```

---

## 実装状況

### 動いているもの

- [x] 距離関数 (5成分の重み付き和)
- [x] ローカルembedding (fastembed, BAAI/bge-small-en-v1.5, APIキー不要)
- [x] 動的閾値 (near/horizonの境界を距離分布から自動計算)
- [x] 場の状態サーバー (Rust + Axum, :7331)
- [x] identity layer — SQLite永続化 + encounter履歴 + 関心ベクトルの進化
- [x] relational distance — encounter履歴から計算
- [x] drift / flow — グローバル活動カウンタからhorizonの引力を生成
- [x] passive absorption — nearにいるだけで関心ベクトルが染まる (alpha=0.02/frame)
- [x] entity persistence — SQLite (WAL) に保存、再起動後も保持
- [x] entity natural decay — 10分ごとにactivity_vecがembeddingに回帰 (~38h半減期)
- [x] entity activity boost — encounterのたびにactivity_vecが更新・DB保存
- [x] 複数ユーザーのpresence — SSE接続中ユーザーが near/horizon に出現
- [x] SSEストリーム (`GET /field/stream`) — 2秒ごとに field state をプッシュ
- [x] presence エンドポイント (`GET /presence`)
- [x] 空間が育つ仕組み — `POST /entities` / `DELETE /entity/:id`
- [x] コネクタ — `POST /connect/rss` (RSS取り込み) / `POST /connect/url` (URL取り込み)
- [x] RSS 自動取り込み — 1時間ごとに登録済みフィードを再取り込み
- [x] rate limiting — 60req/分, バースト10 (governor + dashmap)
- [x] CORS 対応
- [x] インターネット公開 — Cloudflare Tunnel → space.gold3112.online
- [x] systemd サービス — 起動時に自動スタート
- [x] ランディングページ — Canvas可視化 (エンティティが空間に浮かぶ、gold十字が自分)
- [x] ブラウザ拡張 (Manifest V3) — content.js + background.js + popup
- [x] Chrome Store パッケージ (`golden_protocol_extension.zip`)

---

## ディレクトリ構造

```
/GOLDEN_PROTOCOL/
├── README.md
├── README_EN.md
├── golden_protocol_extension.zip   ← Chrome Store 申請用
├── core/                           ← Rust core server
│   ├── Cargo.toml
│   └── src/
│       ├── main.rs             — サーバー + 全エンドポイント + バックグラウンドタスク
│       ├── distance/mod.rs     — 5成分距離計算 + 動的閾値
│       ├── field/mod.rs        — FieldState / DriftSignal
│       ├── graph/mod.rs        — Entity / SpaceGraph (petgraph)
│       ├── embedding/mod.rs    — ローカルembedding (fastembed)
│       └── identity/mod.rs     — Identity / SQLite永続化 / entity CRUD / RSS feeds
└── extension/
    ├── manifest.json       Manifest V3
    ├── content.js          ページテキスト抽出 → chrome.storage.session
    ├── background.js       identity管理 (chrome.storage.local)
    ├── popup.html/js/css   field state 表示UI (3秒ポーリング)
    └── icons/              16px / 48px / 128px PNG
```

---

## APIリファレンス

### `GET /`
ランディングページ。Canvas上にエンティティが浮かぶ空間を表示。

### `GET /field`
場の状態を返す。

| パラメータ | デフォルト | 説明 |
|-----------|-----------|------|
| `user_id` | なし | identity UUID。指定するとそのベクトルを使用 |
| `interest` | `"curiosity exploration knowledge"` | 関心テキスト |
| `passive` | `false` | trueのとき、nearにいるだけで関心ベクトルが受動更新 |

### `GET /field/stream`
SSEストリーム。2秒ごとに field state を配信。接続中は他ユーザーの near/horizon に出現する。

### `GET /presence`
接続中ユーザー一覧。

### `GET /identity/new?interest=...`
新しいidentityを作成してUUIDを返す。

### `GET /identity/:id`
identity状態を取得。

### `POST /encounter`
nearとの出会いを明示的に記録。
```json
{ "user_id": "uuid", "position": "plaza", "near_labels": ["philosophy_debate"], "interest_text": "philosophy" }
```

### `GET /entities`
全エンティティ一覧。

### `POST /entities`
エンティティを追加。
```json
{ "label": "rust_study_group", "kind": "Event", "text": "rust programming" }
```

### `DELETE /entity/:id`
エンティティを削除。

### `POST /connect/rss`
RSS/Atomフィードを Stream エンティティとして取り込む。URLはDBに登録され1時間ごとに自動再取り込みされる。
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

*最終更新: 2026-03-17 (prototype v6 — SQLite persistence + canvas visualization + entity decay)*
