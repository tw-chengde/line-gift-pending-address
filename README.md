# line-gift-pending-address

列出 LINE 禮物「已付款、收禮人尚未填寫地址」的訂單，並對**下單滿 5 天、滿 8 天**（可調整）的訂單寄 email 提醒送禮人，最後寄一封統整信給自己。對 LINE 是唯讀的（只呼叫訂單清單與訂單詳情），結果以 JSON 印到 stdout。

## 提醒規則

- 「滿幾天」以台北時間的日曆天計算：`今天 − 下單日` 等於 5 或 8 就提醒（`REMINDER_DAYS`）。
- 信中的最後填寫地址日期 = 下單日 + 9 天（`DEADLINE_DAYS`）。
- 收件人是訂單的發票 email；沒有 email 或格式不正確的訂單不會寄，會列在統整信裡。
- 一筆訂單寄一封，內容列出該訂單所有尚未填地址的商品。
- 程式不記錄寄過誰：**同一天執行兩次會寄兩次**；某天沒執行，那天該提醒的訂單會被跳過，可用 `--today=YYYY-MM-DD` 補寄。
- 沒帶 `--send` 時不寄任何信，只在輸出的 `reminders` 預覽今天會寄給誰。

## 本機執行（只能預覽，寄信要在 Cloud Run 上）

```powershell
$env:LINE_GIFT_GATEWAY_TOKEN = "..."
cargo run --release
cargo run --release -- --today=2026-11-13 --max=500
```

| 參數 | 說明 |
| --- | --- |
| `--from` / `--to` | 台北時間日期（YYYY-MM-DD），以訂單成立時間篩選；未指定時查最近 `DEADLINE_DAYS` + 1 天（預設 10 天），有帶 `--today` 時從那一天往回算 |
| `--max` | 最多檢查幾筆已付款訂單（預設 200）。由新到舊檢查，超過上限時較早的訂單會漏掉 |
| `--send` | 真的寄出提醒信與統整信 |
| `--today` | 把「今天」當成指定日期來算滿幾天，預設的查詢範圍也會跟著移到那一天 |
| `--raw` | 額外印出原始 deliveryForm（含個資） |
| `--compact` | 輸出單行 JSON（Cloud Run Job 內自動啟用） |

| 環境變數 | 說明 |
| --- | --- |
| `LINE_GIFT_GATEWAY_TOKEN` | 必填 |
| `LINE_GIFT_ENVIRONMENT` | 設為 `beta` 改打測試環境 |
| `MAIL_SENDER` | `--send` 時必填。用來寄信的 Workspace 信箱 |
| `MAIL_SUMMARY_TO` | 統整信收件人，可用逗號分隔多個；未設定時寄給 `MAIL_SENDER` |
| `MAIL_FROM_NAME` | 寄件人顯示名稱（選填） |
| `MAIL_SERVICE_ACCOUNT` | 已設好全網域委派的服務帳號；未設定時用執行環境的服務帳號 |

## 不改程式就能調整的設定

天數和信件內容都是環境變數，改完下次執行就生效，不用重新部署：
Cloud Run 主控台 → Jobs → `line-gift-pending-address` →「編輯」→「變數與密鑰」。
push 觸發的自動部署不會動到環境變數，不會蓋掉這些值。

| 環境變數 | 預設 | 說明 |
| --- | --- | --- |
| `REMINDER_DAYS` | `5,8` | 下單滿幾天要提醒，逗號分隔，可以一個或多個 |
| `DEADLINE_DAYS` | `9` | 最後填寫地址日期 = 下單日 + 這個天數。提醒天數不可大於它 |
| `MAIL_SUBJECT` | 見下方 | 提醒信主旨（純文字） |
| `MAIL_BODY` | 見下方 | 提醒信內文（HTML，換行用 `<br>`） |

主旨和內文裡可以放這些代碼，寄信時會換成該筆訂單的內容：

| 代碼 | 內容 | 範例 |
| --- | --- | --- |
| `{products}` | 尚未填地址的商品 | 禮盒（紅 / L） ×2、卡片 |
| `{deadline}` | 最後填寫地址日期 | 11/14（六） |
| `{ordered}` | 下單日 | 11/5（四） |
| `{days}` | 今天是下單後第幾天 | 8 |
| `{sender_name}` | 送禮人（發票姓名） | |
| `{recipients}` | 收禮人 | |
| `{order_id}` | 訂單編號 | |

預設值：

```text
MAIL_SUBJECT=【LINE 禮物】收件地址填寫提醒（最後日期 {deadline}）
MAIL_BODY=您贈送給朋友的 <strong>{products}</strong>，<br><span style="color:#d93025;"><strong>{deadline}為最後填寫地址日期</strong></span><br>再請朋友留意，<br>避免訂單取消～<br><br>謝謝您❤️<br>祝順心。
```

- 代碼打錯（例如 `{deadine}`）或天數不合理時，Job 會在寄任何信之前就失敗，錯誤訊息會指出是哪個變數。
- 統整信的格式是固定的，不受這些設定影響。
- 改完想先確認，可以用下方「在雲端預覽而不寄信」的方式執行一次；輸出的 `reminders.config` 會列出實際生效的設定。
- 值裡有逗號時，用 gcloud 設定要換分隔符號，例如 `--update-env-vars "^@^REMINDER_DAYS=3,6,8"`；在主控台填則沒有這個問題。

## 寄信的運作方式

用 GCP 服務帳號的「全網域委派」代表 `MAIL_SENDER` 呼叫 Gmail API，不需要密碼或金鑰檔：
服務帳號呼叫 IAM `signJwt` 幫自己簽一張 JWT，再向 Google 換成只有 `gmail.send` 權限的權杖。
GCP 專案和 Workspace 不必屬於同一個帳號或機構。

### 一次性設定

```powershell
$project = "你的專案ID"
$sa = "line-gift-pending-address-job@$project.iam.gserviceaccount.com"

gcloud services enable gmail.googleapis.com iamcredentials.googleapis.com

# Job 專用的服務帳號
gcloud iam service-accounts create line-gift-pending-address-job --display-name="LINE gift pending address job"

# 讓它能幫自己簽 JWT
gcloud iam service-accounts add-iam-policy-binding $sa `
  --member="serviceAccount:$sa" --role="roles/iam.serviceAccountTokenCreator"

# 讓它能讀 LINE token
gcloud secrets add-iam-policy-binding line-gift-gateway-token `
  --member="serviceAccount:$sa" --role="roles/secretmanager.secretAccessor"

# 印出一串數字（用戶端 ID），下一步要用
gcloud iam service-accounts describe $sa --format="value(uniqueId)"
```

接著請 **Workspace 超級管理員**到 [admin.google.com](https://admin.google.com)：
「安全性」→「存取權與資料控管」→「API 控制項」→「管理全網域委派」→「新增」

- 用戶端 ID：上面印出的那串數字
- OAuth 範圍：`https://www.googleapis.com/auth/gmail.send`

設定生效可能要等幾分鐘。這個授權讓服務帳號能以該網域內任何使用者的名義寄信，所以服務帳號請只給這個 Job 用。

## 部署成 Cloud Run Job

Job 由 Cloud Build 建立與更新：push 到 `main` 時，觸發條件依 `cloudbuild.yaml` 建置映像檔、推到 Artifact Registry，再執行 `gcloud run jobs deploy`。Job 不存在就建立，存在就更新，所以不需要手動做第一次部署。

- Job 的服務帳戶、secret、`--send`、重試次數、逾時都寫在 `cloudbuild.yaml`，要改就改檔案後 push。
- 環境變數不在 `cloudbuild.yaml` 裡，在 Cloud Run 主控台設定，每次部署都會保留。
- 部署只建立或更新 Job、不執行，所以部署本身不會寄信。
- `cloudbuild.yaml` 裡的 `--max-retries=0` 請保留：重試會讓已經寄出的信再寄一次。

建置在 GCP 內執行，不需要金鑰檔，GitHub 端也不用設任何變數。

### 1. LINE token 放 Secret Manager（只需一次）

主控台：「安全性」→「Secret Manager」→「建立密鑰」，名稱填 `line-gift-gateway-token`，密鑰值貼上 token。或用指令：

```powershell
Set-Content -Path "$env:TEMP\lg-token.txt" -Value "你的 token" -NoNewline -Encoding ascii
gcloud secrets create line-gift-gateway-token --data-file="$env:TEMP\lg-token.txt"
Remove-Item "$env:TEMP\lg-token.txt"
```

第一次 push 之前，這個密鑰和上面「一次性設定」的服務帳戶 `line-gift-pending-address-job`（含讀取密鑰的權限）都要先建好，否則部署那一步會失敗。

### 2. 設定 Cloud Build（只需一次）

都在 GCP 主控台完成：

1. **啟用 API**：「API 和服務」→「程式庫」，啟用 Cloud Build API、Artifact Registry API 和 Cloud Run Admin API。
2. **建立放映像檔的地方**：「Artifact Registry」→「建立存放區」，名稱 `line-gift-pending-address`（和 Job 同名）、格式 Docker、區域 `asia-east1`。
3. **建立部署專用的服務帳戶**：「IAM 與管理」→「服務帳戶」→「建立服務帳戶」，ID 填 `cloud-build-deployer`，建立時不要加任何角色。
4. **給它四個權限**（主體都填 `cloud-build-deployer@你的專案ID.iam.gserviceaccount.com`）：

   | 在哪裡授權 | 角色 | 用途 |
   | --- | --- | --- |
   | Artifact Registry 存放區清單勾選 `line-gift-pending-address` → 右側面板「權限」 | Artifact Registry 寫入者 | 推映像檔 |
   | 「IAM」頁面（專案層級） | Cloud Run 開發人員 | 建立與更新 Job |
   | 服務帳戶 `line-gift-pending-address-job` →「具有存取權的主體」 | 服務帳戶使用者 | 指定 Job 的執行身分 |
   | 「IAM」頁面（專案層級） | 記錄寫入者 | 寫建置紀錄 |

5. **連結 GitHub repo**：「Cloud Build」→「觸發條件」→「連結存放區」，選 GitHub，依畫面安裝 Cloud Build 的 GitHub App 並選這個 repo。repo 在組織底下時，安裝 App 需要組織擁有者核准。
6. **建立觸發條件**：「建立觸發條件」，設定如下：

   | 欄位 | 值 |
   | --- | --- |
   | 事件 | 推送至分支 |
   | 分支 | `^main$` |
   | 忽略的檔案篩選器 | `**/*.md`（只改 `.md` 檔的 push 不觸發） |
   | 設定 | Cloud Build 設定檔，位置 `/cloudbuild.yaml` |
   | 服務帳戶 | `cloud-build-deployer` |

之後 `git push` 就會自動部署，進度在「Cloud Build」→「記錄」；也可以在觸發條件清單按「執行」手動重新部署。

短時間內連續 push 時，各次建置會同時進行，Job 最後用的是最晚完成的那一版，不一定是最新的 commit；遇到時手動再執行一次觸發條件即可。

### 3. 設定環境變數（第一次部署後）

第一次部署建出來的 Job 還沒有寄信用的環境變數。到 Cloud Run 主控台 → Jobs → `line-gift-pending-address` →「編輯」→「變數與密鑰」新增：

| 環境變數 | 是否必填 |
| --- | --- |
| `MAIL_SENDER` | 必填 |
| `MAIL_FROM_NAME` | 選填 |
| `MAIL_SUMMARY_TO` | 選填 |
| `MAIL_SERVICE_ACCOUNT` | 寄信用的服務帳號和 Job 的執行身分不同時才需要 |

`MAIL_SENDER` 設定之前執行 Job 會直接失敗，不會寄出任何信，所以排程請等這一步完成後再建立。

### 4. 執行

主控台：Job 頁面按「執行」。或用指令：

```powershell
gcloud run jobs execute line-gift-pending-address --region asia-east1 --wait
```

想先在雲端預覽而不寄信，執行時把參數覆寫掉即可。主控台是「執行」旁的選單 →「使用覆寫值執行」，容器引數填 `--max=200`；指令是：

```powershell
gcloud run jobs execute line-gift-pending-address --region asia-east1 --args="--max=200" --wait
```

### 每日 08:00 排程

```powershell
gcloud services enable cloudscheduler.googleapis.com
gcloud iam service-accounts create line-gift-scheduler --display-name="LINE gift job scheduler"
gcloud run jobs add-iam-policy-binding line-gift-pending-address `
  --region asia-east1 `
  --member="serviceAccount:line-gift-scheduler@你的專案ID.iam.gserviceaccount.com" `
  --role="roles/run.invoker"

gcloud scheduler jobs create http line-gift-pending-address-daily `
  --location asia-east1 `
  --schedule "0 8 * * *" `
  --time-zone "Asia/Taipei" `
  --http-method POST `
  --uri "https://run.googleapis.com/v2/projects/你的專案ID/locations/asia-east1/jobs/line-gift-pending-address:run" `
  --oauth-service-account-email "line-gift-scheduler@你的專案ID.iam.gserviceaccount.com"
```

## 輸出與結束狀態

結果以單行 JSON 寫進 Cloud Logging（`jsonPayload`）。`reminders.items` 每筆的 `result`：

| result | 意義 |
| --- | --- |
| `pending` | 預覽（沒帶 `--send`） |
| `sent` | 已寄出 |
| `failed` | 寄送失敗，`note` 有原因 |
| `skipped` | 沒有 email 或格式不正確，未寄 |

有任何一封寄送失敗（含統整信）時，Job 會以失敗狀態結束，但報表仍會先印出。

輸出與統整信都含收禮人姓名與送禮人 email，請留意 log 的存取權限與保存期限。
