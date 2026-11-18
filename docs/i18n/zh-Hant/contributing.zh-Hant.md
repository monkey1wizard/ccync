---
type: guide
title: ccync 貢獻指南
description: Workspace 建置、測試、文件同步與安全變更的維護者工作流程。
tags: [contributing, testing, rust]
status: stable
locale: zh-Hant
translation_key: contributing
---

# ccync 貢獻指南

ccync 為由六個 crate 組成的 Rust workspace。進行程式碼修改時，請先釐清負責該業務邏輯的專屬 crate，嚴格防範底層的 `ccync-foundation` 產生對上層 crate 的逆向依賴。

| Crate | 責任 |
| --- | --- |
| `ccync-foundation` | 路徑、設定與共用資料模型。 |
| `mcp` | MCP 合併、寫入與復原。 |
| `projection` | 共用輸出與各代理的投影。 |
| `ccync-engine` | 來源清單、匯入、安裝與指令型別。 |
| `setup` | 健康檢查。 |
| `ccync-cli` | 執行檔入口與指令處理。 |

## 本機驗證

```text
cargo test --workspace
cargo clippy --workspace
```

interpreter-pip 回歸測試需要一個含 pip 的獨立 Python 環境。請將 `uv` 加入 `PATH`，讓測試以 `uv venv --seed` 建立環境；或將 `CCYNC_RECORDED_PYTHON` 設為含 pip 的 Python 執行檔。兩者都沒有時，測試會失敗。託管 CI 的每個測試工作都會安裝 `uv`。

開發過程中建議先執行與變更直接相關的單元測試，再執行整體 workspace 的完整驗證。在編輯 Rust 程式碼前，請務必先研讀儲存庫的 Rust 開發慣例。程式碼格式化請僅限於本次異動之檔案，避免引入無關的 diff 雜訊。若變更涉及發行管線，亦須於相應平台上執行打包校驗。任何未實際執行的驗證項目必須誠實標註為 `NotRun`，絕不可視為驗證合格。

Hosted CI 會在 Ubuntu 與 Windows 執行完整 workspace 測試，不依名稱略過任何測試，也會在匯出的公開快照中執行。推送至 `main` 與 pull request 時，也會執行 clippy 與打包探測。

## 安全與測試紀律

在向各代理寫入實際生效的設定時，必須保證使用者原有內容不受破壞，並全程以原子操作完成。若新增檔案刪除邏輯，須先行補齊所有權證明校驗與同名衝突防禦測試；若擴充 MCP 欄位，須測試並驗證主機原生設定中的未知欄位是否被妥善保留，而外掛端傳入之未知欄位則應提出警告並予以排除。同時，亦須驗證機密值是否受到隱蔽保護，以及流程中斷後是否能正確復原。

在處理外部來源時，應針對目錄跳脫、封存檔解壓縮邊界以及失敗後的回復性撰寫嚴格測試。程式內部對於可預期的錯誤狀況應全面使用 `Result` 型別處理，僅在遭遇無法恢復的嚴重非預期故障時，才允許觸發 panic。

## 文件與提交規範

### 發布變更

發布權限由私有 GitHub 控制，GitLab 作為停用 writer 的備份。只有在具備 workflow run 與 ref 證據時，才能回報 hosted 發布成功。不得新增 GitLab 發布觸發器或手動 GitLab fallback 指示。私有 runbook 必須保留發布邊界、明確 ref 政策與 S/P/D 復原責任。穩定版須在同一來源 commit 具有相符的 changelog 段落與 base version metadata。RC tag 可使用裸 `-rc` 或編號 `-rcN`，crate 版本維持 base version；Homebrew 與 WinGet 步驟仍僅適用穩定版。

當公開指令或系統行為有所異動時，請更新負責該主題的主題文件，並同步維護 `docs/structure/structure-map.ndjson` 中受影響的結構對應記錄，避免在多份文件中冗餘重複貼上相同規則。提交 commit 時請遵循包含具體 scope 的 Conventional Commit 規範，並妥善保留既有的使用者變更。若欲更動受保護路徑，必須事先完成經正式記錄的架構審查。
