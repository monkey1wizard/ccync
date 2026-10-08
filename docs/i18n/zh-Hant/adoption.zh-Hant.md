---
type: explanation
title: 既有項目匯入與核對
description: 從 master agent 匯入既有安裝、保存 provenance，並以 Graphify 說明 adopted skill 的維護邊界。
tags: [adoption, reconciliation, provenance, graphify]
status: stable
locale: zh-Hant
translation_key: adoption
---

# 既有項目匯入與核對

執行 `ccync init` 時，ccync 會從主要代理匯入既有的外掛、獨立技能與 MCP 伺服器，並於鎖定檔中標記為已匯入項目（Adopted Item），隨後以相同項目識別碼投影至其他目標。此匯入操作不會移動主要代理的目錄，原本由主要代理管理的來源，亦不會自動轉為 ccync 的個人外掛。

## 來源記錄

ccync 會盡可能記錄每個已匯入項目的來源類別、市集或上游識別線索、主要代理識別碼以及基準版本。針對 MCP 伺服器，系統會完整保存其伺服器定義而非僅有名稱；針對獨立技能，則記錄其位於主要代理的原始路徑，以便日後重新擷取快照。即便來源中繼資料不完整，項目仍可順利匯入，惟狀態將標記為 `unresolved`，後續可經由 `ccync repair` 補齊資訊。

## 已匯入項目僅回報版本差異

`ccync upgrade` 對於已匯入項目僅檢查版本並回報狀態，不會主動代為安裝更新。若來源清單與基準版本足供比對，指令會顯示 `current → latest`；若資訊不足，則標示 `version-unresolved`。MCP 伺服器與獨立技能因缺乏可比對之版本編號，其實際更新仍須由來源代理或上游安裝程式負責。若欲改由 ccync 全權託管該來源，請先執行 `ccync remove <id>` 移除匯入記錄，再明確執行 `ccync add <source>` 重新納管。執行 `remove` 時會同步撤回 ccync 已投影之內容，操作前請務必確認影響範圍。

## 同步時的管理界線

在執行同步時，ccync 會重新讀取主要代理所屬的獨立技能並建立共用快照，但絕不覆寫主要代理之原始檔案。若欲自其他代理移除項目，ccync 必須具備明確的所有權證明確認該檔案確由自身投影；單憑匯入來源記錄，並不構成刪除使用者現有內容的充分授權。

## Graphify 技能更新範例

Graphify 為典型的已匯入獨立技能，其生命週期牽涉三個不同層級的管理機制：PyPI 套件 `graphifyy` 負責管理 CLI 執行檔 `graphify`；Graphify 原生安裝程式負責管理 Claude 的主要技能目錄 `~/.claude/skills/graphify/`；而 ccync 則僅負責將該目錄之最新快照投影至其他代理。

更新操作請依序執行：

```text
pip install -U graphifyy
graphify install --platform claude
ccync sync
```

執行時務必明確指定 `--platform claude`。若直接執行不帶參數的 `graphify install`，安裝程式會同時向多個代理目錄寫入檔案，導致各代理持有的複本版本產生分歧；若目標路徑已建立 junction 連結，安裝程式甚至可能直接改寫 ccync 的內部輸出。正確作法為：先更新 Claude 旗下的原始技能，再執行 `ccync sync` 讓其餘代理同步取得新版快照。

ccync 將獨立技能視為不解析內容的黑箱目錄，既不主動判讀其內部版本、不更新上游，亦不改動主要代理的原始檔案。鎖定檔完整記錄來源歷程，允許每次同步時重新擷取技能快照。由於 ccync 不解析 `.graphify_version`，即便所有代理均處於同一個過期版本，系統亦不會提出警示，版本新鮮度仍須由維護者配合 Graphify 工具自行確認。

若 `ccync sync` 針對 Graphify 路徑輸出 `preserved user-owned path` 提示，表示該目標位置已存在既有目錄，但 ccync 缺乏足夠的所有權證明來確認其歸屬。遇此情況，請先備份並釐清檔案歸屬後再評估是否清理多餘複本，切勿僅為了消除警告提示而強制覆寫。
