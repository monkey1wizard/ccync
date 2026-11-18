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

使用者可選取的代理共有五個：`claude`、`codex`、`copilot`、`opencode` 與 `agy`。前四者各自對應單一投影目標；`agy` 則在投影時自動展開為 `agy-cli`、`agy-ide` 與 `agy-gui` 三個獨立目標，因此系統共計有七個投影目標。MCP 對 Claude、Codex、Copilot、OpenCode 與 Antigravity 五個主機獨立規劃路由，不受代理選取狀態影響。

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

來源邊界負責接收內建與個人來源清單、從五種來源代理之一匯入的項目，以及 Git URL、本機路徑、封存檔與 catalog ID。外部來源在寫入快取前，ccync 會嚴格檢查路徑是否逸出允許邊界、正確定位外掛根目錄，並辨識其內含元件型態。外掛提供的 MCP 欄位亦須單獨經過嚴格校驗：未定義之欄位會被明確列出並排除，且諸如 `headersHelper` 等隱式外部輔助指令一律拒絕執行。

各代理實際讀取的設定檔案屬於跨工具共享資源，ccync 絕不宣稱擁有整份 `~/.claude.json`、`~/.codex/config.toml` 或使用者目錄。唯有經由註冊記錄、受管標記區塊或逐主機狀態檔案得以證實之項目，才屬 ccync 之管理範圍。投影、撤回、解除安裝與中斷復原等所有操作，均受此一所有權邊界嚴格限制。

## 4. 解法策略

ccync 先依據來源定義建立共用標準輸出，隨後投影至各代理的原生檔案：

1. 讀取內建來源清單、本機設定、個人來源清單與匯入快照，產生並更新 `build/lock.json`。
2. 依據鎖定版本或內容雜湊將來源下載至 `cache/<id>@<revision>/`；後續投影皆由此快取讀取，無需重複解析外部來源。
3. 在現有 canonical render 旁建立暫存目錄，重新建置其中由 ccync 管轄之各項元件。
4. 暫存目錄建置完成後，呼叫 `ccync_foundation::platform::atomic_swap` 替換 `build/render/`；若交換失敗，現有目錄將完整保留。
5. 各目標 adapter 將技能、指令、自訂代理與受管指示區塊投影至原生設定檔中。對 Claude Code 與 Codex，通過已驗證原生 adapter 的外掛群組會以單一原生套件啟用；其餘路由維持 loose 或元件式投影。Hooks 在所有目標都沒有支援的 route，因此每個目標都會回報為不支援。
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
[ 對已驗證的原生外掛群組進行 cutover（Claude Code、Codex）：寫入私有替代項目、安裝，然後退役 loose 複本 ]
  │
[ 發布 loose 直接路由，並僅在該目標直接發布成功後才退役其合併表面 ]
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

個人輸入為 `~/.agents/mcp.json`，必要根鍵為 `ccyncMcpServers`，選用根鍵為 `hostRoutes`。有效的舊輸入可使用至使用者手動切換。新舊輸入同時存在或第三方輸入會保守停止。first-wins 宣告合併保留來源識別。匯入原件與目的地複本分開，單憑同名不能取得所有權。詳見 [MCP](mcp.zh-Hant.md) 與 [ADR 04](../../adr/04-mcp-trust-boundary-and-recovery.md)。

受管理 manifest 是衍生資料，不是實體所有權證明。`S/build/mcp/projected-state.json` 記錄已提交的逐主機證明及遷移路由意圖。預寫日誌 `S/build/mcp/projection.txn.json` 記錄指紋與交易中繼資料，不記錄解析後機密值。每個原生主機獨立檢查。衝突會保留該主機及其證據，安全主機仍可完成。符合寫入後指紋可確認已落地，符合寫入前指紋則保留原狀。無法讀取的項目或第三種指紋會停止復原並保留日誌。刪除需要該主機已提交的所有權。

restore pending 標記與主機日誌分開，記錄設定檔組及寫入前後指紋。標記存在時其他異動會停止。還原只能使用相同備份繼續，並拒絕已分歧的目的地。預覽與診斷維持只讀。Windows 支援表為空時 stdio 會遭阻擋。合成證據只能支持觀察範圍內的歸因，不能證明真實主機行為。

### External tool lifecycle

生命週期結果格式使用 `schemaVersion: 1`。Windows 的完全相符執行檔選取器依 `PATHEXT` 不分大小寫比對。完整結果欄位與選取規則見[指令參考](commands.zh-Hant.md)。

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

目前的指示寫入器會更新四個使用者範圍 `AGENTS.md` 檔案。目標包含 Codex、Copilot、OpenCode 與 Gemini。六個 target 對應四條路徑，因為三個 Antigravity target 共用 `~/.gemini/AGENTS.md`。寫入器會保留標記外的位元組，並以原子方式更新標記內的區塊。檔案不存在時會建立。標記損壞時會回報並跳過。Claude 沒有目前的寫入路徑，因此 `~/.ccync/AGENTS.md` 不會套用到 Claude 的使用者範圍。ccync 不會寫入 `~/.claude/AGENTS.md`。成功讀取來源後，一般 sync 只會在選取 `claude` 時移除舊版 `~/.claude/CLAUDE.md` 的受管區塊。無論來源不存在或有內容，都會移除該區塊。一般 sync 依設定的 target 選取執行。`ccync doctor` 不受選取、設定錯誤或來源錯誤影響，仍會分類這個舊路徑。`ccync uninstall` 也會檢查所有指示路徑，不受選取影響。詳細契約見[指令同步](commands.zh-Hant.md#指令同步)，所有權與復原見[使用者範圍指示](setup.zh-Hant.md#使用者範圍指示)。

Claude Code 的載入器與 ccync 寫入器分開運作。Claude Code 2.1.277 以上版本支援內建的 `AGENTS.md` 外掛，部分工作階段在 2.1.281 之前可能尚未支援。必須啟用此外掛，可用 `/plugin` 管理啟用狀態。Project instructions 也須選擇合適的檔案載入模式。預設的 `claude-md-or-agents-md` 模式會在工作目錄或其上層含有 `CLAUDE.md`、`.claude/CLAUDE.md` 或 `CLAUDE.local.md` 時，略過儲存庫的 `AGENTS.md`。使用者可以選擇 `claude-md-and-agents-md` 同時載入兩者。ccync 不會修改設定、不會建立橋接、匯入或符號連結，也不保證 Claude Code 會載入指示檔。詳見 [Claude Code 記憶文件](https://code.claude.com/docs/en/memory)。

`user_scope_instruction_write_path` 只解析目前的目的地。`legacy_claude_user_instruction_path` 只解析 ccync 的 Claude 舊版目的地。生命週期會分開收集這兩類路徑，因此舊版目的地不會列入目前寫入清單。舊版清理會將 `None` 傳給現有的 `projection::managed_block::write_block`。寫入器會驗證完整標記，並保留標記外的位元組。來源讀取失敗時必須保留錯誤。若把錯誤視為來源不存在，可能誤刪有效的指示區塊。

### Backup 與 restore

備份包含可攜的宣告與路由意圖，不包含主機所有權。直接 MCP 輸入含明文機密、URL userinfo 或無法安全分類的值時，備份會在建立輸出前拒絕。`restore --dry-run` 只讀。新格式 `mcp.json` 保留原始位元組，舊格式會轉換根鍵。還原不複製原生主機檔或所有權，衝突的明確路由會使還原停止。請在目的機器提供機密值，再預覽同步。

Adapter 的遠端結果範圍刻意保持精簡，回報 `Current`、`UpdateAvailable`、`Unsupported` 或 `NotChecked`。探查失敗時由 engine 轉為 `CheckFailed`，並保留它與成功但沒有更新之結果的差異。Cargo 證明會比對已記錄的精確 ref 與安裝 SHA，或比對 crates.io 套件身分及 SemVer 版本。interpreter-pip 證明會使用已記錄的直譯器與正規化 distribution。 Cargo Git 證明接受 7–40 位十六進位的已安裝 commit 片段。沒有查詢參數時選取遠端 `HEAD`；`branch` 選取 `refs/heads/<name>`；`tag` 選取解參照後的 tag，並允許輕量 tag；40 位十六進位 `rev` 不可變。可移動 ref 必須解析為唯一完整 SHA，且符合已安裝片段，才是 `Current`。crates.io 檢查要求標準來源、精確套件名稱與 SemVer 優先序較高的版本，才是 `UpdateAvailable`。遠端完整 SHA 只暫存於檢查結果，`outdated` 不會持久化。儲存層會讀取 v1 狀態記錄。所有權遷移只匯入已記錄指紋與實際 manifest 相符的舊版目前快照。無法證明的歷史世代仍沒有所有權證據。v2 狀態會以原子方式儲存目前狀態與歷史世代所有權。刪除前必須證明精確 operation ID、build 相對路徑、檔案 manifest 與雜湊及目錄清單。符號連結與 Windows reparse point 不符合所有權驗證。無法證明的舊歷史會 fail-closed。清理重試採用不可變子集，並逐一路徑重新驗證。

復原會先驗證同一工具的待處理日誌，再進行一般資格檢查。管理器已知成功時，流程會續行且不重跑管理器。結果未知時，必須使用 `--retry-manager-step`，且日誌中的工具、操作、意圖與 manifest 都須相符並通過驗證。已提交操作視為完成。兩階段能力關卡須等隔離驗收及收據通過後才開放。即時安裝、更新、復原、投影及還原仍須明確授權。CLI 結果表與 JSON 欄位定義見[指令參考](commands.zh-Hant.md)。

### Provider 身分、載入與復原

Provider 套件在 render 與目標發布期間保留來源身分。不可變來源世代位於 `~/.ccync/build/render/providers/<source-key>/<generation>/`；目標專屬套件檢視由這些世代衍生，用於發布。來源鍵取自已解析的來源身分，而非顯示名稱。元件記錄保留 `source`、原始 `plugin_name`、選填的 `upstream_revision`、`kind` 與上游 `component_id`。原生所有權記錄另保存產生套件的 digest、原生 selector 及已驗證的能力清單。已提交記錄存於 lockfile 的 `_providerLifecycle`；待處理變更使用 `~/.ccync/build/projection.txn.json`。

**身分。** Provider 同時具有 canonical 套件名稱與 ccync 管理 ID。canonical 名稱是原生用戶端列出並用於其命名空間的名稱；管理 ID 是儲存鍵，也是 `list`、`show`、`upgrade` 與 `remove` 接受的選取器。既有的儲存鍵與 pin 保持不變，因此既有管理 ID 仍可使用。`crates/ccync-engine/src/plugin_render.rs` 的 `authoritative_plugin_name` 依下列順序決定 canonical 名稱：第一個宣告名稱的目標 manifest、明確的 catalog `packageName`，最後是 bare skill 的 front-matter 名稱。`plugin`、`skills` 等通用目錄基名不算證據，因此會保留儲存 ID。manifest 名稱與 catalog 名稱互相衝突時，該 provider 會以錯誤阻擋。`show` 指令會印出 `Plugin: <package> | management ID: <id>`，指令接受的選取器仍是管理 ID。

**依來源形式決定命名空間。** 原生外掛來源在支援的 Claude Code 與 Codex adapter 上保留宣告的套件身分與原生套件路由。獨立技能或已物化元件沒有可保留的原生外掛套件身分，其發布身分來自來源與元件名稱，不會加入由管理器產生的 `ccync:` 命名空間。不同來源發生命名衝突時，provider 限定別名會以確定方式處理；仍無法區分的原生命名空間衝突會回報為衝突，ccync 不會默默合併或丟棄來源。

原生目標保留其原生 provider 與元件命名空間。扁平 adapter 保留唯一的上游呼叫名稱；發生名稱碰撞時，使用確定性的 provider-qualified alias。若正規化或截斷後仍會碰撞，alias 會加上 digest 後綴。Alias 記錄包含 target、完整元件身分與發布名稱。移除後重新加入時，先前指派的 alias 仍予保留。Alias 長度遵守目標限制。來源鍵、世代 ID 與 alias 也會驗證為跨平台安全的路徑片段：最多 128 bytes，且不得含路徑分隔符、保留字元或 Windows 裝置名稱。只有身分明確時才會改寫產生的參照；不改寫不透明的上游文字。若產生的參照有歧義，會以診斷訊息失敗。不同來源發生原生命名空間碰撞時視為衝突，ccync 不會丟棄任一 provider。

Provider 套件檢視會排除啟用中的原生 MCP 入口，讓既有集中式 MCP 投影繼續作為唯一的宿主設定路徑。來源世代仍保留 MCP 宣告，供該投影使用。Hook helper 資源則保留於 provider 套件中。若目標檢視無法安全移除或解讀 MCP 宣告，或缺少必要能力，該目標會失敗，不會靜默遺失功能。

只有已提交的 ccync 記錄同時符合精確 selector、來源身分、上游 revision、產生套件 digest 與必要能力時，才能建立原生所有權。名稱相同或內容相同都不足以證明所有權。目前從目標讀取的註冊與 digest 屬於觀察狀態；目前來源所要求的世代屬於期望狀態。若觀察結果仍符合已提交的舊世代，但期望內容已變更，這是受管升級。若觀察結果與已提交世代不符，則屬 drift 或衝突。外部、停用、損毀、無法讀取或無法驗證的註冊會阻止變更，並保持原狀。

待處理意圖會記錄 target、selector 與預期套件 digest 指紋、受範圍限制的 live path、選填的預存原始檔、前一個與期望世代、client scope、來源與 view digest、enablement 意圖，以及目前階段。ccync 會在描述的原生安裝或移除之前，於 provider lifecycle lock 之下將它寫入 `~/.ccync/build/projection.txn.json`。正式程式只寫入兩個階段：原生安裝或切換寫入 `publishing`，解除安裝的移除寫入 `pruning`。在目前審閱的程式碼中，`staged` 與 `verifying` 只出現於測試；`reloading` 是保留的變體，沒有寫入者，也沒有載入器重新載入檢查。Provider rollout 會先驗證來源與觀察狀態、暫存替代套件、寫入此意圖，再發布或安裝，然後重新讀取原生註冊並檢查能力。驗證通過後才提交所有權。只有所有已選取的使用端都具備有效替代項目後，才會清除舊路徑。復原會先觀察記錄中的 selector 與 digest 再重試，不會盲目重複安裝或移除。失敗時會還原已證明可用的世代，或回報可復原的部分狀態，並保留仍在使用的 loader 與所有權證據。解除安裝只會移除精確且已提交、由 ccync 管理的 selector。解除安裝若無法讀取已提交記錄或待處理意圖，會保留所有 provider 世代並回報警告，不會清除。

**暫時證明與復原。** 首次安裝與遷移沒有已提交的記錄，因此 `delivery_apply.rs` 的安裝分支依賴待處理意圖。只有待處理意圖記錄相同的 selector 與操作，且重新觀察到的已安裝 view 符合該意圖記錄的 view digest 時，既有註冊才會被採納並提交（`classify_adopted_view`）。只靠名稱相同，都不能證明所有權，此類同名註冊仍視為衝突。安裝嘗試失敗時，只有在失敗嘗試的操作與先前意圖不同，且該嘗試中沒有任何會改變註冊的 CLI 呼叫成功時，才會還原先前的意圖。同一操作的嘗試會取代先前的意圖。若會改變註冊的 CLI 呼叫已成功，該嘗試的意圖會保留在日誌中，下次執行再協調觀察到的註冊。外部漂移或意圖歸屬不明時，復原會被阻擋：意圖與其預存原始檔保留在磁碟上，並回報衝突。

**各用戶端路由。** `crates/ccync-engine/src/delivery.rs` 的 `native_plugin_capable` 只對 Claude Code 與 Codex 回傳 true。原生外掛群組還必須通過 adapter 檢查（`native_adapter_states`）：只有 CLI 的 `major.minor` 版本位於即時探測已驗證的版本線時，才允許變更。其他版本或無法讀取的版本會保留 loose 路由，並以 `unverified adapter version <version>` 或 `unreadable adapter version` 作為原因。各目標路由如下：

| 目標 | 路由 | 原生狀態 |
| --- | --- | --- |
| `claude`（Claude Code） | 原生外掛套件，依外掛群組透過 `NativeExecutable::Claude` 啟用。 | 只限 Claude Code 2.1 版本線。其他或無法讀取的版本保留 loose 路由。 |
| `codex` | 原生外掛套件，依外掛群組透過 `NativeExecutable::Codex` 啟用。 | 只限 Codex CLI 0.169 版本線。其他或無法讀取的版本保留 loose 路由。 |
| `copilot`（CLI 與 VS Code 橋接） | 位於共用與私有根目錄的 loose skill 與元件路由。 | 沒有原生 adapter；僅使用 loose 路由。隔離的 Copilot CLI 列出探測看到一個 identity。 |
| `agy-cli`、`agy-ide`、`agy-gui` | 位於 AGY 根目錄的 loose skill 與 agent 路由。 | 沒有原生 adapter；僅使用 loose 路由。AGY CLI 探測為 NotRun，IDE 與 GUI 尚未驗證。 |
| `opencode` | 元件路由。skill 另有位於 `~/.config/opencode/skills` 的私有替代根目錄。 | 本程式碼中沒有外掛路由。 |

Claude Desktop 不等同於 Claude Code，本文件不對 Claude Desktop 作任何主張。

**群組切換。** 原生群組是同一個原生目標上同一外掛的所有路由。`plan_native_routes` 只在其 adapter 就緒時才將群組標為 `Native`，群組以整體為單位：任一成員失敗，整組回退。如果群組會載入的共用複本退役後，有讀取端會因此失去該元件，該群組就會被阻擋。`SHARED_ROOT_CONSUMERS` 列出所有已知讀取端，無論是否被選取，判斷方式都相同。讀取端在三種情況下仍保有該元件。第一，讀取端在同一次執行中以原生方式啟用該元件。第二，讀取端探索的另一個共用根目錄仍保留該複本，因為沒有原生群組會退役那份複本。第三，讀取端的私有根目錄有該元件，例如 Copilot 的 `~/.copilot/skills`，或讀取端是 OpenCode，其替代根目錄為 `~/.config/opencode/skills`。只有 skill 複本能涵蓋讀取端，所以共用根目錄中由 command 轉成的 skill 仍會阻擋其群組。所有群組一開始都視為原生。依序第一個被阻擋的群組會回退，檢查會重複，直到沒有原生群組被阻擋。在任何原生變更之前，ccync 會發布每份涵蓋用的複本，並確認其內容與 rendered generation 中的 skill 來源一致。若 provider 本身是單一 skill，且 `SKILL.md` 位於來源根目錄，就以該根目錄作為 skill 目錄發布。只有在 `retire_shared_path` 證明所有權並計入所有讀取者後，才會移除 loose 複本。若群組啟用失敗，`NativeRouteStatus::Fallback` 會記錄原因。沒有已提交原生記錄的群組保留 loose 路由。已有已提交原生記錄的群組則保留先前的原生路由（若註冊使用舊 selector，則為舊的原生路由），且不寫入 loose 路由。回退絕不會被回報為原生成功。

**舊版複本與參照。** 每次實際同步開始時，ccync 會檢查每個已選取目標的原生 skill 根目錄，找出舊版 ccync 留下、沒有所有權記錄的 skill 目錄。這類目錄若不是連結，且內容與同名 skill 目前的來源完全相同，ccync 就把它移到 `~/.ccync/backups/legacy-copies/<unix-seconds>/`，並回報這次移動。同樣的檢查也涵蓋每個已選取目標的原生 agent 根目錄。ccync 以別名發布某個 agent 時，若原名位置的檔案是一般檔案、direct agent 寫入器沒有它的紀錄，而且位元組與同一根目錄的別名副本完全相同，ccync 就會把它移走。這類檔案可能沒有擁有權紀錄，是舊版 ccync 留下的，也可能只由原名 agent 寫入器記錄。使用別名的 agent 路由也會保留原名檔案的位置，所以原名寫入器不會在別名副本旁邊再寫一份。只由原名寫入器記錄的原名檔案，在 direct 寫入器已於任一已選取的 agent 根目錄以別名記錄該 agent 後，即使位元組不同，ccync 也會把它移走，因為原名寫入器不會再寫入或記錄它。其他沒有所有權記錄的項目會保留並視為外來項目，所以 provider 元件會以別名發布。每次同步都會回報仍佔用 provider agent 原名、沒有所有權記錄的 agent 檔案，並附上移除它的命令。ccync 不會自行移除這些檔案。外來項目只會造成別名，不會參與 agent 或 command 內文的參照解析。只有在內文開頭，或空白、`(`、`[`、反引號、引號之後，`/` 才是指令參照的開頭，所以檔案路徑和網址會保留原名。外掛群組在某個目標以原生方式啟用時，若該目標的原生載入器會讀取這個元件類型，被阻擋的元件就不會讓合併 surface 保留下來。Claude Code 會從外掛套件讀取 skill、agent 與 command。Codex 則只計入 skill。

**舊 selector 遷移。** 若同一目標、同一 provider 來源與同一外掛已有使用舊 selector 的已提交 ccync 註冊，新安裝與舊註冊的移除會共用同一個待處理意圖。舊註冊只在新註冊驗證後才移除，且移除時使用新的證明，之後才提交所有權。閘門要求：相同的 provider 來源鍵、相同的上游 revision、形式為 `<name>@ccync-<name>` 的舊 selector、完整的所有權證明，且觀察到的 view digest 相符。停用、無法觀察或無法證明屬於 ccync 的舊註冊會阻擋該群組，兩個註冊都會保留。由於已提交記錄不儲存儲存庫路徑、subpath 與 manifest 身分，閘門不會另外比對這些項目。

**沒有全域原子性。** ccync 無法把 Claude Code、Codex、檔案系統與執行中的工作階段變更，合併成單一交易。lifecycle lock 與 journal 決定 ccync 每個步驟的執行順序。原生副作用之後若發生當機，會留下必須先復原的意圖。原生 CLI 可能複製檔案、改寫中繼資料、要求重新啟動，或在副作用之後失敗，因此 ccync 會在每次重試或刪除前重新觀察原生狀態。

Provider lifecycle journal 使用 v3 schema。內容不含較新意圖類型的 v1 與 v2 journal 可讀取，之後會寫成 v3。v1 不可含獨立發布意圖，v1 或 v2 不可含獨立撤回意圖。待處理的發布或撤回意圖會保留，直到復原證明操作已完成或可安全放棄為止。只要仍有待處理意圖，寫入端就不會降級 journal，因為舊 schema 無法表示所有待處理操作。不支援 v3 的舊讀取端會拒絕該 journal，不會丟棄意圖資料。

獨立發布的 sync 與 doctor 會區分 `standalone published`、`recovery blocked` 與 `identity unresolved`。受阻或無法解析的操作會視情況保留 journal、所有權證明與受影響檔案。保留或 fallback 路由也會與原生啟用成功分開回報。這些 CLI 狀態說明磁碟發布與復原證據，不能證明附加的應用程式或執行中的代理已載入元件。

七個投影目標都在各自的原生探索根目錄取得 provider 元件：技能放在目標的技能根目錄，代理放在代理根目錄，指令放在指令根目錄。沒有原生指令根目錄的目標，會在其技能根目錄以等價的 command-skill 發布指令。ccync 依下方共用根目錄表，對每個實體根目錄最多寫入一次元件。舊的合併表面只作為備援：`~/.claude/skills/ccync` 連結、合併的 `ccync` marketplace 項目、位於 `~/.copilot/installed-plugins/ccync-copilot` 的 Copilot plugin 複本，以及 AGY CLI、IDE、GUI 三個介面的 `plugins/ccync` 根目錄。探索與能力支援依目標而異。缺少 CLI 支援、不支援必要功能或無法驗證探索結果時，該目標會失敗或標記為 `NotRun`，不會回報受管載入成功。只有所有已選取使用端都有替代項目後，才會撤除共用路徑；系統不假設存在通用共用根目錄或傘狀 plugin。

目前狀態：一般 `ccync sync` 預設會透過各目標的直接表面發布 provider 元件。每次執行都會先規劃路由。在發布 loose 路由之前，會先為每個 adapter 已就緒的 Claude Code 與 Codex 外掛群組嘗試原生啟用。啟用時會先寫入共用讀取者所需的私有替代項目、安裝原生註冊、提交所有權，然後退役該群組取代的 loose 複本。adapter 未就緒的群組保留 loose 路由並記錄原因。啟用失敗的群組，若沒有已提交的原生記錄，會保留 loose 路由；若有，則該原生路由保留，且不寫入 loose 路由。其餘路由以受防護的替換方式發布，並在同一次 sync 中，待目標的直接發布成功後退役合併的 `ccync` 表面。替換失敗的目標會保留合併寫入器，作為該目標的備援。同一次 sync 若退役 Codex 合併表面，只會在該次輸出 Codex 手動移除提示。取消選取某目標的 sync 會移除 lockfile 記錄的該目標直接技能與代理複本，除非規劃路由仍指向相同路徑。`ccync uninstall` 會移除 lockfile 記錄的所有直接複本，包括 AGY 技能與代理根目錄。Copilot plugin 複本與三個 AGY `plugins/ccync` 根目錄遵循相同規則：目標的直接發布成功後，一般 sync 會移除 ccync 擁有權記錄能證明的每一個，之後的 sync 不再寫入；取消選取某介面時，也會移除該介面擁有的根目錄。這些路徑上不屬於 ccync 的目錄會保留並回報。有元件必須經使用者操作才能被探索的目標，會保留其合併表面。共用根目錄中的合併複本，在該根目錄的直接複本寫入後即退役。

共用技能根目錄可能被多個使用端讀取。下表每列記錄根目錄、會探索該目錄的使用端、其服務的目標，以及已檢查的來源。各列與 `crates/ccync-engine/src/delivery.rs` 的 `SHARED_ROOT_CONSUMERS` 一致。

| 共用根目錄 | 會探索此目錄的使用端 | 服務的目標 | 來源網址 | 檢查日期 |
| --- | --- | --- | --- | --- |
| `~/.agents/skills` | Codex、Copilot、OpenCode | Codex、OpenCode | <https://learn.chatgpt.com/docs/build-skills>；<https://docs.github.com/en/copilot/concepts/agents/about-agent-skills>；<https://opencode.ai/docs/skills/> | 2026-10-16 |
| `~/.claude/skills` | Claude、OpenCode | Claude、OpenCode | <https://docs.anthropic.com/en/docs/claude-code/skills>；<https://opencode.ai/docs/skills/> | 2026-10-16 |
| `~/.gemini/config/skills` | AGY IDE、AGY GUI | AGY IDE、AGY GUI | <https://antigravity.google/docs/skills> | 2026-10-16 |
| `~/.gemini/config/agents` | AGY CLI、AGY GUI | AGY CLI、AGY GUI | <https://antigravity.google/docs/cli/commands/agents/> | 2026-10-16 |

Copilot CLI 會讀取 `~/.agents/skills`，但 Copilot 不由該根目錄服務，因為其 VS Code 橋接會停用該位置。

2026-10-17 的即時探索檢查各計數到一個 provider 項目。Claude 檢查回報 `count=1`，Claude Code 版本為 2.1.296。Codex 檢查回報 `count=1`，codex-cli 版本為 0.163.0。計數只表示 CLI 探索到該項目，不表示執行中的代理已載入該元件。檔案系統與註冊檢查同樣無法證明代理已有效載入。

檔案發布與註冊檢查只能確認檔案系統及原生中繼資料狀態，無法證明執行中的代理已載入元件。載入器重新載入與有效載入檢查會分開回報；未執行的檢查維持 `NotRun`。

### 直接所有權與退役

- **寫入者。** 同一次執行中，每個實體技能根目錄每個元件最多寫入一次，即使多個目標共用該根目錄。目標選定的路由即為其寫入者：loose 直接複本，或已啟用群組的原生套件。合併複本只在該目標的直接發布失敗或受阻時，作為其備援而保留。
- **所有權記錄。** 每筆記錄對應一個具體路徑，並標明類型，例如直接技能、直接代理、直接指令或來源連結。所有權只能經由這些記錄成立。名稱相符或內容相符，既不會建立記錄，也無法證明所有權。
- **遷移。** sync 在載入設定前，先遷移舊版配置。合併表面只有在其替代項目成功後才會退役。
- **共用根目錄退役。** 共用根目錄中的合併複本，只有在該根目錄所有已知讀取者（無論是否被選取）都已計入後才會移除。不是就緒原生目標的讀取者，必須已有已驗證的替代項目；否則複本保留。只要依賴該複本的任一目標失敗或受阻，該複本也會保留。上表列出各根目錄的讀取者。
- **失敗保留。** 失敗的目標會保留其既有表面與備援。即使其他目標成功，該目標的失敗仍會顯示在其自身結果中。若移除已記錄的複本失敗，該筆記錄會保留，並保留 `build/lock.json`，供後續執行重試。
- **取消選取與解除安裝。** 取消選取某目標時，會移除其記錄的直接複本，除非規劃中的路由仍指向相同路徑。解除安裝會依序移除每個已記錄的直接複本、每個已記錄的來源連結，以及獲授權的舊版子項目。除使用者取代的來源連結外，記錄只會在移除成功後才刪除。被使用者取代的來源連結會留在原處並回報，其記錄仍會刪除。解除安裝也會移除已提交的原生註冊，且每個註冊都必須先經即時重新觀察，以 view digest 證明為 ccync 所擁有後才移除。無法證明所有權，或 adapter 未驗證的註冊，會保留其已提交記錄並回報。取消選取不會移除原生註冊。
- **來源連結。** 機器執行會把它建立的 Copilot 與 Antigravity 面向來源連結，記錄於 `sourceLinkProjectionPaths`。移除來源連結時，不會移除其目標。若後續執行未選取該連結的目標，連結記錄會保留。

### 原生診斷與驗收狀態

`ccync doctor` 會以唯讀的列出指令重新觀察每筆已提交的原生記錄（`crates/ccync-engine/src/doctor.rs` 的 `NativeObservations::observe_committed`），不會變更任何狀態。健康的 CLI 會對每筆已提交記錄各列出一次。列出失敗或逾時後，同一次觀察不會再對該 executable 其餘記錄重試。無法啟動的 CLI 會回報 `Unavailable`。`crates/ccync-engine/src/managed_view.rs` 的 `NativeState` 由輕到重包含以下狀態：

- **registered, loader NotRun（已註冊，載入器未執行）：** 已擁有、已啟用，且與已提交記錄一致。檔案系統與註冊檢查都不能證明執行中的用戶端已載入元件。
- **deliberately disabled（刻意停用）：** 預期載入次數為零，且元件不算遺失。
- **owned, older generation（已擁有，為較舊世代）：** 註冊與較舊的已提交世代相符。
- **unavailable, NotRun 與 unreadable, NotRun：** 無法讀取 CLI 或其列出結果。
- **absent（不存在）：** 沒有註冊占用該 selector。
- **broken（無法載入）：** 用戶端回報該註冊無法載入。
- **drifted from committed generation（與已提交世代漂移）：** 註冊已不符合已提交記錄。
- **foreign registration（外部註冊）：** 占用該 selector 的註冊不屬於 ccync。

路由退回時會顯示原因，且 loose 路由仍為使用中。`_providerLifecycle` 區段無法讀取或格式錯誤時，會輸出 native route state is unknown 的警告，不會沉默，也不會回報完全成功（`native_section_findings`）。

驗收狀態（截至 2026-10-22）：

- 隔離 profile 的即時探測已在 Codex CLI 0.163.0、Claude Code 2.1、GitHub Copilot CLI 1.0.101 與 OpenCode 1.18.35 上執行。每項都回報 identity 與啟用計數為 1，停用計數為 0。這些探測不能證明 desktop、IDE 或 App 已載入該元件。
- OpenCode 2.3.0 沒有列出已載入 skill 的命令，因此探測會檢查隔離 skill 根目錄中的 `SKILL.md` 是否送達與撤回，但不會觀察載入器計數。
- Copilot 探測只涵蓋 CLI，VS Code 橋接尚未驗證。
- AGY CLI 1.4.4 的實機探測為 NotRun。`agy -p /skills` 會啟動模型回合，而不是由 CLI 處理的列出結果，因此探測無法讀取列表。AGY IDE 與 GUI 尚未驗證。
- 擁有者於 2026-10-24 回報，真實機器上的驗收已完成，範圍包括 Codex/ChatGPT 與 Claude 應用程式，以及 OpenCode、Copilot CLI 與 VS Code、AGY IDE 與 GUI，以及未選取的共用根目錄讀取者。證據沒有存放在此儲存庫，本文件也未驗證。CLI 清單與檔案系統檢查無法證明附加 UI 載入、帳戶介面中的啟用或停用數量，或擁有者機器上的遷移。
- 真實機器上的遷移仍待完成。

## 7. Deployment View

ccync 採單一二進位執行檔形式發布。正式發行管線涵蓋六個平台目標：Windows x64/arm64、macOS x64/arm64 以及 Linux musl x64/arm64。各平台均提供獨立執行檔以及僅內含執行檔與 `LICENSE` 的最小化封存檔。Windows MSVC 版本嚴格採用 static CRT 靜態連結建置，徹底防範無 Visual C++ 可轉散發套件環境因 `0xC0000135` 導致啟動失敗。

私有 GitHub 是發布控制器。其受防護發布流程會以單次原子推送，把 `main` 的 curated 快照與發布 tag 推送至公開儲存庫。穩定版 tag 使用 `vX.Y.Z`，候選版使用 `vX.Y.Z-rc` 或 `vX.Y.Z-rcN`。tag 送達後，公開儲存庫的 GitHub Actions 會核對 tag 的版本基底與 crate 版本相符並檢查變更紀錄，產生 `checksums.txt`、發行檔案清單、WinGet 清單以及 Homebrew formula。隨後藉由 GitHub OIDC 機制執行 cosign 無金鑰簽章。唯有正式穩定版才會發布至 Homebrew tap 並向 WinGet 提交；預覽版本絕不進入此兩項公開套件管道。發行後驗收流程會自動校驗 checksum、數位簽章、Cargo 安裝相容性、Unix/Windows 安裝腳本以及穩定版 Homebrew 的可用性。

運行環境無需任何常駐伺服器；ccync 僅需讀寫本機 `~/.ccync/`、呼叫 Git、curl 或套件管理工具，並更新本機代理設定檔。

## 8. Crosscutting Concepts

### 狀態路徑、生效表面與可攜匯出

ccync 把資料放在四個根目錄。環境變數沒有設定時，ccync 使用家目錄下的預設位置。在 Linux 與 macOS 上，XDG 變數是空值時也使用預設位置。

| 根目錄 | Windows | Linux 與 macOS | 內容 |
| --- | --- | --- | --- |
| `P` 可攜 | `%USERPROFILE%\.agents\ccync` | `~/.agents/ccync` | 衍生匯出與可攜套件快照。 |
| `C` 設定 | `%APPDATA%\ccync` | `$XDG_CONFIG_HOME/ccync`，預設 `~/.config/ccync` | 使用者擁有的設定輸入。 |
| `S` 狀態 | `%LOCALAPPDATA%\ccync\state` | `$XDG_STATE_HOME/ccync`，預設 `~/.local/state/ccync` | 私有權威資料、建置輸出與鎖定檔。 |
| `K` 快取 | `%LOCALAPPDATA%\ccync\cache` | `$XDG_CACHE_HOME/ccync`，預設 `~/.cache/ccync` | 本機快取。 |

正式程式碼透過 `ccync-foundation` 已載入的 `EffectiveLayout` 解析已搬移狀態。舊 getter 對照如下：

| 舊 accessor 或路徑 | 已提交位置或替代方式 | 用途 |
| --- | --- | --- |
| `machine_config_path` 與 `CcyncConfig::config_path` | `C/config.json` | 機器設定 accessor。 |
| `mcp_input_path` | `~/.agents/mcp.json` | 使用者所有的個人 MCP 輸入。 |
| `ccync_agents_md_path`、`tools_json_path` | `C` 下同名檔案 | 使用者所有的設定輸入。 |
| `personal_catalog_path` | `C/plugins.json` | 私有來源權威。`P/sources.json` 是衍生匯出檔。 |
| `plugins_lock_path` | `S/build/lock.json` | 私有 lock 與所有權權威。`P/lock.json` 是衍生匯出檔。 |
| `ccync_build_root`、`provider_lifecycle_journal_path`、`generated_mcp_path`，以及工具狀態、產物與操作交易紀錄 accessor | `S/build` 下相同相對路徑 | 私有持久狀態與可重建輸出。 |
| `ccync_cache_root` | `S/sources` | 持久來源快取。 |
| `lifecycle_lock_path` | 固定舊鎖路徑與 `S/lifecycle.lock` | 協調鎖維持在文件指定的固定位置。 |

舊 accessor 保留給遷移與舊權威行為使用。新的正式程式碼使用 `EffectiveLayout`；foundation 的 `provider.rs` resolver 可以使用固定的 `PathRoot` 值。`migrate.rs` 中舊有的 `.join("build")` 子配置只供遷移輸入使用。`ccync-engine/tests/legacy_layout_scan.rs` 的正式掃描會檢查這些界線。

在 P7 搬移前，生效表面維持在已註冊的位置。這些位置包括 `~/.ccync/plugins/`、來源連結、`~/.ccync/build/render/`、`~/.ccync/build/active/`、`~/.ccync/data/` 與 `~/.ccync/install-receipts/`。Copilot 的 `~/.copilot/ccync` 連結仍指向 `~/.ccync`。原生代理程式設定檔與目錄仍透過各自的生效表面 accessor 管理，包括 Claude 與 Codex MCP 設定、代理程式外掛狀態、skill 根目錄及使用者範圍指令檔。`install_receipt_path` 與生效表面 getter 不屬於配置狀態 accessor。

可攜的 `P/sources.json` 與 `P/lock.json` 是從私有權威衍生的匯出檔。寫入器最後才輸出它們。路由與所有權只讀取私有狀態，永不讀取這些匯出檔。手動編輯不會生效，後續匯出會取代它們。透過 P5 匯出的套件快照遵循相同規則，只包含已准入的可攜內容。未分類或含憑證的資料保留在私有儲存。

### Ownership proof

在 ccync 的安全哲學中，刪除操作相較於寫入操作需要更嚴苛的證據門檻。受管檔案註冊記錄、逐主機 MCP 狀態證明、Antigravity 根目錄所有權以及受管標記區塊，皆可作為合法的所有權證明。僅憑檔案名稱相同、內容相似或恰好位於預設路徑，均不構成足夠的所有權證明。一旦證據不足，ccync 會優先保留現有檔案；此舉雖可能暫時殘留孤立檔案，但能徹底杜絕誤刪使用者私有資料的致命風險。

### Atomicity 與 failure isolation

單一檔案的寫入均遵循「在同一目錄建立暫存檔後再行重新命名」之原則；canonical render 則採「在相鄰位置建置暫存目錄後進行原子目錄交換」機制。MCP 因牽涉多個獨立主機設定檔，無法仰賴單一檔案系統交易達成，因而改採預寫式交易記錄、指紋校驗與復原分類器，確保各主機最終收斂至一致狀態。所有錯誤回報均會具體指出對應的目標平台或伺服器名稱，絕不讓局部失敗在巨觀統計中被隱蔽。

### Secrets 與 placeholders

`config.json#secrets` 屬於本機明文輸入。在將設定寫回檔案時，ccync 會智慧合併原始 JSON，完整保留程式模型未定義的擴充欄位。MCP 變數支援 `${VAR}`、`${VAR:-default}`、`PLUGIN_ROOT`、`PLUGIN_DATA` 與受控之 Claude 別名。交易記錄不記錄解析後的機密值。備份拒絕不安全的直接輸入，並可遮蔽受支援的匯入值。此等機制旨在縮減機密值被持久化落盤或意外匯出的暴露面，而非提供磁碟層級的加密防護。

### Native adapters

共用規格並不等同於所有目標均具備相同的功能支援度。Claude、Codex、Copilot、OpenCode 與三個 Antigravity 目標各自擁有專屬 adapter，負責處理 frontmatter 格式轉換、路徑對映、符號連結或複本選擇，以及不支援欄位的安全降級。每個 adapter 均須以明確的目標識別碼回報其執行狀態，禁止以單一通用格式粗暴抹煞各產品的原生特性。原生外掛套件只為 Claude Code 與 Codex 產生；其他目標維持 loose 或元件式路由。

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
- 原生外掛交付沒有跨 Claude Code、Codex、檔案系統與執行中工作階段的原子性；復原以每個已日誌化的意圖為單位進行。
- OpenCode 私有 skill 複本會在原生安裝之前寫入。若安裝失敗，該複本不會回復。
- 較舊世代的待處理意圖若沒有預存原始檔，可能在之後的提交後繼續留存。該意圖永不清除，並會保留其 provider 世代。
- `ccync doctor` 在單次執行中仍會兩次觀察原生 CLI：路由證據行與 native-routes 檢查各呼叫一次 `NativeObservations::observe_committed`。單次觀察中失敗的列出不會重試，但第二次觀察仍是已記錄的後續工作。
- sync 的成功訊息尚未計入原生回退；回退原因會列入執行警告。
- 遷移群組被阻擋時，其共用 skill 複本仍保留在原位，而 Claude Code 也會讀取該複本。
- 若當機發生在所有權提交之後、清除意圖之前，具有多個指紋的遷移意圖可能連同其預存原始檔留在日誌中；所有權仍完整，復原也不會因此被阻擋。
- 報告中的用戶端其 desktop、IDE 與 App 實際載入尚未驗證。請見「原生診斷與驗收狀態」。

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
| adoption | 自五種來源代理之一匯入既有外掛、技能或 MCP 定義之納管流程，不包含自動升級責任。 |
| personal plugin | 使用者主動交付 ccync 進行取得、鎖定版本、升級與投影之個人外掛。 |
| projection transaction | 跨多個 MCP 主機執行寫入時，用於記錄寫入前後指紋之預寫式交易機制。 |
| machine profile | 用於跨機器備份與還原之本機輸入與匯入快照，並非全機或代理檔案之完整鏡像。 |
| 管理 ID（management ID） | provider 的儲存鍵與選取器，供 `list`、`show`、`upgrade` 與 `remove` 使用。它不一定等於原生套件名稱。 |
| canonical 套件名稱（canonical package name） | 原生用戶端為 provider 套件列出的名稱，取自權威 manifest、catalog 套件名稱或 skill 名稱。 |
| 原生套件（native package） | 由已驗證 adapter 為 Claude Code 或 Codex 產生並啟用的單一外掛套件；其他目標不使用。 |
| loose 路由（loose route） | 直接寫入目標 skill、agent 或指令根目錄的檔案式投影。 |
