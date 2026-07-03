---
source: README.md
lang: zh-Hant
source_commit: eb7a9cfadccf4f0befc926335cc30b602a32c3d9
translated_at: 2026-07-03
status: current
---

[English](../../../README.md) | 繁體中文

# ccync

**一個跨代理 (cross-agent) 的外掛程式、MCP 與技能管理器。** 只要安裝一次外掛程式，就能將其投影至您使用的每一個程式碼輔助代理，包含 Claude Code、GitHub Copilot、Codex、Antigravity、Gemini 以及 OpenCode。

ccync 是廣泛生態系中的管理組件。它負責處理任意的第三方外掛程式、MCP 伺服器與技能，並確保它們在您所有的代理之間保持一致。（工作流程對應的部份由獨立的產品負責維護。）

> **狀態：Alpha 版。** 核心的外掛程式管理功能——包含解析、Git 複製、快取、固定版本 (pinning) 以及跨代理的採用與協調 (adopt/reconcile)——已經完全可用。將已安裝的外掛程式投影至每個代理的技能、指令、代理與 MCP 介面已經實作並正式出貨。

## 安裝

### macOS / Linux

```sh
curl -fsSL https://raw.githubusercontent.com/monkey1wizard/ccync/main/packaging/install.sh | bash
```

### Windows (PowerShell)

```powershell
irm https://raw.githubusercontent.com/monkey1wizard/ccync/main/packaging/install.ps1 | iex
```

這兩個腳本都會使用 `checksums.txt` 來驗證二進位檔（SHA-256 為強制驗證，cosign 驗證為盡力而為）。
二進位檔會被安裝至 macOS/Linux 的 `~/.local/bin`，或是 Windows 的 `%USERPROFILE%\.local\bin`。

**winget (Windows):** `winget install Monkey1Wizard.ccync`

**Homebrew (macOS/Linux):**

```sh
brew tap monkey1wizard/tap
brew install ccync
```

### 從原始碼建置

```sh
cargo install --git https://github.com/monkey1wizard/ccync
```

這將會直接安裝至 `~/.cargo/bin/ccync`，無需手動複製原始碼。

如果您需要原始碼樹（例如用於開發）：

```sh
git clone https://github.com/monkey1wizard/ccync.git
cd ccync
cargo build --release        # 產生 target/release/ccync
```

接著，將建置好的二進位檔加入您的 `PATH`：

- **macOS / Linux:** `cp target/release/ccync ~/.local/bin/`
- **Windows:** 複製 `target\release\ccync.exe` 到您的 `PATH` 環境變數所包含的目錄。

驗證安裝：

```sh
ccync --version    # 輸出：ccync x.y.z
```

## 快速入門

```sh
# 1. 初始設定：選擇一個主代理 (master agent) 並採用其外掛程式與 MCP 設定。
ccync init claude        # 跳過 master 提示；顯示執行環境多選，然後立即投影。

# 2. 將所有項目投影至所有已選擇的代理。
ccync sync

# 3. 新增第三方外掛程式。接受 Git URL、本機路徑、壓縮檔或 Catalog ID。
ccync add https://github.com/<owner>/<plugin>   # Git URL
ccync add /path/to/plugin                       # 本機路徑
ccync add plugin-v1.tar.gz                      # 壓縮檔 (.zip / .tar.gz)
ccync add my-catalog-plugin                     # 獨立的 Catalog ID

# 4. 列出所有已安裝的個人外掛程式。
ccync list
```

> **首次執行：** `ccync init` 會引導您完成互動式主代理選擇與執行環境多選（預設：`claude`、`codex`、`copilot`），然後立即投影至已選代理——無需另外確認步驟。在非互動式終端機中，請明確提供 master（例如 `ccync init claude`）；執行環境預設值會自動套用。

ccync 自身的狀態（設定、lockfile、快取、規範渲染）維護於 `~/.ccync/`（一個隱藏的本機專屬目錄）。ccync 同時也會將該狀態**投影**至每個已選代理的即時設定介面，範圍在 `~/.ccync/` 之外——例如 `~/.claude/skills/...`、`~/.claude.json`、`~/.codex/config.toml`、`~/.copilot/mcp-config.json`，以及其他受支援代理的對應位置——這正是「安裝一次，投影至所有代理」的核心意義。

## 指令列表

| 指令 | 說明 |
| --- | --- |
| `ccync init [<master>]` | 執行初始設定，透過選擇一個主代理來採用它的外掛程式與 MCP 設定。 |
| `ccync sync [--dry-run] [--yes]` | 投影引擎。負責解析 Catalog、渲染它，並將技能、指令、代理與 MCP 設定投影至每一個代理。使用 `--yes` 可跳過首次執行的確認提示。 |
| `ccync add <source> [--no-sync]` | 從任何支援的來源（Git URL、本機路徑、`.zip`/`.tar.gz` 壓縮檔或 Catalog ID）新增個人外掛程式，並自動進行同步。 |
| `ccync remove <id>` | 移除個人外掛程式，並自動進行同步。 |
| `ccync list [--upgrade-available]` | 列出已安裝的個人外掛程式;加 `--upgrade-available` 則為唯讀「可更新」檢查,印 `current → latest`,退出碼 `3`(有可更新)/`0`(無)/`1`(錯誤)。 |
| `ccync show <id>` | 印單一外掛完整詳情(source、strategy、釘住的 sha、是否 held、元件數)。對齊 `brew info` / `winget show`。 |
| `ccync cleanup [--dry-run]` | 剪除 `upgrade` 留下的孤兒 cache 目錄(對齊 `brew cleanup`)。fail-closed,絕不誤刪 live/未決目錄;`--dry-run` 只列。 |
| `ccync pin <id>` · `--remove <id>` · `--list` | 把外掛釘住擋升級(winget 式;狀態存 `held` 欄 —— `held ≠ pinned sha`,與記錄 commit sha 的 `pin`/`pinnedSha` 欄不同物)。 |
| `ccync doctor` | 執行唯讀的管理健康狀態檢查。 |
| `ccync backup` / `ccync restore` | 匯出或匯入本機狀態。 |
| `ccync uninstall` | 撤回 ccync 的投影（live MCP／marketplace／skill·command·agent surface）並移除 `~/.ccync/` 衍生狀態，保留 `config.json` 與 `plugins.json`，且不編輯 PATH。詳見 [manual](../../manual.md#ccync-uninstall)。 |
| `ccync update [--check]` | 自我更新 `ccync` 二進位檔（對齊 Homebrew `brew update` 語義）。僅在 curl/irm 安裝時自我替換；在 Homebrew/winget/cargo 安裝下會拒絕並導引至對應管道。`--check` 只回報版本、不寫入。 |
| `ccync upgrade [<plugin>] [--dry-run]` | 將已安裝的 Git 來源外掛升級至最新 commit（對齊 Homebrew `brew upgrade` 語義），完成後重新投影。預設全部升級；`--dry-run` 只顯示差異、不寫入。已 held 的外掛會被跳過。 |

執行 `ccync --help` 以查看完整的指令介面。

## 運作原理

- **Catalog** (`plugins/catalog.json`)：精選的可安裝外掛程式與設定檔 (profiles) 集合。
- **解析 (Resolution)**（在 `ccync sync` 期間執行）：合併 Catalog、機器設定與個人 Catalog，以產生 `~/.ccync/build/lock.json`。
- **通用安裝 (Universal Install)**：`ccync add <source>` 接受四種來源類型——Git URL、本機路徑、壓縮檔 (`.zip` / `.tar.gz`) 與獨立的 Catalog ID——使用單一指令與統一的抓取管道。壓縮檔透過其 SHA-256 前綴來固定版本，而 Catalog ID 在抓取前會被解析為其底層來源。所有來源都會儲存於 `~/.ccync/cache/<id>@<sha-or-hash>/`。
- **規範根目錄 (Canonical Root)**：`render_canonical_root` 會將每個受管理外掛程式的 `skills/`、`commands/`、`agents/` 與 `hooks/` 子目錄複製到 `~/.ccync/build/render/` 中，並合併 `.mcp.json` 項目。在每次重新渲染期間（包含執行 `ccync remove` 之後），會先清除過時的元件目錄。
- **Hooks**：提供 `hooks/hooks.json` 的外掛程式會將其 hooks 投影至規範根目錄，Claude 會透過其外掛根目錄機制自動載入它們。由於非 Claude 代理（Codex、Gemini CLI、OpenCode）缺乏 CC-plugin 的 hook 介面，因此 hooks 對它們來說是不適用的（而非遺失）。
- **投影 (Projection)**（在 `ccync sync` 期間執行）：`projection` 引擎將每個外掛程式的技能、指令、代理與 MCP 設定寫入每個已選擇代理的原生設定格式中。
- **首次執行閘門 (First-Run Gate)**：在尚未進行過投影的新機器上，`ccync sync`、`ccync add` 與 `ccync remove` 共用此閘門——它們都會列出預計修改的即時代理設定檔，並在寫入前要求確認（`remove` 若要在非互動式環境下跳過確認，同樣需要 `--yes`，與 `sync`/`add` 相同）。請使用 `--yes` 標記或設定 `CCYNC_ASSUME_YES=1` 環境變數來進行非互動式執行。`ccync init` 完全略過此閘門——它已透過自己的執行環境多選 UI 取得用戶意圖，投影前不再另行確認。`ccync upgrade` 仍會執行此閘門，但一律傳入 `assume_yes=true`，因此永遠自動確認、不會詢問。對 `add` 而言，即時介面的渲染／投影嚴格發生在閘門通過**之後**——若閘門被拒絕，catalog/cache/lockfile 的寫入仍會保留，但不會產生任何即時介面寫入。

關於 Crate 層次結構的詳細資訊，請參考 [`docs/architecture.md`](../../architecture.md)。關於完整的指令參考，請參閱 [`docs/manual.md`](../../manual.md)。

## 平台注意事項

| | macOS / Linux | Windows |
| --- | --- | --- |
| Home 目錄環境變數 | `HOME` | `USERPROFILE` |
| ccync Home | `~/.ccync` | `%USERPROFILE%\.ccync` |
| 二進位檔 | `ccync` | `ccync.exe` |
| PATH 安裝位置 | `~/.local/bin` | `%USERPROFILE%\.local\bin`（或 `PATH` 中的任一目錄） |

`~/.ccync` 是一個隱藏目錄。您可以在任何作業系統上使用 `cd ~/.ccync` 進入該目錄，或在您的檔案管理員中啟用「顯示隱藏檔案」。

## 疑難排解

- **`ccync: command not found`** — 二進位檔不在您的 `PATH` 中。請查閱[安裝](#安裝)章節，或使用絕對路徑執行它（例如 `./target/release/ccync`）。
- **`Failed to parse <file>: ...`** — ccync 的某個 JSON 狀態檔（例如 `~/.ccync/plugins.json` 或 `~/.ccync/build/lock.json`）格式錯誤。請修正或移除該檔案；`~/.ccync/build/` 可透過 `ccync sync` 完全重建，但 `~/.ccync/plugins.json` 屬於本機專屬的輸入狀態——若您有 `ccync backup` 備份可還原，否則需重新 `add` 您的個人外掛程式。
- **外掛程式尚未出現在代理中** — 先確認該外掛是否已針對該代理的執行環境啟用（`ccync list` 可查看已安裝內容），然後執行 `ccync sync` 重新投影。若仍未出現，執行 `ccync doctor` 檢查規範根目錄是否過時或缺失。
- **`ccync doctor` 回報關於規範根目錄的錯誤** — 這代表真實存在過時或完整性問題（例如規範根目錄相對於 lockfile 已過時）。執行 `ccync sync` 重建即可。
- **在 `add` 期間發生 Git 錯誤** — ccync 依賴系統的 `git`。請確保已安裝 `git` 且該儲存庫 URL 可被存取。

## 文件

此 README 作為進入點（安裝 → 第一個外掛程式 → 同步）。欲了解進一步細節：

### 使用 ccync

- [使用手冊](../../manual.md) — 每個指令與標記的完整文件。

### 開發 ccync

- [架構](../../architecture.md) — Crate 層次結構與資料流。
- [維護者指南](../../devguide.md) — 內部機制、投影引擎機制與狀態拓撲。
- [發行架構](../../release.md) — 建置、簽署與發佈的管道（包含 cosign 無金鑰、winget/Homebrew 與提交閘門）。
- [命名規範](../../naming.md) — 保留術語與 8 個規範的 runtime keys。
- [貢獻指南](../../contributing.md) — 建置說明、測試與程式碼慣例。

## 路線圖

- **Windows 實機驗證** — 在乾淨的 Windows 環境上驗證全新安裝與同步。

## 授權條款

MIT.
