# 長時間稼働の信頼性: 調査・修正・Release Readiness 記録 (2026-10-15)

作業 worktree: `C:\Users\Yuta\AppData\Local\Temp\opencode\gl-stability-20261015`
起点: `origin/main` = `42bf6ac` / ブランチ: `hardening/longrun-stability-20261015`
本番バイナリ (`local-mcp.exe` 稼働中 5 プロセス確認) には触れない。非侵襲の読み取りのみ。

## A. 旧版と最新 main の整理

### A0. 有力とされた旧不具合 `f7a1d1e0` (2026-10-05)

`f7a1d1e0 fix: correct resource bounds found by adversarial review` が以下を修正済み
(最新 main に含まれることを確認):

- **Blocker**: oversized response が writer task を終了させ、receiver drop により
  以後の全 response が黙って破棄される (dispatch だけ継続)。→ substitute 継続に修正。
- **Blocker**: response cap が JSON escape 拡大を無視 (raw 4 MiB → 29 MB frame)。
  → `JSON_ESCAPE_WORST_CASE=7` を導入し compile-time assert。
- **Blocker**: `store_job` の admission 拒否が `JoinHandle` drop =
  detach (生きたプロセスが registry 外に残る)。→ 明示 terminate に修正。
- Major: `execute` の capacity pre-check が foreground 完了可能な要求を誤拒否。
  → pre-check 削除、`store_job` を正門化。
- Major: typed resource marker の文字化により `FailureClass::ResourceLimit`
  が出ない。→ `classify` が marker 優先。
- Major: stdin を reader 生成前に write (pipe 満杯で無期限 block)。
  → reader 先行 + bounded write。

旧リリース版 (稼働中) はこれらを含む前の版であり、全停止事例の有力な説明となる。
ただしこれだけでは全停止を説明できない (下記残存経路)。

### A1. MCP 通信経路 (最新 main の実測)

- Writer は単一 serialized task、`response channel` 容量 64 (`mcp.rs:109`)。
- `write_message` の `write_all`×2 + `flush` に **timeout なし** (修正前)。
  client が stdout を読まなくなると writer が無期限 block。
- channel 満杯で `response_tx.send().await` が block。dispatch は permit
  (`_permit`, `mcp.rs:217`) を保持したまま send (`mcp.rs:255`) するため
  **両 pool が恒久 leak**。read-loop の framing-error 送信
  (`mcp.rs:153/166/183/207`) も block し stdin 読取が停止 (head-of-line)。
- Control admission は `acquire_owned().await` 無期限 (修正前 `mcp.rs:1289`)。
  stalled dispatch が control 16 を埋めると read-loop が次 control で永久停止。
- `write_response_or_substitute` は **全失敗を size 扱い** (修正前):
  broken pipe / stall も substitute 扱いで resource-limit code に化ける。
  戻り値 bool は無視され (`mcp.rs:116`)、writer は死なず・終わらず stall 継続。
- Shutdown drain (`drop(tx); writer_handle.await`, `mcp.rs:261`) も writer stall
  で永久に終わらず `release_all_jobs` に届かない。

→ **最新 main にも残る無期限待機 (実証済み・今回修正)**。

### A2. 承認 IPC

- `approvals::request` (`approvals.rs:190`): connect / write_all / shutdown /
  `read_line` のいずれにも timeout なし (Windows pipe-busy のみ 5 s retry)。
- 120 s (`agent.rs:15 MODEL_TIMEOUT`) が覆うのは `goal_model` の 1 経路のみ
  (`agent.rs:285-300` の `timeout(remaining, request)` + `run_bounded_process`)。
  他の 7 経路は bare `.await`:
  `execution.rs:868` (execute/start_command)、`1347`・`1633` (fallback)、
  `1684` (without_sandbox)、`mcp.rs:835` (Windows write_file)、
  `managed_worktree_prepare.rs:391`、`verifier_git_observation.rs:227`。
- `start()` の pending queue (`VecDeque`, `approvals.rs:422`) は上限なし FIFO。
  先頭未回答が後続を block (head-of-line)。server 側 `read_line`/`write_all`
  も無 timeout。UI 消失時は dispatch + execution permit が恒久 leak
  (execution pool 16 を食い尽くすと新規 execution は即時拒否になるが、
  control は生き残る設計のため session 全体は wedged しない)。

→ **「120 s が全承認経路を覆う」は否認**。今回 request 側に
`APPROVAL_IO_TIMEOUT=10 s` / `APPROVAL_RESPONSE_TIMEOUT=10 min` を追加し
fail-closed (error, never allow) で bound。

### A3. Goal / Job 長時間実行

有界 (長時間処理であり leak ではない):

- `execution.rs:161` 30 s foreground→background、`1711` 20 s host 版、
  `1369` 10 s Codex preflight。
- `agent.rs:279,706` 120 s model deadline + 500 ms cleanup。
- trusted-Git 5/15/30 s + 500 ms (`sandbox.rs`)、stdin 30 s / drain 5 s /
  cleanup 500 ms。
- finished job TTL 15 min、caps 8/session・32/global。

真の無期限 (外部 abort/stop/shutdown が必要):

- `goal_runner` の scheduler/finalizer/prepare awaits + 外側
  `mcp.rs:1153,1181,1184` `spawn_blocking` + `block_on` await。
- `agent.rs:264` `thread::join` (内側 MODEL_TIMEOUT に依存)。
- generic command の `wait_termination` (`sandbox.rs:1255`, 文書上意図的)。
- OS file lock `task_store.rs:674 lock.lock()` (try/timeout なし)。
- dispatch cancel が `execution.rs:893,1702` spawn と `mcp.rs:1024` insert の
  間に割込むと registry 外の live process-group が orphan (refusal 経路の
  明示停止 `mcp.rs:1037` と同形だが cancel 経路にない)。→ 今回は transport
  層の直接 stall ではないため修正対象外とし、残作業に記録。

### A4. Tunnel / 接続境界

- 実機: `local-mcp` 5 プロセス稼働中 (旧版)、`codex` 2 + sandbox-service 1。
  本番 MCP への負荷試験は禁止のため、接続先の切分けはコード境界で実施。
- HTTP 429/404 等 (Tunnel/ChatGPT 側) と MCP 内部無応答は別事象として整理:
  今回の transport stall は **MCP サーバ内部** (stdout 背圧・channel・permit)
  であり、Tunnel エラーとは独立に発生する。
- 実機ログの読み取り可能な範囲では旧版バイナリの内部状態は取得不可
  (プロセス一覧のみ)。欠けている証拠: 旧版の stdout 購読状態・writer task
  生存・channel 占有率。→ 隔離 duplex/mock による再現で代替 (Phase B)。

## B. 再現テスト (隔離・期限付き)

新規 `src/longrun_stability_tests.rs` 7 件 (全て outer `timeout` 付き):

1. `stability_timeouts_are_bounded_and_ordered`
2. `io_failure_is_not_masked_as_size_violation` (FailingWriter → false 即時、
   非 ResourceLimit 型を主張)
3. `oversized_response_is_substituted_and_transport_continues` (17 MiB →
   resource-limit substitute、id 保持、継続)
4. `stalled_writer_times_out_instead_of_hanging` (PendingWriter → 10 s で
   timeout error、非 ResourceLimit)
5. `control_admission_blocks_but_is_releasable_and_execution_fails_fast`
6. `response_channel_send_does_not_hold_permits_forever` (容量1・drain なし →
   bounded send が失敗し permit 解放)
7. `silent_responder_is_bounded_by_timeout_pattern` (無応答 UI に対する
   read_line bounding の機構証明)

実証区分:

- 実証 (修正前 hang / 修正後 pass): 2, 4, 6 の stall 系。3 は `f7a1d1e0`
  由来の継続性を確認する回帰。
- コードから推定 (機構テストで bound を証明): 5, 7。
- 再現せず (対象外・将来作業): dispatch-cancel orphan、OS file lock、
  generic command の意図的無期限待機。

## C. 修正 (最小・互換維持)

- `resource_limits.rs`: `MCP_WRITE_TIMEOUT=10 s`、
  `MCP_RESPONSE_SEND_TIMEOUT=10 s`、`CONTROL_ADMISSION_WAIT_TIMEOUT=30 s`、
  `APPROVAL_RESPONSE_TIMEOUT=10 min`、`APPROVAL_IO_TIMEOUT=10 s` を追加。
  順序 assert を `ceilings_match_the_documented_values` に追加。
- `mcp.rs`:
  - `write_message`: size 先行判定は維持し、各 I/O を write timeout で bound。
    timeout は generic error (ResourceLimit 型にしない)。
  - `write_response_or_substitute`: size のみ substitute。I/O・timeout は
    substitute せず `false` (writer 終了 → receiver drop → send 高速失敗 →
    read-loop が `is_finished` 検知で明示終了)。全失敗の size 扱いを廃止。
  - `send_response_bounded` 新設: channel 送信を 10 s bound。dispatch は
    drop して permit 解放、read-loop は伝播して明示終了。
  - `acquire_admission` control を 30 s bound。期限切れは
    `"control dispatch capacity exhausted"` の bounded resource error。
  - JSON-RPC 互換: frame 境界・id 保持・1 失敗の session 波及を防止。
    正常な長時間処理 (background job 化) は中断しない。
- `approvals.rs`: `request` の machine IPC を 10 s、応答待ちを 10 min で bound。
  失効は fail-closed (error)。UI の yolo/ask 意味は不変。
- 既存テストの削除・弱体化なし。新規 7 件が修正前失敗・修正後成功の組
  (stall 系は outer timeout が修正前 hang を検出)。

## D. 検証

- `cargo fmt --check`: clean (新規 file のみ整形)。
- `cargo check --locked --all-targets --jobs 2`: pass (警告 1 件を修正済み)。
- 新規 `longrun_stability` 7 件: pass (11.63 s)。
- 既存 `resource` 系 66 件 (transport + limits + capture): pass。
- `cargo clippy --locked --all-targets --all-features -- -D warnings`: 実行中→結果追記。
- GitHub Actions CI (Windows/macOS/Linux): Draft PR 作成後に自動実行、ログ確認。
- 実機長時間安定性の証明とは区別: 本検証は隔離 duplex/mock の bound 証明。
  Windows 固有 (Named Pipe) と共通 (writer/channel/permit) を区別して報告。

## E. Release Readiness 監査 (今回範囲)

- スキーマ互換: durable Goal/Task 形式の変更なし。今回の変更は transport
  timeout のみで永続状態に触れない。既存 Session との互換性に影響なし。
- 永続状態 backup/restore・起動時回復: 未変更。本番状態には触れていない。
  復旧試験は合成 fixture (duplex/mock) のみ。
- Release workflow (`release.yml`): dry-run (`workflow_dispatch`) は publish
  不可 (job 条件が `push` + `refs/tags/v*` を要求)。自動実行したのは通常 CI
  のみ。公開・タグ作成・push は未実施 (禁止どおり)。
- パッケージング: Windows exe 名・Linux/macOS 互換・アセット命名に変更なし
  (`local-mcp` 識別子維持、goallatch-maintainer 互換契約どおり)。
- SHA-256・commit 同定: 本ブランチの commit SHA を PR に記録。
  ビルド成果物の再計算は CI の checksums job に委ねる。
- Windows Native Sandbox: 実験的表記を維持。今回の変更は sandbox 境界を
  緩めない (approval fail-closed 維持)。
- `0.2.0-rc.1`: 候補として検討可能だが自動公開は禁止どおり未実施。
- ロールバック: 本変更は timeout 追加のみで、revert すれば旧挙動に戻る。
  ただし revert は stall 再発を意味するため、無期限待機の復活として記録。
- 残作業: dispatch-cancel orphan の明示停止、OS file lock の try/timeout 化、
  approval pending queue の上限・skip 機構、実機での長時間観測
  (本番バイナリ更新は対象外のため未検証)。
