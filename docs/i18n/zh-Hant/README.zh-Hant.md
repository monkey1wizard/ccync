# ccync

[English](../../../README.md) · **繁體中文**

ccync 能在單一位置集中管理外掛、技能、指令、自訂代理與 MCP 伺服器，並統一投影至 Claude Code、Codex、GitHub Copilot CLI、OpenCode 及 Antigravity。工具會先將已選取的來源記錄於 `~/.ccync/build/lock.json`，建置出共用的 canonical render，再依各工具的原生格式進行投影。即使現有路徑名稱相同，只要缺乏所有權證明，ccync 便絕不擅自刪除使用者原有內容。

## 快速開始

```text
ccync init claude
ccync sync --dry-run
ccync sync --yes
ccync add <來源>
ccync doctor
```

請先執行 `init`。選擇一個匯入來源（`claude`、`codex`、`copilot`、`opencode` 或 `agy`）與目標代理；首次初始化預設選取全部五個。ccync 會唯讀掃描來源，預覽匯入項目、即將修改的代理設定檔與 MCP 主機，取得你的同意前不寫入任何內容。來源只用來記錄各匯入項目的出處，不是固定的主要代理。完成初始化後，可藉由 `sync --dry-run` 預覽變更，確認無誤後再執行 `sync --yes`。`add` 支援 catalog ID、Git URL、本機目錄或封存檔；若目前僅需登記來源而不立即投影，可加上 `--no-sync` 旗標。更多指令用法請參閱[指令參考](commands.zh-Hant.md)。

## 公開指令

ccync 提供 19 個公開指令。[指令參考](commands.zh-Hant.md)列出了各指令的用途與詳細規範說明，具體參數請查閱 `ccync <command> --help`。

## 平台注意事項

- Windows、macOS 與 Linux 各平台之代理設定路徑各異，ccync 會根據目標產品與作業系統寫入原生格式。
- Windows 發行版本採用 static CRT 建置；安裝完成後，建議執行 `ccync doctor` 驗證環境。
- 在選取設定時，Antigravity 視為單一代理；而在實際執行投影時，則會展開為 `agy-cli`、`agy-ide` 與 `agy-gui` 三個獨立目標。
- MCP 設定固定同步更新五個主機，不受個別代理之投影選取狀態影響。

## 疑難排解入口

若遇到異常狀況，可先執行 `ccync doctor` 診斷具體問題點：

- 欲確認同步將異動哪些檔案：執行 `ccync sync --dry-run`。
- 匯入的項目若缺少來源記錄：執行 `ccync repair`。
- 更新或移除後若懷疑殘留過期快取：先執行 `ccync cleanup --dry-run` 檢視清理範圍。

若無法明確證實某個同名項目由 ccync 管理，系統將一律保留該項目並提出回報，絕不逕行推測刪除。

目前狀態：一般 `ccync sync` 會透過各目標的直接表面發布 provider 元件。合併的 `ccync` 表面會在同一次 sync 中、直接發布成功後退役。替換失敗的目標會保留合併寫入器作為備援。取消選取某目標的 sync 會移除該目標已記錄的直接複本。以下標籤與外部 selector 處理方式適用於這些直接表面。目前狀態詳見[提供者身分、載入與復原](architecture.zh-Hant.md#provider-身分載入與復原)。

`ccync list` 會分別顯示原始提供者與管理者，例如 `Provider: demo | managed by ccync | loader: demo@ccync-marketplace | ownership: Managed | validation: LoaderNotRun`。若 sync 回報 `Foreign native selector`，請使用該產品的原生管理工具移除精確 selector，再依序執行 `ccync sync --dry-run` 與 `ccync sync --yes`。Sync 不會移除外部註冊，也不會啟用 disabled 外掛。disabled、broken 或 unknown 狀態應保留以供診斷。逐目標同步與磁碟證據不能證明執行中的代理已載入元件。復原指引與證據限制請見[設定與診斷](setup.zh-Hant.md#外掛提供者與管理標籤)。

## 文件地圖

| 文件 | 內容 |
| --- | --- |
| [安全性](SECURITY.zh-Hant.md) | 回報方式、信任邊界與安全不變條件。 |
| [架構](architecture.zh-Hant.md) | arc42 系統架構、projection 與 ADR 索引。 |
| [設定與診斷](setup.zh-Hant.md) | 初始化、同步、診斷、解除安裝、本機狀態、備份與還原。 |
| [外掛搜尋與管理](plugins.zh-Hant.md) | 搜尋、修復來源、新增與移除外掛，以及團隊清單。 |
| [指令參考](commands.zh-Hant.md) | 19 個公開指令，以及更新、升級、固定版本與清理快取的限制。 |
| [MCP](mcp.zh-Hant.md) | 選擇路由、預覽變更並診斷 MCP 嘗試。 |
| [既有項目匯入與核對](adoption.zh-Hant.md) | 從五種來源之一匯入項目、核對來源，以及更新 Graphify 技能。 |
| [貢獻指南](contributing.zh-Hant.md) | 建置、測試與變更紀律。 |
| [命名](naming.zh-Hant.md) | 專案術語與七個發布目標的識別值。 |
| [已知待辦](deferred-work.zh-Hant.md) | 已知但尚未完成的工程工作。 |
| [國際化指南](i18n-guide.zh-Hant.md) | 翻譯路徑、metadata 與配對規則。 |
