---
type: explanation
title: 既有項目匯入與核對
description: 從五種來源之一匯入既有安裝、保存 provenance，並以 Graphify 說明 adopted skill 的維護邊界。
tags: [adoption, reconciliation, provenance, graphify]
status: stable
locale: zh-Hant
translation_key: adoption
---

# 既有項目匯入與核對

執行 `ccync init` 時，ccync 會從所選來源匯入既有外掛、獨立技能與 MCP 伺服器。可攜式 provider 意圖使用穩定的來源與元件識別。通過准入驗證的 HTTPS 參照會保留在可攜式意圖中，其他參照則使用不透明的 binding key，實際來源位置仍留在個人私有目錄。MCP 定義與操作所有權欄位也會保留在私有資料中。來源紀錄會寫入個人 `plugins.json` 的 `_adoption`，並保存使用者同意的目標快照。Lockfile 索引由這些紀錄產生，且會保留未知欄位與其他來源的紀錄。舊格式若缺少 `plugins` 陣列會視為空陣列；若該欄位存在但型別錯誤，則會拒絕處理。

## 來源記錄

每筆紀錄會在可取得時保存來源與元件識別、原始名稱、來源位置、修訂版或摘要、啟用狀態及非機密政策資料。憑證不會複製到來源紀錄，只有通過指紋驗證後才會從來源重新讀取。獨立技能會從紀錄的實際來源位置複製。舊式只有名稱的紀錄需要重新掃描。來源不存在時不會因此刪除輸出。

私有 lockfile 會將 `_adoptedItems`、`_looseSkills` 與 `_mcpServers` 放在獨立命名空間，同步時會由 `_adoption` 重建。MCP 定義與獨立技能來源位置屬於私有綁定資料。可攜式序列化只引用已准入的識別，不會複製這些綁定或操作所有權證據。重新初始化只更新相同來源與元件識別，並保留其他紀錄。掃描結果缺少某項目不代表要移除它。仍支援讀取舊式 `_adoptMaster`。

## 鬆散技能衝突與重新掃描

預覽會依名稱分組獨立技能。同名但來源位置不同的紀錄會形成一個衝突。預覽會列出每個來源位置，sync 會阻擋整組項目，且不寫入其中任何成員。ccync 不會保留第一筆紀錄，也不會挑選勝出者。同一來源的重複紀錄不構成衝突，無關的匯入仍會繼續。

```text
loose conflict: hello has 2 origins: /origin/a, /origin/b; sync blocks every member and writes none
```

沒有記錄來源位置的獨立技能需要重新掃描。預覽會指出它，sync 會略過它。

```text
rescan required: loose skill hello has no recorded origin; sync skips it
```

沒有來源位置、且與匯入項目相符的舊式匯入紀錄，會在任何寫入之前停止確認。

```text
rescan required: hello (skill) has no recorded origin; confirming stops before any write
```

## 被阻擋與已停用的 MCP 定義

`ccync init` 的預覽與完成報告，會列出掃描阻擋的每個 MCP 定義，並附上名稱與原因。已停用的定義另列一行，且保留其停用政策。報告絕不顯示憑證值。

```text
mcp blocked: 1 server(s), not imported; mcp disabled: 1 server(s)
    <name>: <reason> (<origin>)
    disabled by the source: <name>
```

被阻擋的定義不會移除同一來源中的其他有效項目。來源主機已定義的 MCP 項目由誰擁有，不屬於本次修復的範圍，請見[本次修復的範圍](setup.zh-Hant.md#本次修復的範圍)。

## 已匯入項目僅回報版本差異

`ccync upgrade` 對於已匯入項目僅檢查版本並回報狀態，不會主動代為安裝更新。若來源清單與基準版本足供比對，指令會顯示 `current → latest`；若資訊不足，則標示 `version-unresolved`。MCP 伺服器與獨立技能因缺乏可比對之版本編號，其實際更新仍須由來源代理或上游安裝程式負責。若欲改由 ccync 全權託管該來源，請先執行 `ccync remove <id>` 移除匯入記錄，再明確執行 `ccync add <source>` 重新納管。請使用 `ccync list` 顯示的管理 ID；不接受套件名稱作為選取器。執行 `remove` 時會同步撤回 ccync 已投影之內容，操作前請務必確認影響範圍。

## 同步時的管理界線

在執行同步時，ccync 會從匯入時記錄的來源目錄重新讀取每個獨立技能並建立共用快照，但絕不覆寫來源代理的原始檔案，也不會重新匯入 ccync 自己發布的複本。若欲自其他代理移除項目，ccync 必須具備明確的所有權證明確認該檔案確由自身投影；單憑匯入來源記錄，並不構成刪除使用者現有內容的充分授權。

## 獨立發布、撤回與復原

已匯入的鬆散技能屬於獨立內容。`ccync sync` 會將它以一般技能目錄發布到每個已選取 target 的技能根目錄，已持有該技能的根目錄除外。Codex、Copilot 與 OpenCode 共用 `~/.agents/skills`，所以一份已發布的複本就能服務該根目錄的所有讀取者。已發布的技能沿用來源名稱，不會加上 `ccync:` 命名空間。

當來源不再提供該技能時，sync 只會撤回 ccync 自己發布的複本。撤回需要共用根目錄中每一個已知讀取者的證明。從未被選取的讀取者仍會擋下撤回，因為它的移除或替代證據未知。此時複本與所有權記錄都會保留。

若發布或撤回被中斷，ccync 會保留其 journal 與備份。請再次執行 `ccync sync` 以重試。請勿手動刪除 journal、備份或已發布的檔案。這些路由在 sync 中會回報 `standalone published`、`recovery blocked` 或 `identity unresolved`。這些結果描述的是磁碟上的檔案，不能證明代理已載入該技能。復原步驟見[設定](setup.zh-Hant.md)。

## MCP 來源驗證

MCP 匯入會在預覽時記錄來源檔案與指紋。儲存定義前，ccync 會再次讀取同一個來源並檢查指紋。來源遺失、變更或不安全時，匯入會遭到阻擋。含有憑證或其他不可攜欄位的定義會保留在原始來源，不會存入 `_mcpServers`。原始檔案與憑證會留在原處。來源證據不授予刪除權限，也不會納入可攜式備份。未經授權時，五種格式均能保留來源的行為仍未獲證實。

## Graphify 技能更新範例

Graphify 為典型的已匯入獨立技能，其生命週期牽涉三個不同層級的管理機制：PyPI 套件 `graphifyy` 負責管理 CLI 執行檔 `graphify`；Graphify 原生安裝程式負責管理 Claude 的主要技能目錄 `~/.claude/skills/graphify/`；而 ccync 則僅負責將該目錄之最新快照投影至其他代理。

更新操作請依序執行：

```text
pip install -U graphifyy
graphify install --platform claude
ccync sync
```

執行時務必明確指定 `--platform claude`。若直接執行不帶參數的 `graphify install`，安裝程式會同時向多個代理目錄寫入檔案，導致各代理持有的複本版本產生分歧；若目標路徑已建立 junction 連結，安裝程式甚至可能直接改寫 ccync 的內部輸出。正確作法為：先更新 Claude 旗下的原始技能，再執行 `ccync sync` 讓其餘代理同步取得新版快照。

ccync 將獨立技能視為不解析內容的黑箱目錄，既不主動判讀其內部版本、不更新上游，亦不改動來源代理的原始檔案。鎖定檔完整記錄來源歷程，允許每次同步時重新擷取技能快照。由於 ccync 不解析 `.graphify_version`，即便所有代理均處於同一個過期版本，系統亦不會提出警示，版本新鮮度仍須由維護者配合 Graphify 工具自行確認。

若 `ccync sync` 針對 Graphify 路徑輸出 `preserved user-owned path` 提示，表示該目標位置已存在既有目錄，但 ccync 缺乏足夠的所有權證明來確認其歸屬。遇此情況，請先備份並釐清檔案歸屬後再評估是否清理多餘複本，切勿僅為了消除警告提示而強制覆寫。
