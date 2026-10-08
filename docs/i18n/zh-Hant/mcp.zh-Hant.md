---
type: reference
title: MCP 定義、機密值與復原
description: MCP author definitions、placeholder 解析、五個 host writers 與逐主機 ownership recovery。
tags: [mcp, secrets, recovery]
status: stable
locale: zh-Hant
translation_key: mcp
---

# MCP 定義、機密值與復原

## 定義與解析

ccync 以 `~/.ccync/build/_mcpServers/` 目錄中的定義為權威來源，據以管理各 MCP 伺服器。定義可包含 `command`、`args`、`url`、`env`、`headers` 以及受支援的 `cwd` 工作目錄。內建變數解析器支援 `${VAR}`、`${VAR:-default}`、`PLUGIN_ROOT`、`PLUGIN_DATA` 與受控的 Claude 別名語法。若特定伺服器缺少必要的環境變數，ccync 會明確指出受影響項目並予以跳過，其餘伺服器以及非 MCP 的元件投影將持續正常執行。

ccync 僅在即將向主機寫入設定時，才即時解析敏感機密值。原始設定檔與鎖定檔狀態嚴禁持久化儲存解析後的明文機密；預寫式復原交易記錄亦僅儲存寫入前後的檔案指紋或「原本不存在」標記。在執行診斷與記錄時，系統絕不輸出 token、標頭敏感數值或完整的含密指令字串。

## 五個 MCP 主機 (Hosts)

ccync 固定維護並同步更新 Claude、Codex、Copilot、OpenCode 與 Antigravity 五個 MCP 主機設定，不受個別代理選取狀態影響。在寫入各主機設定檔時，ccync 僅合併由自身管轄的伺服器條目；原生設定中未知的頂層欄位，以及使用者自行配置的伺服器，均會完整保留。若既有設定檔格式損毀或無法正確讀取，ccync 會立即中止該主機的寫入流程，絕不以空白設定暴力覆寫原檔案。

## 所有權歸屬與中斷復原

在正式寫入各主機設定檔前，ccync 會先行建立預寫式交易記錄；在各主機完成寫入後，系統會立即重新讀取檔案並校驗其最新指紋。唯有當所有參與交易的主機均能證實寫入結果時，才會將所有權歸屬提交寫入 `projected-state.json`，並安全清除交易記錄。

若投影流程因故意外中斷，在下次啟動時系統會逐一檢驗各主機狀態：檔案若符合寫入後的目標指紋，則承認該主機之所有權歸屬；檔案若仍維持寫入前的初始指紋，則保持現狀；若出現無法辨識的第三種指紋或檔案無法讀取，ccync 會停止自動推論並提出警告。在執行移除伺服器操作時，亦僅處理具備逐主機所有權證明由 ccync 管理之項目。
