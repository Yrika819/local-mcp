# E5 hardening — real-MCP dogfooding: Scenario A (multi-task readonly) and C (NeedsReplan)

> 実施日: 2026-11-08
> ブランチ: `feat/e5-c-replanner`（E5-H2 → E5-B2 → E5-C）
> 目的: E5 の多段 DAG と外部 Replanner を **実 MCP transport** で確認する。

## 0. 前提

- 別プロセス・別セッション ID、隔離 `git init` リポジトリ（`%TEMP%\gl-e5e-ac\<ts>\`）。
- 既存 `localmcp` サーバー（v0.1.1）は置換も再起動もしていない。
- 推論は MCP クライアント（DeepAI Code）側。GoalLatch は検証・適用・検証・永続化を担当。
- 再現: `LOCAL_MCP_REPO_ROOT=<repo> python scripts/e5e-dogfood/driver_e5e_ac.py`
- **区別**: 本記録の計画・調査回答・再計画は、ホストが発行した request を読んだ
  エージェントがその場で生成したものであり、固定 JSON を返すだけのスクリプト試験
  （`src/external_*_tests.rs`）とは別種の証拠である。

## 1. Scenario A — Multi-task Readonly DAG

Plan: `inspect-inventory` → `inspect-notes`（依存あり、両方 mandatory、criterion は両方に束縛）。

| step | 結果 |
|---|---|
| `goal_start(EXTERNAL)` → `goal_next` → `goal_submit`(plan) | `COMMITTED` / `PLAN_APPLIED`（rev 3） |
| `goal_next` → `goal_submit`(inventory evidence) | `COMMITTED` / `READONLY_CANDIDATE_COMPLETE`（rev 5） |
| `goal_next`（Task 検証 → readiness 伝播 → 次 WorkItem 発行） | `MODEL_REQUIRED` / `READONLY`（notes） |
| `goal_submit`(notes evidence) | `COMMITTED`（rev 9） |
| `goal_next` | `HOST_PROGRESS` / `status = COMPLETED` |

永続状態（`%LOCALAPPDATA%\local-mcp\goals\<session>\<goal>.json`）:

```json
{"status": "COMPLETED", "plan_revision": 1, "revision": 12,
 "tasks": [["Inspect inspect-notes","COMPLETED",1],["Inspect inspect-inventory","COMPLETED",1]],
 "receipts": [["COMMITTED",3,"PLAN_APPLIED"],["COMMITTED",5,"READONLY_CANDIDATE_COMPLETE"],["COMMITTED",9,"READONLY_CANDIDATE_COMPLETE"]],
 "work_history": [["PLANNER","COMMITTED"],["READONLY","COMMITTED"],["READONLY","COMMITTED"]],
 "final_verifications": [["PASSED","HOST_DETERMINISTIC_GOAL_VERIFIER"]]}
```

- 依存 Task は先行 Task が host-verified COMPLETED になるまで PENDING のままで、
  readiness 伝播が同一呼び出し内で arm → 発行した。
- 作業ツリーは無変更（`git status --porcelain` 空）。

## 2. Scenario C — NeedsReplan → External Replanner

| step | 結果 |
|---|---|
| plan + readonly 回答（`status: needs_replan`） | `COMMITTED` / `READONLY_NEEDS_REPLAN`（rev 5） |
| `goal_next` | `MODEL_REQUIRED` / **REPLANNER**（`replanner-proposal-v1`） |
| `goal_submit`(replan: 前提 Task 追加 + trigger 解決) | `COMMITTED` / **`REPLAN_APPLIED`**（`committed_revision = 7`） |
| `goal_next` ×2（前提 → 元 Task） | `READONLY` を順に発行、各 `COMMITTED` |
| `goal_next` | `COMPLETED`（`plan_revision = 2`） |

```json
{"status": "COMPLETED", "plan_revision": 2, "revision": 16,
 "receipts": [["COMMITTED",3,"PLAN_APPLIED"],["COMMITTED",5,"READONLY_NEEDS_REPLAN"],
              ["COMMITTED",7,"REPLAN_APPLIED"],["COMMITTED",9,"READONLY_CANDIDATE_COMPLETE"],
              ["COMMITTED",13,"READONLY_CANDIDATE_COMPLETE"]],
 "work_history": [["PLANNER","COMMITTED"],["READONLY","COMMITTED"],["REPLANNER","COMMITTED"],
                  ["READONLY","COMMITTED"],["READONLY","COMMITTED"]],
 "final_verifications": [["PASSED","HOST_DETERMINISTIC_GOAL_VERIFIER"]]}
```

- 再計画は plan revision を 1 だけ進め、新しい前提 Task を READY にし、trigger を
  PENDING に戻した（同一 commit）。
- 無限再試行なし: 2 回の readonly 成功で Goal 完了。作業ツリー無変更。

## 3. 観測した安全性

- Codex CLI spawn: **0**（両シナリオとも。GoalLatch 内部からの起動なし）。
- 未承認操作: 0（readonly のみで、書込み経路に到達していない）。
- 重複 Write: 0（書込み自体なし）。
- Goal 完了は `HOST_DETERMINISTIC_GOAL_VERIFIER` の PASSED のみ。

## 4. 未実施（NOT VERIFIED）

- Scenario B / D（Writer / Reviewer / crash）は PR #43/#44/#45 のドライバで確認済みだが、
  本ブランチのバイナリでは再実行していない（コードは同一系列）。
- Managed Worktree × EXTERNAL（E5-D）: **未実装**。
- Linux / macOS 実機での dogfooding: 未実施（CI のみ）。
- Reviewer 中断復帰の実機確認: 決定論テストのみ。
