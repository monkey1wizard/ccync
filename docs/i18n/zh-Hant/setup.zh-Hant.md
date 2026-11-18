---
type: guide
title: 設定、生命週期與診斷
description: ccync 初始化、同步、診斷、machine state、備份、還原與解除安裝的操作契約。
tags: [setup, lifecycle, doctor, backup]
status: stable
locale: zh-Hant
translation_key: setup
---

# 設定、生命週期與診斷

## 初始化

`ccync init [<source>]` 指定一個來源代理：`claude`、`codex`、`copilot`、`opencode` 或 `agy`。它會掃描該代理已安裝的外掛、獨立技能與 MCP 伺服器定義，但不寫入任何內容，接著列出將匯入的項目與將寫入的目標。操作者確認前，不會寫入任何東西。確認後，ccync 會記錄匯入紀錄、把目標選取寫入 `~/.ccync/config.json`，並執行首次投影。在互動式終端中，若省略來源或目標，指令會提示選取。在非互動式腳本中，操作者必須提供來源、`--agents <list>`，以及 `--yes` 或 `CCYNC_ASSUME_YES=1`。

預覽會把檔案可讀、但指令不在 `PATH` 上的來源標示為 `source state: offline`。匯入仍會繼續，該來源的載入驗證維持 `NotRun`。預覽也會列出已選目標所能到達的每個共用根目錄，並列出該根目錄的所有使用者，包括你未選取的目標。

預覽與完成報告也會列出掃描阻擋的每個 MCP 定義及其原因，並把已停用的定義另列一行：

```text
mcp blocked: 1 server(s), not imported; mcp disabled: 1 server(s)
```

被阻擋的定義不會匯入，同一來源中其他有效項目仍會匯入。報告只顯示名稱、原因與設定路徑，絕不顯示憑證值。若兩個獨立技能同名但來源位置不同，sync 會阻擋整組項目。請見[鬆散技能衝突與重新掃描](adoption.zh-Hant.md#鬆散技能衝突與重新掃描)。

`--agents` 接受五個代理名稱、`all`，以及七個 target 代碼：`claude`、`codex`、`copilot`、`opencode`、`agy-cli`、`agy-ide` 與 `agy-gui`。`agy` 會展開為三個介面。以 target 代碼列出的清單會保留部分 AGY 子集。空清單會被拒絕。

先前的寫法 `ccync init claude` 在腳本中不需確認即會執行。現在它會以結束碼 `64` 停止，直到你補上 `--agents` 與 `--yes`。結束碼定義見[指令參考](commands.zh-Hant.md)。

請特別注意：初始化流程在匯入完成後可能直接引導進入首次投影，並要求確認即將寫入的代理檔案與 MCP 設定。切勿將初始化完成後才執行的 `sync --dry-run` 誤當成初始化寫入前的預覽機制。

在完成初始化之前，`sync`、`remove`、一般 `add` 以及非 dry-run 的 `upgrade` 均不會異動任何本機資料，而是主動提示操作者先執行 `ccync init <source>`。

## 同步與首次覆寫閘門

`ccync sync [--dry-run] [--yes]` 會讀取來源清單、快取與鎖定檔，建置出共用的 canonical render，隨後投影至所選目標。五個固定 MCP 主機則由獨立管道排程更新，不受個別代理之選取狀態影響。

在舊版儲存配置中，`sync --dry-run` 會回報儲存清單與待處理的舊子目錄修正，不會變更檔案。套用時，`sync` 會先復原狀態並提交可攜式配置，再進行投影。`refresh` 不會執行此可攜式遷移。若偵測到舊版配置，會提示由 `ccync sync` 執行遷移。

### 發布結果與重試

每次同步後，每個 target 都有一個狀態：`Applied`、`Unchanged`、`Skipped`、`Failed` 或 `NotRun`。當 target 所需的路由全部已寫入或與目標內容相符時，該 target 才算已發布。若有任一必要路由未寫入，該 target 的證據行會顯示 `publication not published` 並附上重試動作，sync 以 `1` 結束。該 target 的私有合併備援會保留。

```text
evidence [codex]: publication not published; loader NotRun; served-by-root not reported; retry: fix the reported write error, then rerun `ccync sync`
```

復原時，先修正回報的原因，執行 `ccync sync --dry-run`，再執行 `ccync sync`。已相符的路由維持不變。下表列出 sync 會回報的路由結果。

| 路由結果 | sync 的輸出 | 對執行的影響 |
| --- | --- | --- |
| 已寫入或已相符 | `published` | 沒有任何 target 失敗時結束碼為 `0`。直接替代檔完成後，合併副本即可退役。 |
| 寫入失敗 | `not published`，並附上回報的寫入錯誤 | 結束碼為 `1`。私有備援保留。 |
| 因衝突而阻擋 | `not published; conflict: <reason>; action: resolve the conflict, then rerun ccync sync` | 結束碼為 `1`。私有備援保留。 |
| target 不支援 | `not published because unsupported: <reason>`，動作為 `no write is needed for this unsupported route` | 不算失敗，也不需要替代。 |

`loader NotRun` 表示 ccync 未對該路由執行載入檢查。它不代表載入失敗，也不是代理已載入該元件的證據。只有真實的代理工作階段能證明這點。在你檢查該工作階段之前，請記錄為 `NotRun`。

### 外掛提供者與管理標籤

目前狀態：一般 `ccync sync` 會直接在各目標的原生根目錄發布 provider 元件，並對直接發布成功的每個目標退役合併的 `ccync` 表面。對於 Claude Code 與 Codex，一般 sync 也會在 adapter 已驗證、且 CLI 版本位於已驗證版本線（Claude Code 2.1、Codex 0.169）時，把各外掛群組註冊為原生套件。否則該群組維持 loose 路由，sync 會回報原因，例如 `unverified adapter version <version>`。Copilot、AGY 與 OpenCode 維持其 loose 或元件路由。發布只保證檔案層級，因此以下任何標籤都不能證明執行中的代理已載入元件。詳見[提供者身分、載入與復原](architecture.zh-Hant.md#provider-身分載入與復原)。

身分依來源形式決定。原生外掛來源保留其原生套件身分。獨立技能與已物化元件使用來源與元件身分，不加入由管理器產生的 `ccync:` 命名空間。不同來源若有相同的原生身分，會回報為衝突，不會默默合併或移除。

`ccync list` 可能顯示如下的提供者診斷（範例值僅示意格式）：`Provider: demo | managed by ccync | loader: demo@<marketplace> | ownership: Managed | validation: LoaderNotRun`。`Provider` 是原始外掛身分，`managed by ccync` 則另外標示管理者。此例表示提供者仍是 `demo`，而 ccync 管理其目標安裝。載入器與列舉值會依目標及觀察到的狀態而異。單憑此標籤不能證明執行中的代理已載入元件。

selector 命名：標準 selector 為 `<package>@ccync-<package>`，其中 `<package>` 為外掛的套件名稱。舊版 selector 是同一 target 與同一提供者來源下，較早由 ccync 擁有、但名稱不同的 selector。例如較早的 sync 將註冊 `plugin@ccync-plugin` 記錄在套件名稱 `plugin` 之下。之後 manifest 更名為 `example`，因此標準 selector 變為 `example@ccync-example`，而由 ccync 擁有的 `plugin@ccync-plugin` 即為待遷移的舊版 selector。當 sync 找到同一 target、同一提供者來源與同一外掛、且由 ccync 擁有的此類註冊時，會先安裝標準 selector 並加以驗證，再移除舊註冊並提交所有權，全部包在同一筆已記錄的日誌意圖中。此閘門要求相同的提供者來源鍵、相同的上游版本、完整的所有權證明、相符的觀察 view digest，以及等於 `<已記錄的套件名稱>@ccync-<已記錄的套件名稱>` 的舊版 selector。停用、無法觀察或無法證明的舊註冊會阻擋該群組，兩筆註冊都會保留。目的端若有外部註冊，同樣會阻擋該群組，並由 sync 回報原因。此閘門不比對儲存庫路徑、子路徑或 manifest 身分，因為已提交的紀錄並不儲存這些資訊。ccync 絕不會因名稱相符就移除註冊，也絕不編輯外掛來源或外部內容。

若同步回報 `Foreign native selector: demo@...`，ccync 會輸出 `Remedy (not executed): remove <selector> with its native manager, then run ccync sync`。請先核對精確 selector，再使用該產品的原生管理工具移除外部註冊，接著執行 `ccync sync --dry-run`，再執行 `ccync sync --yes`（或在互動提示中確認）。這是交由操作者執行的移除；一般 `ccync sync` 不會移除外部註冊。不得只憑相同名稱或元件內容移除 selector。

若目標狀態為 disabled，請勿預期 sync 會啟用或取代其原生註冊。請使用該產品的原生管理工具決定是否啟用，並在同步前重新檢查狀態。若狀態為 broken 或 unknown／無法驗證，請保留註冊與既有檔案，檢查目標原生設定及回報的 selector 或路徑，並執行 `ccync doctor` 查看診斷。修正原因後，先重新執行 `ccync sync --dry-run` 再套用。若衝突仍在，請保留未解決狀態及既有載入器；不得從名稱或相同內容推定所有權。

證據按目標分開。`ccync list` 回報已記錄的提供者、管理者、載入器、所有權與驗證欄位。sync 會逐目標回報 `Applied`、`Unchanged`、`Skipped`、`Failed` 或 `NotRun`，並在辨識到外部 selector 時列出該 selector。這些回報不能證明代理工作階段實際載入元件，也不能證明 Codex Settings 的呈現方式。請逐一記錄各目標的即時檢查；未檢查目標工作階段時記為 `NotRun`。某個目標的磁碟投影成功，不能證明其他已選或未選目標的狀況。

若管理中的更新中斷或目標只完成部分同步，請先保留回報的 journal／狀態及最後可用的載入器。修正回報原因後執行 `ccync doctor`，再依序執行 `ccync sync --dry-run` 與 `ccync sync`，讓已記錄的操作完成收斂。請勿以手動刪除產生套件或原生註冊作為復原捷徑。若回報仍有 conflict、disabled、broken、unknown、failed 或 not-run 目標，請停止該目標操作並保留診斷證據。

Provider lifecycle journal 以 v3 schema 寫入。只有內容符合其 schema 的 v1 與 v2 journal 才會被接受。v1 不可包含獨立發布意圖，v1 或 v2 不可包含獨立撤回意圖。待處理意圖不會被降級或丟棄。解決衝突時，請保留 journal、preimage、所有權記錄與最後可用路由。sync 與 doctor 會區分 `standalone published`、`recovery blocked`、`identity unresolved` 及保留或 fallback 結果。這些 CLI 結果不能證明附加 UI 已載入或帳戶設定狀態。擁有者於 2026-10-24 回報實機驗收已完成。驗收證據沒有存放在此儲存庫，ccync 也未驗證。CLI 清單與檔案系統檢查無法證明附加 UI 載入。

此發布範圍涵蓋已物化內容的身分、發布、撤回與復原。工具安裝與升級、新工具快照產生，以及工具快照接入原生套件路由，仍屬於獨立的 P5 工具生命週期。MCP 寫入器行為與 hooks 也不在此發布路徑中。

在首次執行實體寫入前，ccync 會分別列出即將修改的各代理檔案與 MCP 主機配置。強烈建議先以 `--dry-run` 檢視預覽；若操作者拒絕首次確認，ccync 絕不改動各代理的現有設定檔，但先前已完成的快取下載等非目標寫入作業可能仍會保留。傳入 `--yes` 或設定環境變數 `CCYNC_ASSUME_YES=1` 可略過互動式確認。

若外掛來源快取遺失或無法下載，應先修復來源與快取狀態，再經由 `ccync sync --dry-run` 確認結果。部分同步或升級路徑可能將缺失的來源誤判為無元件外掛，隨後的投影操作恐有誤刪既有檔案之虞。

### 本次修復的範圍

本次修復讓發布、退役與重試的回報，與 sync 實際寫入的結果一致。它不改變來源主機已定義之 MCP 項目由誰擁有。這項首次初始化的所有權衝突是另一項後續工作，目前仍未解決。既有的 Antigravity（AGY）介面本機 MCP 查詢路徑維持不變。

## 使用者範圍指示

若建立了 `~/.ccync/AGENTS.md`，ccync 會將內容同步到 `<!-- ccync:managed:begin -->` 與 `<!-- ccync:managed:end -->` 標記區塊中的目前寫入檔案。標記區塊以外的使用者內容會完整保留。三個 Antigravity target 共用 `~/.gemini/AGENTS.md`，ccync 每次只更新一次。標記損壞時，ccync 會回報並跳過，不會覆寫整份檔案。Claude 沒有目前的寫入檔案，因此這個來源不會套用到 Claude 的使用者範圍。ccync 不會寫入 `~/.claude/AGENTS.md`。成功讀取來源後，一般 sync 只會在選取 `claude` 時檢查舊版 `~/.claude/CLAUDE.md` 路徑，並移除其中受管的區塊。無論來源不存在或有內容，都會移除該區塊。

`ccync sync --instructions-only [--dry-run] [--yes]` 只套用這個區塊，不會動到外掛、MCP 或其他執行期項目。語法、各路徑狀態與同意形式見[指令同步](commands.zh-Hant.md#指令同步)。

### 來源所有權

`~/.ccync/AGENTS.md` 與目標檔案中 ccync 標記以外的每個位元組都屬於你。ccync 只擁有標記之間的文字。ccync 不會產生、改寫、檢查或判斷你的指令語意。

- 來源不存在或大小為零位元組，代表你不要任何指令。此時 ccync 只會從每個已選取的檔案移除 ccync 擁有的區塊。
- 只含空白字元的來源視為內容，ccync 會寫入。
- 來源無法讀取、不是有效的 UTF-8，或是失效的符號連結時，ccync 絕不移除任何內容。

### 選取行為

ccync 先依標準順序解析已選取的 target，再移除重複的實體路徑。六個邏輯 target 對應四個目前寫入檔案，因為三個 Antigravity target 共用 `~/.gemini/AGENTS.md`。Claude 沒有目前的寫入路徑。

| Target | 指令檔 |
| --- | --- |
| `claude` | 沒有目前的寫入路徑 |
| `codex` | `~/.codex/AGENTS.md` |
| `copilot` | `~/.copilot/AGENTS.md` |
| `opencode` | `~/.config/opencode/AGENTS.md` |
| `agy-cli`、`agy-ide`、`agy-gui` | `~/.gemini/AGENTS.md` |

首次投影與完整 `sync --dry-run` 都會分成兩組列出指示檔。第一組是目前選取的路徑。第二組是選取 Claude 時會移除區塊的舊版檔案。`init` 也使用相同告知內容。指示限定範圍的 dry-run 會逐一列出目前路徑，以及適用時的舊版移除路徑。dry-run 不會寫入檔案。套用時須使用 `--yes`、`CCYNC_ASSUME_YES=1` 或互動確認。第一次完整投影前，每次限定範圍的套用都需要授權。

舊版 Claude 路徑的用途與目前寫入分開：

| 生命週期 | 路徑 | ccync 檢查時機 |
| --- | --- | --- |
| 舊區塊移除 | `~/.claude/CLAUDE.md` | 一般 sync 只在選取 `claude` 時檢查。 |
| Doctor 分類 | `~/.claude/CLAUDE.md` | 不受 target 選取或設定錯誤影響。 |
| 解除安裝 | 所有目前寫入路徑與舊版 Claude 路徑 | 不受 target 選取影響，移除前須有所有權證明。 |

一般 sync 不會更動未選取 target 的目前寫入檔案，也不會檢查舊版 Claude 路徑，除非選取 `claude`。`ccync doctor` 不受選取或設定錯誤影響，仍會分類舊版路徑。`ccync uninstall` 不受選取影響，會檢查所有指示路徑，且只移除有 ccync 所有權證明的區塊。

每個實體檔案都是獨立的寫入單位。單一檔案失敗不會中止其他檔案，也不會回復它們已完成的寫入。

### 復原

機器尚未初始化或設定無法使用時，限定範圍的路徑會在讀取或寫入任何 target 檔案之前以失敗收場。請修正回報的原因，再重新執行同一個指令。已經相符的檔案維持不變。

| 層級 | 失敗情況 | 復原方式 |
| --- | --- | --- |
| 設定 | `~/.ccync/config.json` 不存在、無法讀取，或不是有效的 JSON。 | 執行 `ccync init <source>`，或修復該檔案。 |
| 設定 | 設定同時含有 `selectedRuntimes` 與 `selectedProjectionTargets`。 | 執行 `ccync init <source>` 擇一。 |
| 設定 | 設定沒有選取任何有效 target，或含有被拒絕的 target 值。有效值與被拒絕值並存時同樣失敗。 | 執行 `ccync init <source>`，或移除訊息所列出的被拒絕值。 |
| 來源 | `~/.ccync/AGENTS.md` 無法讀取，或不是有效的 UTF-8。 | 修正檔案權限，或以 UTF-8 重新儲存。 |
| 來源 | `~/.ccync/AGENTS.md` 是失效的符號連結。 | 修復連結目的地與存取權。 |
| Target | 無法讀取 target 檔案。 | 修正 target 檔案的權限。 |
| Target | target 檔案中的 ccync 標記已損壞。 | 手動修復標記，再執行 `ccync doctor`。ccync 絕不修復標記。 |
| Target | target 檔案是失效的符號連結。 | 保留連結。檢查連結目的地與存取權，恢復存取權後重新執行 `ccync doctor`。 |

來源失敗絕不會移除區塊。在完整的 `ccync sync` 中，來源失敗會略過所有指令寫入、保留其他已完成的階段，並回傳 `1`。若完整 sync 無法建立 machine options，也會跳過指示階段。

### 重新載入建議與證據邊界

至少一個檔案在套用後有變更時，請重新載入或重啟你的代理。ccync 無從觀察代理是否已載入此變更，因此不回報載入狀態。指令來源中提到某個技能，並不代表每個主機都提供該技能。

對於 Codex、Copilot、OpenCode 與三個 AGY 介面（`agy-cli`、`agy-ide`、`agy-gui`），ccync 會把檔案寫入各自的磁碟位置，但無法確認主機是否已載入。首次投影後請重新載入該代理。`ccync init` 的預覽在確認前也會提出相同的重新載入需求。

Claude Code 的內建 `AGENTS.md` 外掛會獨立載入指示檔。Claude Code 2.1.277 以上版本支援此外掛，部分工作階段在 2.1.281 之前可能尚未支援。必須啟用此外掛，可用 `/plugin` 管理啟用狀態。Project instructions 也須選擇合適的檔案載入模式。預設的 `claude-md-or-agents-md` 模式會在工作目錄或其上層含有 `CLAUDE.md`、`.claude/CLAUDE.md` 或 `CLAUDE.local.md` 時，略過儲存庫的 `AGENTS.md`。使用者可以選擇 `claude-md-and-agents-md` 同時載入兩者。ccync 不修改設定，也不保證載入。詳見 [Claude Code 記憶文件](https://code.claude.com/docs/en/memory)。

### 原生套件驗收狀態

Claude Code 或 Codex 的原生套件，必須完全關閉應用程式、重新開啟並開始新的工作階段後，才會在執行中的應用程式生效。ccync 不會重新載入代理。在用戶端中被停用的註冊維持零次載入，sync 不會將其啟用；請使用該用戶端自己的管理工具重新啟用。

成功的 CLI 註冊並不能證明桌面應用程式已載入該套件。2026-10-22 於隔離設定檔中的探查結果：Codex CLI 0.163.0、Claude Code 2.1、GitHub Copilot CLI 1.0.101（僅 CLI，非 VS Code）與 OpenCode 1.18.35 皆回報 identity 1、啟用計數 1、停用計數 0。這些探查不能證明桌面、IDE 或 App 已載入該元件。AGY CLI 1.4.4 的探查為 NotRun，因為 `agy -p /skills` 會啟動一個模型回合，無法讀取清單。AGY IDE 與 GUI 尚未驗證。

擁有者於 2026-10-24 回報，真實用戶端上的實機驗收已完成，範圍包括 Codex/ChatGPT 與 Claude 應用程式、OpenCode、Copilot CLI 與 VS Code、AGY IDE 與 GUI，以及未選取的共用根目錄讀取者。驗收證據沒有存放在此儲存庫，ccync 也未驗證。CLI 清單與檔案系統檢查無法證明附加 UI 載入。

六個目前寫入 target、四個檔案的選取，其證據有四種來源，各自證明的事項不同。Claude 的舊版移除路徑會在選取時另行檢查。

| 證據 | 證明的事項 | 不能證明的事項 |
| --- | --- | --- |
| `sync --instructions-only --dry-run` | 預測選取的目前寫入路徑與 Claude 舊版移除路徑的狀態。 | 是否真的發生寫入。 |
| `sync --instructions-only` | 回報每個選取檔案有變更、相符或遭阻擋。 | 完整區塊位元組，或任何代理是否載入該檔案。 |
| `ccync doctor` | 回報選取檔案的唯讀健康狀態，並獨立分類 Claude 舊版路徑。 | 任何代理是否載入了該檔案。 |
| 真實代理工作階段 | 只有工作階段探查能觀察代理是否載入了指令。 | 其他代理的任何狀況。 |

磁碟結果是必備的驗收證據。只有來自該代理真實工作階段的觀察，才算載入證據。沒有執行這類工作階段時，記錄為 `NotRun`。切勿從 doctor 輸出或套用成功推論代理已載入。

## 診斷與解除安裝

`ccync doctor [--dry-run] [--release-gate]` 用於檢查整體設定、本機狀態、輸出內容、代理檔案健康度與發行條件。所有警告與錯誤均會標示具體受影響的項目。目前 `--dry-run` 與 `--release-gate` 旗標不會異動實際檢查項目，切勿將其視為額外的獨立發行驗證程序。

Doctor 不受 target 選取、設定錯誤或來源錯誤影響，仍會分類 Claude 舊版路徑。檔案不存在、為空或沒有 ccync 區塊時不需處理。殘留的受管區塊會列為警告。選取 Claude 後，執行 `ccync sync --instructions-only --yes`，只移除受管區塊與標記。若未選取 Claude，手動移除受管區塊與標記，或執行 `ccync uninstall`。檔案無法讀取、標記損壞或連結失效時會列為錯誤。Doctor 不會修復損壞標記。遇到失效連結時，請保留連結，檢查目的地與存取權，恢復存取權後重新執行 `ccync doctor`。

`ccync doctor` 的原生套件診斷為唯讀。doctor 會以用戶端的列舉指令為每筆已提交的原生紀錄執行列舉。列舉失敗或逾時後，doctor 不會再為該可執行檔其餘的紀錄重複列舉。doctor 絕不改動註冊。列舉中出現某項目，並不能證明執行中的用戶端已載入該套件。回退路由會顯示原因，且 loose 路由維持生效。

`ccync uninstall` 僅撤回具備明確所有權證明由 ccync 管理的 MCP、技能、指令、代理以及使用者指示區塊。它不受 target 選取影響，會檢查目前寫入檔案與 Claude 舊版路徑。此操作會移除工具記錄與衍生狀態，包括 `tools.json`、工具快照、操作日誌及安裝收據。此操作也會清除衍生的 `cache/` 與 `build/` 資料，但會保留 `config.json`、`plugins.json`、存在時的 `mcp.json`，以及各代理的原生設定目錄。命令會回報實際保留的 machine INPUT 檔案。持續使用的協調鎖定檔 `build/lifecycle.lock` 會保留，讓後續命令沿用同一個鎖定檔。該指令不會刪除 `ccync` 執行檔本身，執行檔請依照當初採用的安裝管道進行移除。

## 結束代碼 (Exit Codes)

結束代碼 `0` 表示執行成功，`1` 表示操作失敗，`2` 表示已辨識的指令尚未接線，`3` 表示指定的唯讀查詢偵測到可用更新，`64` 表示指令用法錯誤。`update --check` 僅查詢版本；即使安裝來源不支援自我更新，查詢成功仍回傳 `0`。實際執行 `update` 而無法自動更新時，則回傳 `1`。完整分類見 [ADR 05](../../adr/05-machine-lifecycle-adoption-and-cli-contract.md)。

## 本機機器狀態 (Machine State)

`~/.ccync/` 包含機器輸入、來源快取與產生的狀態。個人 MCP 輸入獨立位於 `~/.agents/mcp.json`，舊輸入與路由詳見 [MCP](mcp.zh-Hant.md)。`config.json`、`plugins.json`、選用的 `AGENTS.md` 與 `tools.json` 是機器輸入。產生的狀態包括鎖定檔、render、MCP 所有權與日誌。已提交的工具快照只能使用 `ccync tool refresh <id>` 重新產生。

各代理實際讀取的設定檔位於 `~/.ccync/` 之外，兩者不可混為一談。ccync 僅管轄其中具備所有權證明之特定項目或標記區塊，並不擁有整份原生設定檔。

## 備份與還原

`ccync backup [--output <dir>] [--strip-secrets]` 建立可攜設定檔。直接 MCP 輸入含明文機密、URL userinfo、佔位符名稱衝突或無法安全分類的值時，會在建立輸出前遭拒。機器機密值與主機所有權不會匯出。`--strip-secrets` 保留受支援遮蔽的相容行為。

先執行 `ccync restore --from <dir> --dry-run`，再執行 `ccync restore --from <dir>`。新個人清單保留原始位元組，舊備份的 `mcpServers` 根鍵會轉為 `ccyncMcpServers`。作用中輸入或明確路由衝突時會停止還原。還原不寫入原生主機設定，也不轉移所有權。提供缺少的本機值後，以 `ccync sync --dry-run` 預覽。中斷時請使用相同備份重跑並處理第三種指紋。詳見 [MCP](mcp.zh-Hant.md)。

## MCP 復原邊界

`ccync init <source>` 會在同意前預覽匯入，之後可能直接套用。後續可使用 `ccync sync --dry-run` 預覽路由，或使用 `ccync doctor --mcp-evidence <path>` 分析去敏事件檔。對保留項目，先執行 `ccync sync --reselect-mcp SOURCE SERVER HOST --dry-run` 預覽，再移除 `--dry-run` 重新選取。P7 實際復原每次都需要另行授權。合成證據無法證明歷史事件的原因。
