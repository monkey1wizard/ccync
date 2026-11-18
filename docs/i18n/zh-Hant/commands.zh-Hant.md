---
type: reference
title: ccync 指令參考
description: 19 個公開指令的用途，以及更新、升級、固定與清理的操作限制。
tags: [commands, update, upgrade, cleanup]
status: stable
locale: zh-Hant
translation_key: commands
---

# ccync 指令參考

## 公開指令

ccync 提供 19 個公開指令。下表列出每個指令的用途與詳細規範說明連結。具體參數請查閱 `ccync <command> --help`。

| 指令 | 用途 | 行為說明 |
| --- | --- | --- |
| `init` | 由一個來源代理初始化並選取 target | [設定與診斷](setup.zh-Hant.md) |
| `sync` | 建立 render 並投影至原生介面 | [設定與診斷](setup.zh-Hant.md) |
| `add` | 新增 personal plugin 來源 | [外掛搜尋與管理](plugins.zh-Hant.md) |
| `search` | 搜尋可新增的來源 | [外掛搜尋與管理](plugins.zh-Hant.md) |
| `repair` | 補齊 adopted item 的來源 | [外掛搜尋與管理](plugins.zh-Hant.md) |
| `remove` | 移除 personal plugin | [外掛搜尋與管理](plugins.zh-Hant.md) |
| `list` | 列出 personal 與 adopted items | [外掛搜尋與管理](plugins.zh-Hant.md) |
| `show` | 檢視單一項目的來源與狀態 | [外掛搜尋與管理](plugins.zh-Hant.md) |
| `doctor` | 診斷本機狀態 | [設定與診斷](setup.zh-Hant.md) |
| `backup` | 建立可攜 machine profile | [設定與診斷](setup.zh-Hant.md) |
| `restore` | 驗證並合併 machine profile | [設定與診斷](setup.zh-Hant.md) |
| `uninstall` | 撤回可證明由 ccync 管理的內容 | [設定與診斷](setup.zh-Hant.md) |
| `update` | 檢查或更新 ccync binary | 本文件「Binary 自我更新」 |
| `upgrade` | 檢查或升級 plugins | 本文件「Plugin 升級」 |
| `cleanup` | 清除未參照的 plugin cache | 本文件「Pin 與 cleanup」 |
| `pin` | 固定或解除固定 plugin | 本文件「Pin 與 cleanup」 |
| `roster` | 匯出或安裝團隊 plugin 清單 | [外掛搜尋與管理](plugins.zh-Hant.md) |
| `tool` | 探索、追蹤、檢查或更新外部工具快照 | 本文件「外部工具」 |
| `outdated` | 檢查受管理外掛與已追蹤工具的更新 | 本文件「外部工具」 |

## 還原 machine profile

`ccync restore --from <dir> [--dry-run]` 會將五個 machine profile 檔案還原至目前使用的儲存配置。備份必須包含 `plugins.json`。還原會在寫入前驗證備份項目，拒絕連結與不區分大小寫的檔名衝突，並驗證 portable 匯出檔但不匯入其中內容。若儲存配置已提交，還原後會重新產生 portable 匯出檔。之後執行 `ccync sync` 以重新取得遠端來源。若記錄的本機來源路徑不存在，會回報 `needs-rebind`。還原不會匯入所有權。`--dry-run` 會顯示差異且不寫入檔案。

## MCP 路由與診斷

`ccync sync --dry-run` 只讀預覽逐主機決策，`ccync sync` 套用可安全執行的變更。`ccync sync --help` 會列出兩種用法，且不會變更設定或取得生命週期鎖定。對已保留的單一項目，使用 `ccync sync --reselect-mcp SOURCE SERVER HOST [--dry-run]`。來源、伺服器與主機必須精確符合報告。`ccync doctor [--dry-run] [--release-gate] [--mcp-evidence <path>]` 接受去敏事件檔。提供事件檔時只分析該檔，不執行一般診斷。`ccync init <source>` 在同意前預覽匯入，之後可能進行實際投影。備份使用 `ccync backup [--output <dir>] [--strip-secrets]`，還原使用 `ccync restore --from <dir> [--dry-run]`。詳見 [MCP](mcp.zh-Hant.md) 與[設定](setup.zh-Hant.md)。

## Binary 自我更新

`ccync update [--check]` 僅更新 ccync 執行檔本身，不更動任何外掛內容。建議先加上 `--check` 查詢目前版本、遠端最新發行版本及安裝管道來源；此檢查操作純屬查詢，不進行任何下載或寫入。

僅有經由 curl 或 irm 獨立安裝的執行檔，才支援藉由 `ccync update` 自我更新。若執行檔係經由 Homebrew、WinGet 或 Cargo 安裝，ccync 會主動中止並提示操作者使用對應的套件管理器指令。下載新版本後，ccync 必須依據 `checksums.txt` 完成 SHA-256 完整性校驗，隨後才會以原子方式替換現有執行檔。

## Plugin 升級

`ccync upgrade [<plugin>] [--dry-run]` 專門處理受 ccync 管轄且來源為 Git 的個人外掛。此指令會比對遠端儲存庫的 HEAD 與本機鎖定版本，待新版本完整下載至快取後，才更新鎖定版本並執行同步投影。封存檔與一般目錄快照因無可供比對的上游版本，會被直接標記為跳過。請以管理 ID 作為參數，例如 `ccync upgrade plugin --dry-run`；不接受套件名稱作為選取器，見[套件名稱與管理 ID](plugins.zh-Hant.md#套件名稱與管理-id)。

單一來源的連線或升級失敗，不會阻礙其餘項目完成分類與更新。`--dry-run` 模式不會執行 clone、不修改鎖定版本，亦不觸發投影；若偵測到有可用更新則結束代碼回傳 `3`，無可用更新回傳 `0`，發生錯誤則回傳 `1`。

針對已匯入項目，`upgrade` 僅回報版本狀態而不代為套用更新。若來源清單與基準版本足供比對，會顯示具體版本差異；若證據不足，則顯示 `version-unresolved`。MCP 伺服器與獨立技能因無版本比對機制亦不予升級。若欲改由 ccync 全權託管，須先以 `ccync remove <id>` 移除匯入記錄，再以 `ccync add <source>` 重新加入；`upgrade` 絕不會擅自變更其所有權歸屬。

## 新增來源

`ccync add <source> [--no-sync]` 會在 stdout 輸出一個穩定的 intake token：`added`、`fetch-failed` 或 `no-contribution`。Token 表示 intake 結果，結束代碼則表示指令結果。

| Intake token | 意義 | 結束行為 |
| --- | --- | --- |
| `added` | 來源含有受支援內容，且 catalog 記錄已提交。 | 使用 `--no-sync` 時回傳 `0`。否則依投影結果回傳：成功為 `0`，投影失敗為 `1`。投影失敗不會移除已提交的 catalog 記錄。 |
| `fetch-failed` | 無法取得或檢查來源。 | 回傳 `1`，不提交 catalog 記錄。 |
| `no-contribution` | 已取得來源，但其中沒有受支援的元件或 MCP 伺服器。 | 回傳 `1`，並維持 `plugins.json` 原有內容。 |

受支援的內容位置為 `skills/`、`commands/`、`agents/`、`hooks/`、`.mcp.json` 與 `mcp.json`。`hooks/` 中的 hooks 可被接受作為 loose intake，但不會投影至原生 Claude Code 或 Codex 套件。詳見 [Claude Code 與 Codex 的原生套件](plugins.zh-Hant.md#claude-code-與-codex-的原生套件)。非空的 MCP manifest 必須有效。Manifest 損壞時，intake 會在修改 catalog 前失敗。若要直接宣告由使用者撰寫的 MCP 伺服器，請使用 `~/.agents/mcp.json`；ccync 會讀取這個選用輸入檔，只有明確還原時才會寫入。若 URL 沒有受支援的 manifest，請參閱 [MCP 定義與 URL 處理程序](mcp.zh-Hant.md#個人清單與路由)。

## 外部工具

`ccync tool discover <command-or-path> [--online] [--json]` 會依完全相符的執行檔名稱或路徑選取工具，不會依套件名稱選取。預設採離線探索。加上 `--online` 後，才會連線或讀取、更新套件管理器快取。若候選項目不唯一或探查失敗，ccync 會回報原因，不會任意挑選。`ccync tool track <id> ... [--yes]` 會記錄工具識別資料。對 Cargo 與 interpreter-pip 工具，狀態查詢與升級會依經驗證的執行檔、來源、直譯器、distribution、交付方式及管理器接管資訊，從已記錄的宣告重建 adapter。無法重建的 adapter 類型會採 fail-closed 處理。`ccync tool refresh <id> [--dry-run] [--json]` 會核對實際安裝狀態與已提交快照，並在適用時更新投影快照。

選取器比對執行檔身分，不會用套件名稱猜測。Windows 會依 `PATHEXT` 不分大小寫比對、去除 verbatim 路徑前綴，並保留 UNC 共用與目錄身分。路徑選取器會比對標準化後的執行檔身分。Unix 的路徑與執行檔比對區分大小寫。候選項目會以固定順序列出。仍有多個候選時，探索不會猜選。

狀態會分開回報本機安裝證據與遠端更新結果。離線與本機查詢會保留先前的遠端結果及檢查時間，並標示為過時。線上查詢會重新探查本機證據，再將遠端結果更新為 `Current`、`UpdateAvailable`、`Unsupported`、`NotChecked` 或 `CheckFailed`，並標示為最新。線上檢查完成或失敗時，都會更新 `last_check`。最新檢查不會沿用舊的可更新結果。

追蹤工具、固定狀態與 refresh 可能會寫入機器記錄或投影內容。執行即時變更前，ccync 會要求確認；支援 `--yes` 的操作可用此旗標接受確認。`--dry-run` 只預覽 refresh，不會提交記錄或投影檔案。離線探索只使用本機資訊，不會連線。線上檢查可能查詢套件管理器狀態。Cargo 遠端檢查只接受不含憑證的 HTTPS Git 來源，且來源須有有效的已安裝 commit 片段，或使用 crates.io 標準來源。interpreter-pip 檢查會使用已記錄的直譯器與正規化後的 distribution。其他 adapter 除非能證明遠端結果，否則會回報不支援或未檢查。`ccync outdated [--offline] [--json]` 會檢查已追蹤工具與受管理外掛；離線模式不查詢遠端狀態。發現最新更新或可處理的外部變更時回傳代碼 `3`，未知需處理事項時回傳 `0`，操作錯誤時回傳 `1`，用法錯誤時回傳 `64`。遠端結果為 `CheckFailed` 代表操作錯誤，不代表有更新。

> **Beta：`ccync upgrade tool:<id>` 尚不可用於正式環境。** 截至 2026-10-01，正式版所有套件更新關卡都是 `Gated`，包含 Cargo（`rtk` 使用）與 interpreter-pip（Graphify 使用）。此指令會在執行任何套件管理器之前回傳 `blocked` 與結束代碼 `3`，`--dry-run` 同樣被擋下。`ccync outdated` 的偵測與 `ccync tool refresh <id>` 的核對不依賴此指令。
>
> Windows 上經授權的真機驗收未通過，因此關卡維持關閉。該次驗收的已知限制：
>
> - 使用 `--json` 時，已提交的 `tool refresh` 或 `upgrade` 會在 JSON 文件之前，於 stdout 印出投影摘要行。在這條路徑上，stdout 不是單一 JSON 文件。
> - 兩次 `ccync upgrade tool:rtk` 都以管理器結束代碼 `101` 結束。原因尚未確認，且升級路徑不會顯示管理器的 stderr。
> - 復原只用注入的日誌演練過，沒有用自然中斷的操作驗證。
>
> 以下內容描述此 beta 功能預期的契約。證據見驗收收據 `.dev/plans/fix-external-tool-lifecycle-remaining-gaps/real-machine/final.md`。

工具升級使用 `ccync upgrade tool:<id> [--dry-run] [--yes] [--json]`。adapter 會回報支援能力；若不支援該操作，ccync 不會嘗試執行。`outdated` 會偵測外部更新；請執行 `ccync tool refresh <id>` 核對狀態。工具升級後，若結果要求更新快照，請執行 refresh。當 refresh 或工具升級更新投影時，ccync 會列出受影響的 agent 並提示重新載入。執行中的 agent 不會自動載入新世代工具。

若套件管理器可能已完成操作，但 ccync 無法確認結果，ccync 會記錄 `manager-outcome-unknown` 並停止。恢復前請先檢查工具與套件管理器狀態。只有該工具留下未知管理器結果記錄時，才能使用 `ccync upgrade tool:<id> --retry-manager-step`。這是僅供復原的旗標，重試前仍須明確確認。批次升級絕不會自動重試結果未知的管理器操作。復原後，請執行 `ccync tool refresh <id>` 核對已提交快照與投影內容。

工具快照是已提交的記錄。安裝或升級套件不會自動重建已提交快照。請執行 `ccync tool refresh <id>` 建立或核對快照。快照投影也會遵守其他受管理內容的所有權檢查。工具 adapter 只支援其宣告的能力；不支援的安裝、升級或狀態查詢不會執行。

生命週期指令會將診斷輸出至 stderr，並將 stdout 留給要求的 JSON 或指令資料。JSON 結果會保留 `Current`、`UpdateAvailable`、`Unsupported`、`NotChecked` 與 `CheckFailed` 等具型別結果，不會把未證明的結果改成 `Current`。檢查成功或沒有已知待處理事項時回傳 `0`，發現最新更新或可處理的偏差時回傳 `3`，操作或未完成探查失敗時回傳 `1`，用法錯誤時回傳 `64`。JSON 輸出不會取代結束代碼。

新升級只有在最新的具型別遠端檢查證明 `UpdateAvailable` 時，`ccync upgrade tool:<id>` 才能執行更新。證據不支援、未檢查、失敗、過時或有歧義時，系統會 fail-closed，不會呼叫套件管理器。套件操作會在生命週期日誌記錄意圖與結果。若結果不明，ccync 會停止並要求先檢查狀態。只有同一工具與操作的有效日誌，才能授權復原續行。清理或取代項目必須有經驗證的快照所有權證明，且包含已記錄的 generation 身分；名稱或套件管理器清單不足以授權刪除。中斷的清理會依已記錄計畫及已證明的項目清單續行。證據不明或互相衝突時，系統會保留受影響檔案。

生命週期能力關卡分兩階段執行。只有隔離驗收項目及其收據均通過，能力才能標示為支援。在此之前，adapter 關卡維持 `Gated`，CLI 不會執行該套件更新。支援時，`--yes` 只接受畫面列出的操作，不會自動授權其他即時異動或復原。若缺少必要的即時授權，異動、復原、投影及還原步驟都維持未執行。

### CLI 結果契約

有效的指令呼叫若支援 JSON，會在 stdout 輸出一份文件，診斷訊息輸出至 stderr。一般文字輸出依下表處理。用法錯誤時回傳結束代碼 `64`，診斷訊息輸出至 stderr。`tool track` 與 `tool untrack` 不接受 `--json`，會回傳 `64`。

| 指令 | 結果與結束代碼 | 一般文字輸出 | JSON |
| --- | --- | --- | --- |
| `tool discover` | `selected` (0)、`not-found` (3)、`ambiguous` (3)、`probe-error` (1) | 結果輸出至 stdout；不完整探查的診斷與重試提示輸出至 stderr。 | 支援；候選依 adapter、標準化執行檔路徑、candidate ID 排序。 |
| `outdated` | `current`、`unsupported`、`not-checked` (0)；`update-available`、`actionable-drift` (3)；`check-failed` (1) | 狀態輸出至 stdout；個別工具的檢查診斷輸出至 stderr。 | 支援。 |
| `tool track` | `tracked` (0)；`not-found`、`ambiguous`、`blocked`、`confirmation-required` (3)；`probe-error` (1) | 成功結果輸出至 stdout；診斷與確認輸出至 stderr；沒有重新載入建議。 | 不支援。 |
| `tool untrack` | `untracked` (0)；`not-found`、`blocked`、`confirmation-required`、`recovery-required` (3) | 成功結果輸出至 stdout；診斷與復原提示輸出至 stderr。 | 不支援。 |
| `tool refresh` | `committed`、`no-op`、`dry-run` (0)；`blocked`、`recovery-required` (3)；`check-failed` (1) | 主要結果輸出至 stdout；診斷與復原提示輸出至 stderr。 | 支援。 |
| `upgrade tool:<id>` (beta) | `committed`、`current`、`dry-run` (0)；`blocked`、`recovery-required` (3)；`check-failed` (1) | 主要結果輸出至 stdout；診斷與復原提示輸出至 stderr。 | 支援。 |

`init` 沒有 JSON 輸出，因此不在上表中。結束碼為：首次投影完成時為 `0`；來源無效或無法讀取、寫入失敗或投影失敗時為 `1`；提示被取消、缺少同意、`--agents` 為空或含有未知 target、或非互動式執行未提供來源時為 `64`。取消提示或缺少同意時會印出 `nothing written`。非互動執行缺少來源或目標清單，或指定未知目標時，會以 `64` 結束並指出缺少或無效的輸入，同樣不寫入任何內容。先前未經同意即執行 `ccync init claude` 的腳本，現在會以 `64` 結束，必須補上 `--agents` 與 `--yes`。在尚未初始化的機器上，`sync` 會以 `1` 結束，而非 `64`，因此兩個指令目前的結束碼不同。將 `sync` 與此矩陣對齊的事項記錄於[延後工作](deferred-work.zh-Hant.md)。

共用 JSON 欄位包含 `schemaVersion: 1`、`command`、`outcome`、`selectionState`、`updateState`、`updateFreshness`、`nextAction`、`affectedAgents`、`projectionChanged` 與 `reloadAdvice`。`outcome` 是表中的小寫字串。適用的選取或更新狀態是小寫字串，不適用時為 `null`。遠端狀態包含 `current`、`update-available`、`unsupported`、`not-checked` 與 `check-failed`。`updateFreshness` 是 `fresh`、`stale`，不適用時為 `null`。`nextAction` 與 `reloadAdvice` 是字串或 `null`。`affectedAgents` 永遠是字串陣列，未知時為空陣列。`projectionChanged` 永遠是布林值。

只有成功的 `committed` 或 `untracked` 結果，且實際改變投影時，才會建議重新載入。未變更、預覽、失敗、遭阻擋或需要復原時，都沒有重新載入建議。`loaded generation unknown` 只會在投影成功改變後隨重新載入建議出現。

先用完全相符的選取器探索，再用結果中的 candidate ID 消除歧義。以下指令檢查候選，並在確認後記錄選定工具：

```powershell
ccync tool discover graphify --json
ccync tool discover C:\Python314\Scripts\graphify.exe --json
ccync tool track graphify --command graphify --candidate <candidate-id> --recipe graphify-agents --yes
```

Windows 的 `PATHEXT` 若允許 `.EXE`，`graphify`、`graphify.exe` 及大小寫變體會比對相同的執行檔名稱。單純名稱仍可能比對不同目錄而產生歧義。絕對路徑會比對標準化後的執行檔，去除 verbatim 前綴後仍保留 UNC 共用與目錄身分。Unix 比對區分大小寫。探查錯誤會列出 adapter、不完整結果與重試提示，不會誤報為 `not-found`。

新升級會檢查遠端更新資格。復原會先驗證同一工具的既有生命週期日誌。管理器已知成功時，流程會續行且不重跑管理器。結果未知時，必須使用 `--retry-manager-step`，且日誌中的工具、操作、意圖與 manifest 都須相符並通過驗證。已提交操作會辨識為完成。所有權與清理復原詳見[架構](architecture.zh-Hant.md)。


## Pin 與 cleanup

`ccync pin <id>` 會將指定外掛標記為 `held` 鎖定狀態，暫停自動版本檢查與重新 clone；`ccync pin --remove <id>` 可解除鎖定；`ccync pin --list` 則列出目前所有受鎖定之項目。

`ccync pin tool:<id>` 會暫停已追蹤工具的自動升級。執行 `ccync pin --remove tool:<id>` 可解除固定。固定會修改工具宣告，但不會安裝或升級工具。

`ccync cleanup [--dry-run]` 用於清理 `~/.ccync/cache/` 中已不再被鎖定檔引用的孤立外掛快取。若鎖定檔損壞或無法讀取，ccync 絕不刪除任何內容；若某項目缺乏確定的現用版本、但存在多個候選快取目錄，ccync 會全數保留並提出警示。建議先搭配 `--dry-run` 檢視預計清理的範圍，確認無誤後再執行清理。

## 同步與初始化結果參照

下表說明 `sync` 與 `init` 的人類可讀輸出。每個原因都是 CLI 實際印出的文字。各狀態的復原方式見[設定](setup.zh-Hant.md#發布結果與重試)。

| 輸出 | 意義 | 結束碼 | 下一步 |
| --- | --- | --- | --- |
| `published` | target 所需的路由全部已寫入或已相符。 | 整次執行成功時為 `0` | 無。 |
| `not published`，或 `publication not published` | 所需路由未寫入。 | `1` | 修正回報的寫入錯誤，再執行 `ccync sync`。 |
| `not published; conflict: <reason>` | 路由因衝突而被阻擋。 | `1` | 解決衝突，再執行 `ccync sync`。 |
| `not published because unsupported: <reason>` | 此 target 沒有支援該元件類型的介面。 | 不算失敗 | 無。動作為 `no write is needed for this unsupported route`。 |
| `loader NotRun` | 該路由未執行載入檢查。 | 不適用 | 啟動代理並確認元件已列出。此狀態不證明已載入。 |
| `loose conflict: <name> has N origins` | 初始化預覽發現同一獨立技能名稱來自多個來源位置。sync 會阻擋整組項目。 | 不適用 | 解決重複項目，再執行 `ccync sync`。 |
| `mcp blocked: N server(s), not imported` | 初始化預覽與完成報告列出每個被阻擋的 MCP 定義及其原因。 | 不適用 | 逐一檢查來源中列出的定義與原因。 |

### 獨立發布與撤回

`ccync sync` 會將獨立內容，例如已匯入的鬆散技能，發布到每個已選取 target 的技能根目錄。sync 與 `ccync doctor` 會將這些路由標示為 `standalone published`、`recovery blocked` 或 `identity unresolved`。撤回只會移除 ccync 自己發布的複本，並且會等待共用根目錄中每一個已知讀取者的證明。從未被選取的讀取者仍會擋下撤回。被中斷的發布或撤回會保留其 journal 與備份，再次執行 `ccync sync` 即可重試。這些標示描述的是檔案系統結果，不能證明代理已載入該元件。復原方式見[設定](setup.zh-Hant.md#發布結果與重試)。

## 指令同步

`ccync sync --instructions-only [--dry-run] [--yes]` 只預覽或套用指令來源 `~/.ccync/AGENTS.md`，目標是每個已選取 target 的目前寫入檔案。Claude 沒有目前的寫入檔案，因此此來源不會套用到 Claude 的使用者範圍，也沒有 `~/.claude/AGENTS.md` 目的地。成功讀取來源後，若選取 `claude`，此路徑也會從舊版 `~/.claude/CLAUDE.md` 移除受管區塊。來源不存在或有內容時都會移除該區塊。舊檔案不存在或為空時會維持原狀。標記損壞或路徑無法存取時，該路徑會維持原狀並阻擋處理。此路徑不會執行遷移、工具快照驗證、catalog 解析、canonical render、外掛投影或 MCP 作業，也不會寫入 `~/.ccync/` 的任何狀態。未加 `--instructions-only` 時，`ccync sync` 仍會把指示當作完整同步的一個階段。

### 前置條件

遇到下列任一情況，此路徑會在讀寫任何 target 檔案之前失敗並回傳 `1`：

- 本機尚未初始化。
- `~/.ccync/config.json` 不存在、無法讀取，或不是有效的 JSON。
- 設定同時含有 `selectedRuntimes` 與 `selectedProjectionTargets`。請執行 `ccync init <source>` 擇一。
- 設定沒有選取任何有效 target，或含有被拒絕的 target 值。有效值與被拒絕值並存時同樣失敗。錯誤訊息會列出被拒絕的值。

### Target 與去重

ccync 先依標準順序解析已選取的 target，再移除重複的實體路徑。六個邏輯 target 對應四個目前寫入檔案，因為三個 Antigravity target 共用一個檔案。Claude 沒有目前的寫入路徑。

| Target | 指令檔 |
| --- | --- |
| Claude Code | 沒有目前的寫入路徑 |
| Codex | `~/.codex/AGENTS.md` |
| Copilot | `~/.copilot/AGENTS.md` |
| OpenCode | `~/.config/opencode/AGENTS.md` |
| `agy-cli`、`agy-ide`、`agy-gui` | `~/.gemini/AGENTS.md` |

Claude 舊版路徑只用於移除區塊。一般 sync 只在選取 `claude` 時檢查。`ccync doctor` 不受選取或設定錯誤影響，仍會分類此路徑。`ccync uninstall` 也不受選取影響，會檢查所有目前寫入路徑與 Claude 舊版路徑，且只在有 ccync 所有權證明時移除區塊。

每個目前寫入檔案都是獨立的寫入單位。一般 sync 會跳過未選取 target 的檔案；未選取 `claude` 時也會跳過 Claude 舊版路徑。

### 來源狀態

只有來源檔不存在或大小為零位元組，才代表你不要任何指令。此時 ccync 只會從每個已選取的檔案移除 ccync 擁有的區塊。只含空白字元的來源視為內容，ccync 會寫入。

來源無法讀取、不是有效的 UTF-8，或是失效的符號連結時，ccync 絕不移除任何內容。預覽與套用都會列出每個可解析的路徑，並標示阻擋原因 `blocked: source read`、`blocked: source encoding` 或 `blocked: dangling source link`。輸出以 `no files written` 結尾，並回傳 `1`。

### 各路徑的狀態

| 狀態 | 意義 |
| --- | --- |
| `create` | 檔案不存在，ccync 會建立檔案並寫入區塊。 |
| `append` | 檔案沒有區塊，ccync 會附加一個。 |
| `rewrite` | 區塊與轉換後的來源不同，ccync 會取代它。 |
| `prune` | 來源不存在而檔案有區塊，ccync 會移除該區塊。 |
| `unchanged` | 檔案已經相符。ccync 不發布寫入，並保留修改時間。 |
| `blocked: target read` | 無法讀取 target 檔案。 |
| `blocked: malformed marker` | target 檔案中的 ccync 標記已損壞。ccync 絕不修復標記。 |
| `blocked: dangling link` | target 是符號連結，但連結目的地不存在。 |

ccync 會保留擁有區塊以外的每個位元組，包含混合的換行格式。

### 預覽（dry-run）

`--dry-run` 不會提示確認，也不寫入任何檔案。它會印出 `--- INSTRUCTIONS ONLY: DRY RUN ---`、每個去重後目前寫入路徑的狀態行，以及選取 Claude 時獨立列出的 `legacy instruction files (prune only)` 區塊。某個阻擋原因不會隱藏其他路徑。預覽完整時結束代碼為 `0`，出現任何阻擋原因時為 `1`。

### 同意與套用

套用時先印出模式（`write` 或 `prune`）、去重後的路徑，以及各路徑的預定狀態，接著需要授權。只有下列形式能授權寫入：

- `--yes` 旗標。
- 環境變數值剛好是 `CCYNC_ASSUME_YES=1`。
- 互動式回答 `y` 或 `yes`。

環境變數未設定、為 `0`、為空，或為其他任何值，都會拒絕。拒絕，或非互動工作階段沒有授權時，不會寫入任何內容，結束代碼為 `1`，並提示你加上 `--yes` 重新執行。此範圍化路徑不會寫入投影鎖定檔，因此在第一次完整投影之前，每次套用都需要重新授權。

### 部分成功與復原

單一 target 的失敗不會中止其他獨立 target，也不會回復它們已完成的寫入。套用會逐一印出每個路徑，最後印出 `changed: N; unchanged: N; failed: N`。被阻擋的路徑計入失敗，ccync 不會寫入它。`failed` 大於 `0` 時，結束代碼為 `1`。

復原時，請修正回報的原因，再重新執行同一個指令。未變更的路徑在重跑時仍維持未變更。

| 回報的原因 | 復原方式 |
| --- | --- |
| 設定或 target 值被拒絕 | 執行 `ccync init <source>`，或從 `config.json` 移除被拒絕的值。 |
| 來源讀取或編碼失敗 | 修正 `~/.ccync/AGENTS.md` 的權限，或以 UTF-8 重新儲存。 |
| 來源連結失效 | 修復預期的連結目的地與存取權。 |
| Target 連結失效 | 保留連結。檢查連結目的地與存取權，恢復存取權後重新執行 `ccync doctor`。 |
| Target 讀取失敗 | 修正 target 檔案的權限。 |
| 標記損壞 | 手動修復 target 檔案中的標記，再執行 `ccync doctor`。 |

### 完整同步

完整的 `ccync sync` 使用相同的來源讀取器與 target 檢查器。其 dry-run 會分別列出目前指示檔，以及選取 Claude 時會移除區塊的舊版指示檔。各檔案仍會列出相同的路徑狀態或來源阻擋原因。完整 dry-run 會印出整份報告，包括其他所有表面與每個可解析的指示路徑。預覽完整時結束代碼為 `0`，任何指示路徑出現阻擋原因時為 `1`。若完整 sync 無法建立 machine options，就會跳過指示階段。

完整套用時若來源失敗，ccync 會略過所有指令寫入、保留其他已完成的階段、在 stderr 回報指令失敗，並回傳 `1`。

### Doctor

`ccync doctor` 讀取嚴格設定、來源與已選取的目前寫入檔案，且不寫入任何內容。它會比對轉換後的來源與每個已選取檔案中的區塊，也會不受選取、設定或來源錯誤影響，獨立分類 Claude 舊版路徑。舊檔案不存在、為空或沒有受管區塊時，不需處理。殘留區塊會列為警告。選取 Claude 後，執行 `ccync sync --instructions-only --yes`，只移除受管區塊與標記。若未選取 Claude，手動移除受管區塊與標記，或執行 `ccync uninstall`。檔案無法讀取、標記損壞或連結失效時，會列為錯誤。遇到失效連結時，請保留連結，檢查目的地與存取權，恢復存取權後重新執行 `ccync doctor`。下列其他情況也會回報為錯誤，結束代碼為 `1`：

- 設定不存在、無法讀取、格式錯誤、全部無效或混合有效與無效值。
- 來源讀取、編碼或失效連結失敗。
- Target 無法讀取、標記損壞，或是失效連結。
- 區塊不存在、區塊與來源不同，或來源已移除但區塊仍然存在。
- 無法讀取的 Codex 覆寫檔 `AGENTS.override.md`。

下列情況回報為警告。只有警告時，結束代碼為 `0`：

- 未選取 target 中的過時區塊。
- 受管區塊旁有可讀取且非空的 Codex `AGENTS.override.md`。

每項發現都會列出原因與復原動作。

### 證據邊界

| 證據 | 能證明的事 |
| --- | --- |
| Dry-run | 依目前檔案預測套用會做什麼。 |
| 套用 | 回報選取檔案有變更、相符或遭阻擋。 |
| Doctor | 回報檔案在磁碟上的唯讀健康狀態。 |
| 真實 agent 工作階段 | 只有工作階段探測能觀察 agent 是否載入了指令。 |

沒有任何 ccync 指令會回報執行中的 agent 是否載入了該檔案。套用造成變更後，請重新載入或重新啟動 agent。ccync 不知道 agent 的載入狀態。指令來源中出現某個 skill 名稱，不代表每個 host 都提供該 skill。
