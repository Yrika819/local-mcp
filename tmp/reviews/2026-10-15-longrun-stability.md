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

## R1-R5. 独立レビュー指摘への再対応 (PR #11 同一ブランチ、未マージ)

レビューで追加の懸念 4 件 + テスト監査 1 件。いずれも同一ブランチで
再現テスト (修正前失敗確認) → 最小修正 → 回帰 → CI の順で対応。

### R1. writer 終了後に read loop が next_frame に残る

- 実証: `is_finished()` の loop 先頭チェックだけでは、追加入力のない stdin で
  `next_frame().await` に永久残留。新規テスト 2 件
  (`broken_writer_ends_transport_without_further_input`,
  `stalled_writer_ends_transport_without_further_input`) が修正前に失敗
  (各 outer deadline で hang 検出、30.03 s で 2 件とも FAILED)。
- 修正: `tokio::select!` で `next_frame` と `writer_handle` を同時待ち。
  writer 側が先に終われば loop を抜けて明示終了。半読み `next_frame` の
  cancel は安全 (保持バイトは reader 側に残り、本 arm は必ず loop 脱出)。
- 修正後: 2 件とも pass (10.02 s)。正常 EOF 経路・frame 境界・admission 意味は不変。

### R2. 承認 10 分期限切れ後の stale pending

- 実証 (コード): request 側 timeout 後も server の `VecDeque` に
  `(Request, Stream)` が残り、(a) operator の回答 `write_all(...).await?` が
  死 stream で Err → `handle_input` の `?` → `start()` 終了 (UI 死亡)、
  (b) yolo drain・未認識入力も同形、(c) stale が先頭 block。
- 修正: `PendingApproval { request, stream, enqueued }` を導入。
  回答は best-effort `answer()` (10 s bound、失敗 swallow、pop 先行)。
  `evict_expired()` が `APPROVAL_RESPONSE_TIMEOUT` 超の先頭連続 corpses を
  破棄 ([expired] を operation+id のみで表示、detail 非出力)。
  Evict は何も許可・拒否しない (client は既に fail-closed 終了済み)。
- テスト 7 件 (`approval_stability_tests`、phase0 include 経由で二重実行=14):
  dead 回答でも UI 生存、期限切れ evict 後に live 承認へ `allow\n` 到達、
  yolo drain 生存。既存 phase0 契約 (FIFO・allow/deny・yolo・allow/revoke)
  は helper の型更新のみで assertion 不変 (弱体化なし)。

### R3. send 失敗時の個別レスポンス破棄

- 整理: dispatch 同時最大 32 < channel 64 のため、live writer 下で dispatch
  送信だけが詰まることはない。送信失敗は writer 死亡/stall を意味し、
  R1 の select が transport を明示終了させる。破棄は transport EOF として
  client に表面化し、id 単位の無限待ちにならない。
- 修正: 上記因果を dispatch コメントに明記。加えて writer 失敗経路でも
  `release_all_jobs()` を実行してから error を返すよう順序修正
  (PR #11 時点では `??` が先に return し cleanup を skip する新 orphan が
  あったため)。in-flight dispatch の扱いは EOF 経路と同一
  (serve return → runtime 終了で cancel)。
- テスト: R1 の 2 件が transport 終了 (明示 error、hang なし) を証明。

### R4. server 側 accept 後の read_line head-of-line

- 実証 (コード): 旧 accept arm は `read_line().await?` 無期限 + `?` 伝播。
  部分送信・無応答が単一 accept/read 順序を占有し後続接続を block、
  切断・garbage は `?` で UI 全体を死亡させた。
- 修正: `read_session_message_with_timeout` (production は
  `APPROVAL_IO_TIMEOUT`) に抽出。accept arm は失敗時に当該接続を破棄して
  継続 (`eprintln` のみ)。yolo 即時許可の write も best-effort。
- テスト 4 件: 正常 frame 受理、garbage 拒否 (hang なし)、部分送信 timeout、
  切断 fail-closed。いずれも duplex 隔離・outer deadline 付き。

### R5. 既存 7 件の監査

- 7 件は production 関数 (`write_message`, `write_response_or_substitute`,
  `send_response_bounded`, `acquire_admission`, `admission_pool`, 定数) を
  直接呼ぶ真正な検証。不足は統合経路
  (`serve_with_io` end-to-end、承認 server 側、stale queue) であり、
  今回 2 + 7 件で充填。`silent_responder` は機構証明に留まる旨を明記
  (request 側 production bound は Windows pipe 実テストとコードで担保)。

### 再監査 (新デッドロック・孤立・権限)

- デッドロック: select 両 arm は即時 break/継続、await 跨ぎの lock なし、
  evict は同期 VecDeque 操作のみ。`answer` は pop 後に bound 実行のため
  先頭 block なし。FIFO + 単調 Instant のため front-only evict で完全。
- 孤立: writer 失敗経路の cleanup 順序を修正済み。承認 evict は死 stream
  解放 (旧は回答まで蓄積)。権限: evict は何も付与せず、best-effort 回答は
  operator 決定済み verdict のみ、yolo 意味不変、pipe からの yolo 有効化なし
  (既存テスト pass)、秘密情報のログなし。
- 残存: stdin 非 EOF I/O error 時の早期 return (修正前からの挙動、今回は不変)、
  transient channel-full 時の単発 drop (上記因果で transport 終了に収束)、
  dispatch-cancel orphan・file lock・実機長時間観測 (前回報告どおり未検証)。

### R2 対応の検証

- `cargo fmt --check`: clean。
- `cargo check --locked --all-targets --jobs 2`: pass、警告なし。
- 新規・更新: longrun 9 件 pass (20.24 s)、approval 系 62 件 pass、
  transport/resource 既存は次回 CI + 個別実行で確認。
- `cargo clippy --locked --all-targets --all-features -- -D warnings`: clean。
- GitHub Actions: PR #11 更新後に自動実行、結果を追記予定。

### CI 失敗と修正 (PR #11、同一ブランチ)

1. `test (windows-x64)` が `expired_approvals_are_evicted...` で失敗
   (2 件残存 vs 1 件期待)。原因はテストヘルパーの
   `Instant::checked_sub(660s)` が起動直後の CI マシン (uptime < 11 分) で
   アンダーフロー → entry が fresh 扱い。生産コードは無罪
   (evict 判定は `elapsed` 参照)。修正は時刻注入 (`evict_expired_at`、
   前方向 checked_add + saturating_duration_since) + fresh 非 evict の
   assertion 追加 (`537372c`)。
2. 続いて `quality (ubuntu-24.04)` が `cargo fmt --check` で失敗。
   上記修正コミット直前の fmt 実行と最終編集の順序が逆で、整形差分が
   残ったまま push していた。eviction helper の空白のみ修正 (`4880996`)、
   fmt clean + approval_stability 14 件 pass を確認して push。
   教訓: コミット直前に必ず `cargo fmt --all -- --check` を実行する。
   (以前の `echo $?` 方式では Select-Object の終了状態を見ており、
   `cargo fmt` 自体の成否を見ていなかった。今後は `$LASTEXITCODE` を使用。)
3. API rate limit: CI 監視の `gh api` 連打で 403 を踏んだ。監視は間隔を
   空けた `gh pr checks` のみに制限する。

### 最終 CI 結果 (HEAD 4880996)

- PR #11 の全 22 checks が SUCCESS (quality + test + compat の
  Windows/macOS/Linux matrix)。`test (windows-x64)` の再発なし。
- マージ・タグ・リリースは未実施 (Draft 維持)。本番バイナリ・Tunnel・
  永続 Goal 状態・既存未追跡ファイルは無改変。

## R6. 独立レビュー第3ラウンド (同一ブランチ、未マージ)

### 最優先: 承認の表示/回答の取り違え

- 実証 (コード): 新規到着ごとに `show_request(新規)` を表示する一方、
  回答は FIFO 先頭を pop。A 表示→B 到着表示→"y" で A が承認される。
  最後に表示された操作と承認される操作が一致しない。
- 修正: `enqueue_approval` が常に head を表示して表示 id を返す
  (到着時は head が新規自身なので従来表示と同一、2 件目以降は
  "Queued …; awaiting decision on:" + head 再表示)。
  `answer_head` が決定内容 `(id, operation)` を返し、y/n/未認識の各 arm が
  "Allowed/Denied <operation> <id>" を確認表示。yolo 即時許可は逐次表示の
  まま (到着=決定で乖離なし)。FIFO・allow/deny・fail-closed 不変。
- テスト: 表示==回答の対応付け (連続到着・回答後追加到着)、期限切れ後も
  対応維持、切断でも FIFO 維持、の 3 件 + 既存 phase0 FIFO 契約は不変で pass。

### MCP 終了経路の統一

- 監査で早期 return (release_all_jobs 迂回) 4 箇所を特定:
  stdin 非 EOF I/O error の `?`、read-loop 送信 error の `?` ×3、
  writer JoinError の `?`。いずれも cleanup なしで return していた。
- 修正: frame 処理を `process_one_frame` に抽出 (`continue`→`Ok`) し、
  read 失敗は `read_error` に記録して break、writer 結果も格納して break。
  全終了は `finish_transport` (drain→release→report の単一経路) のみ。
  error 優先順位は `report_shutdown_outcome` に固定し unit test。
  `release_all_jobs` 呼び出しは全コードで 1 箇所のみ (grep 検証)。
- テスト: LeaveRegistry のみで全 exit の明示終了を証明
  (stdin error→Err、writer 死亡→Err、EOF→Ok)。
  グローバル registry を触る e2e は並列テスト干渉
  (phase0_mcp の legacy job を release_all が巻き込む) のため採用せず、
  制限として明記。単一 tail + precedence unit + 既存全 suite の無変化で担保。

### 追加監査: 入力サイズ・pending 上限・低速大量接続

- `MAX_APPROVAL_MESSAGE_BYTES=1MB` (正規 argv 由来の正当 traffic は
  compile-time assert で適合保証) + 新 `ResourceLimit::ApprovalMessage`。
  送信側は送信前に typed refusal、受信側は `read_bounded_line` で増分 cap
  (timeout は時間、cap はメモリを bound)。超過は whole 拒否+接続破棄。
- `MAX_PENDING_APPROVALS=32` (= 輸送並列総数)。超過は明示 `deny`
  (fail-closed、never allow) で即時回答、head 不変。
- accept 本体を `handle_approval_connection` に抽出し実リスナーで検証:
  部分送信は timeout 破棄し後続が queue 化、低速→正規の順序不定でも成立、
  33 並列 flood は 32 queue + ちょうど 1 deny。
- テスト事故の記録: exact-size テスト初版が同一タスク内で 1MB を inline
  write_all (reader なし) して自己デッドロック (CPU 0、timer 不発) した。
  原因特定に pump/reader/volume の三段階プローブを使用。
  教訓: バッファ超の write は必ず concurrent reader と組にする。
  本件はテストコードのみの問題で生産コードは無罪 (reader 自体は全容量で
  正常動作をプローブで証明)。

### 再監査 (新 deadlock/orphan/権限)

- select 構造不変 (process 抽出は逐次実行、cancel 対象は next_frame のみ)。
  enqueue/answer/evict は同期+bounded のみ。refuse の deny は inline await
  (flood 時の task 無限増殖を避けるため spawn しない)。
- evict/refuse は何も許可せず、deny は fail-closed。yolo 意味不変。
  expiry 表示は operation+id のみ。秘密情報ログなし。
- 残存: ReleaseAllJobs の e2e (上記理由で非採用)、dispatch-cancel orphan・
  file lock・実機長時間観測 (従来どおり未検証)。

### R6 検証

- `cargo fmt --check`: clean (コミット直前実行)。
- `cargo check --locked --all-targets --jobs 2`: pass、警告なし。
- 新規・更新: longrun 11 件、approval 76 件 (stability 28 を含む)、
  resource 66 件、すべて pass。
- `cargo clippy --locked --all-targets --all-features -- -D warnings`: clean
  (抽出に伴う `&&` 二重参照を修正)。
- GitHub Actions: PR #11 更新後に自動実行、結果を追記予定。

## R7. 独立レビュー第4ラウンド (同一ブランチ、未マージ)

### P1: 期限切れ表示と入力の競合

- 実証: A (期限切れ・表示中) + B (有効) で "y" を入力すると、evict で A が
  消えた直後に B が承認される。新規テスト 2 件 (allow/deny) が修正前に
  4 件とも FAILED (B が決定されることを確認)。
- 修正: `handle_input` を時刻注入の `handle_input_at` に分割
  (既存呼び出しは wrapper 経由で不変)。表示 head を snapshot してから
  evict し、`still_showing` で一致検証。不一致なら入力破棄+新 head 表示で
  再確認 (allow も deny も決定しない)。yolo 切替・permission 系は
  対象外 (表示への回答ではない)。start() arm 側の先行 evict は撤去
  (snapshot 前に evict すると guard が発火しないため)。
- 修正後: 4 件とも pass。既存 phase0 契約 (fresh entry) は無変化で pass。

### P2: 終了ドレインの上限

- 監査: `finish_transport` の drain (`writer_handle.await`) は全 sender
  drop待ち。未完了 dispatch が sender を保持し続けると無期限待機し、
  release・子プロセス終了の全 cleanup が遅延する。旧コードで同シナリオ
  (leak した sender + 待機 writer) のテストが outer deadline で FAILED
  することを確認 (red)。
- 修正: drain→release の順序は維持 (clean EOF の応答配送意味を不変にし、
  正常な長時間 Goal=background job 化済みに影響させない) し、
  drain のみ `SHUTDOWN_DRAIN_TIMEOUT=30s` で bound
  (`finish_transport_with_timeout` に抽出、生産 wrapper は定数使用)。
  期限切れは error 報告 (黙殺なし)。単一 tail・precedence 構造は不変。
- テスト: 300ms 注入で期限切れ Err (名前に "drain" を含む) を即時証明、
  sender 正常時は即時 Ok。既存の shutdown-cancel 契約テストも pass。
- serve 経由の stuck-dispatch e2e は安価な実 request では構築不可
  (全 dispatch が即時完了する) のため、finish 層の直接検証+配線レビューで
  代替することを明記。

### R7 検証

- `cargo fmt --check`: clean (コミット直前、`$LASTEXITCODE` 使用)。
- `cargo check --locked --all-targets --jobs 2`: pass、警告なし。
- longrun 13 件、approval 80 件、resource 66 件、すべて pass。
- `cargo clippy --locked --all-targets --all-features -- -D warnings`: clean。
- GitHub Actions: PR #11 更新後に自動実行、結果を追記予定。

## R8. 独立レビュー第5ラウンド: P2 タイムアウト経路の回帰 (同一ブランチ、未マージ)

レビュー指摘: `timeout(drain_timeout, writer_handle).await.map_err(...)?` は
タイムアウト時に `?` で早期 return し、ReleaseAllJobs でも
release_all_jobs() が実行されない。

### 実証

- 構造的事実: `?` の下にあった release 呼び出しは到達不能
  (drain timeout / writer 失敗の異常系すべて)。
- 実行証拠: cleanup 順序を変えた一時版 (report-first) に対し、
  新規 hook テスト 2 件が FAILED (flag 未設定を検出)。
  正常系テストは通過したため、異常系のみ cleanup が欠落することが確定。

### 修正

- 終了結果を `Result` として保持し、cleanup 後に報告する構造に変更。
  `finish_transport_with_timeout` は cleanup hook
  (`impl Future<Output = ()>`) を受け、生産側は policy 対応の
  release_all_jobs future を渡す。LeaveRegistry のテストでは何も実行
  されないため、グローバル JobRegistry に触れずに ReleaseAllJobs 相当の
  順序 (cleanup→報告) を証明できる。
- drain は `select!` (handle 保持) + deadline で競合させ、タイムアウト時は
  writer を abort して join (reap) するため detached task を残さない。
  abort により receiver が落ち、send 待ち dispatch は高速失敗して終了する。
- `timeout()` (handle を消費して abort 不能になる) は不採用。

### 終了 4 経路の再監査 (cleanup 順序と error 優先順位)

全 exit は単一 tail (drop → drain → release → report) を通る:

1. 正常 EOF: drain (bounded) → release → Ok 報告 (drain 期限切れ時のみ Err)。
2. Writer 異常終了 (transport 失敗・panic): drain skip (task 終了済み、
   handle 完了のため leak なし) → release → writer error 報告。
3. stdin I/O エラー (read-loop 送信失敗含む): bounded drain → release →
   報告は drain-Ok なら read error、drain 期限切れなら drain error
   (優先順位は unit test で固定)。
4. ドレインタイムアウト: writer abort+reap → release 実行 →
   drain-timeout error 報告。cleanup 後の報告を hook flag で証明。

- 正常な長時間 Goal 実行は background job 化済みのため drain 対象外
  (dispatch の send 完了のみ待つ)。release が retained entry を止める点は
  従来の EOF 経路と同一。
- 権限: hook は policy 対応の既存 release のみ実行し、LeaveRegistry では
  何も poll しない。新規権限なし。`terminate().await` の無期限待機は
  従来経路と同一のため残存事項として記録 (今回の対象外)。
- permit/channel は serve 単位の local のため、process 終了時の teardown
  で回収される。cross-serve の共有状態なし。

### R8 検証

- `cargo fmt --check`: clean (コミット直前)。
- `cargo check --locked --all-targets --jobs 2`: pass、警告なし。
- 新規: cleanup 順序 3 件 + reap 1 件、すべて pass。
  (red 版では順序 2 件が FAILED したことを確認済み)
- longrun / approval / resource 全件、clippy: 下記実行結果を参照。
- GitHub Actions: PR #11 更新後に自動実行、結果を追記予定。
