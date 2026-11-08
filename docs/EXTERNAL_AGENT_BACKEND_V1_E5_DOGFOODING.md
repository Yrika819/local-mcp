# External Agent Backend V1 — E5 dogfooding record (Writer / Reviewer, real MCP)

> 実施日: 2026-11-08
> 対象ブランチ: `feat/external-e5-0-dag-readiness`（E5-0 → E5-A1 → E5-A2 → E5-B）
> 目的: DeepAI Code を外部エージェントとして使い、GoalLatch が
> **Writer 提案の検証 → 私的 Stage → Host Publication → Reviewer → Host Verification**
> までを実 MCP transport で完走できることを確認する。

## 0. 前提

- Codex CLI は 1 回も起動しない（EXTERNAL 経路は Codex transport に到達しない）。
- 推論は MCP クライアント側（DeepAI Code）が担当し、GoalLatch は検証・Stage・
  Publication・ReviewGate・Task/Goal Verification を担当する。
- 実行は **このブランチのビルドを別プロセス・別セッション** として起動した。
  既存 `localmcp` サーバー（v0.1.1）は置換も再起動もしていない。
- 隔離 Git リポジトリを `%TEMP%\gl-e5e-dogfood\<timestamp>\` に新規作成した。

再現:

```text
LOCAL_MCP_REPO_ROOT=<repo> python scripts/e5e-dogfood/driver_e5e.py
```

## 1. Scenario B — Writer + Reviewer 完走

| step | 呼び出し | 結果 |
|---|---|---|
| 1 | `goal_start(model_backend=EXTERNAL)` | Goal 作成 |
| 2 | `goal_next` → `goal_submit`(plan) | `COMMITTED` / `PLAN_APPLIED` |
| 3 | `goal_next` | `MODEL_REQUIRED` / `WRITER` / `writer-proposal-v1` |
| 4 | `goal_submit`(WRITE_UTF8 proposal) | `ACCEPTED_FOR_HOST_PROCESSING` / `WRITE_INTENT_PREPARED` |
| 5 | `goal_next` | `MODEL_REQUIRED` / `REVIEWER` / `reviewer-verdict-v1`（この step で Host Publication 実行） |
| 6 | `goal_submit`(review verdict) | `COMMITTED` / `REVIEWER_RECORDED` |
| 7 | `goal_next` | `HOST_PROGRESS` / `status = COMPLETED` |
| 8 | `goal_result` | `result_kind = COMPLETED` |

観測値:

```json
{
  "writer_submission": {"outcome": "ACCEPTED_FOR_HOST_PROCESSING", "code": "WRITE_INTENT_PREPARED"},
  "publication_step": {"outcome": "MODEL_REQUIRED", "role": "REVIEWER"},
  "reviewer": {"outcome": "COMMITTED", "code": "REVIEWER_RECORDED"},
  "final": {"outcome": "HOST_PROGRESS", "status": "COMPLETED", "result_kind": "COMPLETED"},
  "content_digest": "9afd59a24deeb0bd89188c743e6fc6f781d954906fe90b5810e854be23923670",
  "git_status": " M greeting.txt\n",
  "git_diff": " greeting.txt | 2 +-\n 1 file changed, 1 insertion(+), 1 deletion(-)\n"
}
```

- 適用されたファイル内容は **Stage に封印したバイト列と完全一致**（`greeting.txt`
  のみが変更、他ファイルは無変更）。
- 変更は 1 ファイル・1 行のみで、Goal は Host Task Verifier（`FILE_DIGEST` +
  `REVIEW_GATE`）と Host Goal Verifier によってのみ `COMPLETED` と判定された。
- Reviewer の verdict だけでは Task は `VERIFYING` に留まり、`COMPLETED` にはならない。

### 事前に観測した拒否（fail-closed の実証）

同じドライバの初版は、Python のテキストモードが改行を変換したため preimage の
SHA-256 が実ファイルと一致せず、`WRITER_PREIMAGE_MISMATCH` で **拒否**された。
これはホストの preimage 強制が実 transport でも効いていることの実証であり、
ドライバ側をバイト単位に修正して再実行した。

## 2. Scenario D — 中断・再接続（Publication 前後）

| 時点 | 観測 |
|---|---|
| Writer 提出直後（Stage 済み・未公開）にプロセス終了 | `greeting.txt` は**未変更**（`content_unchanged: true`）。WorkItem と Stage は durable |
| 再接続して `goal_next` | Stage から**同一バイト列**を公開し、`REVIEWER` WorkItem を返却（`content_is_staged_bytes: true`） |
| 公開後に再度プロセス終了 → 再接続 | 内容は**変化なし**（二重適用なし）。Reviewer WorkItem は**同一 `work_id`** で再配信 |
| verdict 提出 → `goal_next` | `COMPLETED` |

- モデルへの再要求は 1 回も発生していない（Stage の durable bytes を使用）。
- `MutationIntent` は `Applied`、Reviewer は `Persisted` で確定し、公開は 1 回のみ。

## 3. 未実施（NOT VERIFIED）

- **Scenario A**（Multi-task Readonly）: E5-0 の決定論テスト（AT-E5-001..012）で
  多段 DAG は検証済みだが、E5 バイナリでの実機再実行は未実施。
- **Scenario C**（NeedsReplan → Replanner）: E5-C 未実装のため未実施。
- Managed Worktree（E5-D）連携の実機確認: 未実装のため未実施。
- Linux / macOS 実機 dogfooding: 未実施（CI のみ）。
- Reviewer 中断後の **復帰**（`RunReviewer` からの再開）は
  `REVIEWER_RECOVERY_DEFERRED` として明示停止する（推測再開はしない）。

## 4. Model accounting

- GoalLatch 内部からの Codex CLI spawn: **0**。
- Codex transport 到達: **0**（external モジュールは transport 型を参照しない）。
- 外部推論はすべて MCP クライアント（DeepAI Code）側。
