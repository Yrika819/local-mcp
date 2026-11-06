# GoalLatch External Agent Backend V1 — 詳細テストプラン

> **文書状態:** DRAFT / Ownerレビュー待ち（実装前・非規範的）
> **基準:** `main` = `e6950fdc3bab72982285888a81cce822743fbba5`
> **親文書:** `docs/EXTERNAL_AGENT_BACKEND_V1_ARCHITECTURE.md`（PR #30, HEAD `ee231d10e5ed0483751e6fcbda2a09a3d78f97d0`、main未統合のため文書を直接参照）
> **位置づけ:** 基本設計 §11 の最小テストマトリクス T-01〜T-20 を受入項目として詳細化したもの。実装スライス（E3〜）の完了条件を定義する。
> **関連文書:** `EXTERNAL_AGENT_BACKEND_V1_THREAT_MODEL.md`（脅威と対策の対応）、`EXTERNAL_AGENT_BACKEND_V1_API_SCHEMA_DRAFT.md`（B-01〜B-08の確定案）

---

## 0. 全体方針

1. **実モデルをテストに使わない。** すべての受入項目は deterministic fixture・Mock Transport・scripted backend で再現する。実モデルによる確認は E7 の Dogfooding 受入（AT-19）に限定し、1回の実施で完了を主張しない項目は NOT VERIFIED と表示する。
2. **セキュリティ制御を無効化して合格しない。** 承認・sandbox・権限チェックはすべて有効なまま、mock承認チャネルと隔離 fixture で操作する。
3. **Codex 起動回数は stub で機械的に保証する。** EXTERNAL の全経路で `codex_spawn_count == 0` を transport stub（起動が試みられたら即 fail）で検証する（AT-02/AT-17）。
4. **合格基準は状態と証拠で判定する。** モデル出力の文言・「成功」主張は合格根拠にしない。Host Verifier 結果・durable state・receipt のみを根拠とする。
5. テスト順序は（1）対象モジュール単体 →（2）本プランの回帰 →（3）結合 →（4）`cargo fmt` →（5）`cargo check` →（6）`cargo clippy` →（7）全体 `cargo test` →（8）Windows release build。
6. 既存 CODEX 系テスト（planner/readonly/writer/replanner/goal_api の現行 suite）は**全パスを維持**することが前提（AT-01/AT-20）。

用語: **IW** = isolated workspace（一時ディレクトリに作る隔離 fixture + 実 Git リポジトリ）。すべてのテストは IW 内で完結し、許可ディレクトリ外へ触れない。

---

## 1. 受入項目 AT-01〜AT-20（基本設計 T-01〜T-20 の詳細化）

### AT-01（T-01）CODEX モード既存回帰

- **目的:** EXTERNAL 追加が CODEX の既存契約を一切変えないこと。
- **前提:** `model_backend` 省略（既定 CODEX）の Goal。
- **手順:** (1) 既存全テスト suite 実行。(2) `goal_start`〜`goal_run`〜`goal_result` を既存 fixture で実施し、応答フィールド（`result_state` 等）を従来と比較。(3) 旧 GoalStore ファイル（v0.1.2 形式）を読み込む。
- **合格基準:** 既存テスト 100% 成功。既存フィールドの値・意味に変化なし（追加フィールドのみ許可）。旧形式の読込成功。
- **ADR 対応:** ADR-01, ADR-11。

### AT-02（T-02）Codex CLI 未導入環境での EXTERNAL 完走

- **目的:** EXTERNAL が Codex を一切必要としないこと（ADR-08 の中心保証）。
- **前提:** PATH に `codex` が存在しない環境（または codex 実体を起動即 fail に差し替えた stub）。`model_backend: "EXTERNAL"` の Goal。IW に実 Git リポジトリ。
- **手順:** `goal_start` → `goal_next` →（mock 外部 AI が応答）→ `goal_submit` を Planner → Readonly → Writer → Reviewer の各 Role で最後まで進める。全過程で起動監視 stub を有効化。
- **合格基準:** Goal が host-verified 完了まで到達。**Codex プロセス起動 0 回**（stub が 1 回でも呼ばれたら fail）。`goal_run` を EXTERNAL で呼んだ場合は `EXTERNAL_MODEL_REQUIRED` 等の明示応答で終了し、Codex を起動しない。
- **実装メモ:** `ModelTransport` 実装を「呼ばれたら panic/fail」の stub に差し替える test seam が必須。
- **ADR 対応:** ADR-08, ADR-11。

### AT-03（T-03）Criterion ID の整合（省略/単一/複数）

- **目的:** ホスト所有 `CompletionCriterionId` のみが受理され、複数条件でも PLANNING が止まらないこと。
- **手順:** `completion_criteria` を (a) 省略、(b) 1 件、(c) 3 件で Goal を作り、Planner 提案に**正しいホスト ID** の `criterion_bindings` を含めて `goal_submit`。
- **合格基準:** 3 ケースとも提案受理・plan materialize 成功。省略時は objective 由来の 1 条件がホスト生成される。各 binding は host ID と完全一致。
- **関連回帰:** G0-B `criteria_omitted_synthesizes_one_host_owned_criterion_handed_to_planner` / `host_criterion_id_echo_is_accepted_for_one_and_many_criteria`（本 campaigns で追加済み）。
- **ADR 対応:** ADR-05, ADR-07。

### AT-04（T-04）未知 Criterion ID / 創作 ID の拒否

- **目的:** 未知 ID・description の ID 化・異形 UUID を自動補正せず拒否すること。
- **手順:** (a) 発行済みでない UUID、(b) criterion description 文字列、(c) 大文字 UUID、(d) 重複 binding を `goal_submit` で提出。
- **合格基準:** すべて `REJECTED_SCHEMA` 系。Goal revision・plan_revision・durable state が**不変**（バイト一致で確認）。1 度も自動補正・再試行しない。
- **関連回帰:** G0-B `invented_criterion_id_is_rejected_with_the_observed_error` / `description_used_as_criterion_id_is_rejected` / `alternate_uuid_spelling_is_not_auto_corrected` / `duplicate_criterion_bindings_are_rejected`。
- **ADR 対応:** ADR-07、脅威 TM-05/TM-09。

### AT-05（T-05）Readonly 提案の反映

- **目的:** 外部 AI の Readonly 出力がホスト Validator を通過してのみ反映されること。
- **手順:** `goal_next`（role=READONLY）→ mock 応答（structured evidence、bounded）→ `goal_submit`。
- **合格基準:** evidence が bounded 契約を満たす場合のみ Task が VERIFYING へ。契約違反（巨大 evidence、未知 kind、identity 不一致）は拒否で durable state 不変。
- **ADR 対応:** ADR-05, ADR-09。

### AT-06（T-06）Writer 提案と preimage 不一致

- **目的:** `expected_preimage` 不一致の WRITE_UTF8 を一切適用しないこと。
- **手順:** IW にファイルを作成 → Writer 提案（正しい SHA256）を受理。別ケースで、submit 前にファイルを外部から変更して preimage をずらす。ABSENT 指定で既存ファイルがあるケースも実施。
- **合格基準:** 不一致ケースは `REJECTED_PREIMAGE`（相当）で**ファイル内容バイト不変**。一致ケースのみ host-owned 適用成功。二重適用なし。
- **ADR 対応:** ADR-09、脅威 TM-03/TM-04（TOCTOU/二重 Write）。

### AT-07（T-07）Writer + Reviewer + Verifier + Owner 承認

- **目的:** 承認が必要な適用は Owner 承認後のみ、完了判定は Host Verifier のみ。
- **手順:** 正常 Writer 提案 →（mock 承認チャネルで allow）→ 適用 → Reviewer（blocking_findings=0）→ Host Verifier（FILE_DIGEST/GIT_SCOPE 等）→ Goal 完了。deny ケースも実施。
- **合格基準:** allow ケースは適用後に Host Verifier 合格でのみ COMPLETED。deny ケースは適用なし・ブロッカー記録。モデルの「完了」主張のみで COMPLETED にならない。
- **ADR 対応:** ADR-07, ADR-09。

### AT-08（T-08）同一 submission の再送（冪等性）

- **目的:** `submission_id + payload_digest` 同一の再送が同じ receipt を返し、副作用が増えないこと。
- **手順:** `goal_submit` 成功後、**同一内容**で再送（クラッシュ復旧を模擬）。
- **合格基準:** 2 回目は `REPLAY_OF_ACCEPTED_SUBMISSION`（相当）で同一 receipt。Writer 適用回数・evidence 件数・revision 増加がすべて 0。
- **ADR 対応:** ADR-04, ADR-10、不変条件 3。

### AT-09（T-09）同一 submission_id・異なる payload の再送

- **目的:** 冪等キーの衝突を CONFLICT として後発内容を採用しないこと。
- **手順:** 同一 `submission_id` で request_digest の異なる response を再送。
- **合格基準:** `CONFLICTING_DUPLICATE`（相当）。後発 payload は保存も適用もされない。receipt は初回のまま。
- **ADR 対応:** ADR-10。

### AT-10（T-10）別 Session / 別 Goal / 古い Revision / 無効 work_id

- **目的:** 認可境界を ID のみで突破できないこと。
- **手順:** (a) 別 session の `session_id`、(b) 別 goal の `goal_id`、(c) `expected_goal_revision` が古い、(d) 未発行/使用済み `work_id`、(e) 外部が創作した `work_id` を `goal_submit`。
- **合格基準:** すべて拒否。**エラー応答に他 session/他 goal の情報・パス・秘密を含めない**（応答の赤化を機械的に比較）。durable state 不変。
- **ADR 対応:** ADR-07、脅威 TM-02/TM-06/TM-08。

### AT-11（T-11）中断・再起動（クラッシュポイント別復旧）

- **目的:** 基本設計 §6.2 のクラッシュポイント表どおりに復旧すること。
- **手順:** 以下の各ポイントでプロセスを kill し、再起動後に `goal_next` で再開。(1) WorkItem 保存前、(2) 保存後・配信前、((3) 外部推論中、(4) submit 受付前、(5) receipt 保存後・応答前、(6) Writer 適用中、(7) Verifier 途中。
- **合格基準:** (2) は同一 `work_id` 再配布。(5) は同一 receipt 返却。(6) は既存 durable Writer reconciliation に従い UNKNOWN 副作用を勝手に再適用しない。(1)(3)(4) は副作用 0。**WorkItem の重複発行・二重適用が一切ない。**
- **ADR 対応:** ADR-04, ADR-10、不変条件 4/8。

### AT-12（T-12）blocked 提案と実行拒否の分類

- **目的:** モデルの `blocked` 提案・Codex/実行ポリシー拒否・承認拒否・環境不足を記録上区別すること。危険な自動 retry がないこと。
- **手順:** G0-B の原因分類表（`GOAL_ORCHESTRATION_G0B_REGRESSION_NOTES.md` §2.1）の 6 ケースを各 Role で再現。`goal_next` の `NO_ACTION_WITH_REASON` / `blockers[].detail` を確認。
- **合格基準:** 6 ケースが異なる typed なマーカーで区別可能。`BLOCKED`→`NEEDS_REPLAN` の無条件変換がない。goal-level BLOCKED と task-level stall が区別できる。
- **関連回帰:** G0-B `readonly_blocked_proposal_blocks_task_without_replan_and_stays_running` / `readonly_backend_error_is_not_recorded_as_readonly_blocked` / `goal_level_blocked_reports_goal_blocked_no_action`。
- **ADR 対応:** ADR-06、基本設計 §6.3。

### AT-13（T-13）Windows 承認境界（拒否/不在/YOLO）

- **目的:** host-native 操作の承認境界が EXTERNAL でも維持されること。
- **手順:** G0-A の fail-closed マトリクス（`WINDOWS_AUTHORITY_AUDIT_G0A.md` §3）を EXTERNAL の Writer 適用・worktree 作成でも実施。承認拒否・承認チャネル不在・不正応答・切断・YOLO の各ケース。
- **合格基準:** 承認なしに host-native 副作用 0。拒否は fail-closed。YOLO は session 管理者が stdin でのみ設定でき、MCP 経由で変更不能。
- **関連回帰:** G0-A 新規テスト（`request_fails_closed_when_session_disconnects_without_responding` 他）。
- **ADR 対応:** ADR-09、脅威 TM-01/TM-07。

### AT-14（T-14）pause/resume/cancel と遅延提出

- **目的:** 無効化された WorkItem の遅延提出を拒否すること。
- **手順:** `goal_cancel` 後に当該 `work_id` の submit を実施。pause 中 submit、resume 後の同一 WorkItem 再配信も実施。
- **合格基準:** cancel 後 submit は `CANCELLED/STALE`（相当）で拒否、Goal が復活しない。resume 後は同一 `work_id` か明示的な無効化記録のどちらかで、二重状態にならない。
- **ADR 対応:** ADR-04、不変条件 10。

### AT-15（T-15）不正 JSON・巨大 payload・境界値

- **目的:** bounded 拒否で DoS/OOM/無限 retry を防ぐこと。
- **手順:** (a) 不正 JSON、(b) 巨大 response（上限超過）、(c) 巨大配列・深ネスト、(d) 制御文字・非 UTF-8、(e) 上限ちょうどの値を `goal_submit` / `goal_next` に送る。
- **合格基準:** すべて bounded な拒否応答。リトライループ・OOM・panic なし。上限ちょうどの値は受理（境界を1バイト超で拒否）。
- **ADR 対応:** 脅威 TM-09。

### AT-16（T-16）予算超過時の停止

- **目的:** モデル作業予算超過で新規 WorkItem を発行しないこと。
- **手順:** `max_model_work_items` / `max_role_attempts` を小さく設定して超過させる。
- **合格基準:** 超過後は `goal_next` が新規 WorkItem を発行せず `BUDGET_EXHAUSTED`（相当）の理由付き停止。Goal は非成功の説明可能状態。
- **ADR 対応:** 基本設計 §8.1、不変条件 9。

### AT-17（T-17）EXTERNAL 失敗時の Codex fallback 禁止

- **目的:** 失敗・タイムアウト・不正出力のいずれでも自動 Codex fallback が走らないこと。
- **手順:** AT-05/AT-15/AT-16 の失敗ケースに codex 起動監視 stub を併用。`codex_fallback` ツールの EXTERNAL ゴールからの呼び出しも試す。
- **合格基準:** **spawn 0**（計測 + stub の二重保証）。失敗は理由付きの待機/拒否/停止へ。`codex_fallback` は EXTERNAL ゴールに対して拒否または既存承認境界どおりで、黙って実行しない。
- **ADR 対応:** ADR-08, ADR-11。

### AT-18（T-18）goal_result の結果判別

- **目的:** TERMINAL を成功と誤読せず、COMPLETED/FAILED/CANCELLED/BLOCKED を判別できること。
- **手順:** 各終着状態の Goal で `goal_result` を取得。
- **合格基準:** `result_state` は既存どおり `TERMINAL`/`NOT_TERMINAL`。追加の `result_kind`（G0-B で実装: COMPLETED/FAILED/CANCELLED/BLOCKED/NONTERMINAL）で 5 区別。CANCELLED/FAILED を SUCCEEDED 扱いしない。
- **関連回帰:** G0-B `result_kind_discriminates_five_outcomes_without_touching_result_state` / `result_kind_mapping_is_pinned_for_every_goal_status`。
- **ADR 対応:** ADR-06。

### AT-19（T-19）実機 Dogfooding（DeepAI Code）

- **目的:** 実クライアントでの完走証拠。
- **手順:** 隔離 fixture で (a) read-only Goal 1 件、(b) verified mutation Goal 1 件を EXTERNAL で完了。
- **合格基準:** 両方 host-verified 完了。Codex spawn 0 を実測確認。承認 UI の表示/応答を記録。**1 回の実施で汎用的な成功を主張しない**（回数・条件を記録）。
- **ADR 対応:** ADR-12。

### AT-20（T-20）CI と後方互換

- **目的:** Windows/Ubuntu/macOS の既存リリース機能に退行がないこと。
- **手順:** PR CI 全 matrix + 旧 GoalStore 読込 + rollback（EXTERNAL ゴールを作ったバイナリから旧バイナリへ戻す手順の検証）。
- **合格基準:** main 既存テスト退行 0。未実施項目は NOT VERIFIED と明示。downgrade で壊れないこと（読めない新形式は fail-closed に拒否）。
- **ADR 対応:** ADR-08（B-08 migration）。

---

## 2. B-01〜B-08 決定事項と検証の対応

| ID | 本キャンペーンでの確定案（詳細は API_SCHEMA_DRAFT） | 検証する AT |
|---|---|---|
| B-01 | モデル待機は `execution_wait` の**追加 read-only view**。GoalStatus 不変 | AT-18, AT-12 |
| B-02 | 1 Goal につき未完了 ModelWorkItem 最大 1（ADR-10） | AT-08, AT-09, AT-11 |
| B-03 | 複数 MCP クライアントは host session lock + optimistic revision で衝突拒否 | AT-10, AT-11 |
| B-04 | 既定 CODEX、EXTERNAL は明示 opt-in | AT-01, AT-02 |
| B-05 | `goal_next` は「host-only bounded 進行 + 次のモデル要求返却」で維持。肥大化時は E5 で `goal_advance` 分割を再議 | AT-11, AT-16 |
| B-06 | `goal_submit` の transaction 境界は「receipt 保存 + host-owned 状態遷移」を 1 durable commit。長時間副作用（承認・適用）は commit 後の別 phase で receipt により再適用防止 | AT-06, AT-07, AT-08, AT-11 |
| B-07 | raw model response は保存しない。receipt（id/ハッシュ/decision）と bounded な要約のみ | AT-15, TM-10 |
| B-08 | v0.1.2 GoalStore からの additive migration。旧バイナリの rollback は新フィールド無視 or fail-closed | AT-01, AT-20 |

## 3. ADR-01〜ADR-12 との矛盾チェック

| ADR | 本プランでの扱い | 矛盾 |
|---|---|---|
| ADR-01 | EXTERNAL は opt-in（AT-01/02 の既定値確認） | なし |
| ADR-02 | client-driven tool loop（goal_next/goal_submit のみ） | なし |
| ADR-03 | `ModelTransport::invoke` の同期契約は変更しない（EXTERNAL は到達しないだけ） | なし |
| ADR-04 | durable ModelWorkItem（AT-08/09/11） | なし |
| ADR-05 | goal_next=進行+取得 / goal_submit=提出+検証（AT-05〜07） | なし |
| ADR-06 | GoalStatus 破壊的変更なし（AT-12/18） | なし |
| ADR-07 | submit は権限付与でない（AT-04/10） | なし |
| ADR-08 | Codex spawn=0（AT-02/17） | なし |
| ADR-09 | Writer/Verifier/TaskStore/Approval/Worktree 再利用（AT-06/07/13） | なし |
| ADR-10 | 未完了 WorkItem 最大 1（AT-08/09/11） | なし |
| ADR-11 | CODEX 動作保持（AT-01/20） | なし |
| ADR-12 | Experimental 範囲（AT-19 の NOT VERIFIED 表示） | なし |

## 4. 未確定事項（Owner 判断待ち）

1. `max_model_response_bytes` の確定値（暫定 2MiB = 既存 stdout cap 整合。E2 の計測で調整）。
2. `max_model_work_items` / `max_role_attempts` の既定値。
3. `goal_submit` の `response` 上限と `request_digest` の正規化方式（canonical JSON の定義）。
4. `result_kind` / `outcome_summary` の最終フィールド名（G0-B では `result_kind` を先行実装）。
5. AT-19 の Dogfooding 実施回数と判定者。
6. rollback（downgrade）ポリシーの厳密な定義（新形式ファイルの扱い）。
