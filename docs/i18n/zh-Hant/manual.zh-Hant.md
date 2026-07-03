---
source: docs/manual.md
lang: zh-Hant
source_commit: 20006ea456affec0c9971566eb3796f450c4d93e
translated_at: 2026-07-03
status: current
---

[English](../../manual.md) | 繁體中文

# ccync 使用手冊

本文件詳細說明了所有的 `ccync` 指令。ccync 是一個跨代理 (cross-agent) 的外掛程式、MCP 與技能管理器。ccync 自身的狀態——它為自己讀寫的一切——都維護於 `~/.ccync/` 目錄中。它同時也會把這份狀態**投影**到每個代理位於 `~/.ccync/` 之外的即時設定介面上(`~/.claude.json`、`~/.claude/skills/...`、`~/.codex/config.toml` 等各代理專屬路徑)——完整佈局請見下方的[機器資料佈局](#機器資料佈局)。

在您的終端機中執行 `ccync --help` 或 `ccync --version` 可查看即時的指令介面。公開的命令列介面精確包含 **14 個指令**：
`init`、`sync`、`add`、`remove`、`list`、`show`、`doctor`、`backup`、`restore`、`uninstall`、`update`、`upgrade`、`cleanup` 與 `pin`。

> 注:本繁中手冊以英文 [`manual.md`](../../manual.md) 為準;新動詞 `show` / `cleanup` / `pin` 與 `list --upgrade-available` 旗標的完整說明見英文版,以下補充三個重點。
>
> - **`ccync list --upgrade-available`**:唯讀的「可更新」檢查,對每個 git 外掛印 `id  current → latest  source`;退出碼 `0`(全部最新)/`3`(有可更新,`UpdatesAvailable`)/`1`(真實錯誤),可供 script 判斷。held 外掛報 `held` 不查詢。
> - **`ccync show <id>`**:印單一外掛完整詳情(displayName、source、installStrategy、釘住的 sha、是否 held、投影元件數)。對齊 `brew info` / `winget show`。
> - **`ccync cleanup [--dry-run]`**:剪除 `~/.ccync/cache/` 中 `upgrade` 留下的孤兒 `<id>@<old-sha>` 目錄(對齊 `brew cleanup`)。**fail-closed**:lockfile 無法解析 → 不刪任何東西;同 id 多目錄又無 resolved pin → ambiguous,保留 + 告警,絕不猜。`--dry-run` 只列。
> - **`ccync pin <id>` / `--remove <id>` / `--list`**:把外掛釘住擋升級(winget 式;狀態存 `held` 欄)。**`pin` 動詞 ≠ `pin` 欄**:既有 `pin`/`pinnedSha` 欄是「鎖定的 commit sha(held ≠ pinned sha)」,`pin` 動詞是「擋升級」,存於獨立 `held` 欄,不改變 sha。
> - **`ccync add` 來源判定優先序**(第一個符合的規則勝出):(1) 以 `.zip`/`.tar.gz`/`.tgz` 結尾 → Archive;(2) 以 `http://`/`https://`/`git@`/`git://` 開頭 → Git(不限 GitHub,任何 git host / SSH);(3) 含 `/` 或 `\`,或**開頭**是 `.` 或 `~`,或為絕對路徑 → 本地路徑(磁碟上的 git 工作副本);(4) 其餘(不含任何分隔符的裸識別字)→ Catalog ID。陷阱:裸字 `my-plugin` 視為 catalog id 而非相對路徑,要加本地資料夾請寫 `./my-plugin`。

## 生命週期指令

### `ccync init [<master>]`

執行首次運行的初始設定。此指令會選擇一個**主代理 (master agent)**（`claude` 或 `codex`）。若在互動式終端機中省略了 `<master>` 參數，則會顯示方向鍵選擇提示；在非互動式終端機中，必須明確提供 master。它會將主代理已安裝的外掛程式採用並匯入至 ccync 的中央真實來源 (`~/.ccync/build/lock.json`)，並另外將主代理**完整的 MCP server 定義**（command、args、url、env、headers——不只是名稱）快照進 lockfile 的 `_mcpServers` 命名空間,讓 ccync 之後能把每個 MCP server 重新投影到你其他選定的代理,而不必每次都重讀主代理的設定。接著會顯示互動式多選讓您選擇要投影到哪些代理（預設勾選：`claude`、`codex`、`copilot`；`opencode`、`agy`、`gemini-cli` 預設不勾選）。在記錄下您的執行環境選擇後，它將執行統一的投影引擎，且不會另行顯示首次執行確認——執行環境多選本身即為意圖確認（`ccync init` 略過下方首次執行閘門;詳見閘門段落的分工說明）。若提供未知的代理將會導致用法錯誤。

### `ccync sync [--dry-run] [--yes]`

這是核心的投影引擎。它會將 Catalog（合併內建 Catalog、機器設定與您的個人 Catalog）解析至 lockfile 中。接著，它會在 `~/.ccync/build/render/` 底下渲染規範根目錄 (canonical root)，並盡力將所有受管介面——技能、指令、代理與 MCP 主機設定——投影至每一個選擇的代理中。您應該在修改了設定、profiles 或已安裝的外掛程式後執行此指令。此操作是完全冪等的 (idempotent)。

- `--dry-run`: 僅印出會被修改的即時代理設定檔，而不會實際寫入它們。
- `--yes`: 跳過首次執行的覆寫確認提示。您也可以設定環境變數 `CCYNC_ASSUME_YES=1`。

**首次執行閘門 (First-Run Gate):** 在全新的機器上（先前尚未成功進行過投影），執行 `ccync sync` 會列出其預計修改的即時代理設定檔——例如 `~/.claude.json`、`~/.codex/config.toml`、`~/.copilot/mcp-config.json` 以及 OpenCode 的設定檔。接著它會暫停並在進行任何寫入前要求確認。在非互動式的終端機環境中，您必須傳入 `--yes` 或設定 `CCYNC_ASSUME_YES=1` 以在無提示的情況下繼續執行。

**四個生命週期指令的分工:** `sync`、`add`、`remove` 三者共用同一個閘門——`remove` 在非互動環境下同樣需要 `--yes`(或 `CCYNC_ASSUME_YES=1`)才能繞過,與 `sync`/`add` 完全一致。`init` 完全略過此閘門(傳入 `skip_first_run_gate=true`),因為它在投影前已經顯示過自己的代理選擇 UI。`upgrade` 會走這個閘門,但永遠自動確認(`assume_yes=true`),所以從不提示。`add` 的即時介面渲染/投影嚴格發生在**閘門通過之後**——閘門遭拒時,catalog/cache/lockfile 的寫入會保留,但不會產生任何即時介面寫入。

*（注意：透過 `--import-from` 的主代理採用功能已經被移至 `ccync init` 指令中。）*

### `ccync uninstall`

移除 ccync 管理的狀態並撤回 ccync 的投影，同時保留你的輸入檔與共用使用者目錄。

**會移除：**

- `~/.ccync/` 下所有衍生狀態 —— `cache/`、`build/` 與任何遺留的 `ledger.json` —— 最終只留下兩個輸入檔（見「會保留」）。不再寫入 uninstall ledger。
- live agent config 內由 ccync 注入的 MCP server entries（Claude `~/.claude.json`、Copilot、OpenCode、Codex `~/.codex/config.toml`），依先前投影的 managed 名稱比對移除；使用者自有的 MCP server 會保留。
- ccync 的 marketplace／plugin-state 記錄：Claude `installed_plugins.json`／`known_marketplaces.json` 內的 `ccync` marketplace 與 `*@ccync` plugin 記錄，以及（best-effort，當 `codex` 在 PATH 上時）Codex 的 `ccync@ccync-marketplace` 註冊。
- ccync 擁有的 skill／command／agent 投影與 AGY CLI/IDE 連結，授權嚴格依投影 lockfile 判定（共用 surface 上使用者自有的同名同層檔案絕不移除）。

**會保留：**

- 你的輸入檔：`~/.ccync/config.json` 與 `~/.ccync/plugins.json`。
- 共用的 `~/.local/bin/` 目錄與你的 `PATH`（不會被編輯）。
- Antigravity GUI 使用者可調整的設定檔，以及所有非 ccync 的 agent 設定。

CLI 不會刪除自己的執行檔——執行中的程序無法可靠地自我刪除。請改用包裝層腳本（POSIX 用 `packaging/uninstall.sh`、Windows 用 `packaging/uninstall.ps1`）先執行 `ccync uninstall` 再移除 `~/.local/bin/ccync(.exe)`。沒有 `--purge` 旗標；你的輸入檔一律保留。

若某個 surface 無法清理（設定檔解析失敗，或 Codex de-registration 時 `codex` 不在 PATH 上），該次執行會回報具名 warning，而非靜默宣稱成功，且該檔案會被保留。

### `ccync update [--check]`

自我更新 `ccync` 執行檔，對齊 `brew update`。它比對執行中版本與最新 GitHub release，較新時下載對應平台的 artifact，依 `checksums.txt` 驗證 SHA-256（與安裝腳本同一契約），再原子替換執行檔。舊執行檔會搬到 `<ccync>.old`（盡力移除；Windows 上可能留待下次執行清理）。

`ccync update` 會**偵測安裝來源**，僅對 curl/irm 安裝（`~/.local/bin/ccync`、`%USERPROFILE%\.local\bin\ccync.exe`）執行自我替換。若執行檔由套件管理器託管則**拒絕**並導向原生指令 —— `brew upgrade ccync`（Homebrew）、`winget upgrade Monkey1Wizard.ccync`（winget）、或 `cargo install --git … --force`（cargo）。無法辨識的安裝位置採 fail-closed（拒絕並提示手動更新）。

- `--check`：回報已安裝版本、最新可用版本與偵測到的安裝來源，**不**下載也不寫入任何東西。

### `ccync upgrade [<plugin>] [--dry-run]`

把已安裝的 **git-source** 外掛升級到最新 upstream commit，對齊 `brew upgrade`。對每個 git 外掛以 `git ls-remote` 比對遠端 `HEAD` 與釘住的 commit；不同時重新 clone 到全新的 `~/.ccync/cache/<id>@<new-sha>`、驗證 cache 落地後，才原子重釘並執行 `sync` 重新投影。archive 與 local 來源的外掛沒有 upstream「最新版」概念，會被列為 `skipped`。單一外掛失敗（網路/clone）不會中斷其餘外掛。

- 無參數升級**全部** git 外掛；`<plugin>` 升級單一外掛 id（未知 id 會回報可操作錯誤）。
- `--dry-run`：印出升級差異（哪些外掛、`<old>` → `<new>`、以及略過項），**不** clone、不重釘、不投影。至少有一個外掛可升級時退出碼為 `3`；沒有任何外掛需要升級時為 `0`；發生錯誤時為 `1`。
- 你已**held**（`ccync pin`）的外掛會回報 `held` 並被略過——絕不查詢，絕不重新 clone。

## 外掛程式管理

### `ccync add <source> [--no-sync]`

從**任何支援的來源**新增一個個人外掛程式，將其註冊至 `~/.ccync/plugins.json` 內，固定其版本於 lockfile 中，並自動執行 `sync` 將其投影至每個已選擇的代理。來源偵測會自動運作：

| 來源類型 | 範例 | 快取鍵值 (Cache Key) |
| --- | --- | --- |
| Git URL | `https://github.com/owner/plugin.git` | `<id>@<commit-sha>` |
| 本機路徑 | `/path/to/plugin` 或 `./my-plugin` | `<id>@<commit-sha>` |
| 壓縮檔 (`.zip` / `.tar.gz`) | `plugin-v1.tar.gz` | `<id>@<sha256-first12>` |
| Catalog ID | `my-catalog-plugin` (獨立的識別碼) | 透過內建 Catalog 解析 |

外掛程式 ID 取自於來源的最後一個路徑片段（並去掉 `.git`）。Catalog IDs 則完全依照您所提供的字串。

**來源判定方式（優先序）：** 偵測是自動且有序的——第一個符合的規則勝出：

1. 以 `.zip`、`.tar.gz` 或 `.tgz` 結尾 → **Archive**。
2. 以 `http://`、`https://`、`git@` 或 `git://` 開頭 → **Git**（任何 git host——GitHub、GitLab、Bitbucket、自架、`git@` SSH；**不限** GitHub）。
3. 含 `/` 或 `\`，或**開頭**是 `.` 或 `~`，或為絕對路徑 → **本地路徑**（磁碟上的 git 工作副本）。
4. 其餘（不含任何分隔符的裸識別字）→ **Catalog ID**。

> **陷阱：** 不含斜線或點的裸字——例如 `my-plugin`——會被視為 **Catalog ID**，*不是*相對路徑。要新增名為 `my-plugin` 的本地資料夾，請寫成 `./my-plugin` 以被偵測為路徑。

壓縮檔會被解壓縮至 `~/.ccync/cache/<id>@<sha256-first12>/`，並嚴格保留完整的 CC-plugin 目錄結構。若壓縮檔內包含單一的、GitHub 格式的根目錄封裝，它將會自動被提升 (promoted)。重新以相同的壓縮檔位元組（產生相同的 SHA-256 前綴）執行 `add`，會命中已存在的快取目錄並立即返回，不會重新解壓縮。

**重複新增已存在的外掛程式 ID 會報錯，不是靜默成功。** 若推導出的外掛 ID 已存在於 `~/.ccync/plugins.json` 中，`add` 會以用法錯誤（退出碼 `64`）拒絕執行，並提示先執行 `ccync remove <id>`——它**不會**靜默成功。這與上述的快取層去重不同：快取去重管的是重新抓取**相同來源內容**，而非重新註冊**相同的外掛 ID**。

**Hooks:** 若一個外掛程式包含了 `hooks/hooks.json` 檔案，其 hooks 會被投影，隨後由 Claude 載入。對於非 Claude 代理而言，hooks 是不適用的。更深入的機制請參見 [`architecture.md`](../../architecture.md#hooks-projection) 文件。

- `--no-sync`: 僅註冊外掛程式而不立即進行投影。您必須在之後執行 `ccync sync` 來完成投影作業。
- `--yes`: 跳過首次執行的覆寫確認提示。

### `ccync remove <id>`

從 Catalog 中移除一個個人外掛程式，徹底清除**所有**它所投影的產物（包含技能、指令、代理、hooks 與 MCP 項目），並自動執行 `sync`。

### `ccync list [--upgrade-available]`

列出所有已安裝的個人外掛程式，顯示它們固定的 commit 與來源位置。

- `--upgrade-available`：改印**唯讀**的可更新檢查——對每個 git 外掛比對遠端 `HEAD` 與釘住的 commit，印出 `id  current → latest  source`。這與 `ccync upgrade --dry-run` 是同一套引擎，只是以查詢方式呈現。退出碼可供 script 判斷：全部最新時為 `0`，至少一個可升級時為 `3`（`UpdatesAvailable`），發生真實錯誤（網路/解析）時為 `1`。held 外掛（見 `ccync pin`）回報 `held`，絕不查詢。

## 診斷

### `ccync doctor`

執行唯讀的管理健康狀態檢查。它會驗證規範根目錄、主代理採用狀態、MCP 投影的完整性、Claude 外掛程式快取的新鮮度，以及共用技能的投影。此指令絕對不會修改任何狀態。

## 設定檔 (`config.json`)

ccync 的機器設定位於 **`~/.ccync/config.json`**。它是純 JSON，由你手動編輯；ccync 本身沒有 `config set` 指令。變更會在下一次 `ccync sync`（或任何會解析狀態的指令）時生效。檔案缺失或鍵值缺失時會回退到明確的預設值——ccync 對於缺失或格式錯誤的設定檔絕不會 panic。

**安全編輯方式：**

1. 用任何文字編輯器開啟 `~/.ccync/config.json`（若不存在則自行建立）。
2. 保持它是合法的 JSON——解析錯誤會讓 ccync 靜默回退到預設值，因此存檔前請先驗證。
3. 執行 `ccync sync` 套用變更。執行 `ccync doctor` 確認設定如預期被讀取。

**範例：**

```json
{
  "devMode": false,
  "ccyncRoot": "/path/to/ccync/checkout",
  "secrets": {
    "GITHUB_TOKEN": "ghp_xxx"
  }
}
```

**鍵值：**

| 鍵 | 型別 | 意義 |
| --- | --- | --- |
| `devMode` | bool | 為 `true` 時，ccync 會從 `ccyncRoot` 指向的工作目錄渲染，而非使用打包後的原始碼。一般模式請省略或設為 `false`。 |
| `ccyncRoot` | string | ccync repo checkout 的路徑。當 `devMode` 為 `true` 時必填；若 `devMode` 為 `true` 但此路徑不可用，ccync 會報錯，而非靜默回退。 |
| `secrets` | object | 具名憑證（例如 API key、token）。透過 `${VAR}` 佔位符從 MCP server 定義中引用，因此密文不會直接出現在投影出的代理設定中。密文絕不會被路徑替換，也不會在佔位符模型下以明文寫入即時代理檔案。 |

**MCP `${VAR}` 替換：** 受管 MCP server 項目中的任何 `${NAME}` 佔位符，會在投影時先從行程環境變數解析，再疊加 `config.json#secrets`——若鍵名衝突，`config.json#secrets` 勝出。這讓單一設定即可驅動每個代理的 MCP host，而不需要為每個代理重複填寫憑證。

> 注：`devMode`/`ccyncRoot` 是權威的模式欄位。舊有的 `installMode` 欄位已棄用——若存在，只會觸發 `ccync doctor` 的遷移警告，不影響實際模式。

## 機器資料佈局

ccync 在你機器上擁有的一切都位於 `~/.ccync/` 之下，分為三類：

- **INPUT（輸入）**——你（或 `ccync add`/`ccync init`）直接寫入的兩個檔案；永不自動刪除，`sync` 也不會重建它們。
- **CACHE（快取）**——抓取下來的外掛本體，依 pin 值做鍵值，因此重新抓取相同內容是無操作；可從網路/來源重建。
- **OUTPUT（輸出）**——`build/` 之下的一切；由 `ccync sync` 完全重新產生，可安全刪除。

公開外掛 catalog **內嵌於 `ccync` 執行檔本身**——`~/.ccync/` 之下沒有任何 catalog 檔案。解析直接讀取內嵌副本；沒有任何東西會為此被下載或部署到磁碟。

```txt
~/.ccync/                              ccync 的全部機器狀態
├── config.json          [INPUT]       機器設定 (devMode/ccyncRoot、secrets)
├── plugins.json         [INPUT]       個人外掛 catalog（由 `ccync add` 寫入）
├── cache/               [CACHE]       個人外掛本體，嚴格釘住版本
│   └── <id>@<sha | sha256-first12>/  clone/解壓縮後的外掛本體
└── build/               [OUTPUT]      由 `ccync sync` 完全重新產生
    ├── lock.json                      已解析的 lockfile (_personalPlugins、_mcpServers、_adoptMaster、_ccyncProjection);屬備份集合——承載無法重建的採用快照
    ├── render/                        規範根目錄：skills/、commands/、agents/、hooks/、.mcp.json
    ├── mcp/managed.json               彙總後的受管 MCP manifest
    └── active/<provider>/             各代理投影的登記資料
            │
            ▼
     投影 ──► 即時代理設定介面（`~/.ccync/` 之外）：
             ~/.claude.json、~/.claude/skills/…、~/.codex/config.toml、
             ~/.copilot/mcp-config.json、opencode.json、Antigravity 各介面
```

這就是「安裝一次，處處投影」的機制：`ccync sync`（或 `add`/`remove`/`init`/`upgrade`——它們都呼叫同一套引擎）在 `build/` 下解析並渲染一次，再把這單一渲染結果投影到你選定的每一個代理。若你想直接檢視內嵌於執行檔中的 catalog（而非透過 `ccync list`/`ccync add`），並沒有檔案可開——它的來源就是執行檔本身。

## 備份與還原

### `ccync backup [--output <dir>]`

將無法重建的本機狀態檔案複製到備份目錄:

- `config.json` — 機器設定(INPUT)
- `plugins.json` — 個人外掛程式目錄(INPUT)
- `plugins.lock.json`(來自 `build/lock.json`)— 雖屬 OUTPUT,但因其承載 init 採用快照(`_adoptedItems` / `_mcpServers`,無法從 INPUT 檔重新產生)而納入備份

機器上不存在的檔案(例如沒有個人外掛程式的機器上的 `plugins.json`)會附註跳過——這不是錯誤。`cache/` 與 `build/` 其餘部分刻意排除:它們可由 `ccync sync` 完整重建。

### `ccync restore --from <dir>`

從先前建立的備份目錄將同樣三個檔案還原到其即時位置(`plugins.lock.json` 還原回 `build/lock.json`)。備份中不存在的檔案——包括在 `plugins.json` 納入備份集合之前建立的舊備份——會附註跳過。目的地既有檔案會被覆寫。之後請執行 `ccync sync` 重建衍生狀態並重新投影到你的代理。

## 退出碼 (Exit Codes)

| 碼 | 意義 |
| --- | --- |
| `0` | 成功 |
| `1` | 操作錯誤 |
| `3` | 有可更新項目——唯讀的可更新檢查發現至少一個待升級項目（`ccync list --upgrade-available`、`ccync upgrade --dry-run`）。與 `1` 區分開來，讓 script 能分辨「有可更新」與真正的失敗。 |
| `64` | 用法錯誤（例如：未知或遺失指令，或無效的標記） |
