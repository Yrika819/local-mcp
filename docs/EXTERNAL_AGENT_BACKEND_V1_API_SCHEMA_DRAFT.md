# GoalLatch External Agent Backend V1 — API スキーマ草案（B-01〜B-08 の確定案）

> **文書状態:** DRAFT / Ownerレビュー待ち（実装前・非規範的）
> **基準:** `main` = `e6950fdc3bab72982285888a81cce822743fbba5`
> **親文書:** `docs/EXTERNAL_AGENT_BACKEND_V1_ARCHITECTURE.md`（PR #30）§4/§5/§13
> **位置づけ:** 基本設計 §13 の未決 B-01〜B-08 を、実装可能な粒度まで具体化する。JSON は**説明用の草案**であり、`tools/list` にそのまま載せるものではない（Rust 構造体と MCP の実際の上限に合わせて E3 で確定する）。

---

## B-01 `goal_next` の責務 — 「host-only bounded 進行 + 次のモデル要求の返却」

**確定案:** `goal_next` は次の 2 つを一体で行う。

1. **host-only bounded 進行**: Scheduler/Verifier/適用完了待ちなど、**モデル不要な進行**を最大 `max_host_steps`（暫定 8、上限は E3 で確定）だけ進める。
2. **停止点の返却**: モデル推論が必要になった時点で停止し、`outcome` と（必要な場合）`ModelWorkItem` を返す。それ以上は進めない。

`outcome` の閉じた集合（追加は破壊的変更として審査）:

| outcome | 意味 | 追加フィールド |
|---|---|---|
| `MODEL_REQUIRED` | 外部モデルの応答が必要 | `model_work`, `next_action: "RESPOND_WITH_GOAL_SUBMIT"` |
| `HOST_PROGRESS` | host-only 進行が行われた（まだ続行可能） | `host_steps_executed`, `progress_digest` |
| `WAITING_FOR_APPROVAL` | Owner 承認待ちで進行不能 | `approval_operation`, `pending_work_id?` |
| `BLOCKED` | 外部前提の不足等で進行不能 | `blockers[]`（code/detail/mandatory）、`no_action_reason` |
| `NO_ACTION_WITH_REASON` | 進行対象なし（依存待ち等） | `no_action_reason` |
| `TERMINAL` | Goal 終了 | `result_kind`（COMPLETED/FAILED/CANCELLED）、`result_state` |

**固定事項:**

- `goal_next` は既存の pending WorkItem を**同一 `work_id` + 同一 `request_digest`** で返す。再採番しない。
- `goal_next` は Git 許可・プラン承認・host-native 承認を**代行しない**（すべて既存のホスト経路）。
- transport 切断に依存しない（要求状態は durable store）。
- `max_host_steps` 超過は `HOST_PROGRESS` で返し、ロックを占有し続けない。

**未確定:** `HOST_PROGRESS` と `NO_ACTION_WITH_REASON` の境界（`goal_advance` 分割の要否は E5 で再議 — 肥大化が実測された場合のみ）。

## B-02 `goal_submit` の transaction 境界

**確定案:** `goal_submit` の durable commit は次の原子的単位。

```text
begin (session lock + optimistic revision check)
  1. session 認可 / Goal 所有照合 / WorkItem 照合
  2. expected_goal_revision + request_digest 照合
  3. Role 別 strict validator（planner/readonly/writer/reviewer/replanner）
  4. SubmissionReceipt の作成（decision = COMMITTED / REJECTED_SCHEMA 等）
  5. host-owned 状態遷移（WorkItem: PENDING → COMMITTED/REJECTED、Role 応じた Task/Goal 状態）
  6. 1 durable commit（既存 TaskStore の snapshot + journal + revision 方式）
end
```

- **E4 確定:** Planner と Readonly の「検証 → 適用 → Receipt 確定 → WorkItem 遷移」は上記 6 段を**同一 commit** で行う（H1-2）。`ACCEPTED_FOR_HOST_PROCESSING`（受理のみ）は E3 の後方互換として残るが、E4 のホストは受理だけの状態を作らない。
- **commit の外**に出すのは長時間副作用のみ（Owner 承認待ち、Writer の実ファイル適用、Host Verifier の実行）。Host Verifier は commit 後に `goal_next` の host step として実行する。
- クラッシュ時は receipt の状態が再適用の防止材になる（SUBMITTED で止まった適用は既存 Writer reconciliation に従い、UNKNOWN を勝手に再実行しない）。**E4 追記:** 旧 E3 が残した `SUBMITTED` WorkItem はモデル成果物を永続化していないため適用不能であり、`goal_next` は `EXTERNAL_SUBMITTED_RECOVERY_UNSAFE` の mandatory blocker を返して止まる（推測適用も Receipt 破棄もしない）。
- **冪等キーは `(work_id, submission_id, payload_sha256)` の 3 組**（H1-1）。`submission_id` 単独一致では replay を返さない。`work_id` が一致しない再送は `REJECTED_AUTHORITY / SUBMISSION_WORK_ID_MISMATCH` として fixed code のみを返し、別 Goal・別 Session の情報を漏らさない。
- `decision` の閉じた集合: `ACCEPTED_FOR_HOST_PROCESSING` / `COMMITTED` / `REJECTED_SCHEMA` / `REJECTED_STALE_REVISION` / `REJECTED_AUTHORITY` / `REJECTED_PREIMAGE` / `REPLAY_OF_ACCEPTED_SUBMISSION` / `CONFLICTING_DUPLICATE` / `CANCELLED_STALE` / `BUDGET_EXHAUSTED`。`COMMITTED` は `committed_revision` を必ず伴う。

**固定事項:** 受理 ≠ 成功。`goal_submit` は決して `SUCCESS`/`COMPLETED` を返さない。完了は Host Verifier 達成後の `goal_result` でのみ確定。

**未確定:** Writer 適用（別 phase）の再開順序と journal への記録粒度。E3 で durable commit と適用の順序を実証する（B-06 の検証項目）。

## B-03 WorkItem / Receipt の永続形式

```text
ModelWorkItem (host-authored, durable)
  work_id: UUID                       # opaque handle（権限トークンではない）
  goal_id: UUID, session_id: string   # host-verified 所有関係
  role: PLANNER | READONLY | WRITER | REVIEWER | REPLANNER
  task_id: UUID?                      # PLANNER は None 可
  attempt_id: UUID?                   # Role に応じて必須
  goal_revision: u64, plan_revision: u32, task_revision: u32?
  request_digest: sha256              # canonical request の SHA-256
  request_payload: bounded            # 256KiB 暫定上限
  output_contract_version: string     # 例 "planner-proposal-v1"
  state: PENDING | SUBMITTED | REJECTED | COMMITTED | CANCELLED
  created_at: RFC3339, deadline_at: RFC3339?
  issue_counter: u32                  # 同一 Goal 内の発行連番（監査用）

SubmissionReceipt (durable, append-only 相当)
  work_id: UUID, submission_id: UUID  # submission_id はクライアント生成
  payload_sha256: sha256              # 提出 response のハッシュ（raw は保存しない）
  decision: （B-02 の閉じた集合）
  committed_revision: u64?            # host-owned commit 成功時の revision
  code: string, redacted_detail: string?
  received_at: RFC3339
```

- **raw model response は保存しない**（B-07 / TM-10）。監査は id・ハッシュ・decision・状態遷移で行う。
- receipt は bounded durable set（上限・圧縮方針は E3 で確定。上限到達時は古い receipt を**要約に潰す**が、冪等性に必要な `submission_id + payload_sha256` の照合表は保持する）。
- WorkItem の `request_payload` は各 Role の既存 request 生成ロジック（`planner_request_for_goal` 等）から**ホストが**構築する。外部は内容を指定できない。

## B-04 既存 GoalStore からの migration（B-08 と一体）

**確定案:** additive migration のみ。

1. 新フィールド（`model_backend`, `external_model`）は **optional** に追加。旧形式ファイルは `CODEX` 扱いで読み込む。
2. 既存フィールドの意味・enum・`result_state` を変更しない（ADR-06）。
3. 旧バイナリの rollback: 新フィールドを**無視**できる形（unknown field 拒否の struct にはしない）か、読み取りを fail-closed に拒否するかのどちらかを E3 で検証し、片方に固定する。**方針案:** Goal 本体は unknown-field 許可で downgrade 耐性を持たせ、**権威フィールド（scope/preimage/権限関連）だけ deny_unknown_fields を維持**する。
4. `schema_version` を持ち、新→旧→新 の往復を AT-20 で検証。

**未確定:** downgrade 時に `external_model` データを保持するか破棄するか（保持推奨。破棄する場合も receipt の監査コピーは残す）。

## B-05 CODEX との互換性

**確定案:**

- `model_backend` は **Goal 作成時のみ**指定可能な immutable な host policy。既存 Goal / 省略時は `CODEX`。
- `goal_run` は CODEX で従来どおり。EXTERNAL Goal では `EXTERNAL_MODEL_REQUIRED`（相当の明示 outcome）を返し、**Codex を起動しない**。黙って CODEX にフォールバックしない。
- `codex_fallback` ツールは EXTERNAL ゴールに対して拒否（または既存承認境界どおりで自動実行しない）。計測上も spawn 0。
- 既存の `ModelTransport::invoke` 同期契約は変更しない（ADR-03）。EXTERNAL はその経路に**到達しないだけ**。
- 利用計測（E2 の `codex_spawn_count` 等）で CODEX の既存挙動不変を機械的に確認する。

## B-06 複数 MCP クライアントの競合防止

**確定案:** V1 は client binding / lease を導入せず、既存機構で防ぐ。

1. **session lock**（既存 TaskStore のセッションロック）+ **optimistic revision check**（`expected_goal_revision`）で同時 submit の衝突を拒否。後勝ちはさせない（先勝ち + 後発は `REJECTED_STALE_REVISION`）。
2. 同一 WorkItem への二重 submit は receipt の `submission_id + payload_sha256` 照合で冪等化（同一）/ CONFLICT（異 payload）。
3. `goal_next` は session 単位で 1 非終端 Goal の既存制限をそのまま維持（`ACTIVE_GOAL_ALREADY_EXISTS`）。
4. 2 クライアントが交互に `goal_next` しても WorkItem は同一（再採番なし）。

**残存リスク:** 2 クライアントが同一 WorkItem に別 payload をほぼ同時提出した場合、片方は CONFLICT になる（データ破壊なし）。lease が必要かは運用実測後に V2 で判断。

## B-07 ModelWorkItem の上限

**確定案（暫定値。E2 の計測で調整）:**

| 項目 | 上限 | 根拠 |
|---|---|---|
| 1 Goal の未完了 ModelWorkItem | **1**（ADR-10） | レース・再送・監査の単純化 |
| `max_model_work_items`（Goal 生涯の発行数） | 暫定 64 | Role 別 attempt 上限 × Role 数 + 余裕 |
| `max_role_attempts`（Role 別） | 暫定 8 | 既存 semantic attempt 預算の考え方を踏襲 |
| `request_payload` | 暫定 256KiB | 既存 `REPLANNER_REQUEST_MAX_BYTES` と整合 |
| `max_host_steps` / `goal_next` 呼び出し | 暫定 8 | 1 呼び出しのロック占有防止 |
| `max_goal_wall_duration` | 暫定 8h（Soak と整合） | 長期占有の防止 |

超過時の動作: 新規 WorkItem 発行 0 + `BUDGET_EXHAUSTED` + Goal は非成功の説明可能状態（不変条件 9）。

## B-08 モデル応答サイズの上限

**確定案:**

| 項目 | 上限 | 根拠 |
|---|---|---|
| `goal_submit` の `response` 全体 | 暫定 **2MiB** | 既存 `MODEL_STDOUT_CAP`（2MiB）と整合（アーキテクチャ §1 確認済み基線） |
| JSON ネスト深さ | 暫定 32 | DoS 防止（serde の再帰爆弾対策） |
| 配列要素数 | Role 既存上限に従う（tasks 1..=128 等） | 既存 validator の上限を二重化しない |
| 文字列フィールド | Role 既存上限（summary 8192B 等） | 同上 |

- 上限超過・不正 JSON・非 UTF-8 は**bounded な拒否**（`REJECTED_SCHEMA`）で、無限 retry・OOM・panic を起こさない。上限ちょうどの値は受理。
- 生 response は保存しないので、上限は「受理時の解析バッファ」にのみ効く（B-07）。

---

## goal_submit 入出力（草案）

```json
// request
{
  "session_id": "<actual-session>",
  "goal_id": "<uuid>",
  "work_id": "<host-generated-uuid>",
  "submission_id": "<client-generated-uuid>",
  "expected_goal_revision": 1,
  "request_digest": "<sha256-from-goal_next>",
  "response": { }
}
// response
{
  "outcome": "ACCEPTED_FOR_HOST_PROCESSING",
  "receipt": {
    "work_id": "<uuid>",
    "submission_id": "<uuid>",
    "payload_sha256": "<sha256>",
    "decision": "ACCEPTED_FOR_HOST_PROCESSING",
    "code": "OK"
  },
  "goal_state": {
    "status": "RUNNING",
    "result_state": "NOT_TERMINAL",
    "result_kind": "NONTERMINAL",
    "execution_wait": "NONE",
    "next_action": "CALL_GOAL_NEXT"
  }
}
```

- `execution_wait` は `pending_work` から派生する read-only view（B-01/ADR-06）。`WAITING_FOR_EXTERNAL_MODEL` 等。
- `result_kind` は G0-B で先行実装済みの追加フィールド（`goal_result` と同義）。

## 状態遷移（ModelWorkItem）

```text
            goal_next (host issue)
                  │
                  ▼
               PENDING ──── goal_cancel / revision invalidation ──▶ CANCELLED
                  │
                  │ goal_submit (strict validation passed)
                  ▼
              SUBMITTED ─── host phase (approval / apply / verify)
                  │                │
       (validation failed)         ├─ success ──▶ COMMITTED
                  ▼                └─ rejected ─▶ REJECTED
               REJECTED
```

- `PENDING` はクライアント切断後も `goal_next` で再取得可能（同一 ID）。
- `SUBMITTED` は「受け取った」だけで、適用成功ではない。
- `COMMITTED` はホストの検証・durable commit 完了。Goal の `COMPLETED` とは別軸。

## ADR との整合

本草案は ADR-01〜ADR-12 と矛盾しない（対応表は `EXTERNAL_AGENT_BACKEND_V1_TEST_PLAN.md` §3）。特に: 既定 CODEX 維持（ADR-01/11）、client-driven loop（ADR-02）、同期契約不変（ADR-03）、durable WorkItem（ADR-04）、役割分担（ADR-05）、GoalStatus 不変（ADR-06）、submit は権限でない（ADR-07）、spawn=0（ADR-08）、既存ホスト権限の再利用（ADR-09）、1 pending WorkItem（ADR-10）、Experimental 範囲（ADR-12）。

## 未確定事項の一覧（Owner 判断）

1. `max_host_steps` / 各予算の既定値（B-07 表の暫定値）。
2. receipt の圧縮・保持方針（冪等性照合表の最小保持範囲）。
3. downgrade 時の `external_model` 扱い（B-04）。
4. `outcome` 集合の最終確定と `goal_advance` 分割の要否（B-01）。
5. canonical JSON（`request_digest` の正規化）の定義。
6. `decision` コード名の最終確定（本文書の名称は草案）。

---

## E4 実装追記（H1-2 / E4-A / E4-B で確定した事項）

> 追記日時: 2026-11-07 / 対象: `feat/external-e3-hardening-v1` 以降の stacked PR

### 1. goal_submit の role 別適用

| role | 検証 | 同一 commit で行う適用 | 成功時の outcome / decision |
|---|---|---|---|
| PLANNER | 既存 Phase 4 strict validator（criterion ID・worker kind・scope・revision・DAG 循環・writer 順序） | Task DAG materialization + Criterion Binding + plan revision 更新 | `COMMITTED` / `COMMITTED`（code `PLAN_APPLIED`） |
| READONLY | 既存 readonly parser（identity・revision・schema・evidence 上限） | Attempt への evidence/worker report 記録 + Candidate Complete / Blocked / NeedsReplan / Failed の Task 遷移 | `COMMITTED` / `COMMITTED`（code は `READONLY_*`） |
| WRITER / REVIEWER / REPLANNER | 未接続（E5） | なし | `REJECTED_SCHEMA` / `REJECTED_SCHEMA`（code `ROLE_NOT_WIRED_EXTERNAL_V1`） |

- 適用は **モデルの成功宣言ではなくホストの状態表**で決まる。`candidate_complete` は `VERIFYING` への遷移要求にすぎず、Task 完了は既存 Task Verifier の Pass のみ。
- 外部 readonly の evidence は `source = EXTERNAL_READONLY_AGENT` として保存され、Codex readonly の `CODEX_READONLY` と区別できる（監査用）。

### 2. goal_next の host step

1 呼び出しは最大 `max_host_steps`（既定・上限 8）の host step を実行し、次のいずれかで停止する。

| 停止理由 | outcome |
|---|---|
| モデル推論が必要（Planner / Readonly WorkItem 発行） | `MODEL_REQUIRED` |
| Task Verifier / Goal Verifier / Goal Finalizer / retry re-arm を実行 | `HOST_PROGRESS`（terminal 到達時は `next_action: OBSERVE_TERMINAL_STATE`） |
| role 未接続（Writer/Reviewer/Replanner） | `NO_ACTION_WITH_REASON` + `EXTERNAL_ROLE_NOT_WIRED` |
| 予算・validator 拒否・旧 SUBMITTED | `BLOCKED` + mandatory blocker |
| それ以外の Scheduler `NoAction` | `NO_ACTION_WITH_REASON` + 型付き理由 |

- step は毎回 1 durable commit で前進し、`Progress` を返したのに revision が変わらない場合は同一状態を繰り返さず停止する。
- モデル推論待ちの間、Session Lock は保持しない（推論はクライアント側）。

### 3. 既知の未接続（E5 以降）

- WRITER / REVIEWER / REPLANNER の外部適用。
- 依存 Task の READY 伝播（`READINESS_PROPAGATION_DEFERRED`）。E4 の確定テストは単一 Task 計画で検証している。
- Writer の実ファイル適用に伴う preimage / side-effect 管理。
