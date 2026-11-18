---
type: guide
title: 外掛搜尋與管理
description: 搜尋、修復來源、新增、移除、檢視及分享 ccync plugins 的操作指南。
tags: [plugins, discovery, management, roster]
status: stable
locale: zh-Hant
translation_key: plugins
---

# 外掛搜尋與管理

## 搜尋與修復來源

`ccync search <term> [--limit N] [--no-add]` 用於搜尋 GitHub 與 GitLab 上的公開儲存庫。該指令會整合 CLI、公開 API 與網頁來源的搜尋結果，並依據標準化後的 clone URL 自動排除重複項目。在互動模式下，操作者可直接選取搜尋結果並轉交 `add` 處理；`--no-add` 旗標則僅列出搜尋結果。搜尋功能絕不擅自為已匯入項目猜測來源，亦不會未經確認即修改設定。

若已匯入的項目缺少來源記錄，可執行 `ccync repair [<id>]` 進行修復。此指令會列出可能的候選來源供操作者核對，僅在獲得明確選取後才會寫入記錄；若無法確認來源，項目將持續標示為 `unresolved`。`upgrade` 流程所顯示、建議執行 `ccync repair` 的提示僅為操作指引，並不會自動代為修復。

## 新增外掛及取得來源

`ccync add <source> [--no-sync]` 支援接收 catalog ID、Git URL、本機 Git 工作目錄、一般目錄與封存檔。針對 Git 來源，ccync 會先下載並快取儲存庫內容，隨後定位外掛根目錄。若指定了遠端儲存庫的子路徑，該子路徑必須嚴格限制於儲存庫範圍內，任何絕對路徑或包含 `..` 的目錄跳脫路徑均會被直接拒絕；封存檔解壓縮流程亦受到相同的路徑邊界限制。

一般本機目錄會以快照形式納管，ccync 不假設其具備可持續更新的上游來源。`--no-sync` 旗標僅更新個人來源清單，不要求機器必須完成初始化，亦不會觸發代理投影。

外掛可提供技能、指令、自訂代理與 MCP 伺服器定義。ccync 會將符合 Claude 規格之外掛中繼資料、目錄與元件清單彙整為共用規格；若來源目錄為空或未包含任何有效元件，ccync 會於診斷中明確回報，絕不可將其視為安裝成功。

指令會保留在可用的原生指令介面。若目標只能以技能呈現指令，ccync 僅轉換不會改變引數語意的單層指令。使用位置引數、shell 樣式引數展開或命名空間的指令會在發布前回報為不支援；無法保留呼叫方式時，不會發布部分內容。受支援的單層轉換會保留指令本文、描述與 `$ARGUMENTS` 佔位符。

## 移除、列出與檢視

`ccync remove <id>` 會將指定項目自個人來源清單、後續產生的鎖定檔及輸出中移除，並於經操作者確認首次寫入後，更新各代理實際生效之檔案。若遭遇同名路徑但缺乏明確所有權證明，ccync 將一律保留該檔案。

`ccync list [--upgrade-available]` 用於列出所有個人外掛與已匯入項目；`ccync show <id>` 則顯示單一項目的來源設定、鎖定版本、內含元件與目前狀態。這兩個查詢指令在機器初始化前亦可直接使用。

請注意：來源管理與代理投影屬於兩個獨立階段。本文件專注於來源的登記、修復與移除；投影機制請參閱[架構](architecture.zh-Hant.md)，版本升級與快取清理請參閱[指令參考](commands.zh-Hant.md)。

## 套件名稱與管理 ID

每個外掛都有套件名稱與管理 ID。套件名稱是原生用戶端列出並用於其命名空間的正式名稱，例如 `Impeccable`。管理 ID 是 ccync 指令接受的儲存鍵，例如 `plugin`。既有的管理 ID 與 pin 不會改變。套件名稱只是顯示用的身分，任何指令都不接受它作為選取器。

命名空間依來源形式決定。原生外掛在支援的原生路由上保留來源套件身分。獨立技能或已物化元件直接使用來源與元件身分，不會加入由管理器產生的 `ccync:` 命名空間。不同來源若在原生命名空間衝突，sync 會回報衝突並保留來源供診斷。

`ccync list` 會對套件名稱與 ID 不同的外掛各輸出一行，建議指令則列出管理 ID：

```text
Package: Impeccable | management ID: plugin (remove with: ccync remove plugin)
```

`ccync show plugin` 會先列出套件名稱，再列出管理 ID：

```text
Plugin: Impeccable | management ID: plugin
```

`ccync show Impeccable` 會失敗，並在 stderr 印出以下錯誤：

```text
ccync show: 'Impeccable' not found. Run `ccync list` to see installed items.
```

每個指令都請使用管理 ID：

```text
ccync show plugin
ccync upgrade plugin --dry-run
ccync remove plugin --yes
```

`ccync remove` 會先更新 ccync 自身的狀態，才要求確認。它會從個人目錄（或已採用項目的狀態）移除該外掛、更新 ccync lockfile，並產生 canonical render。確認關卡之後只管制即時代理投影。未加 `--yes` 時，ccync 會停在該關卡，不寫入任何即時代理設定。請執行 `ccync sync --yes` 套用剩餘步驟。ccync 沒有 enable 或 disable 指令，啟用狀態由用戶端自己的外掛管理工具負責。

## Claude Code 與 Codex 的原生套件

當用戶端的 adapter 已驗證時，ccync 會透過該用戶端自己的外掛管理工具，把 Claude Code 與 Codex 的每個外掛群組安裝為一個原生套件。原生套件保留正式套件名稱與來源資產。其 view 會移除啟用中的 MCP 項目並排除 hooks，因為 hooks 不屬於原生套件發布範圍。MCP 伺服器仍由 ccync 的集中 MCP 寫入器負責。此群組作為一個單位運作。只要任一成員無法啟用，群組就只保留該外掛的一條路由：沒有已提交原生紀錄的群組維持 loose 路由；已有先前或舊版原生紀錄的群組，則保留該原生路由，loose 副本不會與之並存載入。無論哪種情況，sync 都會回報原因。

原生元件宣告若無法被套件 view 安全承載，會以原因拒絕。拒絕會阻擋該 view，因此群組會改走上述的回退路由。拒絕訊息會列出 target、外掛 ID 與原因。

Portable package 會另存於非即時使用的 `P/packages/`。ccync 只會在 digest 驗證通過，且每項受認證宣告都符合以下封閉規則清單時接納套件：

- Plugin manifest 的 `name`、`version`、`description`、`license` 必須是字串，`keywords` 必須是字串陣列；`author` 可為字串，或包含可選字串欄位 `name`、`email` 與可選 portable HTTPS `url` 的物件；`homepage` 必須是 portable HTTPS URL；`repository` 可為 portable HTTPS URL 字串，或包含必要 portable HTTPS `url` 與可選字串 `type` 的物件。Portable HTTPS URL 使用 ASCII host 與 path，可含非空數字 port，不得含使用者資訊、query、fragment、百分比 escape，path 也不得含 `.` 或 `..` 段。`commands`、`agents`、`skills` 必須是安全的套件相對路徑字串或字串陣列。manifest 的 `mcpServers` 值可為根 MCP 檔名參照或空物件。`hooks` 與 `lspServers` 一律拒絕。manifest 的 `name` 必須符合 `^[A-Za-z_][A-Za-z0-9_.-]{0,63}$`。
- 根 MCP 檔只接受 `mcpServers` 物件。每個 server 只允許 `type`、`command`、`args`、`url`、`env`、`headers` 欄位。server 名稱及 `env`、`headers` 的鍵名都必須符合相同識別碼格式。type 限 `stdio`、`http`、`sse`，command 不可含空白，`args` 必須是字串陣列，URL 必須符合上述 portable HTTPS 規則。每個 `env` 與 `headers` 值都必須是 `${NAME}` placeholder。
- MCP `args` 中，每個 option 名稱會先轉為小寫並移除 `-`、`_`、`.`。若名稱包含 `token`、`secret`、`password`、`passwd`、`bearer`、`credential`、`apikey`、`key`、`auth`、`cookie`、`session`、`signature` 或 `private`，其值必須是 `${NAME}` placeholder。含空白的引數值只接受 `name: [label ]${NAME}` 格式。
- 唯一接受的 skill sidecar 路徑是恰好四段的 `skills/<非空-id>/agents/openai.yaml`。根節點只能為 null 或僅含 `interface`、`policy` 的 mapping。`interface` 只接受字串欄位 `display_name`、`short_description`、`icon_small`、`icon_large`、`brand_color`、`default_prompt`；icon 路徑必須安全且相對。`policy` 只接受布林值 `allow_implicit_invocation`，MCP dependencies 不獲准。YAML 重複鍵會拒絕該 sidecar。
- 受認證的 JSON 表面只有 plugin manifest 與根 MCP 檔。解析器只解析一次，並在每一層拒絕重複物件鍵；判定重複鍵前會先解碼 JSON escape。ccync 使用該解析值進行驗證，且只有驗證成功後才發布原始位元組。

Hooks、未支援或未分類的設定檔會拒絕整個套件，來源仍保留為私有資料。未受上述規則約束的字串值，依定義屬於未認證自由文字。其他未認證表面包括 opaque assets、副檔名不在設定清單內的檔案、Markdown 本文與 frontmatter、不含空白的 MCP positional 與 short-option literals，以及已接受宣告中不透明的部分。這些表面的發現列入後續審查風險或延至 P3，不代表機密已排除；opaque assets 仍逐位元組保留，且不保證沒有機密。原生 view 仍會移除 MCP 與 hook entrypoints。

Copilot、AGY 與 OpenCode 在 ccync 中沒有原生套件路由，維持其 loose 技能或元件路由。

原生變更必須完全關閉應用程式、重新開啟並開始新的工作階段，才會到達執行中的應用程式。ccync 不會重新載入代理。你在用戶端中停用的註冊會維持停用、零次載入，sync 不會將其啟用。

復原狀態：

- **待定意圖（pending intent）。** 中斷的原生變更會在 `build/projection.txn.json` 留下一筆項目。下一次 sync 會先將其與現行註冊核對，之後才會重試。ccync 絕不會盲目重複安裝或移除。
- **採納（adoption）。** 只有當待定意圖指名相同的 selector，且新的觀察結果符合已記錄的 view digest 時，才會採納既有註冊。缺少此證明的同名註冊視為衝突。
- **復原受阻（recovery blocked）。** 外部偏移或模糊的意圖會阻擋復原。ccync 會保留意圖、preimage 與註冊，並回報衝突。請勿手動刪除 ccync 檔案或原生註冊。
- **獨立發布與撤回。** sync 與 doctor 會分別回報獨立發布、復原受阻、身分無法解析，以及保留或 fallback 結果。Provider lifecycle journal 以 v3 schema 寫入。只有內容不含該 schema 無法表示的意圖類型時，才接受 v1 與 v2。待處理意圖不會被降級或丟棄。重試寫入或移除前，復原會使用已記錄的意圖與所有權證據。

`ccync doctor` 以唯讀方式回報原生狀態。狀態包括：已註冊且 loader 未執行；刻意停用；屬於較舊世代的所有者；無法使用或無法讀取；不存在；損壞；與已提交世代偏移；外部註冊。用戶端列舉中出現的註冊，並不能證明用戶端已載入該套件。目前的驗收狀態請見[設定](setup.zh-Hant.md#原生套件驗收狀態)。

CLI 狀態描述觀察到的註冊與檔案系統狀態，不能證明附加的桌面或 IDE 介面已載入、帳戶介面的啟用數量，或真實機器遷移。這些檢查仍屬擁有者驗收。工具生命週期與新工具快照安裝不在此套件發布範圍，MCP 發布變更與 hooks 亦同。

## 團隊清單

團隊清單為置於專案根目錄的 `ccync.roster.json` 檔案，旨在定義團隊成員共同約定安裝的個人外掛集合。該檔案並非 catalog bundle、鎖定檔或全機備份，亦不包含已匯入項目、MCP 機密值或各代理的原生設定快照。

`ccync roster dump [--out <path>]` 會依據目前登記的個人外掛來源產生團隊清單，預設輸出為 `ccync.roster.json`。清單會完整記錄足以重新取得外掛之來源資訊與必要的子路徑，並採用確定性排序以便於 Git 差異比對；若項目缺乏可靠的遠端來源或無法在跨機器間重現，ccync 會明確指出該項目，絕不隨意以本機暫存路徑替代。

`ccync roster install [--in <path>] [--dry-run] [--no-sync] [--yes]` 在讀取清單時會先行驗證格式與檢查重複識別碼，隨後依照標準 `add` 規範逐項取得來源。`--dry-run` 模式僅列出預計新增、已存在、衝突與失敗的項目；`--no-sync` 模式僅完成來源登記而不修改各代理檔案；若直接執行同步，首次寫入仍需經操作者確認。

團隊清單旨在協助團隊成員同步外掛配置，絕不授權刪除使用者清單以外的私人物件。當同一 ID 對應至不同來源時，必須由使用者明確排解衝突；名稱相同絕不代表 ccync 具備該物件的所有權。建議將團隊清單納入版本控制與程式碼審查流程，藉由清單變更確認外掛來源的合法性。
