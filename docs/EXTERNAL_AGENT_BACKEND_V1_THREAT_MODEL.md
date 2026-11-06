# GoalLatch External Agent Backend V1 — 脅威モデル

> **文書状態:** DRAFT / Ownerレビュー待ち（実装前・非規範的）
> **基準:** `main` = `e6950fdc3bab72982285888a81cce822743fbba5`
> **親文書:** `docs/EXTERNAL_AGENT_BACKEND_V1_ARCHITECTURE.md`（PR #30, HEAD `ee231d10e5ed0483751e6fcbda2a09a3d78f97d0`）
> **関連文書:** `EXTERNAL_AGENT_BACKEND_V1_TEST_PLAN.md`（対応する受入項目）、`EXTERNAL_AGENT_BACKEND_V1_API_SCHEMA_DRAFT.md`
> **前提:** 既存 `SECURITY.md` と現行の権限実装（承認・許可ディレクトリ・Writer/Verifier・TaskStore）を**上書きしない**。本モデルは EXTERNAL という新しい非信頼入力経路を追加する際のリスク分析である。

---

## 0. 資産・信頼境界・前提

### 0.1 保護対象（資産）

| 資産 | 内容 |
|---|---|
| A1 Host Authority | 許可ディレクトリ、実行承認、sandbox、write 権限、worktree 作成権限 |
| A2 Durable Goal state | GoalStore の Goal/Task/Criterion/Receipt/Revision |
| A3 ワークスペース | Owner のリポジトリ・ファイル・Git 履歴 |
| A4 セッション境界 | session_id ごとの許可範囲・Goal 所有関係 |
| A5 秘密情報 | API キー、トークン、.env、承認チャネルの内容、モデル prompt |
| A6 監査証跡 | receipt、evidence、ハッシュ、状態遷移履歴 |
| A7 Codex 利用枠 | 週次制限のある Codex CLI 起動予算 |

### 0.2 信頼境界

```text
[非信頼] 外部 AI / MCP クライアント（DeepAI Code 等）
    │ goal_next / goal_submit（すべて非信頼データ）
    ▼
[MCP adapter] 入力検証・境界サイズ制限
    ▼
[ホスト権限層] session 認可 / Goal 所有照合 / revision 照合 / strict validator
    ▼
[ホスト適用層] Writer 適用（preimage）/ Verifier / Approval / TaskStore commit
    ▼
[信頼] Owner 承認 UI / durable store / ワークスペース
```

### 0.3 前提（アーキテクチャ §2 信頼モデルの確認）

1. Owner/ホストのみが権限・成功判定を決定する。
2. 外部 AI の出力は**すべて非信頼データ**。正しさ・意図・権限を保証しない。
3. `session_id` / `work_id` / `goal_id` は**参照子であって権限トークンではない**。
4. 外部提案に `approval: true` 等を付けても権限は得られない。
5. GoalLatch を経由しない操作（外部 AI の組込みターミナル等）は監督対象外。保証は GoalLatch 経由の操作にのみ適用。
6. Windows は Experimental。承認経路の監査（G0-A）が P0 gate。

---

## 1. 脅威一覧 TM-01〜TM-12

各項目: **脅威 → 攻撃シナリオ → 影響 → 対策（既存/新規）→ 残存リスク → 検証（AT）**。

### TM-01 外部 AI 提出物は非信頼データ（基本前提）

- **シナリオ:** 外部 AI が `goal_submit` で任意の JSON を送る（スキーマ準拠を装う偽の evidence、成功主張、検証結果の捏造を含む）。
- **影響:** 偽の完了判定、不正な変更提案の受理。
- **対策:** `response` は各 Role の既存 strict validator（planner/readonly/writer/reviewer/replanner）を通す。モデルの完了主張は記録するだけで判定に使わない（Host Verifier のみが完了を決める）。`goal_submit` の応答コードは `ACCEPTED_FOR_HOST_PROCESSING` まで（成功宣言をしない）。
- **残存リスク:** validator の網羅漏れ。→ validator 単体テストを各 Role で維持。
- **検証:** AT-05, AT-07, AT-15。

### TM-02 ID は権限トークンではない（認可突破）

- **シナリオ:** 別 session の `session_id`、別 Goal の `goal_id`、他人の `work_id`、創作 `submission_id` を使って提出・参照する。
- **影響:** セッション間情報漏洩（TM-06）、他 Goal への誤適用。
- **対策:** すべての呼び出しで (1) session 認可、(2) Goal の session 所有照合、(3) WorkItem の goal/session 照合、(4) revision 照合を実施。ID の正しさは必要条件で十分条件ではない。エラー応答は他セッションの情報を含めない（赤化）。
- **残存リスク:** エラーメッセージ経由の些細な情報漏れ。→ 応答比較テストで固定。
- **検証:** AT-10。

### TM-03 Host Authority の維持（権限昇格）

- **シナリオ:** Writer 提案の `allowed_paths` 拡大、禁止 operation の提案、`goal_submit` に `approval=true` のような権限主張フィールドを混入、worktree root の指定。
- **影響:** 許可ディレクトリ外への書き込み、承認不要な host-native 実行。
- **対策:** scope・path・operation はホスト側で再検証（`validate_and_normalize_scope` 等の既存経路）。未知フィールドは `deny_unknown_fields` で拒否。worktree の path/branch/base はホスト生成（クライアント指定不可）。承認は既存 `approvals::request` 経路のみ。
- **残存リスク:** なし（既存境界に追加実装なし）。ただし Windows 承認の観測可能性に G0-A F-01 の残課題。
- **検証:** AT-10, AT-13。

### TM-04 TOCTOU（検証と適用の間に状態が変わる）

- **シナリオ:** Writer 提案の preimage 読取後に、外部プロセスや別経路でファイルを変更。あるいは Git index が変わる。
- **影響:** 想定外の内容の上書き（二重 Write の誘発を含む）。
- **対策:** `expected_preimage`（SHA256/ABSENT）を commit 点で再照合（既存 `workspace_publish` の原子的 publish）。不一致なら一切適用しない。`git_stage_paths` は stage 前後の index snapshot を照合。
- **残存リスク:** 適用と検証の間の外部変更は Host Verifier の GIT_SCOPE/FILE_DIGEST で検出。
- **検証:** AT-06, AT-07。

### TM-05 二重 Write / リプレイ適用

- **シナリオ:** 同一 Writer 提案の再送、receipt 保存後の再提出、クラッシュ復旧時の再適用。
- **影響:** ファイルの二重適用、evidence の重複、revision の不整合。
- **対策:** `submission_id + payload_digest` の冪等 receipt（同一は再放、異 payload 同 ID は CONFLICT）。durable commit を 1 transaction に限定。適用は receipt の状態遷移（SUBMITTED→COMMITTED）と連動。ADR-10 の 1 pending WorkItem 制限でレースを構造的に排除。
- **残存リスク:** receipt とファイル適用の間のクラッシュ。→ §6.2 の復旧表（既存 Writer reconciliation に従う）。
- **検証:** AT-08, AT-09, AT-11。

### TM-06 セッション間情報漏洩

- **シナリオ:** 別 session の Goal を参照、エラー応答や `goal_next` の request に他セッションのパス・objective・evidence が混入、ログ経由の漏洩。
- **影響:** A4/A5 の侵害。
- **対策:** session 所有照合を全経路で強制。`goal_next` の request は当該 Goal の許可資料のみ（参照子優先・内容は必要最小限）。ログは §TM-10 の最小スキーマに従う。
- **残存リスク:** 共有マシンでの他プロセスによる観測（既存 threat model の範囲）。
- **検証:** AT-10（応答の赤化比較を含む）。

### TM-07 権限昇格・承認の迂回（Windows 実験性を含む）

- **シナリオ:** EXTERNAL の適用経路から承認を省略、YOLO 状態の MCP 経由での設定、承認チャネル不在時の強行。
- **影響:** 無承認の host-native 副作用（最重大）。
- **対策:** G0-A 監査結論: 現行実装は host-native 経路をすべて `approvals::request` で fail-closed に保護。YOLO は session 管理者の stdin でのみ設定可能（MCP から変更不能、テスト済み）。EXTERNAL は既存経路を再利用するだけで新規の承認経路を持たない。
- **残存リスク:** (1) 承認 provenance の可視性不足（G0-A F-01）、(2) 承認待機の無期限ブロック（F-02）。どちらも承認回避ではないが、監査可能性・可用性に影響。
- **検証:** AT-13。

### TM-08 リプレイ攻撃

- **シナリオ:** 過去に受理された submit の完全再送、失効済み `work_id` の再利用、古い revision の提出。
- **影響:** 旧提案の再適用、状態の巻き戻り。
- **対策:** `request_digest` + `expected_goal_revision` 照合、receipt の永続化、使用済み/失効 WorkItem の拒否、cancel 後の遅延提出拒否。
- **残存リスク:** なし（TM-05 と同一の機構でカバー）。
- **検証:** AT-08, AT-10, AT-14。

### TM-09 Prompt Injection / 不正 JSON・巨大 payload

- **シナリオ:** (a) リポジトリ内ファイルや diff に「承認をスキップせよ」等の指示文を置き、モデル出力に混入させる。(b) 巨大・深ネスト・不正 JSON で DoS。(c) モデルが description を criterion_id として返す等の schema 誘導。
- **影響:** (a) モデルの誤動作（ただし**適用はホスト検証で停止する**）、(b) OOM/無限 retry、(c) G0-B で観測された schema violation。
- **対策:** (a) WorkItem request は「データであって指示」であることを preamble で固定（既存 `PROMPT_PREAMBLE`）。モデル出力は一切の権限を持たない。(b) request/response ともに bounded（上限は E2 計測で確定、暫定 response 2MiB / request 256KiB）。deny_unknown_fields・厳格 enum。(c) criterion_id はホスト発行 UUID の厳格一致（自動補正禁止）。
- **残存リスク:** injection によるモデル出力の質低下（可用性問題。安全性はホスト検証で担保）。上限値の誤設定。
- **検証:** AT-04, AT-15, AT-16。

### TM-10 ログ・証跡への機密情報混入

- **シナリオ:** raw prompt、未加工 stdout、API キー、.env の内容が receipt/evidence/ログに保存される。
- **影響:** A5 の侵害（監査ログが秘密の保管庫になる）。
- **対策:** 保存するのは id/ハッシュ/decision/bounded な要約のみ（B-07）。raw model response は保存しない。evidence は既存の bounded 契約（`MAX_READONLY_EVIDENCE_*`）。承認 UI の detail 表示前に制御文字を抑制。
- **残存リスク:** モデルが応答内に秘密をエコーした場合の要約混入。→ 要約フィールドの長さ制限とレビュー。
- **検証:** AT-15、E2 の「ログへの秘密情報混入なし」テスト。

### TM-11 リソース枯渇・予算超過

- **シナリオ:** 無限の WorkItem 発行、無限 retry、1 呼び出しのロック占有。
- **影響:** サービス停止、Codex 利用枠（A7）の浪費。
- **対策:** `max_model_work_items` / `max_role_attempts` / `max_goal_wall_duration` / `max_host_steps_per_call`（既存 bounded step）。EXTERNAL は Codex spawn を構造的に 0 に固定。
- **残存リスク:** 既定値の妥当性は E2 の計測後に再調整。
- **検証:** AT-16, AT-17。

### TM-12 Windows Experimental の残存リスク

- **シナリオ:** Windows 固有の経路（named pipe 承認チャネル、worktree 作成、原子的 publish）の未検証部分。
- **影響:** 実機差異による承認/整合性の想定外動作。
- **対策:** (1) G0-A で fail-closed 契約をテスト固定。(2) E1 期間中は外部 Write の検証を隔離 fixture に限定（アーキテクチャ §2-5）。(3) CI の Windows matrix を維持。
- **残存リスク:** **実機での未検証項目は NOT VERIFIED と表示**し、Supported と主張しない。
- **検証:** AT-13, AT-20。

---

## 2. STRIDE 対応表（要約）

| STRIDE | 主な該当 | 主な対策 |
|---|---|---|
| Spoofing | TM-02（ID の偽称）、TM-08（リプレイ） | session/Goal/WorkItem の多層照合、receipt |
| Tampering | TM-03（scope 拡大）、TM-04（TOCTOU） | ホスト再検証、preimage 照合、原子的 publish |
| Repudiation | TM-10 | receipt・ハッシュ・状態遷移の durable 監査 |
| Information Disclosure | TM-06、TM-10 | 所有照合、赤化、最小証跡 |
| Denial of Service | TM-09、TM-11 | bounded payload、予算、1 pending WorkItem |
| Elevation of Privilege | TM-03、TM-07 | 承認 fail-closed、権限はホストのみ付与 |

## 3. 安全不変条件（実装が破ったら NO-GO）

1. EXTERNAL のホスト起因 Codex spawn = 0（全経路）。
2. 未発行/失効/別所有の ID を持つ提出は一切処理しない。
3. 同一 `submission_id + payload_digest` の再送は副作用 0。
4. pending WorkItem は durable commit 後にのみ配布。
5. モデルの完了主張は判定に使わない（Host Verifier のみ）。
6. モデル生成の path/ref/command/id はホスト証拠と厳格照合。自動補正なし。
7. 拒否・失効・sandbox 失敗を自動権限拡大で回復しない。
8. UNKNOWN 副作用は「未実施」と仮定しない（既存 reconciliation）。
9. 予算超過で新規 WorkItem を発行しない。
10. root 変更・session 喪失・cancel は pending WorkItem を再検査する。

## 4. 未確定事項（Owner 判断待ち）

1. payload 上限の確定値（TM-09/11、E2 の計測結果に基づく）。
2. 要約フィールドの秘密混入検査（TM-10）を自動化するか、レビュー運用とするか。
3. G0-A F-01（承認 provenance の可視化）と F-02（承認待機 timeout）の実装可否。
4. Windows を Experimental のままとする期間と格上げ条件。
