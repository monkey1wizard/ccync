---
type: reference
title: ccync 命名與術語
description: Agent、ProjectionTarget、MCP Host、plugin lifecycle 與 canonical target keys 的權威定義。
tags: [naming, vocabulary, targets]
status: stable
locale: zh-Hant
translation_key: naming
---

# ccync 命名與術語

## 核心名詞

| 名詞 | 定義 |
| --- | --- |
| Agent | 使用者選取的程式輔助代理產品，共五個：`claude`、`codex`、`copilot`、`opencode`、`agy`。 |
| ProjectionTarget | 可獨立進行投影並回報狀態的目標，共七個。 |
| ProjectionEngine | 負責協調整體投影流程的核心元件：解析來源清單、建置 canonical render，並指派各目標專屬投影器。 |
| ProjectionPath | 一次投影流程中可能建立、更新或移除之具體檔案路徑。 |
| MCP Host | 接收 MCP 設定的主機宿主產品，固定為五個，有別於被設定之 MCP Server。 |
| Personal Plugin | 經由 `ccync add` 登記、快取並由 ccync 管理的個人外掛。 |
| Adopted Item | 自主要代理匯入之既有項目；ccync 僅記錄其來源快照，不負責自動升級。 |
| Roster | 記錄團隊成員共用外掛清單的專案檔案（`ccync.roster.json`），經由 `roster dump/install` 管理，與 catalog bundle 無涉。 |

## 七個投影目標識別碼

七個規範投影目標識別碼分別為 `claude`、`codex`、`copilot`、`opencode`、`agy-cli`、`agy-ide` 與 `agy-gui`。在設定選取代理時使用代碼 `agy`，於實際執行投影時，則會自動展開為三個 Antigravity 目標。識別碼嚴禁使用單純的 `gemini` 或 `antigravity`。過往獨立的 Google Gemini CLI 支援已退役，目前 `~/.gemini/` 目錄下的相關配置均歸屬 Antigravity 管轄。

## 保留動詞與命名規範

在本專案的文件與介面中，各動詞具備嚴格的語意區隔：

- `update`：專指更新 **ccync 執行檔本身**（類似 `brew update` 語意）。
- `upgrade`：專指將 Git 來源之外掛升級至最新上游版本（類似 `brew upgrade` 語意）。
- `search`：專指在公開程式碼託管平台（GitHub/GitLab）搜尋外掛儲存庫。
- `bundle`：專指 catalog 內建之 `bundled-local` 發布機制。
- `roster`：專指團隊共用之個人外掛專案清單。

產品與執行檔名稱一律寫為全小寫 `ccync`。

Rust 程式碼命名規範：函式與模組採用 `snake_case`，型別採用 `UpperCamelCase`，常數採用 `SCREAMING_SNAKE_CASE`；外掛識別碼則採用 kebab-case。
