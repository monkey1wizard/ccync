---
type: reference
title: MCP 定義、機密值與復原
description: 選擇路由、預覽變更並診斷 MCP 嘗試。
tags: [mcp, secrets, recovery]
status: stable
locale: zh-Hant
translation_key: mcp
---

# MCP 定義、機密值與復原

## 個人清單與路由

編輯 `~/.agents/mcp.json`，管理直接宣告與路由覆寫。必要的根鍵為 `ccyncMcpServers`，另一個選用根鍵為 `hostRoutes`。空清單仍須包含 `"ccyncMcpServers": {}`。若出現第三方根鍵 `mcpServers`、混用根鍵、未知根鍵、無法讀取的輸入，或新舊輸入同時存在，ccync 會保留原檔並在 MCP 寫入前停止，不會自動合併或搬移。若兩者都不存在，直接宣告為空。

若只有舊輸入，預覽會列出實際來源與新目的地。切換時，先將舊檔複製至非作用中的備份位置，再把複本的根鍵 `mcpServers` 改為 `ccyncMcpServers`，保留 `hosts` 與 `hostRoutes`，放到 `~/.agents/mcp.json`，移走作用中的舊檔，最後執行 `ccync sync --dry-run`。保留備份並先解決衝突。

以下個人清單供隔離環境使用。名稱與網址均為虛構。健康伺服器只是對照組，不能證明實際服務健康。

```json
{
  "ccyncMcpServers": {
    "failing": {"command": "node", "args": [], "hosts": ["codex"]},
    "healthy": {"url": "https://mcp.example.com", "type": "http", "hosts": ["claude"]}
  },
  "hostRoutes": []
}
```

`hosts` 是明確的主機集合。新 stdio 伺服器沒有明確路由時為 `unrouted`，不會新增投影。空集合要求撤回具有所有權證明的複本。`enabled: false` 代表停用。若要覆寫 provider 或匯入項目，可在 `hostRoutes` 加入 `{"source":"user","server":"failing","hosts":["codex"]}`，並將 `source` 改為報告中的來源識別。覆寫優先於內嵌 `hosts`，互相衝突的覆寫會遭拒。遠端伺服器沒有明確路由時可使用預設值。選擇多個 stdio 主機時，每個作用中的主機工作階段都可能啟動自己的程序。

五個路由鍵及其原生目的地如下：

| 鍵 | 原生設定 |
| --- | --- |
| `claude` | `~/.claude.json` |
| `codex` | `~/.codex/config.toml` |
| `copilot` | `~/.copilot/mcp-config.json` |
| `opencode` | 平台設定目錄下的 `opencode/opencode.json` |
| `agy` | `~/.gemini/config/mcp_config.json` |

每個主機獨立規劃，不受代理投影選擇影響。來源與外部同名項目均會保留。同名衝突會阻擋該主機並使 sync 失敗，其他安全的主機仍可完成。無法讀取的主機不會取得或失去所有權。匯入原件留在來源檔案，即使其內容不適合複製亦然。受管理的目的地是獨立複本，只有該主機具備已提交證明時才能撤回。

## 可攜宣告與機密值

選擇一種傳輸方式：stdio 使用 `command`，HTTP/SSE 使用 `url`。使用 `args`、`env`、`headers`、`cwd` 等受支援欄位，勿使用 `headersHelper` 等輔助指令。可攜備份會拒絕 URL userinfo、明文機密、私有絕對路徑及無法分類的不透明參數。無害字串若無法安全分類也可能遭拒。以 `${MCP_LOCAL_DIR}` 等具名參照取代機器專屬路徑。機密值使用 `${MCP_TOKEN}`，並透過本機環境或機器機密設定提供。機密參照不得包含內嵌預設值。一般變數可使用支援的預設值，未解析的一般變數與 `missing-secret` 分開回報。

解析後的機密值只限存在於執行程序、機器機密設定、原本已有該值的來源檔，以及格式需要該值的主機檔。宣告、匯出、鎖定檔、所有權記錄、日誌、診斷及備份不得新增解析後機密值。備份會在建立輸出前拒絕不安全的直接輸入、userinfo 與佔位符名稱衝突，並可遮蔽受支援的匯入值。詳見[備份與還原](setup.zh-Hant.md#備份與還原)。

## 預覽、同步與復原

先執行 `ccync sync --dry-run` 檢查每個伺服器與主機的決策，再執行 `ccync sync`。`ccync init <source>` 也會在同意前預覽匯入原件，之後可能直接執行實際投影。預覽與 doctor 只讀。報告會顯示來源、路由原因與啟動形式，包含 `unrouted`、`excluded`、`disabled`、`unsupported`、`missing-secret`、`unresolved-ordinary-variable`、`source-preserved`、`blocked` 及 `failed`。`applied` 表示寫入已驗證，不表示已連線。缺少機密或啟動形式未驗證時，既有受管理複本與證據會保留。Windows 支援表為空時，stdio 投影一律為 `blocked`。本例沒有驗證任何主機版本。後續執行時資格測試計畫負責測量與事件擷取工具。

狀態不變時，第二次 sync 會保留未變更主機檔的位元組，也不會重新載入。日誌記錄寫入前後指紋，不記錄機密值。中斷後，符合寫入後指紋可認列結果，符合寫入前指紋則維持原狀。無法讀取的項目或第三種指紋會停止復原並保留日誌。請勿刪除日誌以強制繼續，應保留來源及主機證據並解決衝突。中斷的還原另有 pending 標記，請使用相同備份重跑 restore。第三種指紋須手動處理。

對先前移除的項目，先依逐主機證據區分可確認缺少、歷史不確定及目前明確選擇。檢查後只重新選取確切的保留項目：先執行 `ccync sync --reselect-mcp SOURCE SERVER HOST --dry-run`，再執行 `ccync sync --reselect-mcp SOURCE SERVER HOST`。其他保留項目不變。證據清單見[復原候選項目](adoption.zh-Hant.md)。實際復原與 P7 套用每次執行都需要另行授權。

### 隔離復原重播

此 Windows fixture 使用虛構的 HTTP 端點與來源標籤 `user`。它自行建立先前記錄，不會還原使用者歷史。先用 `cargo build -p ccync-cli` 建置 debug CLI，再從儲存庫根目錄執行：

```powershell
powershell -NoProfile -File verification/mcp/Invoke-Verification.ps1 -Case replay -Mode Fixture -EvidenceRoot <evidence-root>
```

報告會指向各自獨立的 `en` 與 `zh-Hant` fixture 目錄。每個目錄都包含先前主機檔位元組與 SHA-256 摘要、指令參數、輸出與結束碼、快照、差異及離線檢查表。每次執行先將虛構的 `alpha` 與 `beta` 投影到 Claude 和 Codex，記錄先前狀態，再移除複本以建立保留狀態。先前記錄可確認 `user alpha claude` 與 `user alpha codex` 缺少。未選取的 `beta` 列代表目前選擇，沒有伺服器名稱的數量則屬歷史不確定。

請檢查完整的 `preview` 輸出。fixture 對每個確認項目先以確切三元組加上 `--dry-run` 執行，再移除該參數執行。兩次選取是獨立操作。檢查 `after` 中的主機檔位元組及逐主機 `publications`、`holds`，再比較 `second` 與 `after`。未選取的 `beta` 仍受保留，第二次 sync 不會擴增主機檔。fixture 也會加入外部 `gamma` 衝突，並在 `conflict-diff.json` 記錄非零 sync 結束碼及保留的來源與主機檔位元組。此 fixture 的授權不適用於任何實際操作。

請將 `offline-checklist.json` 與預覽、所有權記錄及任何去敏事件報告一起檢查。版本與實際連線仍未驗證。fixture 通過不表示 Windows 主機取得資格，也不能證明真實程序、連線或復原。stdio 投影以支援表及其確切資格證據為準。

## 診斷重複嘗試

將以下合成且已去敏的內容儲存為 `events.json`。它表示一次使用操作有兩次失敗嘗試，目標程序與視窗數均為零，不記錄 PID。版本 `1.0` 為虛構且未驗證。

```json
{
  "version": 1,
  "run": "run1",
  "events": [
    {
      "seq": 1,
      "time": "2026-01-01T00:00:01Z",
      "host": "codex",
      "hostVersion": "1.0",
      "session": "s1",
      "server": "failing",
      "operation": "op1",
      "attempt": "0",
      "parent": null,
      "kind": "observe-start",
      "source": "native-config",
      "actor": "unknown",
      "result": "ok",
      "digest": null
    },
    {
      "seq": 2,
      "time": "2026-01-01T00:00:02Z",
      "host": "codex",
      "hostVersion": "1.0",
      "session": "s1",
      "server": "failing",
      "operation": "op1",
      "attempt": "1",
      "parent": null,
      "kind": "use",
      "source": "native-config",
      "actor": "agent",
      "result": "ok",
      "digest": null
    },
    {
      "seq": 3,
      "time": "2026-01-01T00:00:03Z",
      "host": "codex",
      "hostVersion": "1.0",
      "session": "s1",
      "server": "failing",
      "operation": "op1",
      "attempt": "1",
      "parent": 2,
      "kind": "dispatch",
      "source": "native-config",
      "actor": "agent",
      "result": "ok",
      "digest": null
    },
    {
      "seq": 4,
      "time": "2026-01-01T00:00:04Z",
      "host": "codex",
      "hostVersion": "1.0",
      "session": "s1",
      "server": "failing",
      "operation": "op1",
      "attempt": "1",
      "parent": 3,
      "kind": "launch-request",
      "source": "native-config",
      "actor": "agent",
      "result": "ok",
      "digest": null
    },
    {
      "seq": 5,
      "time": "2026-01-01T00:00:05Z",
      "host": "codex",
      "hostVersion": "1.0",
      "session": "s1",
      "server": "failing",
      "operation": "op1",
      "attempt": "1",
      "parent": 4,
      "kind": "failure",
      "source": "native-config",
      "actor": "agent",
      "result": "error",
      "digest": null
    },
    {
      "seq": 6,
      "time": "2026-01-01T00:00:06Z",
      "host": "codex",
      "hostVersion": "1.0",
      "session": "s1",
      "server": "failing",
      "operation": "op1",
      "attempt": "2",
      "parent": 5,
      "kind": "dispatch",
      "source": "native-config",
      "actor": "agent",
      "result": "ok",
      "digest": null
    },
    {
      "seq": 7,
      "time": "2026-01-01T00:00:07Z",
      "host": "codex",
      "hostVersion": "1.0",
      "session": "s1",
      "server": "failing",
      "operation": "op1",
      "attempt": "2",
      "parent": 6,
      "kind": "failure",
      "source": "native-config",
      "actor": "agent",
      "result": "error",
      "digest": null
    },
    {
      "seq": 8,
      "time": "2026-01-01T00:00:08Z",
      "host": "codex",
      "hostVersion": "1.0",
      "session": "s1",
      "server": "failing",
      "operation": "op1",
      "attempt": "2",
      "parent": 7,
      "kind": "bound-exceeded",
      "source": "native-config",
      "actor": "agent",
      "result": "ok",
      "digest": null
    },
    {
      "seq": 9,
      "time": "2026-01-01T00:00:09Z",
      "host": "codex",
      "hostVersion": "1.0",
      "session": "s1",
      "server": "failing",
      "operation": "op1",
      "attempt": "0",
      "parent": null,
      "kind": "observe-end",
      "source": "native-config",
      "actor": "unknown",
      "result": "ok",
      "digest": null
    }
  ]
}
```

執行 `ccync doctor --mcp-evidence events.json`。診斷只讀取最大 256 KiB 的檔案。報告應指出 `codex`、`failing`、操作 `op1`、兩次嘗試、由 agent 發起、觸發原因先為 user operation 再為 agent retry，以及 `OutOfBounds`。事件 3–5 與 6–7 分別構成兩條嘗試鏈。健康宣告是獨立對照組，不屬於此事件範圍。使用 `ccync doctor` 查看目前的設定決策。靜態設定無法證明連線或真實重試次數，視窗、程序數或 sync 時間也不能單獨證明原因。缺少父事件或觀察邊界時，歸因或停止狀態仍不確定。事件檔只接受上例的版本化允許欄位，不含原始指令、參數、內容、環境或標頭值。本合成範例無法解釋歷史事件。

離線 P7 檢查時，逐一比對來源、伺服器及主機的預覽、所有權證據與去敏事件報告。沒有測量證據的版本與實際連線應標為未驗證，未選取或不確定的項目維持原狀。後續測試計畫將在隔離環境擷取真實事件並測量啟動與重試界限，這不構成實際復原的授權。
