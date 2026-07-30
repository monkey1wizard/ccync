---
source: docs/manual.md
lang: zh-Hant
source_commit: 6188d2bfd968c53a213368506eff2e46111d210c
translated_at: 2026-07-27
status: current
---

[English](../../manual.md) | 繁體中文

# ccync 使用手冊

本文件詳細說明了所有的 `ccync` 指令。ccync 是一個跨代理 (cross-agent) 的外掛程式、MCP 與技能管理器。ccync 自身的狀態——它為自己讀寫的一切——都維護於 `~/.ccync/` 目錄中。它同時也會把這份狀態**投影**到每個代理位於 `~/.ccync/` 之外的即時設定介面上（`~/.claude.json`、`~/.claude/skills/...`、`~/.codex/config.toml` 等各代理專屬路徑）——完整佈局請見下方的[機器資料佈局](#機器資料佈局)。

在您的終端機中執行 `ccync --help` 或 `ccync --version` 可查看即時的指令介面。公開的命令列介面精確包含 **16 個指令**：
`init`、`sync`、`add`、`search`、`repair`、`remove`、`list`、`show`、`doctor`、`backup`、`restore`、`uninstall`、`update`、`upgrade`、`cleanup` 與 `pin`。

## 生命週期指令

### `ccync init [<master>]`

進行首次執行的初始設定。此指令會選擇一個**主代理 (master agent)**（`claude` 或 `codex`）。若在互動式終端機中省略了 `<master>` 參數，則會顯示方向鍵選擇提示；在非互動式終端機中，必須明確提供 master。它會將主代理已安裝的外掛程式採用並匯入至 ccync 的中央真實來源 (`~/.ccync/build/lock.json`)，並另外將主代理**完整的 MCP server 定義**（command、args、url、env、headers——不只是名稱）快照進 lockfile 的 `_mcpServers` 命名空間，讓 ccync 之後能把每個 MCP server 重新投影到你其他選定的代理，而不必每次都重讀主代理的設定。接著會顯示互動式多選讓您選擇要投影到哪些代理（預設勾選：`claude`、`codex`、`copilot`；`opencode`、`agy`、`gemini-cli` 預設不勾選）。在記錄下您的代理選擇後，它將執行統一的投影引擎，並顯示與下方首次執行閘門相同的分組觸及範圍揭露（已選代理介面 + 四個固定的 MCP 主機檔案），但不會出現*第二次*確認提示——代理多選本身即為意圖確認（詳見閘門段落的分工說明）。若提供未知的代理將會導致用法錯誤。

由於 `ccync init` 在呼叫投影引擎之前就已經寫入 `~/.ccync/config.json`，因此下方 `ccync sync` 段落所述的「初始化要求」永遠不會擋住 `ccync init` 本身——`init` 正是為其他生命週期指令滿足此要求的指令。

在採用摘要 (adoption summary) 之後，`init` 會檢查是否有任何剛採用的項目缺少來源紀錄；若有，會印出與下方 [`ccync upgrade`](#ccync-upgrade-plugin---dry-run) 所述相同的單行 `Tip: run \`ccync repair\`` 提示——當所有已採用項目都已有來源時則不印出任何內容，且 `init` 自身絕不會主動搜尋。

### `ccync sync [--dry-run] [--yes]`

這是核心的投影引擎。它會將 Catalog（合併內建 Catalog、機器設定與您的個人 Catalog）解析至 lockfile 中。接著，它會在 `~/.ccync/build/render/` 底下渲染規範根目錄 (canonical root)，並盡力將技能、指令與代理投影至每一個**已選擇**的代理中。MCP 設定是分開投影的：它一律會寫入四個固定的 MCP 主機檔案，與你選擇了哪些代理無關——見[機器資料佈局](#機器資料佈局)。您應該在修改了設定、profiles 或已安裝的外掛程式後執行此指令。此操作是完全冪等的 (idempotent)。

- `--dry-run`：僅印出會被修改的即時代理設定檔，而不會寫入它們。`~/.ccync/` 下的一次性內部佈局遷移一律會先執行，且不受 `--dry-run` 閘控——遷移可能發生在預覽之前。
- `--yes`：跳過首次執行的覆寫確認提示。您也可以設定環境變數 `CCYNC_ASSUME_YES=1`。

**首次執行閘門 (First-Run Gate)：** 在全新的機器上（先前尚未成功進行過投影），執行 `ccync sync` 會顯示一份分組的觸及範圍揭露，分成兩個標示清楚的區塊：已選代理介面——例如 `~/.claude.json`、`~/.codex/config.toml`、`~/.copilot/mcp-config.json` 以及 OpenCode 的設定檔——標示為預覽性質的「將會觸及」（僅限目前選擇）；以及四個固定的 MCP 主機檔案，標示為「可能寫入」——若主機內容與渲染結果已一致則會略過（位元組與 mtime 均保留）。接著它會暫停，並在進入 live-surface 投影前要求確認。若拒絕提示，指令會以退出碼 `1` 結束；視不同指令而定，較早完成的 catalog/cache/lockfile 工作可能已經發生，但不會寫入任何即時代理介面。在非互動式的終端機環境中，您必須傳入 `--yes` 或設定 `CCYNC_ASSUME_YES=1` 以在無提示的情況下繼續執行。

**四個生命週期指令的分工：** `sync`、`add`、`remove` 三者共用同一個閘門——`remove` 在非互動環境下同樣需要 `--yes`（或 `CCYNC_ASSUME_YES=1`）才能繞過，與 `sync`/`add` 完全一致。`init` 在自己的代理選擇 UI 之後（傳入 `skip_first_run_gate=true`）會顯示同一份分組揭露，但略過的是*確認提示*——不是揭露內容——因為代理選擇 UI 已經是意圖確認，再問一次是多餘的。`upgrade` 會走這個閘門，但永遠自動確認(`assume_yes=true`)，所以從不提示。`add` 的即時介面渲染/投影嚴格發生在**閘門通過之後**——拒絕閘門會以退出碼 `1` 結束，並保留任何指令特有的非即時寫入，但不會產生任何即時介面寫入。同一個分組揭露渲染器也支撐著下方的 `sync --dry-run`，因此三個呼叫方（`init`、通用閘門、`--dry-run`）對相同範圍的格式化方式完全一致。

*（注意：透過 `--import-from` 的主代理採用功能已經被移至 `ccync init` 指令中。）*

### 初始化要求

在機器完成 `ccync init` 之前，`~/.ccync/config.json` 尚不存在。`ccync sync`、有進行投影的 `ccync add`（未加 `--no-sync`）、`ccync remove`，以及非 dry-run 的 `ccync upgrade`，都會檢查該檔案是否存在，並在缺少時以退出碼 `1` 拒絕——指名 `ccync init <master>` 作為修復方式——拒絕會發生在任何資料變更（cache、catalog、lockfile 或即時介面寫入）之前。有幾個指令即使在 `ccync init` 之前也能使用：

| 指令 | 需要先執行 `ccync init` 嗎？ | 初始化前的例外情況 |
| --- | --- | --- |
| `ccync init [<master>]` | 不需要——這正是滿足此要求的指令 | 永遠可用 |
| `ccync sync` | 需要 | `--dry-run` 仍可用（唯讀預覽即時介面寫入；遷移仍可能先於它發生） |
| `ccync add <source>` | 有投影時需要 | `--no-sync` 仍可用（僅註冊不投影） |
| `ccync remove <id>` | 需要 | 無 |
| `ccync upgrade [<plugin>]` | 有實際套用時需要 | `--dry-run` 仍可用（唯讀預覽，只分類套件，不進行 clone、pin 或投影——不會有版面配置遷移先於它發生） |
| 初始化前即可使用的指令：`list`、`show`、`doctor`、`search`、`repair`、`backup`、`restore`、`pin`、`update`（裸指令或 `--check`）、`cleanup`、`uninstall` | 不需要 | 永遠可用 |

`ccync add <source> --no-sync` 在初始化前仍會把外掛程式註冊進 `~/.ccync/plugins.json`；之後（`ccync init` 完成後）執行 `ccync sync` 即可完成投影。

`ccync cleanup` 與 `ccync uninstall` 完全沒有初始化檢查——`cleanup` 在讀不到 lockfile 時會 fail-closed（什麼都不剪除），`uninstall` 則根本沒有衍生狀態可移除。裸指令 `ccync update`（不同於 `sync`/`add`/`remove`/`upgrade`）與 `~/.ccync/config.json` 無關，永遠會執行。

### `ccync uninstall`

移除 ccync 管理的狀態並撤回 ccync 的投影，同時保留你的輸入檔與共用使用者目錄。

**會移除：**

- `~/.ccync/` 下所有衍生狀態 —— `cache/`、`build/` 與任何遺留的 `ledger.json` —— 最終只留下兩個輸入檔（見「會保留」）。
- live agent config 內由 ccync 注入的 MCP server entries（Claude `~/.claude.json`、Copilot、OpenCode、Codex `~/.codex/config.toml`），依先前投影的 managed 名稱比對移除；使用者自有的 MCP server 會保留。
- ccync 的 marketplace／plugin-state 記錄：Claude `installed_plugins.json`／`known_marketplaces.json` 內的 `ccync` marketplace 與 `*@ccync` plugin 記錄，以及（best-effort，當 `codex` 在 PATH 上時）Codex 的 `ccync@ccync-marketplace` 註冊。在 Claude 上，這些記錄只有在你自己註冊過 ccync 隨附的 manifest（`claude plugin marketplace add ~/.ccync/plugins`）時才存在——ccync 對 Claude 的預設路徑是 session-load-only，從不建立它們，所以預設安裝下這裡根本沒有東西需要移除。
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

- `--check`：回報已安裝版本、最新可用版本與偵測到的安裝來源，**不**下載也不寫入任何東西。這是唯讀查詢，故**永遠回 exit `0`**——即使安裝來源是套件管理器託管或無法辨識，也一併印出目前版本與相同的安裝渠道指引，而非失敗。這與裸 `ccync update` 在同一安裝來源下的行為**刻意不對稱**：裸 `update` 會拒絕並回 exit `1`（動作未成 = 失敗）；`--check` 是查詢，永遠能成功回答問題。

### `ccync upgrade [<plugin>] [--dry-run]`

把已安裝的 **git-source** 外掛——包含以 URL clone 或本機 git 工作副本——升級到最新 upstream commit，對齊 `brew upgrade`。對每個 git 外掛以 `git ls-remote` 比對遠端 `HEAD` 與釘住的 commit；不同時重新 clone 到全新的 `~/.ccync/cache/<id>@<new-sha>`、驗證 cache 落地後，才原子重釘並執行 `sync` 重新投影。archive 來源與純本機目錄快照(plain-directory-snapshot)的外掛沒有 upstream「最新版」概念，會被列為 `skipped`——要重新整理快照，用 `ccync remove <id>` 再 `ccync add <同一路徑>`。單一外掛失敗（網路/clone）不會中斷其餘外掛。

- 無參數升級**全部** git 外掛；`<plugin>` 升級單一外掛 id（未知 id 會回報可操作錯誤）。
- `--dry-run`：印出升級差異（哪些外掛、`<old>` → `<new>`、以及略過項），**不** clone、不重釘、不投影。至少有一個外掛可升級時退出碼為 `3`；沒有任何外掛需要升級時為 `0`；發生錯誤時為 `1`。`--dry-run` 同時也是初始化前的例外情況——見[初始化要求](#初始化要求)：非 dry-run 的 `upgrade` 在未初始化的機器上會拒絕執行，但 `upgrade --dry-run` 仍可使用。
- 你已**held**（`ccync pin`）的外掛會回報 `held` 並被略過——絕不查詢，絕不重新 clone。

**採用的項目（唯讀語義）：** `ccync upgrade` 對採用的項目只報告、不自動套用：

| 採用的種類 | 升級行為 |
| --- | --- |
| 來源已知,catalog 版本與 baseline 皆存在,catalog 較新 | `current → latest`（有變更時）。要套用：執行 Claude 自己的 `/plugin` 更新，或 `ccync add <source>` 接手管理 |
| 來源已知,catalog 版本與 baseline 皆存在,並無較新版本 | 回報為 `unchanged`（省略不印，與個人外掛已在最新版時相同） |
| 來源已知,但 catalog 版本與／或 baseline 缺漏（catalog／marketplace 本身也可能無法解析） | 回報為 `version-unresolved (source: <source>)`。這是**有來源的項目**——絕不會因為升級比對證據不全就誤標為「來源未解析」 |
| 完全沒有可解析的來源（entry 上未記錄 marketplace，也沒有記錄的來源後備值） | 回報為已略過（來源未解析） |
| MCP server | 回報為已略過（採用的 MCP server——設定項目，不具版本） |
| 鬆散技能 | 回報為已略過（採用的技能——本地內容，無上游） |

退出碼**完全由個人外掛的狀態決定**——採用項目的結果本身絕不會引發 `UpdatesAvailable (3)` 或 `Error (1)`。

**`Tip: run \`ccync repair\`` 提示行。** 只要**任何一個**採用項目（外掛、MCP server 或鬆散技能）缺少已記錄的來源，畫面就會在採用區塊之後印出恰好一行——`Tip: run \`ccync repair\` to resolve missing adopted-item sources.`——無論有多少項目未解析都只印這一行。這取代了舊有的、僅限外掛、逐項的 `ccync search <name>` 提示：新提示是由 lockfile 推導而來（統一涵蓋每一種採用類型，不只是外掛），且一律指名 `repair` 這個動詞，絕不是 `search`。它只是靜態文字——絕不自動執行 `repair`，也絕不自動綁定任何 URL；要不要執行由你決定。詳見 [`ccync repair`](#ccync-repair-id)。

### `ccync cleanup [--dry-run]`

剪除 `~/.ccync/cache/` 之下的孤兒外掛快取，對齊 `brew cleanup`。當 `upgrade` 把某個外掛重新 clone 到 `<id>@<new-sha>` 後，先前的 `<id>@<old-sha>` 目錄會被遺留下來；`cleanup` 會移除這些過時目錄。它預設會實際刪除；`--dry-run` 只列出它「將會」移除的項目，而不觸碰任何東西。

安全性為 **fail-closed**——lockfile 是判定何者存活的唯一權威：

- 若 lockfile 無法讀取/解析，存活集合即無法判定，**不剪除任何東西**。
- id 已不在 lockfile 中的快取目錄（已移除的外掛）會被剪除。
- 某個外掛「有」已解析存活 pin 時，它的過時 sha 目錄會被剪除。
- 某個受管外掛「無」已解析 pin 卻同時擁有多個快取目錄時，即為 **ambiguous（有歧義）**——那些目錄會**保留並附告警**，絕不臆測。

## 外掛程式管理

### `ccync search <term> [--limit N] [--no-add]`

當你知道某個外掛程式的名稱（例如 `context7`）但不知道其儲存庫 URL 時，`ccync search` 會到 GitHub/GitLab 上查詢，讓你可以把解析出的 clone URL 交給 `ccync add` 新增為**個人 (personal)** 外掛。它絕不會自動把查到的 URL 綁定到任何東西，也絕不寫入 `config.json`——每一次新增都必須是明確的確認（互動模式），或完全交由你自己決定（非互動模式）。若要為既有的**採用 (adopted)** 項目補上來源，請改用 [`ccync repair`](#ccync-repair-id)。

- `<term>`：要查詢的名稱（必填——遺漏或未知的詞會是用法錯誤）。
- `--limit N`：限制考慮/顯示的結果數量，預設 `10`。同時接受 `--limit N` 與 `--limit=N` 兩種寫法；非整數或遺漏數值會是用法錯誤。
- `--no-add`：即使在真實終端機上，也強制使用非互動式的僅列出輸出（見下方）。

**提供者 (Providers)。** 每次執行都會依優先序查詢：已安裝的 `gh` CLI(`gh search repos`)、已安裝的 `glab` CLI(`glab repo search`)，以及 GitHub 與 GitLab 兩者未經授權的公開 web/API 後備方案。這四者一律都會被呼叫並合併結果——這**不是**單純的優先序後備：即使 `gh` 可用，也不會跳過 `glab` 或 web 後備。兩個面向 GitHub 的查詢（`gh` 與 GitHub web/API 後備方案）都會在你的搜尋詞旁加上 `fork:true`，因此 forked 儲存庫不會被排除在結果之外。單一提供者失敗（未安裝、驗證/設定錯誤、輸出無法解析）只會回報為可見警告，絕不會捨棄其他提供者的結果。結果依正規化後的 clone URL 去除重複，並依確定性規則排序：先比對 `owner/name` 完全相符，再依星數 (stars) 由高到低，接著依提供者優先序(`gh` < `glab` < web)，最後依 `full_name` 字典序。

**互動模式**（真實終端機，且未傳入 `--no-add`）：有多個相異結果時，會顯示方向鍵選單，每個候選項列出 `full_name (stars stars) - clone_url`；只有單一結果時仍需要明確的是/否確認，並顯示完整的 clone URL——絕不會自動選取。取消選單或拒絕確認都是無操作：不會新增任何東西。確認後的結果會以確認時的確切 clone URL，交給既有的 `ccync add <clone_url>` 流程處理。

**非互動模式**（傳入 `--no-add`，或 stdout 不是終端機——例如被導管、重新導向，或在 CI 中執行）：會改為在 stdout 印出穩定、可解析的表格，而不會提示，且絕不呼叫 `ccync add`。格式為 TSV（以定位字元分隔的值）：標頭列 `full_name\tstars\tdescription\tclone_url`，接著每個結果一列（缺少 `description` 時該欄位留空，以維持欄位數量固定）。

**零結果：** 若每個被查詢的提供者都成功執行，但沒有任何一個回傳相符結果，`ccync search` 會印出：

> No results found. Search manually, then run `ccync add <git-url>`.

這不算錯誤——退出碼為 `0`。（如果你需要手動編輯 ccync 的磁碟狀態，該看的檔案是文件化的 `~/.ccync/plugins.json`；`ccync search` 絕不會提及或觸碰 `config.json`。）

**退出碼：**

| 碼 | 意義 |
| --- | --- |
| `0` | 找到結果（執行了互動式新增，或印出僅列出的表格），或無結果但附上手動後備訊息 |
| `1` | 每個提供者都失敗——無法檢查任何內容 |
| `64` | 用法錯誤（遺漏 `<term>`、未知旗標，或無效的 `--limit` 值） |

**範例**——一個名為 `context7`、沒有記錄來源的採用項目：

```console
$ ccync search context7
? Multiple matches found — pick one to add ›
❯ upstash/context7 (18000 stars) - https://github.com/upstash/context7.git
  someone/context7-clone (3 stars) - https://gitlab.com/someone/context7-clone.git
✔ Multiple matches found — pick one to add · upstash/context7 (18000 stars) - https://github.com/upstash/context7.git
Add https://github.com/upstash/context7.git? (y/N) y
```

確認後會委派給 `ccync add https://github.com/upstash/context7.git`，就如同你自己輸入了這個指令一樣。

### `ccync repair [<id>]`

為缺少來源的採用項目解析出一個可知、可 clone 的來源——或明確覆寫既有的來源——採用與 `ccync search` 相同、需使用者確認的搜尋流程。**Pre-init 且不投影**：無論 `ccync init` 是否已完成，`repair` 一律可用（見[初始化要求](#初始化要求)），且它絕不會觸發投影——只會寫入單一 `_adoptedItems` 列的 `source` 欄位。

- 無參數：處理每一個缺少已記錄 `source` 的採用項目（外掛、MCP server 或鬆散技能）——批次模式。
- 明確指定 `<id>`：只鎖定該特定的採用列，且可**覆寫**已記錄的來源，不僅限於補上缺漏。
- 最多一個位置參數；未知旗標或多餘的位置參數皆為用法錯誤。

**搜尋詞的組成方式。** 對於採用的**外掛**（儲存格式為 `<外掛>@<marketplace>`），搜尋前會先去除 `@<marketplace>` 後綴——你要搜尋的是外掛本身，而不是加上 marketplace 限定詞的名稱。採用的 MCP server 與鬆散技能名稱則會原樣完整搜尋，不做任何修改。

**提示文案是 repair 專屬的。** 選單提示（「Multiple matches found — pick the real source」）與確認提示（「Record `<url>` as the source?」）絕不會重用 `ccync search` 的「Add `<url>`?」文案——repair 是在既有的採用項目上記錄 metadata，不是新增個人外掛。

**寫入前先驗證。** 確認的候選 URL 必須是非空的 `https://` URL；任何其他情況（`http://` URL、裸主機/路徑、空字串）都會在觸及寫入步驟之前就被拒絕——不會發生任何寫入。

**實際寫入**會經過 ccync 共用的原子寫入原語（`ccync_foundation::platform::atomic_write_bytes`——先寫暫存檔再 rename，因此 lockfile 絕不會停在寫到一半的狀態）：只更新唯一命中的那一列的 `source` 欄位，其餘所有 lockfile 命名空間維持不變。未知的 `<id>` 會回報可操作錯誤。若有超過一個採用列意外共用同一個名稱，寫入會 **fail closed（失敗並拒絕動作）**——`repair` 絕不會臆測該更新哪一列。每一種不寫入的路徑（未知 id、重複名稱、拒絕、無候選結果、非互動模式）都會讓 lockfile 保持**位元組層級完全相同**。

**非互動模式**（stdin 或 stdout 任一不是終端機——例如被導管、重新導向，或在 CI 中執行）：只會列出被鎖定的 id，明確說明未執行任何搜尋或寫入，並且**零**次呼叫任何提供者——無論批次模式或明確指定 id 模式，`repair` 在非互動情況下絕不搜尋。

**互動模式**會獨立處理每一個被鎖定的項目；某一項目的提供者失敗、URL 無效或寫入失敗，都不會阻止其餘項目繼續嘗試。

**退出碼：**

| 碼 | 意義 |
| --- | --- |
| `0` | 沒有被鎖定的項目（無需修復）、非互動式列出、拒絕、無搜尋結果，或所有嘗試的項目皆成功 |
| `1` | 任何被嘗試的項目遇到提供者全部失敗、URL 無效、寫入失敗，或 `<id>` 未知 |
| `64` | 用法錯誤（未知旗標，或超過一個位置參數） |

**範例**——修復一個缺少已記錄來源的採用 MCP server：

```console
$ ccync repair codebase-memory-mcp
? Multiple matches found — pick the real source ›
❯ DeusData/codebase-memory-mcp (42 stars) - https://github.com/DeusData/codebase-memory-mcp.git
  someone/codebase-memory-mcp-fork (3 stars) - https://github.com/someone/codebase-memory-mcp-fork.git
✔ Multiple matches found — pick the real source · DeusData/codebase-memory-mcp
Record https://github.com/DeusData/codebase-memory-mcp.git as the source? (y/N) y
ccync repair: recorded source for codebase-memory-mcp
```

### `ccync add <source> [--no-sync]`

從**任何支援的來源**新增一個個人外掛程式，將其註冊至 `~/.ccync/plugins.json` 內，固定其版本於 lockfile 中，並自動執行 `sync` 將其投影至每個已選擇的代理。來源偵測會自動運作：

| 來源類型 | 範例 | 快取鍵值 (Cache Key) |
| --- | --- | --- |
| Git URL | `https://github.com/owner/plugin.git` | `<id>@<commit-sha>` |
| 本機路徑 — git 工作副本 | `/path/to/plugin`（含 `.git`） | `<id>@<commit-sha>` |
| 本機路徑 — 純目錄 | `/path/to/plugin`（無 `.git`） | `<id>@<sha256-first12>` |
| 壓縮檔 (`.zip` / `.tar.gz`) | `plugin-v1.tar.gz` | `<id>@<sha256-first12>` |
| Catalog ID | `my-catalog-plugin`（獨立的識別碼） | 透過內建 Catalog 解析 |

外掛程式 ID 取自於來源的最後一個路徑片段（並去掉 `.git`）。Catalog IDs 則完全依照您所提供的字串。

**來源判定方式（優先序）：** 偵測是自動且有序的——第一個符合的規則勝出：

1. 以 `.zip`、`.tar.gz` 或 `.tgz` 結尾 → **Archive**。
2. 以 `http://`、`https://`、`git@` 或 `git://` 開頭 → **Git**（任何 git host——GitHub、GitLab、Bitbucket、自架、`git@` SSH；**不限** GitHub）。
3. 含 `/` 或 `\`，或**開頭**是 `.` 或 `~`，或為絕對路徑 → **本地路徑**。目錄內含 `.git` 檔案或子目錄（一般 git 工作副本或 linked worktree）時，會如同 Git 來源般抓取；目錄不含 `.git` 時，會被抓取為**不可變內容快照**（見下方）。
4. 其餘（不含任何分隔符的裸識別字）→ **Catalog ID**。

> **陷阱：** 不含斜線或點的裸字——例如 `my-plugin`——會被視為 **Catalog ID**，*不是*相對路徑。要新增名為 `my-plugin` 的本地資料夾，請寫成 `./my-plugin` 以被偵測為路徑。

壓縮檔會被解壓縮至 `~/.ccync/cache/<id>@<sha256-first12>/`，並嚴格保留完整的 CC-plugin 目錄結構。若壓縮檔內包含單一的、GitHub 格式的根目錄封裝，它將會自動被提升 (promoted)。重新以相同的壓縮檔位元組（產生相同的 SHA-256 前綴）執行 `add`，會命中已存在的快取目錄並立即返回，不會重新解壓縮。

**純本機目錄是不可變快照。** 不是 git 工作副本（無 `.git` 檔案或子目錄）的本機目錄不會被 clone——而是逐位元組複製到 `~/.ccync/cache/<id>@<sha256-first12>/`，雜湊值涵蓋每個被接受檔案的正規化相對路徑與內容（在 Windows 與 Unix 上結果一致）。重新新增相同目錄內容會命中相同快取鍵並立即返回（已存在）。純目錄沒有 upstream「最新版」概念，因此 `ccync upgrade` 永遠回報為 `skipped`；要重新整理其內容，請用 `ccync remove <id>` 再 `ccync add <同一路徑>`。目錄中任何位置的符號連結、連接點 (junction) 與其他 reparse/連結項目都會被**拒絕**——`add` 會以錯誤失敗而不是複製它們；這是刻意設計（快照絕不能沿著連結走出所選的目錄樹），解法是先解除連結參照（把連結指向的內容以實際檔案複製進目錄樹），或改用真正的 git 工作副本。

**重複新增已存在的外掛程式 ID 會報錯，不是靜默成功。** 若推導出的外掛 ID 已存在於 `~/.ccync/plugins.json` 中，`add` 會以用法錯誤（退出碼 `64`）拒絕執行，並提示先執行 `ccync remove <id>`——它**不會**靜默成功。這與上述的快取層去重不同：快取去重管的是重新抓取**相同來源內容**，而非重新註冊**相同的外掛 ID**。

**Hooks:** 若一個外掛程式包含了 `hooks/hooks.json` 檔案，其 hooks 會與 skills/commands/agents 一起被具現化至規範根目錄。ccync 不會執行它們或自動將它們註冊給 Claude——Claude 可透過一次性、僅限該工作階段的 `claude --plugin-dir <canonical-root>` 呼叫來載入它們。對於非 Claude 代理而言，hooks 是不適用的。更深入的機制請參見 [`architecture.md`](../../architecture.md#hooks-projection) 文件。

- `--no-sync`：僅註冊外掛程式而不立即進行投影。您必須在之後執行 `ccync sync` 來完成投影作業。這同時也是初始化前的例外情況——見[初始化要求](#初始化要求)：有進行投影的 `add`（未加 `--no-sync`）在未初始化的機器上會拒絕執行，但 `add <source> --no-sync` 仍會註冊外掛程式。
- `--yes`：跳過首次執行的覆寫確認提示。

### `ccync remove <id>`

移除一個已管理的項目（個人或採用的），徹底清除**所有**它所投影的產物（包含技能、指令、代理、hooks 與 MCP 項目），並自動執行 `sync`。`remove` 沒有初始化前的例外情況——見[初始化要求](#初始化要求)。

- **個人外掛程式**：直接從個人 catalog（`~/.ccync/plugins.json`）移除。
- **採用的項目**：從 lockfile 的 `_adoptedItems` 及其對應的次要命名空間（`_looseSkills` 或 `_mcpServers`）移除。若要升級已採用的項目，請先用 `ccync remove <id>` 移除，再用 `ccync add` 從上游來源重新新增。

### `ccync list [--upgrade-available]`

列出所有已安裝的已管理項目（個人及採用的），顯示它們固定的 commit 與來源位置。採用的項目會標示 `origin: adopted`。

- `--upgrade-available`：改印**唯讀**的可更新檢查——對每個個人 git 外掛比對遠端 `HEAD` 與釘住的 commit，印出 `id  current → latest  source`。採用的項目與 `ccync upgrade` 顯示完全相同的畫面——同一套 `changed`/`unchanged`/`version-unresolved`/`skipped` 規則，並在任何採用列缺少來源時附上同一行 `Tip: run \`ccync repair\`` 提示（完整的採用項目表格見 [`ccync upgrade`](#ccync-upgrade-plugin---dry-run)）。退出碼完全由個人外掛的狀態決定：全部最新時為 `0`，至少一個可升級時為 `3`（`UpdatesAvailable`），發生錯誤時為 `1`。held 外掛（見 `ccync pin`）回報 `held`，絕不查詢。

### `ccync show <id>`

印出單一受管項目的完整詳情。未知的 id 是可操作的錯誤。對齊 `brew info` / `winget show`。

- **個人外掛程式**：顯示名稱、來源、安裝策略、釘住的 commit sha、是否 **held**，以及盡力而為的元件計數（技能 / 指令 / 代理 / hooks）。
- **採用的項目**：來源標記（`adopted`）、種類（`plugin` / `mcp` / `skill`）、來源（已解析時）、記錄的基準版本（可得時），以及可升級性——`up to date`、`update available: <current> → <latest>` 附啟用指示、`version-unresolved`（來源已知，但版本比對證據不全），或 `not upgradeable: <reason>`（完全沒有可解析的來源，或非版本化的 MCP/技能）四者之一。
- **來源未解析**：當項目——不論是外掛、MCP server 或鬆散技能——完全沒有已記錄的來源時，可升級性那行之後會接一行縮排的 `Tip: run \`ccync repair <id>\``，指名這個特定項目。一旦記錄了來源，此提示就不會再出現，且絕不會自動搜尋或自動綁定任何來源。

### `ccync pin <id>` · `ccync pin --remove <id>` · `ccync pin --list`

把外掛釘住以擋住 `upgrade`（winget 式的 pin）。`pin <id>` 把外掛凍結在目前的 sha，`pin --remove <id>` 釋放它，`pin --list` 顯示目前有哪些外掛被 held。被 held 的外掛會被 `ccync upgrade` 與 `ccync list --upgrade-available` 略過（回報為 `held`，絕不查詢）。

> **`pin` 動詞 vs `pin` 欄位——是兩回事。** `plugins.json` / lockfile 裡的 `pin` / `pinnedSha` 欄位記錄的是*鎖定的 commit sha*（安裝的是哪個版本）。`ccync pin` **動詞**記錄的是*擋升級的 hold*，存於獨立的 `held` 欄位。Hold 一個外掛不會改變它的 sha；它只是阻止 `upgrade` 移動它。

## 診斷

### `ccync doctor [--dry-run] [--release-gate]`

執行唯讀的管理健康狀態檢查。它會驗證規範根目錄、主代理採用狀態、MCP 投影的完整性、Claude 外掛程式快取的新鮮度，以及共用技能的投影。此指令絕對不會修改任何狀態，因此 `--dry-run` 與 `--release-gate` 雖然會被接受，但目前不會新增任何檢查、也不會改變任何行為——doctor 不會比較渲染結果與 lockfile。

## 疑難排解

### 修正此逐主機擁有權追蹤機制上線前遺留的 MCP 項目

ccync 目前的 MCP 投影機制使用逐主機、逐 server 的擁有權紀錄（`~/.ccync/build/mcp/projected-state.json`，並以 write-ahead transaction journal 佐證），讓它能在刪除任何項目前，逐一主機證明哪些即時 MCP 項目是自己建立的。較早版本只用單一份全域 `managed.json` manifest 追蹤擁有權。若該擁有權紀錄已遺失，或早於這套逐主機模型（例如它曾被重建、清除，或屬於此擁有權追蹤機制上線前就存在的安裝）——ccync 便無法安全地重建哪些即時 MCP 項目曾經是自己建立的：舊模型從未記錄足夠的逐項目證據，讓它在紀錄消失後，還能分辨出「ccync 建立的項目」與「你自己加的項目」。用猜的等於冒著刪除你自己設定的風險，因此這裡套用與其他未證明擁有權項目完全相同的 fail-closed 規則：保留該項目不動。

也就是說：任何在這套擁有權模型上線**之前**由某次 ccync build 投影出來、而 ccync 現在已無法認領的 MCP server 項目，不會被自動偵測或刪除——無論是 `ccync sync`、`ccync remove`、`ccync uninstall`，或未來任何其他指令都一樣。這是刻意且永久的限制，不是之後版本會補上的缺口。若你有這類項目，請依下方步驟手動清理一次即可。

**如何檢查每個主機。** 比對 `ccync list`（ccync 目前管理的項目）與各主機檔案裡實際存在的即時項目。任何 ccync 目前未管理的具名 MCP server 項目，就是需要人工檢視的候選對象。

| 主機 | 即時設定檔 | server 存放位置 |
| --- | --- | --- |
| Claude | `~/.claude.json` | 頂層 `mcpServers` JSON 物件 |
| Copilot | `~/.copilot/mcp-config.json` | 頂層 `mcpServers` JSON 物件 |
| OpenCode | `<config 目錄>/opencode/opencode.json`（Linux/macOS 為 `~/.config/opencode/opencode.json`，Windows 為 `%APPDATA%\opencode\opencode.json`） | 頂層 `mcp` JSON 物件 |
| Codex | `~/.codex/config.toml` | `[mcp_servers.<name>]` TOML 區段 |

**如何只移除該筆遺留項目。** 用文字編輯器開啟該主機的即時設定檔，只刪除你已確認為遺留項目的那一筆具名 server 項目——不是整個檔案，也不是其他任何項目：

- Claude / Copilot（JSON，`mcpServers`）：從 `mcpServers` 物件中移除那一組 `"<server-name>": { ... }` key/value，保留其餘所有 key。
- OpenCode（JSON，`mcp`）：從 `mcp` 物件中移除那一組 `"<server-name>": { ... }` key/value，保留其餘所有 key。
- Codex（TOML）：刪除 `[mcp_servers.<server-name>]` 區段（若存在，連同其巢狀的 `[mcp_servers.<server-name>.env]` 區段一併刪除），保留其餘所有 `[mcp_servers.*]` 區段。

儲存檔案時維持編輯前的結構與編碼。

**何時該重新執行 `ccync sync`。** 手動移除遺留項目後，執行 `ccync sync`（或讓下一個生命週期指令觸發投影）。這**不會**「清理」你剛剛移除的那筆項目——你已經處理過了——但它會為 ccync 目前確實管理的 server，重新建立乾淨的逐主機擁有權追蹤，讓未來對這些 server 執行 `remove` 或 `uninstall` 時，能如本節所述自動完成。

## 設定檔 (`config.json`)

ccync 的機器設定位於 **`~/.ccync/config.json`**。它是純 JSON，由你手動編輯；ccync 本身沒有 `config set` 指令。變更會在下一次 `ccync sync`（或任何會解析狀態的指令）時生效。檔案缺失或鍵值缺失時會回退到明確的預設值——ccync 對於缺失或格式錯誤的設定檔絕不會 panic。

**安全編輯方式：**

1. 用任何文字編輯器開啟 `~/.ccync/config.json`（若不存在則自行建立）。
2. 保持它是合法的 JSON——解析錯誤會讓 ccync 靜默回退到預設值，因此存檔前請先驗證。
3. 執行 `ccync sync` 套用變更。執行 `ccync doctor` 確認設定如預期被讀取。

**範例：**

```json
{
  "secrets": {
    "GITHUB_TOKEN": "ghp_xxx"
  }
}
```

**鍵值：**

| 鍵 | 型別 | 意義 |
| --- | --- | --- |
| `devMode`、`ccyncRoot` | bool、string | **不啟用的相容性欄位。** ccync 的設定模型仍會解析並原樣保留它們，但正式投影只有單一模式：一律從規範根目錄渲染——目前程式碼路徑中沒有 Dev/Normal 分流，也不會解析 `devMode`/`ccyncRoot`。設定它們沒有任何可觀察的效果；請勿依賴它們來改變行為。 |
| `secrets` | object | 具名憑證（例如 API key、token）。透過 `${VAR}` 佔位符從 MCP server 定義中引用，讓一個值只需在此存一份、不必在每個外掛重複填寫。**佔位符能讓字面值不出現在 ccync 自有的 manifest 檔案中**（`build/lock.json#_mcpServers`、`build/mcp/managed.json`）；解析發生在投影當下，而解析後的明文值，依設計就是會被寫入四個即時 MCP 主機檔案——詳見下方的載體清單。 |

**MCP `${VAR}` 替換：** 受管 MCP server 項目中的任何 `${NAME}` 佔位符，會在投影時先從行程環境變數解析，再疊加 `config.json#secrets`——若鍵名衝突，`config.json#secrets` 勝出。這讓單一設定即可驅動每個代理的 MCP host，而不需要為每個代理重複填寫憑證。

> **解析後的 MCP 憑證會落在哪裡。** 由於靜態無加密，ccync 會把 MCP 憑證以明文寫入磁碟上的四個位置：(1) 四個即時 MCP 主機檔案各一份（`~/.claude.json`、`~/.copilot/mcp-config.json`、`~/.codex/config.toml`、OpenCode 設定）——它們需要解析後的值才能通過驗證；(2) `~/.ccync/build/lock.json`（`_mcpServers` 命名空間）與 (3) `~/.ccync/build/mcp/managed.json`，兩者都會在 `ccync init` 時、任何 `${VAR}` 替換發生**之前**，逐字快照每個*被採用*的 MCP server **完整定義**（含 `env` 與 `headers`）——替換發生在之後、且僅在記憶體內，因此這兩個檔案保存的是 master 代理未解析前的原始位元組；以及 (4) `config.json#secrets` 本身，由你直接填入。**因此 master 代理設定中的字面權杖，不論你是否在別處使用了 `${VAR}` 佔位符，都會以明文儲存於 (2) 與 (3) 兩個檔案中**——請直接在 master 設定裡改用 `${VAR}` 佔位符（真值放在 `config.json#secrets` 或環境變數），才能讓字面權杖不落入其中。若你正在清除某台機器上的憑證，或要把備份目錄交給他人：`config.json` 與 `build/lock.json` 屬於備份集合（見[備份與還原](#備份與還原)）；`build/mcp/managed.json` 與四個即時主機檔案**不會**被 `ccync backup` 備份，必須另行處理。架構層契約見 [`architecture.md`](../../architecture.md#managed-mcp-ownership-and-recovery)（僅英文）。

> 注：`devMode`/`ccyncRoot` 是不啟用的相容性欄位（見上方鍵值表）——它們不是權威的模式欄位。舊有的 `installMode` 欄位已棄用——若存在，只會觸發 `ccync doctor` 的遷移警告，不影響實際模式。

## 機器資料佈局

ccync 在你機器上擁有的一切都位於 `~/.ccync/` 之下，分為三類：

- **INPUT（輸入）**——你（或 `ccync add`/`ccync init`）直接寫入的兩個檔案；永不自動刪除，`sync` 也不會重建它們。
- **CACHE（快取）**——抓取下來的外掛本體，依 pin 值做鍵值，因此重新抓取相同內容是無操作；可從網路/來源重建。
- **OUTPUT（輸出）**——`build/` 之下的一切；由 `ccync sync` 完全重新產生。可重新產生不代表每個檔案都同等安全，可以直接手動刪除——動手刪除 `build/` 下任何檔案（相對於透過 `ccync uninstall`）之前，請先看下方樹狀圖裡的逐檔案說明。

另有兩個根層級項目落在這三類之外——`dist/targets/` 與 `plugins/` 目錄。它們都是衍生資料（由下一次 `ccync sync` 重建、由 `ccync uninstall` 移除），只是不位於 `build/` 之下。你不需要手動編輯它們，它們也不在備份集合中。細節見 [`architecture.md`](../../architecture.md#state-topology-ccync)（僅英文）。

公開外掛 catalog **內嵌於 `ccync` 執行檔本身**——`~/.ccync/` 之下沒有任何 catalog 檔案。解析直接讀取內嵌副本；沒有任何東西會為此被下載或部署到磁碟。

```txt
~/.ccync/                              ccync 的全部機器狀態
├── config.json          [INPUT]       機器設定 (secrets;devMode/ccyncRoot 已解析但不啟用)
├── plugins.json         [INPUT]       個人外掛 catalog（由 `ccync add` 寫入）
├── dist/targets/        [衍生]        各 target 的投影狀態 (claude/copilot/codex/agy)
├── plugins/             [衍生]        ccync 自有的 marketplace manifest（這是目錄——
│                                      不是上方那個 INPUT 檔 `plugins.json`）
├── cache/               [CACHE]       個人外掛本體，嚴格釘住版本
│   └── <id>@<sha | sha256-first12>/  clone/解壓縮後的外掛本體
└── build/               [OUTPUT]      由 `ccync sync` 完全重新產生
    ├── lock.json                      已解析的 lockfile (_personalPlugins、_mcpServers、_adoptMaster、_ccyncProjection);屬備份集合——承載無法重建的採用快照(刪除它會遺失「哪些項目是在 `ccync init` 時被採用的」,不只是快取)
    ├── render/                        規範根目錄：skills/、commands/、agents/、hooks/、.mcp.json——刪除永遠安全,下次 `ccync sync` 會完整重建
    ├── mcp/managed.json               彙總後的受管 MCP manifest——同時也是 MCP 擁有權復原的舊版 bootstrap 基準;刪除它會讓所有逐主機追蹤上線前的項目落入下方的「遺留項目」情境
    ├── mcp/projected-state.json       逐主機的 MCP 擁有權證明;刪除它不會移除即時 MCP 項目,但 ccync 將無法再證明擁有權,未來 `remove`/`uninstall` 可能因此把它們遺留下來(見疑難排解)
    ├── mcp/projection.txn.json        write-ahead MCP transaction journal;只在投影進行中或正從中斷復原時才存在;若此時刪除它,會造成與程序被中斷相同的 ambiguous-recovery fail-closed 狀態,因此除非你已確認目前沒有投影在進行,否則請勿動它
    └── active/<provider>/             各代理投影的登記資料——刪除永遠安全,下次 `ccync sync` 會完整重建
            │
            ▼
     投影 ──► 即時代理設定介面（`~/.ccync/` 之外）：
             ~/.claude.json、~/.claude/skills/…、~/.codex/config.toml、
             ~/.copilot/mcp-config.json、opencode.json、Antigravity 各介面
```

這就是「安裝一次，處處投影」的機制：`ccync sync`（或 `add`/`remove`/`init`/`upgrade`——它們都呼叫同一套引擎）在 `build/` 下解析並渲染一次，再把這單一渲染結果投影到你選定的每一個代理。若你想直接檢視內嵌於執行檔中的 catalog（而非透過 `ccync list`/`ccync add`），並沒有檔案可開——它的來源就是執行檔本身。

## 備份與還原

### `ccync backup [--output <dir>]`

將無法重建的本機狀態檔案複製到備份目錄：

- `config.json` — 機器設定(INPUT)
- `plugins.json` — 個人外掛程式目錄(INPUT)
- `plugins.lock.json`（來自 `build/lock.json`）— 雖屬 OUTPUT，但因其承載 init 採用快照（`_adoptedItems` / `_mcpServers`，無法從 INPUT 檔重新產生）而納入備份

機器上不存在的檔案（例如沒有個人外掛程式的機器上的 `plugins.json`）會附註跳過——這不是錯誤。`cache/` 與 `build/` 其餘部分刻意排除：它們可由 `ccync sync` 完整重建。

三個檔案中有兩個可能承載明文憑證：`config.json` 的 `secrets` 欄（見上方），以及 `plugins.lock.json` 的 `_mcpServers` 命名空間——後者逐字保存每個被採用的 MCP server 定義，包含 master 代理設定中任何字面權杖（見〈[設定檔 (`config.json`)](#設定檔-configjson)〉中的「被採用的 MCP 憑證會落在哪裡」）。`ccync backup` 會在警語中**具名列出實際複製到的那些檔案**，提醒該備份目錄可能含機密、應妥善保管——請以保護原始檔案同等的謹慎程度保存這份備份目錄。

### `ccync restore --from <dir>`

從先前建立的備份目錄將同樣三個檔案還原到其即時位置（`plugins.lock.json` 還原回 `build/lock.json`）。備份中不存在的檔案——包括在 `plugins.json` 納入備份集合之前建立的舊備份——會附註跳過。目的地既有檔案會被覆寫。之後請執行 `ccync sync` 重建衍生狀態並重新投影到你的代理。

## 退出碼 (Exit Codes)

| 碼 | 意義 |
| --- | --- |
| `0` | 成功 |
| `1` | 操作錯誤，包含拒絕首次執行閘門或無法完成的部分投影 |
| `3` | 有可更新項目——唯讀的可更新檢查發現至少一個待升級項目（`ccync list --upgrade-available`、`ccync upgrade --dry-run`）。與 `1` 區分開來，讓 script 能分辨「有可更新」與真正的失敗。 |
| `64` | 用法錯誤（例如：未知或遺失指令，或無效的標記） |
