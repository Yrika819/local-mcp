# External Agent Backend V1 — E4 dogfooding record

> 実施日: 2026-11-07
> 対象: `feat/external-e4b-readonly-v1`（stacked: H1 → E4-A → E4-B）
> 目的: DeepAI Code の実モデルを外部エージェントとして使い、GoalLatch が
> Planner → Readonly → Task Verification → Goal Verification → Finalization
> まで **実際に完走** できることを実機で確認する。

## 0. 前提と非目標

- Codex CLI は 1 回も起動しない（EXTERNAL 経路は Codex transport に到達しない）。
- 推論は MCP クライアント側（DeepAI Code）が担当し、GoalLatch は
  Goal 管理・検証・永続化・復旧のみを担当する。
- 既存の `localmcp` セッション、承認デーモン、既存 Goal は変更しない。
  検証は **新しいセッション ID** と **隔離 Git リポジトリ** で行う。

## 1. 実行環境

| 項目 | 値 |
|---|---|
| ホスト | Windows / local-mcp 0.1.2 (debug build from this branch) |
| バイナリ | `target/debug/local-mcp.exe`（`mcp` サブコマンド = stdio JSON-RPC） |
| セッション | `e4c-dogfood-<rand>` / `e4c-reconnect-<rand>` / `e4c-adverse-<rand>` |
| セッション cwd | `%TEMP%\gl-e4c-dogfood\<timestamp>\repo`（新規 `git init`） |
| 状態保存 | `%LOCALAPPDATA%\local-mcp\`（セッション ID で名前空間分離） |
| Goal 対象 | `inventory.txt`（`MARKER=E4C-DOGFOOD-OK` を含む） |

再現手順は `scripts/e4c-dogfood/`:

```text
python scripts/e4c-dogfood/driver_exp1.py    # 実験1: Readonly Goal 完走
python scripts/e4c-dogfood/driver_exp23.py   # 実験2: 中断・再接続 / 実験3: 異常入力
```

ドライバは実 MCP サーバーを起動し、`initialize` → `tools/call` のみで進む。
実際のファイル読み取りには `read_file` ツールを使い、Evidence は
**読んだ内容に基づいて** 生成している。

## 2. 実験1 — Readonly Goal 完走

シーケンスと結果（すべて実 transport 経由）:

| step | 呼び出し | 結果 |
|---|---|---|
| 1 | `goal_start(model_backend=EXTERNAL)` | Goal 作成（revision 1） |
| 2 | `goal_next` | `MODEL_REQUIRED` / `PLANNER` / `planner-proposal-v1` |
| 3 | `goal_submit`(plan) | `COMMITTED` / `PLAN_APPLIED` |
| 4 | `goal_next` | `MODEL_REQUIRED` / `READONLY` / `readonly-investigation-v1` |
| 5 | `read_file` + `goal_submit`(evidence) | `COMMITTED` / `READONLY_CANDIDATE_COMPLETE` |
| 6 | `goal_next` | `HOST_PROGRESS` / status `COMPLETED` / host steps 3 |
| 7 | `goal_result` | `result_kind: COMPLETED` |

永続 Goal 文書（`%LOCALAPPDATA%\local-mcp\goals\<session>\<goal>.json`）の要点:

```json
{
  "model_backend": "EXTERNAL",
  "status": "COMPLETED",
  "plan_revision": 1,
  "revision": 8,
  "tasks": [{"worker": "CODEX_READONLY", "status": "COMPLETED"}],
  "external_model": {
    "receipts": [
      {"decision": "COMMITTED", "committed_revision": 3, "code": "PLAN_APPLIED"},
      {"decision": "COMMITTED", "committed_revision": 5, "code": "READONLY_CANDIDATE_COMPLETE"}
    ],
    "work_history": [
      {"role": "PLANNER", "state": "COMMITTED"},
      {"role": "READONLY", "state": "COMMITTED"}
    ]
  },
  "final_verifications": [
    {"outcome": "PASSED", "source": "HOST_DETERMINISTIC_GOAL_VERIFIER",
     "criterion_results": [{"outcome": "PASSED", "observations": [{"kind": "TASK_VERIFIED"}]}]}
  ]
}
```

Task の evidence は `STRUCTURED_OBSERVATION / source=EXTERNAL_READONLY_AGENT /
requirement_id=inventory-marker` として保存され、Task Verifier の
`VERIFICATION_OBSERVATION / HOST_DETERMINISTIC_VERIFIER` と分離して監査できる。

合格条件の確認:

| 条件 | 結果 |
|---|---|
| Host Verifier が Goal を COMPLETED と判定 | ✅ `final_verifications[].source = HOST_DETERMINISTIC_GOAL_VERIFIER` |
| Task DAG が正式保存 | ✅ `plan_revision = 1` / Task 1 件 |
| Evidence と Revision が整合 | ✅ receipt の `committed_revision` が各 commit の revision と一致 |
| Codex spawn = 0 | ✅ 実行前後で Codex プロセス増加なし（既存 PID は実行開始前から稼働） |
| 無許可のファイル変更 0 | ✅ `git status --porcelain` が実行前後とも空 |
| CODEX 既存経路への退行なし | ✅ `cargo test --locked` 1489 件 GREEN |

## 3. 実験2 — 中断・再接続・再送

| step | 結果 |
|---|---|
| Readonly WorkItem 発行 → クライアント切断（MCP プロセス終了） | durable state に `PENDING` として残る |
| 再接続して `goal_next` | **同一 `work_id` / 同一 `request_digest` / `PENDING` / host steps 0** |
| `goal_submit` | `COMMITTED`（revision 5） |
| 同一 `submission_id` + 同一 payload の再送 | `REPLAY_OF_ACCEPTED_SUBMISSION`、**revision 5 のまま** |
| 同一 `submission_id` + 異なる payload | `CONFLICTING_DUPLICATE` |
| `goal_next` | `HOST_PROGRESS` / `COMPLETED` |

副作用は増えない（revision 不変・receipt 件数不変）。

## 4. 実験3 — 異常入力（fail-closed）

| 入力 | 結果 |
|---|---|
| 未知 Criterion ID を含む Plan | `REJECTED_SCHEMA` / `PROPOSAL_SCHEMA_VIOLATION` |
| 無効 `work_id` | `REJECTED_AUTHORITY` / `UNKNOWN_WORK_ID`（durable write なし） |
| 古い `expected_goal_revision` | `REJECTED_STALE_REVISION` / `GOAL_REVISION_STALE` |
| 同一 `submission_id` + 異なる payload | `CONFLICTING_DUPLICATE` |
| Cancel 後の提出 | `CANCELLED_STALE` / `GOAL_CANCELLED`、Goal は `CANCELLED` のまま |
| 必須 Evidence 不足（無関係な requirement のみ） | 提出は `COMMITTED`、しかし Task は `BLOCKED` / Goal は `RUNNING` のまま。`goal_result` は `NONTERMINAL` |

どのケースでも Goal が `COMPLETED` と報告されることはなく、
作業ディレクトリは無変更（`git status --porcelain` 空）。

## 5. 未検証・制約

- WRITER / REVIEWER / REPLANNER の外部適用は未接続（E5）。したがって
  External Backend V1 全体は未完成であり、本記録は E4 範囲の実機確認である。
- 既存 `localmcp` セッションのバイナリは置換していない。本記録の実行は
  このブランチのビルドを **別プロセス・別セッション** として起動した。
- 依存 Task の READY 伝播（`READINESS_PROPAGATION_DEFERRED`）は未実装のため、
  確定テストは単一 Task 計画で行っている。
- transport 到達数（Codex transport）は 0。GoalLatch 内部からの
  Codex CLI 起動も 0。
