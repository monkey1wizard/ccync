---
type: guide
title: 文件國際化指南
description: ccync 雙語文件的路徑、metadata、translation key、標題配對與維護規則。
tags: [i18n, documentation, zh-Hant]
status: stable
locale: zh-Hant
translation_key: i18n-guide
---

# 文件國際化指南

英文為本專案儲存庫的權威原始語言，翻譯版本集中維護於 `docs/i18n/<locale>/`。根目錄的 `README.md` 對應 `docs/i18n/zh-Hant/README.zh-Hant.md`，`CONTRIBUTING.md` 對應 `docs/i18n/zh-Hant/contributing.zh-Hant.md`。其餘主題文件沿用英文檔名並加上語言字尾，例如 `docs/setup.md` 對應 `docs/i18n/zh-Hant/setup.zh-Hant.md`。

## Metadata

除 README 外，繁體中文主題文件與安全性文件均具備七個 YAML metadata 欄位：`type`、`title`、`description`、`tags`、`status`、`locale` 與 `translation_key`。英文與繁體中文 README 均不使用 metadata，依檔案路徑與 H2/H3 標題層級配對。根目錄的英文 `SECURITY.md` 使用 `translation_key` HTML 註解，其繁中版本保留 YAML metadata。其他雙語文件以檔案路徑、`translation_key` 與標題層級配對。配對機制不依賴來源 commit hash，因此單純調整 metadata 不會使譯文自動失效。

## 文字與連結

繁體中文文件統一採用臺灣慣用語法與標準標點符號。程式識別碼、CLI 旗標、路徑與專案定義之專有名詞一律保留原文。文件內部的交互參照一律使用標準 Markdown 檔案或標題錨點連結，嚴禁手動插入 HTML 錨點標籤。每個技術主題應由單一負責文件完整陳述，其餘文件則以超連結引用，以防範跨文件的規範描述隨時間產生語意分歧。

## 維護流程

各階段完成文稿撰寫後，應檢查適用文件的 YAML metadata、UTF-8 編碼（換行符號嚴格採用 LF）、臺灣技術用語、超連結有效性與論述完整度。英文與繁體中文版本的規範強度、警示訊息與示例說明必須對等，但不要求逐字翻譯。若調整標題文字，須同步更新雙語配對文件與 structure map。若遇到無法確認的配對或失效連結，應明確標示為待釐清。
