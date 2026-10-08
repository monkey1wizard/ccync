---
type: guide
title: 設定、生命週期與診斷
description: ccync 初始化、同步、診斷、machine state、備份、還原與解除安裝的操作契約。
tags: [setup, lifecycle, doctor, backup]
status: stable
locale: zh-Hant
translation_key: setup
---

# 設定、生命週期與診斷

## 初始化

`ccync init [<master>]` 允許操作者選擇 `claude` 或 `codex` 作為主要代理。ccync 會自動匯入該主要代理中既有的外掛、獨立技能與 MCP 伺服器定義，將配置記錄於 `~/.ccync/config.json`，隨後提示選取預計投影之目標代理。在具備互動式終端的環境中可直接省略 `<master>`，於非互動式腳本環境則必須明確指定主要代理代碼。

請特別注意：初始化流程在匯入完成後可能直接引導進入首次投影，並要求確認即將寫入的代理檔案與 MCP 設定。切勿將初始化完成後才執行的 `sync --dry-run` 誤當成初始化寫入前的預覽機制。

在完成初始化之前，`sync`、`remove`、一般 `add` 以及非 dry-run 的 `upgrade` 均不會異動任何本機資料，而是主動提示操作者先執行 `ccync init <master>`。

## 同步與首次覆寫閘門

`ccync sync [--dry-run] [--yes]` 會讀取來源清單、快取與鎖定檔，建置出共用的 canonical render，隨後投影至所選目標。五個固定 MCP 主機則由獨立管道排程更新，不受個別代理之選取狀態影響。

在首次執行實體寫入前，ccync 會分別列出即將修改的各代理檔案與 MCP 主機配置。強烈建議先以 `--dry-run` 檢視預覽；若操作者拒絕首次確認，ccync 絕不改動各代理的現有設定檔，但先前已完成的快取下載等非目標寫入作業可能仍會保留。傳入 `--yes` 或設定環境變數 `CCYNC_ASSUME_YES=1` 可略過互動式確認。

若外掛來源快取遺失或無法下載，應先修復來源與快取狀態，再經由 `ccync sync --dry-run` 確認結果。部分同步或升級路徑可能將缺失的來源誤判為無元件外掛，隨後的投影操作恐有誤刪既有檔案之虞。

## 使用者範圍指示

若建立了 `~/.ccync/AGENTS.md`，ccync 會將其內容原封不動地注入各目標檔案的 `<!-- ccync:managed:begin -->` 與 `<!-- ccync:managed:end -->` 標記區塊之內，標記區塊以外的使用者自訂內容將完整保留。三個 Antigravity 目標共用 `~/.gemini/AGENTS.md` 檔案，每次同步僅寫入一次。若目標檔案之標記缺漏或損壞，ccync 會明確指出該檔案並跳過處理，絕不會暴力覆寫整份檔案。

## 診斷與解除安裝

`ccync doctor [--dry-run] [--release-gate]` 用於檢查整體設定、本機狀態、輸出內容、代理檔案健康度與發行條件。所有警告與錯誤均會標示具體受影響的項目。目前 `--dry-run` 與 `--release-gate` 旗標不會異動實際檢查項目，切勿將其視為額外的獨立發行驗證程序。

`ccync uninstall` 僅撤回具備明確所有權證明由 ccync 管理的 MCP、技能、指令、代理以及使用者指示區塊。此操作會移除工具記錄與衍生狀態，包括 `tools.json`、工具快照、操作日誌及安裝收據。此操作也會清除衍生的 `cache/` 與 `build/` 資料，但會保留 `config.json`、`plugins.json` 與各代理的原生設定目錄。持續使用的協調鎖定檔 `build/lifecycle.lock` 會保留，讓後續命令沿用同一個鎖定檔。該指令不會刪除 `ccync` 執行檔本身，執行檔請依照當初採用的安裝管道進行移除。

## 結束代碼 (Exit Codes)

結束代碼 `0` 表示執行成功，`1` 表示操作失敗，`2` 表示已辨識的指令尚未接線，`3` 表示指定的唯讀查詢偵測到可用更新，`64` 表示指令用法錯誤。`update --check` 僅查詢版本；即使安裝來源不支援自我更新，查詢成功仍回傳 `0`。實際執行 `update` 而無法自動更新時，則回傳 `1`。完整分類見 [ADR 05](../../adr/05-machine-lifecycle-adoption-and-cli-contract.md)。

## 本機機器狀態 (Machine State)

`~/.ccync/` 目錄包含機器專屬輸入、來源快取與產生的狀態。`config.json`、`plugins.json` 與選用的 `AGENTS.md` 是輸入。`tools.json` 也是機器專屬輸入，解除安裝時會移除。工具狀態包含已提交快照、位於 `build/lifecycle.lock` 的持續性生命週期鎖定檔，以及安裝收據。操作日誌也屬於工具狀態。其他產生的狀態包括 `build/lock.json`、`render/`、`_mcpServers/`、MCP 所有權記錄、寫入前交易日誌與啟用標記。快取與產生的項目可依輸入重建。已提交的工具快照只能藉由 `ccync tool refresh <id>` 重新產生。

各代理實際讀取的設定檔位於 `~/.ccync/` 之外，兩者不可混為一談。ccync 僅管轄其中具備所有權證明之特定項目或標記區塊，並不擁有整份原生設定檔。

## 備份與還原

`ccync backup [--output <dir>]` 用於建立可跨機器轉移的設定備份封包，內容包含基本設定、個人外掛來源、重建已匯入項目所需的快照以及使用者全域指示。備份不包含 `tools.json`、工具快照、操作日誌、`build/lifecycle.lock` 與安裝收據。這些工具識別資料只適用於原機器。v1 尚未提供轉移工具記錄的命令。備份流程預設會過濾程式能辨識的機密值，並將 MCP 環境變數與標頭的敏感數值替換為 `${CCYNC_SECRET_<SERVER>_<KEY>}` 具名佔位符。`--strip-secrets` 旗標僅為相容舊版指令而保留，預設備份本就會執行脫敏處理；若內容包含無法安全脫敏的明文機密值，ccync 會直接拒絕產生備份。

`ccync restore --from <dir> [--dry-run]` 會先完整驗證備份檔案結構，隨後安全合併使用者輸入。`--dry-run` 模式僅比對並指出哪些項目已一致、有差異或無法讀取，不執行任何寫入。還原操作不會直接複製各代理的原生設定檔；還原完成後，操作者須重新提供必要的機密值環境變數、安裝團隊清單或來源，並執行 `sync`。若備份檔來自不同機器，亦須重新驗證其中的外部路徑與 junction 連結有效性。
