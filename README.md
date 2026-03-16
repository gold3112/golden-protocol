# GOLDEN PROTOCOL

> ブラウザは窓だった。次に来るのは箱だ。

## デモ

![demo](Sample_python_demo.png)

2D空間マップクライアント (`test_python.py`) の動作。関心テキストを変えると空間の幾何学が変わり、near/horizonの分布が変化する。

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

### HTTPSのように隠す

HTTPSを誰も理解していないのに全員が使っている。なぜなら存在を意識させないから。

新プロトコルも同じ構造でなければならない。`ox://` のような新スキームをユーザーに意識させてはいけない——それは「知っている人だけが使う扉」になる。**ユーザーは設定しない。空間がそこにある。**

### 動機

世界のためではない。自分がこれを使いたいから作る。
人間の知的好奇心は枯渇しない。それを受け止める箱が、まだ存在していない。

---

## 空間の数学的構造

### 空間 = distributed activity field

```
space = interaction field

node  = 存在 (反応可能なもの)
edge  = 関係
flow  = 活動
```

人・AI・サービス・ストリーム・イベント・データ——すべてが同じレイヤーの「存在」。
これはsocial networkではなく**activity ecosystem**。

### パラダイム転換

```
object web  (page, post, video, repo)
    ↓
process web (議論, 制作, 研究, イベント, データ生成)

document internet
    ↓
activity internet
```

### 距離の定義

```
distance(A, B) = interaction_cost(A, B)
```

距離 = 互いに影響するまでのコスト。5つの成分から構成される：

| 成分 | 定義 | 性質 |
|------|------|------|
| semantic(A,B) | 意味的近さ (embedding cosine distance) | 客観・安定 |
| relational(A,B) | グラフ上の関係距離 | 客観・動的 |
| activity(A,B) | 現在の活動の差異 | 客観・動的 |
| temporal(A,B) | 最終接触からの経過時間 | 客観・動的 |
| attention(U,B) | ユーザーUの関心からBへの距離 | **主観・個人** |

成分1〜4はA,B間の客観的コスト。成分5だけはユーザー視点の主観的コスト。
これにより**同じ空間でも、誰が見るかによって幾何学が変わる**。

```
distance(A,B,U) =
    0.25 × semantic(A,B)
  + 0.20 × relational(A,B)
  + 0.20 × activity(A,B)
  + 0.15 × temporal(A,B)
  + 0.20 × attention(U,B)
```

### 奥行きの実装: near / horizon

```
near    = interaction latencyが低い (見える、直接触れられる)
horizon = 存在は検知できるが未解決  (気配はある、未解像)
beyond  = この場には存在しない
```

nearとhorizonの対比が情報空間の奥行きを作る。メタバースの「3D深度」ではなく、**information visibility**による奥行き。

### 散策アルゴリズム

```
position(t+1) = position(t) + curiosity + flow + interaction
```

ユーザーは「検索 → ページ」ではなく「**存在 → 反応 → 発見**」で探索する。

---

## プロトコル設計

### サーバーが返すもの——「場の状態」

サーバーはページを返さない。**場の状態 (field state)** を返す。

```json
{
  "position":   "plaza",
  "density":    0.27,
  "presence":   7,
  "near": [
    { "label": "music_history",  "distance": 0.33, "visibility": "Near" },
    { "label": "deep_archive",   "distance": 0.48, "visibility": "Near" }
  ],
  "horizon": [
    { "label": "philosophy_debate", "distance": 0.52, "visibility": "Horizon" },
    { "label": "wandering_ai",      "distance": 0.53, "visibility": "Horizon" }
  ],
  "drift": [
    { "toward": "philosophy_debate", "strength": 1.0 },
    { "toward": "knowledge_stream",  "strength": 0.6 }
  ],
  "thresholds": [0.49, 0.54],
  "timestamp":  "2026-03-16T00:00:00Z"
}
```

UIはこの状態を受け取って、自分の形式で表現する。
CLIなら文字の濃淡と配置で。2Dなら光と影で。3Dなら物理空間で。
**プロトコルが返すのは空間の状態。描き方はクライアントに委ねる。**

### 動的閾値

near/horizonの境界は固定ではなく、**その場の距離分布から動的に計算**する。

```
thresholds = percentile(all_distances, near_pct=0.30, horizon_pct=0.70)
```

エンティティの分布がどうあっても、常に意味ある near/horizon の分割を保証する。

### 3つの新しい層

既存プロトコルの上に重ねる形で定義する：

```
アイデンティティ層  — プラットフォームをまたいで持続する自分
空間層            — URLを超えた、散策できる新しい参照系
インタラクション層  — 既存サービスと高解像度で繋がる拡張仕様
```

---

## 実装

### アーキテクチャ

```
[Core Protocol Server]  ←→  [UI Clients]
Rust + Axum                  (何でもよい)
場の状態を生成・管理           状態を受け取って表現
        ↑
  LAN: 192.168.1.5:7331
```

Coreが堅牢なら、UIは後から何通りでも繋げられる。

### 技術選定

| 役割 | 技術 | 理由 |
|------|------|------|
| Core server | Rust + Axum | 距離計算・グラフ操作の性能と正確さ |
| Embedding | fastembed-rs (BAAI/bge-small-en-v1.5) | ローカル動作、APIキー不要 |
| Graph | petgraph | Rustネイティブのグラフライブラリ |
| CLI client | Python | UI実験のスピード優先 |

将来: Rust coreにPythonバインディング (PyO3) を追加してLLM層と接続。

### ディレクトリ構造

```
/GOLDEN_PROTOCOL/
├── README.md           ← このファイル
├── vision.txt          ← 思想まとめ (日本語)
├── for_claude.txt      ← AI引き継ぎ文書 (英語)
├── vision2.txt         ← 数学的構造の深化 (別AIとの対話)
├── client.py           ← CLIクライアント (near/horizon + drift表示)
├── test_python.py      ← 2D空間マップクライアント (距離を座標に変換)
└── core/               ← Rust core server
    ├── Cargo.toml
    └── src/
        ├── main.rs             — サーバー + エンドポイント定義
        ├── distance/mod.rs     — 5成分の距離計算 + 動的閾値
        ├── field/mod.rs        — FieldState / DriftSignal
        ├── graph/mod.rs        — Entity / SpaceGraph (動的グラフ)
        ├── embedding/mod.rs    — ローカルembedding (fastembed)
        └── identity/mod.rs     — Identity / encounter履歴 / passive absorption
```

### 起動方法

```bash
# サーバー起動 (初回はembeddingモデルをダウンロード ~130MB)
cd /GOLDEN_PROTOCOL/core
cargo run

# CLIクライアント (同一LAN上の別PCから)
python3 client.py

# APIを直接叩く
curl http://192.168.1.5:7331/field
curl "http://192.168.1.5:7331/field?interest=music+history"
```

### APIリファレンス

#### `GET /field`

場の状態を返す。

| パラメータ | 型 | デフォルト | 説明 |
|-----------|-----|-----------|------|
| `user_id` | uuid | なし | identityを指定。あればそのベクトルを使用 |
| `interest` | string | `"curiosity exploration knowledge"` | 関心テキスト (user_idなしの場合) |
| `near_pct` | float | `0.30` | nearに含める距離パーセンタイル |
| `horizon_pct` | float | `0.70` | horizonに含める距離パーセンタイル |
| `passive` | bool | `false` | trueのとき、nearにいるだけで関心ベクトルが受動的に更新される (alpha=0.98) |

#### `GET /identity/new`

新しいidentityを作成してUUIDを返す。

| パラメータ | 型 | デフォルト | 説明 |
|-----------|-----|-----------|------|
| `interest` | string | `"curiosity exploration knowledge"` | 初期関心テキスト |

#### `GET /identity/:id`

identity状態を取得。

#### `POST /encounter`

nearとの出会いを明示的に記録。関心ベクトルを更新 (alpha=0.85) し、グローバル活動カウンタを加算してdriftに反映させる。

```json
{
  "user_id":       "uuid",
  "position":      "plaza",
  "near_labels":   ["philosophy_debate", "knowledge_stream"],
  "interest_text": "philosophy"
}
```

---

## 現在地と次のステップ

### 動いているもの

- [x] 距離関数 (5成分の重み付き和)
- [x] ローカルembedding (fastembed, APIキー不要)
- [x] 動的閾値 (near/horizonの境界を距離分布から自動計算)
- [x] 場の状態サーバー (Rust + Axum, LAN公開済み)
- [x] identity layer — 永続化 + encounter履歴 + 関心ベクトルの進化
- [x] relational distance — encounter履歴から計算 (出会うほど近くなる)
- [x] drift / flow — グローバル活動カウンタからhorizonの引力を生成
- [x] passive absorption — nearにいるだけで関心ベクトルが染まる (alpha=0.02/frame)
- [x] CLIクライアント (near/horizon/drift表示, identity対応)
- [x] 2D空間マップクライアント (距離を座標に変換、ターミナルに描画)
- [x] 同一LAN上の別デバイスからアクセス確認済み
- [x] **position の動的化** — near最近傍エンティティのラベルから文脈として自動生成
- [x] **複数ユーザーのpresence** — SSE接続中ユーザーが `wanderer_*` として near/horizon に出現
- [x] **SSEストリーム** (`GET /field/stream`) — 2秒ごとに field state をプッシュ配信
- [x] **presence エンドポイント** (`GET /presence`) — 接続中ユーザー一覧

### 未実装

- [x] **空間が育つ仕組み** — `POST /entities` でエンティティを動的追加、`DELETE /entity/:id` で削除
- [x] **コネクタ** — `POST /connect/rss` (RSS取り込み) / `POST /connect/url` (URLテキスト取り込み)
- [x] **CORS 対応** — ブラウザ拡張からのリクエストを許可
- [x] **ブラウザ拡張** — Manifest V3、ページ閲覧を散策に変換 (`extension/`)
- [ ] **activity vector** — エンティティの活動状態のリアルタイム更新
- [ ] **identity の永続化改善** — ローカルファイルからDBへ
- [ ] **インターネット公開** — LAN外からアクセス可能にする

### 未解決の設計問題

- **移動** の概念 — interestを変えることが移動なのか、それとも別の概念か
- **distance の非対称性** — attention は一方向。空間は誰から見るかで形が変わる

### APIリファレンス追記

#### `GET /field/stream`

SSEストリーム。2秒ごとに `field` イベントとして field state を配信。
接続中はこのユーザーが他ユーザーの near/horizon に `wanderer_*` として現れる。
クエリパラメータは `GET /field` と同じ。

#### `GET /presence`

接続中ユーザーの一覧を返す。

```json
{
  "count": 2,
  "users": [
    { "id": "uuid", "position": "philosophy_debate", "last_seen": "..." }
  ]
}
```

---

#### `GET /entities`

現在の空間にいる全エンティティ一覧を返す。

#### `POST /entities`

エンティティを動的に追加する。

```json
{
  "label": "rust_study_group",
  "kind":  "Event",
  "text":  "rust programming systems language"
}
```

`kind`: `Human` / `AI` / `Service` / `Stream` / `Event` / `Data`（省略時 `Data`）
`text`: embedding のシードテキスト。省略時は `label` をそのまま使う。

#### `DELETE /entity/:id`

エンティティを空間から除去する。

---

#### `POST /connect/rss`

RSS/Atom フィードを Stream エンティティとして一括取り込む。

```json
{ "url": "https://hnrss.org/frontpage", "max_items": 20 }
```

#### `POST /connect/url`

URL のテキストを抽出して Data エンティティとして追加する。

```json
{ "url": "https://example.com/article", "label": "optional label" }
```

---

### ブラウザ拡張 (`extension/`)

```
extension/
├── manifest.json   Manifest V3
├── content.js      ページテキスト抽出 + SPA対応URL監視
├── background.js   SSE接続管理 + identity永続化 (chrome.storage)
├── popup.html      field state 表示UI
├── popup.js
└── popup.css
```

**インストール方法 (開発版):**
1. Chrome で `chrome://extensions` を開く
2. 「デベロッパーモード」を有効化
3. 「パッケージ化されていない拡張機能を読み込む」→ `extension/` を選択
4. サーバーを起動してウェブを閲覧すると popup に near/horizon/drift が現れる

---

*最終更新: 2026-03-16 (prototype v4 — connectors + browser extension)*
