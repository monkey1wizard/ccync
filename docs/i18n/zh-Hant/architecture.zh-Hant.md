---
type: architecture
title: ccync 架構
description: ccync 的系統邊界、六個 crate、投影流程、部署方式與品質目標。
tags: [ccync, architecture, arc42]
status: stable
locale: zh-Hant
translation_key: architecture
---

# ccync 架構

## 1. 簡介與目標

ccync 是以單一 Rust 執行檔為核心的跨代理外掛管理系統，專門集中管理各類程式輔助代理的外掛、技能、指令、自訂代理與 MCP 伺服器。使用者僅需在單一位置維護一份 ccync 狀態，系統會先建置出共用的 canonical render，隨後再依據各目標產品的原生配置格式執行投影。ccync 並不取代各代理本身的運作，亦不假設各產品的設定規格彼此相容。

核心設計目標如下：

- 一次安裝或匯入，即可同時投影至 Claude Code、Codex、GitHub Copilot、OpenCode 與 Antigravity。
- 嚴格保護使用者現有內容：唯有具備明確所有權證明歸屬 ccync 管理之項目，才允許執行移除。
- 全面採用原子寫入、暫存建置交換以及具備中斷回復能力的 MCP 交易機制，杜絕中途異常留下殘缺狀態。
- 冪等性保證：相同輸入重複執行必然收斂至穩定狀態；若發生錯誤，執行報告必須具體指出受影響的目標或伺服器。

使用者可選取的代理共有五個：`claude`、`codex`、`copilot`、`opencode` 與 `agy`。前四者各自對應單一投影目標；`agy` 則在投影時自動展開為 `agy-cli`、`agy-ide` 與 `agy-gui` 三個獨立目標，因此系統共計有七個投影目標。MCP 則固定同步更新 Claude、Codex、Copilot、OpenCode 與 Antigravity 五個宿主主機，不受代理選取狀態影響。

## 2. 限制條件

- 本系統為純粹的單一二進位執行檔架構，由六個 crate 組成，無任何外部資料庫相依性，亦不常駐背景服務。
- `~/.ccync/` 為 ccync 專屬之本機機器狀態邊界。各代理實際生效之原生設定檔皆位於此邊界之外，任何寫入操作皆必須嚴格保留他人配置。
- 各投影目標的原生格式互不相容；共用內容統一源自 canonical render，但各目標仍須由專屬 adapter 負責轉換。
- `plugins/catalog.json` 在建置時直接編譯嵌入執行檔內；使用者自行擴充之個人來源清單則記錄於 `~/.ccync/plugins.json`，兩者責任與生命週期截然不同。
- 正式執行流程不採 Dev/Normal 雙軌分支。`config.json` 雖保留部分歷史相容欄位，但絕不會因此啟用另一套分支投影邏輯。
- 本文件專注於說明系統架構、canonical render、原生投影以及所有權判定規則；具體操作請參閱[設定與診斷](setup.zh-Hant.md)及[指令參考](commands.zh-Hant.md)，MCP 安全信任邊界請參閱[MCP](mcp.zh-Hant.md)。

## 3. 脈絡與範圍

使用者輸入、外部來源、ccync 本機狀態，以及各代理實際生效的檔案，分屬不同信任邊界：

```text
使用者意圖
  |  config.json / plugins.json / roster
  v
ccync 來源邊界 ---- curated catalog / git / archive / local path
  |  resolve + fetch + validate
  v
~/.ccync/ 機器狀態 ---- cache / lock / canonical render / ownership proof
  |  target adapters + MCP serializers
  v
代理原生介面 ---- 使用者與其他工具亦擁有現有配置
```

來源邊界負責接收內建與個人來源清單、主要代理之既有項目，以及 Git URL、本機路徑、封存檔與 catalog ID。外部來源在寫入快取前，ccync 會嚴格檢查路徑是否逸出允許邊界、正確定位外掛根目錄，並辨識其內含元件型態。外掛提供的 MCP 欄位亦須單獨經過嚴格校驗：未定義之欄位會被明確列出並排除，且諸如 `headersHelper` 等隱式外部輔助指令一律拒絕執行。

各代理實際讀取的設定檔案屬於跨工具共享資源，ccync 絕不宣稱擁有整份 `~/.claude.json`、`~/.codex/config.toml` 或使用者目錄。唯有經由註冊記錄、受管標記區塊或逐主機狀態檔案得以證實之項目，才屬 ccync 之管理範圍。投影、撤回、解除安裝與中斷復原等所有操作，均受此一所有權邊界嚴格限制。

## 4. 解法策略

ccync 先依據來源定義建立共用標準輸出，隨後投影至各代理的原生檔案：

1. 讀取內建來源清單、本機設定、個人來源清單與匯入快照，產生並更新 `build/lock.json`。
2. 依據鎖定版本或內容雜湊將來源下載至 `cache/<id>@<revision>/`；後續投影皆由此快取讀取，無需重複解析外部來源。
3. 在現有 canonical render 旁建立暫存目錄，重新建置其中由 ccync 管轄之各項元件。
4. 暫存目錄建置完成後，呼叫 `ccync_foundation::platform::atomic_swap` 替換 `build/render/`；若交換失敗，現有目錄將完整保留。
5. 各目標 adapter 將技能、指令、自訂代理、hooks 與受管指示區塊投影至原生設定檔中。
6. MCP 規劃器為五個主機產生專屬配置，並經由預寫式交易記錄與逐主機指紋校驗，確保設定安全落地。
7. `ProjectionRunAccumulator` 根據各目標與各階段的實體執行證據，精確判定為 `NotRun`、`Applied`、`Unchanged`、`Skipped` 或 `Failed`，絕不以全域成功計數模糊個別結果。

可重建之暫存輸出、本機共享之原生設定以及各項所有權證明均各自獨立儲存。單一目標遭遇失敗時，其餘目標之執行結果仍清晰可辨；在執行刪除前若缺乏充足證據，系統一律選擇保留現有項目。

## 5. Building Block View

| Crate | 責任 | 直接內部依賴 |
| --- | --- | --- |
| `ccync-foundation` | paths、config、platform、health、target 與共用資料模型 | 無 |
| `mcp` | variable resolution、planner、transaction、五種 host serializer、atomic writer | `ccync-foundation` |
| `projection` | canonical component projection、target adapter、managed artifact registry、Antigravity faces | `ccync-foundation` |
| `ccync-engine` | catalog、fetch/install、adoption、upgrade、roster、doctor、command/exit model | `ccync-foundation`、`projection`、`mcp` |
| `setup` | 由 `doctor` 聚合的 management health checks | `ccync-foundation` |
| `ccync-cli` | binary entry point、argument classification 與 command orchestration | 其餘五個 crate |

依賴關係以 workspace `Cargo.toml` 與各 crate 的宣告為準。底層 `ccync-foundation` 絕不依賴任何上層 crate；`ccync-cli` 負責整合所有功能。`mcp` 與 `projection` 分別走獨立路徑：前者負責更新各主機之 MCP 配置，後者專注於技能、指令、自訂代理與指示檔之投影。

公開指令共有 19 個，清單請參閱[指令參考](commands.zh-Hant.md)。`refresh` 與 `rollback` 可由內部呼叫，但不顯示於公開說明中。實際支援的指令集合由程式碼中之 `CommandKind::PUBLIC` 與 `CommandKind::ALL` 定義。

## 6. Runtime View

### Sync 與 projection

```text
(start) ccync sync
  │
[ 搬移舊版目錄配置；讀取設定 ]
  │
{ 使用 --dry-run？ }
  ├── 是 ──▶ [ 解析 catalog；顯示預覽 ] ──▶ (end >>|) 略過投影
  │
  否
  ▼
{ 已初始化且目標選擇有效？ }
  ├── 否 ──▶ (end ×) 投影前結束並回報錯誤
  │
  是
  ▼
[ 更新 catalog lockfile；失敗時僅警告 ]
  │
{ 首次投影關卡允許繼續？ }
  ├── 否 ──▶ (end ×) 投影遭拒
  │
  是
  ▼
[ 讀取先前 MCP 清單與各主機現況 ]
  │
[ 在 render 前復原或建立 MCP 所有權記錄 ]
  │
{ MCP 前置處理成功？ }
  ├── 否 ──▶ (end ×) render 前結束並回報錯誤
  │
  是
  ▼
[ 暫存 canonical root 並原子交換 ]
  │
[ 投影 canonical 目標；產生受管理 MCP 清單 ]
  │
[ 規劃 MCP 異動；寫入預寫紀錄及主機、驗證並提交所有權 ]
  │
{ MCP 投影回傳錯誤？ }
  ├── 是 ──▶ (end ×) 結束並回報錯誤
  │
  否
  ▼
[ 執行 machine 投影 ]
  │
[ 更新 AGENTS.md 的受管理區塊 ]
  │
[ 回報各目標證據與 MCP 主機失敗情形 ]
  │
{ 目標為 Failed／NotRun，或執行／主機／區塊失敗？ }
  ├── 是 ──▶ (end ×) 結束並回報錯誤
  └── 否 ──▶ (end √) 成功結束
```

ccync 分別記錄 canonical 與 machine 兩個投影階段。Claude、Codex 與 Copilot 均須取得兩個階段之完成證據；`agy-cli` 與 `agy-ide` 僅經歷 canonical 階段；OpenCode 與 `agy-gui` 則僅經歷 machine 階段。若目標缺乏對應階段之完成證據，狀態將保持為 `NotRun`，絕不可因未發生錯誤便誤記為成功。

`ProjectionEngine` 會先在現有目錄旁建立暫存之 canonical render，待全部元件建置完成後，才原子交換至 `~/.ccync/build/render/`；若流程中斷，現有內容將完整保留。外掛是否具備可投影之內容，取決於內含元件與目標平台支援度，無法僅憑來源清單斷定。

`ManagedArtifactRegistry` 負責逐項記錄由 ccync 投影之具體檔案。若遭遇同名路徑但缺乏明確所有權證明，ccync 必須保留該檔案並回報 `preserved user-owned path`。`sync --dry-run` 僅檢視列出預計異動之檔案清單，絕不建立、修改或清理任何實體路徑。

### MCP ownership 與 recovery

MCP 採用預寫式交易記錄機制。日誌中僅記錄寫入前後的檔案指紋或「原本不存在」標記，絕不儲存指令、環境變數、標頭或 token 等敏感資訊。在各主機完成寫入後，ccync 會重新讀取並逐一校驗指紋；唯有所有參與主機均成功取得實體驗證時，才會將所有權歸屬提交至 `projected-state.json`，並清除交易記錄。

若前次執行意外中斷，在下次投影啟動前，`prepare_projection_recovery` 會主動檢查上次殘留的交易：若檔案符合寫入後的目標指紋，則承認該主機之所有權歸屬；若檔案仍維持寫入前之初始狀態，則保持原樣；若出現無法辨識的第三種指紋或主機檔案無法讀取，ccync 將立即中止自動推論並提出警示。若僅為個別伺服器的機密值佔位符無法解析，ccync 會跳過並指出該伺服器，其餘伺服器與非 MCP 投影流程則持續正常推進。

### External tool lifecycle

工具管理器將套件來源設定與本機狀態視為不同的信任邊界。它會先驗證來源身分與能力，再執行操作。它藉由具型別的操作呼叫套件管理器，並回報結果而不揭露憑證。

管理器依序取得鎖定：先取得全域管理器鎖定，再取得個別工具鎖定。每把鎖定都使用持久存在的鎖定檔。管理器釋放鎖定時不會刪除檔案，因此所有並行程序都會藉由相同路徑協調。

生命週期日誌會記錄具型別的操作階段：意圖、開始執行及記錄結果。重新啟動時，復原流程會檢查最後一個已持久化的階段，並核對工具的實際狀態，再決定重試或回報結果不明。日誌只儲存具型別且不含機密的重試意圖。日誌不得儲存原始 argv、憑證或解析後的機密值。

若行程在外部命令可能已執行、但 ccync 尚未記錄結果時停止，管理器會回傳 `manager-outcome-unknown`。它不會回報成功，也不會在核對前重複執行非冪等操作。

管理器只有在具備該項目的正向快照所有權證據時，才會刪除或取代已安裝項目。同名項目或目前的套件管理器清單本身，都不足以證明所有權。

狀態會分開記錄本機安裝證據與遠端更新結果。Offline 與 Local 模式不會執行網路檢查，並會將先前的遠端結果及時間保留為過時狀態。Online 模式會重新探查本機狀態，再將遠端結果更新為其中一種具型別結果：`Current`、`UpdateAvailable`、`Unsupported`、`NotChecked` 或 `CheckFailed`。結果會標示為最新，且檢查完成或失敗後會更新 `last_check`。最新結果不會沿用先前的 `UpdateAvailable`。`outdated` 使用同一條狀態查詢路徑。發現最新更新或可處理的本機偏差時回傳結束代碼 `3`；遠端檢查失敗時回傳 `1`；沒有已知待處理事項時回傳 `0`。

探索與以指令追蹤工具共用完全相符的執行檔選取器。選取器接受指令名稱或路徑，不會依模糊名稱推測套件。找不到候選項目、候選不唯一及探查錯誤會分開回報。Engine 會依經驗證的追蹤身分重建 Cargo 與 interpreter-pip adapter，身分包含執行檔、來源、直譯器、distribution、交付方式及管理器接管資料。若其他 adapter 類型不支援依記錄重建，系統會採 fail-closed 處理。遠端 adapter 會回傳範圍精簡的具型別結果，不會將已持久化或不支援的觀察偽裝成遠端探查結果。Cargo 檢查需要安全且不含憑證的 HTTPS Git 身分及已記錄的 commit 片段，或 crates.io 標準來源與精確的套件、版本證據。interpreter-pip 檢查會使用已記錄的直譯器與正規化後的 distribution。

新升級只有在最新的遠端結果證明 `UpdateAvailable`，才能執行升級。`Current`、`Unsupported`、`NotChecked`、`CheckFailed`、過時證據、候選歧義或缺少身分都不能授權套件管理器異動。生命週期日誌會記錄具型別的意圖、開始執行與結果，不含原始參數、憑證或解析後的機密值。復原流程會檢查持久化操作與實際狀態，且只會續行同一工具的有效日誌操作。狀態不明或互相衝突時，系統會 fail-closed，要求先檢查，不會自動重試。

工具狀態 v2 會獨立追蹤 generation 所有權，不依賴目前的套件管理器清單。清理流程會記錄可續行的具型別計畫與固定的已證明項目清單。取代或刪除產物前，流程會驗證 generation 證明，並在復原時辨識已提交的操作。未證明、資訊不符、屬於符號連結或證據衝突的產物都會保留。清理完成狀態與快照投影會依日誌中的復原狀態處理，因此中斷後可依原記錄續行，不必重新搜尋刪除目標。

CLI JSON 會保留具型別結果，診斷訊息輸出至 stderr，stdout 則供 JSON 或指令資料使用。啟用 JSON 不會改變結束代碼：檢查成功或沒有已知待處理事項時回傳 `0`，發現更新或可處理偏差時回傳 `3`，操作或未完成探查失敗時回傳 `1`，用法錯誤時回傳 `64`。具型別的檢查失敗不會被改寫成目前狀態。

生命週期能力關卡分兩階段。個別關卡必須先通過隔離驗收項目，並由收據記錄證據，才能開放。候選狀態或單元測試通過本身不會授權正式環境異動。即時安裝、升級、復原、投影或還原仍須取得該操作所需的明確授權。支援時，`--yes` 只接受畫面列出的操作，不會擴大授權範圍。若缺少授權，即時驗收項目維持 `NotRun`，能力關卡維持關閉。

生命週期能力分為 E0、E1 與 E2。E0 支援檢查，E1 支援具復原能力的安全操作，E2 支援完整的受管生命週期。平台或套件管理器只能宣告已驗證能力所支援的級別。

| 能力門檻 | 目前狀態的依據 |
| --- | --- |
| 各平台及套件管理器能力門檻 | 最終驗收收據列出目前狀態。尚未證明的 Phase B 門檻維持 `gated`。 |

本表是本架構文件中門檻狀態的唯一依據。最終驗收收據提供目前狀態，因此本文件不重複容易過時的狀態清單。

### Managed instruction block

使用者全域層級之 `AGENTS.md` 由一組專屬標記界定受管區塊。寫入模組會完整保留標記區塊以外的原有內容，僅以原子方式更新標記內部之文字。若目標檔案尚不存在則自動建立；若標記缺漏或檔案損毀，ccync 會明確標示並跳過處理。三個 Antigravity 目標共用 `~/.gemini/AGENTS.md` 檔案，每次執行僅寫入一次。

### Backup 與 restore

本機機器設定備份包含 `config.json`、`plugins.json`、匯入項目所需的鎖定檔快照以及使用者全域指示。標準 `backup` 流程預設會過濾能辨識的機密值，將 MCP `env` 與 `headers` 的數值轉換為 `${CCYNC_SECRET_<SERVER>_<KEY>}` 具名佔位符；若偵測到無法安全脫敏之明文機密值，ccync 會直接拒絕產生備份。`restore --dry-run` 僅回報比對結果（一致、有差異或無法讀取），絕不寫入檔案。還原完成後，操作者仍須重新配置環境變數、安裝團隊清單並執行 `sync`。請注意：備份檔僅為設定封包，並非各代理的原生設定檔複本。

Adapter 的遠端結果範圍刻意保持精簡，回報 `Current`、`UpdateAvailable`、`Unsupported` 或 `NotChecked`。探查失敗時由 engine 轉為 `CheckFailed`，並保留它與成功但沒有更新之結果的差異。Cargo 證明會比對已記錄的精確 ref 與安裝 SHA，或比對 crates.io 套件身分及 SemVer 版本。interpreter-pip 證明會使用已記錄的直譯器與正規化 distribution。 Cargo Git 證明接受 7–40 位十六進位的已安裝 commit 片段。沒有查詢參數時選取遠端 `HEAD`；`branch` 選取 `refs/heads/<name>`；`tag` 選取解參照後的 tag，並允許輕量 tag；40 位十六進位 `rev` 不可變。可移動 ref 必須解析為唯一完整 SHA，且符合已安裝片段，才是 `Current`。crates.io 檢查要求標準來源、精確套件名稱與 SemVer 優先序較高的版本，才是 `UpdateAvailable`。遠端完整 SHA 只暫存於檢查結果，`outdated` 不會持久化。儲存層會讀取 v1 狀態記錄。所有權遷移只匯入已記錄指紋與實際 manifest 相符的舊版目前快照。無法證明的歷史世代仍沒有所有權證據。v2 狀態會以原子方式儲存目前狀態與歷史世代所有權。刪除前必須證明精確 operation ID、build 相對路徑、檔案 manifest 與雜湊及目錄清單。符號連結與 Windows reparse point 不符合所有權驗證。無法證明的舊歷史會 fail-closed。清理重試採用不可變子集，並逐一路徑重新驗證。

復原會先驗證同一工具的待處理日誌，再進行一般資格檢查。管理器已知成功時，流程會續行且不重跑管理器。結果未知時，必須使用 `--retry-manager-step`，且日誌中的工具、操作、意圖與 manifest 都須相符並通過驗證。已提交操作視為完成。兩階段能力關卡須等隔離驗收及收據通過後才開放。即時安裝、更新、復原、投影及還原仍須明確授權。CLI 結果表與 JSON 欄位定義見[指令參考](commands.zh-Hant.md)。

## 7. Deployment View

ccync 採單一二進位執行檔形式發布。正式發行管線涵蓋六個平台目標：Windows x64/arm64、macOS x64/arm64 以及 Linux musl x64/arm64。各平台均提供獨立執行檔以及僅內含執行檔與 `LICENSE` 的最小化封存檔。Windows MSVC 版本嚴格採用 static CRT 靜態連結建置，徹底防範無 Visual C++ 可轉散發套件環境因 `0xC0000135` 導致啟動失敗。

GitHub Actions 會在版本標籤觸發時，自動核對 crate 版本編號與變更紀錄，產生 `checksums.txt`、發行檔案清單、WinGet 清單以及 Homebrew formula。隨後藉由 GitHub OIDC 機制執行 cosign 無金鑰簽章。唯有正式穩定版才會發布至 Homebrew tap 並向 WinGet 提交；預覽版本絕不進入此兩項公開套件管道。發行後驗收流程會自動校驗 checksum、數位簽章、Cargo 安裝相容性、Unix/Windows 安裝腳本以及穩定版 Homebrew 的可用性。

運行環境無需任何常駐伺服器；ccync 僅需讀寫本機 `~/.ccync/`、呼叫 Git、curl 或套件管理工具，並更新本機代理設定檔。

## 8. Crosscutting Concepts

### Ownership proof

在 ccync 的安全哲學中，刪除操作相較於寫入操作需要更嚴苛的證據門檻。受管檔案註冊記錄、逐主機 MCP 狀態證明、Antigravity 根目錄所有權以及受管標記區塊，皆可作為合法的所有權證明。僅憑檔案名稱相同、內容相似或恰好位於預設路徑，均不構成足夠的所有權證明。一旦證據不足，ccync 會優先保留現有檔案；此舉雖可能暫時殘留孤立檔案，但能徹底杜絕誤刪使用者私有資料的致命風險。

### Atomicity 與 failure isolation

單一檔案的寫入均遵循「在同一目錄建立暫存檔後再行重新命名」之原則；canonical render 則採「在相鄰位置建置暫存目錄後進行原子目錄交換」機制。MCP 因牽涉多個獨立主機設定檔，無法仰賴單一檔案系統交易達成，因而改採預寫式交易記錄、指紋校驗與復原分類器，確保各主機最終收斂至一致狀態。所有錯誤回報均會具體指出對應的目標平台或伺服器名稱，絕不讓局部失敗在巨觀統計中被隱蔽。

### Secrets 與 placeholders

`config.json#secrets` 屬於本機明文輸入。在將設定寫回檔案時，ccync 會智慧合併原始 JSON，完整保留程式模型未定義的擴充欄位。MCP 變數支援 `${VAR}`、`${VAR:-default}`、`PLUGIN_ROOT`、`PLUGIN_DATA` 與受控之 Claude 別名。交易記錄中嚴禁記錄解析後的機密數值，備份封包亦預設將機密值轉換為佔位符。此等機制旨在縮減機密值被持久化落盤或意外匯出的暴露面，而非提供磁碟層級的加密防護。

### Native adapters

共用規格並不等同於所有目標均具備相同的功能支援度。Claude、Codex、Copilot、OpenCode 與三個 Antigravity 目標各自擁有專屬 adapter，負責處理 frontmatter 格式轉換、路徑對映、符號連結或複本選擇，以及不支援欄位的安全降級。每個 adapter 均須以明確的目標識別碼回報其執行狀態，禁止以單一通用格式粗暴抹煞各產品的原生特性。

## 9. 架構決策

- [ADR 01：產品範圍與發布模型](../../adr/01-product-scope-and-projection-model.md)
- [ADR 02：外掛來源解析與快取建立](../../adr/02-source-resolution-and-plugin-materialization.md)
- [ADR 03：發布項目歸屬與原子寫入](../../adr/03-projection-ownership-and-atomic-publication.md)
- [ADR 04：MCP 信任邊界與中斷復原](../../adr/04-mcp-trust-boundary-and-recovery.md)
- [ADR 05：本機初始化、既有項目匯入與指令規則](../../adr/05-machine-lifecycle-adoption-and-cli-contract.md)
- [ADR 06：設定備份與機密值處理](../../adr/06-backup-profiles-and-secret-handling.md)
- [ADR 07：發行與安裝方式](../../adr/07-release-and-distribution.md)

## 10. 品質需求

| 情境 | 刺激 | 預期反應與可驗證結果 |
| --- | --- | --- |
| Canonical render 中途失敗 | source tree 無法讀取或 staging 失敗 | live render byte-for-byte 保持不變，不輸出成功 Applied block |
| Live surface 有同名外部內容 | sync、remove 或 uninstall 嘗試 reconcile | 沒有 positive ownership proof 就保留，並提供 warning 或 skipped evidence |
| MCP 第二個 host 寫入失敗 | 第一個 host 已落地 | 保留 transaction。下一次 run 依 fingerprint recovery，不把未寫 host 認成 owned |
| Secret placeholder 無法解析 | 一個 server 缺少必要變數 | 跳過該 server，點名缺少項目，繼續其他 server 與 projection |
| 使用者重跑相同輸入 | lock、cache 與 live config 都已收斂 | 不重寫 byte-identical host，target 結果為 `Unchanged` |
| 備份搬到新機 | profile 不含 secret | restore 後明確列出要重新提供的變數，不把 literal secret 帶到新機 |
| Release 安裝到乾淨 Windows | 沒有 VC++ redistributable | static-CRT binary 可啟動。CI 的 import probe 阻擋 regression |

驗收必須綜合考量整體 workspace 測試、clippy 靜態檢查、整合測試與發行管線校驗；標註為 `NotRun` 的檢查項目絕不可視為驗證合格。

## 11. 風險與技術債

- 匯入模組中仍有少數邏輯使用標準 `std::fs::write` 寫入鎖定檔，僅有較新的來源修復管線，改用共用之原子寫入機制。
- 若歷史所有權證明遺失，將無法安全辨別過往 MCP 項目的管轄權，該類舊項目可能因此長期被安全保留於設定中。
- `doctor --dry-run` 與 `--release-gate` 目前雖可正常接收參數，但尚未改變底層實際執行的檢查集合。
- 若外掛快取遺失，部分 `upgrade` 或 `sync` 路徑可能將缺失的來源誤判為無元件外掛，隨後的投影操作恐有誤刪既有檔案之風險。
- Antigravity 一旦降級退回實體目錄複本，即便日後作業系統環境復原符號連結支援，亦不會主動切換回連結模式；取消選取時，三個目標亦各自採取保守程度不一的清理策略。
- 專門針對正式穩定版執行的 WinGet 發布流程，無法單純仰賴預覽版本演練獲得全面驗證，仍必須依據正式版的實際發行結果做最終確認。

其餘未完成工作請參閱[已知待辦](deferred-work.zh-Hant.md)；本節僅收錄直接影響系統架構評估之關鍵風險。

## 12. 詞彙表

| 詞彙 | 定義 |
| --- | --- |
| agent | 使用者選取的目標產品，共五個；`agy` 在實際投影時展開為三個目標。 |
| projection target | 可獨立進行投影並個別回報狀態之目標，共七個。 |
| MCP host | 接收 MCP 伺服器配置之宿主產品，固定為五個。 |
| canonical render | 置於 `~/.ccync/build/render/`，可完全依據鎖定檔與快取重建之標準共用輸出。 |
| live surface | 各代理實際讀取、亦可能由使用者或其他第三方工具改動的原生設定檔案或目錄。 |
| ownership proof | ccync 異動或刪除特定代理檔案前必須具備之所有權證明。 |
| adoption | 自主要代理匯入既有外掛、技能或 MCP 定義之納管流程，不包含自動升級責任。 |
| personal plugin | 使用者主動交付 ccync 進行取得、鎖定版本、升級與投影之個人外掛。 |
| projection transaction | 跨多個 MCP 主機執行寫入時，用於記錄寫入前後指紋之預寫式交易機制。 |
| machine profile | 用於跨機器備份與還原之本機輸入與匯入快照，並非全機或代理檔案之完整鏡像。 |
