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
| `init` | 初始化並選擇 master agent | [設定與診斷](setup.zh-Hant.md) |
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

## Binary 自我更新

`ccync update [--check]` 僅更新 ccync 執行檔本身，不更動任何外掛內容。建議先加上 `--check` 查詢目前版本、遠端最新發行版本及安裝管道來源；此檢查操作純屬查詢，不進行任何下載或寫入。

僅有經由 curl 或 irm 獨立安裝的執行檔，才支援藉由 `ccync update` 自我更新。若執行檔係經由 Homebrew、WinGet 或 Cargo 安裝，ccync 會主動中止並提示操作者使用對應的套件管理器指令。下載新版本後，ccync 必須依據 `checksums.txt` 完成 SHA-256 完整性校驗，隨後才會以原子方式替換現有執行檔。

## Plugin 升級

`ccync upgrade [<plugin>] [--dry-run]` 專門處理受 ccync 管轄且來源為 Git 的個人外掛。此指令會比對遠端儲存庫的 HEAD 與本機鎖定版本，待新版本完整下載至快取後，才更新鎖定版本並執行同步投影。封存檔與一般目錄快照因無可供比對的上游版本，會被直接標記為跳過。

單一來源的連線或升級失敗，不會阻礙其餘項目完成分類與更新。`--dry-run` 模式不會執行 clone、不修改鎖定版本，亦不觸發投影；若偵測到有可用更新則結束代碼回傳 `3`，無可用更新回傳 `0`，發生錯誤則回傳 `1`。

針對已匯入項目，`upgrade` 僅回報版本狀態而不代為套用更新。若來源清單與基準版本足供比對，會顯示具體版本差異；若證據不足，則顯示 `version-unresolved`。MCP 伺服器與獨立技能因無版本比對機制亦不予升級。若欲改由 ccync 全權託管，須先以 `ccync remove <id>` 移除匯入記錄，再以 `ccync add <source>` 重新加入；`upgrade` 絕不會擅自變更其所有權歸屬。

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
