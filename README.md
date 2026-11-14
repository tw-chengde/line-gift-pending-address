# line-gift-pending-address

列出 LINE 禮物「已付款、收禮人尚未填寫地址」的訂單，並對**下單滿 5 天、滿 8 天**（可調整）的訂單寄 email 提醒送禮人，最後寄一封統整信給自己。對 LINE 是唯讀的（只呼叫訂單清單與訂單詳情），結果以 JSON 印到 stdout。

## 提醒規則

- 「滿幾天」以台北時間的日曆天計算：`今天 − 下單日` 等於 5 或 8 就提醒（`REMINDER_DAYS`）。
- 信中的最後填寫地址日期 = 下單日 + 9 天（`DEADLINE_DAYS`）。
- 收件人是訂單的發票 email；沒有 email 或格式不正確的訂單不會寄，會列在統整信裡。
- 一筆訂單寄一封，內容列出該訂單所有尚未填地址的商品。
- 程式不記錄寄過誰：**同一天執行兩次會寄兩次**；某天沒執行，那天該提醒的訂單會被跳過，可用 `--today=YYYY-MM-DD` 補寄。
- 沒帶 `--send` 時不寄任何信，只在輸出的 `reminders` 預覽今天會寄給誰。

## 本機執行

```powershell
$env:LINE_GIFT_GATEWAY_TOKEN = "..."
cargo run --release
cargo run --release -- --today=2026-11-13 --max=500
```

沒帶 `--send` 只會預覽。本機要真的寄信，需另外設好 `MAIL_SENDER` 和三個 `MAIL_OAUTH_*` 環境變數。

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
| `MAIL_SENDER` | `--send` 時必填。用來寄信的 Gmail 信箱，必須是授權時登入的那個帳號 |
| `MAIL_OAUTH_CLIENT_ID` | `--send` 時必填。OAuth 用戶端 ID |
| `MAIL_OAUTH_CLIENT_SECRET` | `--send` 時必填。OAuth 用戶端密鑰（Cloud Run 上來自 Secret Manager） |
| `MAIL_OAUTH_REFRESH_TOKEN` | `--send` 時必填。更新權杖（Cloud Run 上來自 Secret Manager） |
| `MAIL_SUMMARY_TO` | 統整信收件人，可用逗號分隔多個；未設定時寄給 `MAIL_SENDER` |
| `MAIL_FROM_NAME` | 寄件人顯示名稱（選填） |
| `MAIL_REPLY_TO` | 收件人按「回覆」時的收件地址（選填）；未設定時回到 `MAIL_SENDER` |
| `GEMINI_MODEL` | 選填。設定後用 Vertex AI 濃縮信裡的商品名稱，見「濃縮商品名稱」 |
| `SHORT_NAMES_BUCKET` | 選填。存放商品名稱對照表的 Cloud Storage bucket，見「濃縮商品名稱」 |
| `GOOGLE_CLOUD_PROJECT` | 選填。Vertex AI 用的專案 ID；Cloud Run 內不必設 |
| `GOOGLE_ACCESS_TOKEN` | 本機測試上面兩項用：`gcloud auth print-access-token` 的輸出 |

## 濃縮商品名稱（選用）

商品名稱常帶有促銷字眼（例如「【限時免運】★熱銷破萬★」）。設定 `GEMINI_MODEL` 後，程式會把今天要提醒的訂單裡的商品名稱
交給 Vertex AI 的 Gemini 去掉這些字眼，信裡的 `{products}` 和統整信都改用濃縮後的名稱。沒設定時行為和原本完全一樣。

- 只送商品名稱，不送姓名、email、訂單編號。
- 任何失敗（權限、逾時、回傳筆數對不上）都不影響寄信：問不到的商品用原名，提醒信照寄；統整信會用紅字註明，輸出的 `reminders.shortNameError` 有原因。
- 只濃縮商品名稱；規格（顏色、尺寸等）照原樣接在名稱後面。
- 沒有變短的結果不採用。輸出的 `reminders.shortNames` 列出這次信裡換掉名稱的商品（鍵是 `productId`，`name` 是原名、`short` 是信裡用的名稱）。
- 預覽（沒帶 `--send`）時也會濃縮，正式啟用前請先預覽幾次確認結果。
  那天沒有要提醒的訂單時就沒有商品可濃縮，`shortNames`、`newShortNames`、`shortNameError` 都會是空的，
  這不代表設定正確；請加 `--today=YYYY-MM-DD` 指定一個有提醒的日期來試。
- 模型的輸出不保證每次相同，同一個商品在不同天的信裡名稱可能略有差異；要固定下來請搭配下方的對照表。

啟用步驟（只需一次）：

```powershell
gcloud services enable aiplatform.googleapis.com
gcloud projects add-iam-policy-binding 你的專案ID `
  --member="serviceAccount:line-gift-pending-address-job@你的專案ID.iam.gserviceaccount.com" `
  --role="roles/aiplatform.user"
```

再到 Cloud Run 主控台替 Job 新增環境變數 `GEMINI_MODEL`，值填 Vertex AI 上的型號 ID，例如 `gemini-3.1-flash-lite`。
型號會汰換，下架後濃縮會失敗（信仍以原名寄出），到時換成新的型號 ID 即可；拿掉這個變數就停用。
要啟用的是 Vertex AI API（`aiplatform.googleapis.com`），不是 Discovery Engine API。

### 對照表（選用）

設定 `SHORT_NAMES_BUCKET` 後，濃縮結果會累積在該 bucket 的 `short-names.json`，以 LINE 的商品編號（`productId`）為鍵：

```json
{
  "322533113": { "name": "【限時免運】★熱銷破萬★ 鳳梨酥禮盒 12入", "short": "鳳梨酥禮盒 12入" }
}
```

`short` 是信裡要用的名稱；`name` 是加入當時的原始名稱，只是給人對照用的。

- 表裡已經有的商品直接用 `short`，不再問 Gemini，所以同一個商品每次的名稱都一樣。
- 因為是用商品編號對照，**在賣場改了商品名稱（例如換一批促銷字眼）信裡的名稱也不會變**。
- 表裡沒有的才問 Gemini，結果寫回表裡；沒變短的 `short` 會和 `name` 一樣，之後不會重問。
- 統整信和輸出的 `reminders.newShortNames` 會列出這次新加的項目。不滿意就到 Cloud Storage 主控台下載
  `short-names.json`、改掉 `short` 再上傳覆蓋；手動改的名稱不受「必須比原名短」的限制。想讓某個商品重新濃縮，把那一筆刪掉即可。
- 預覽（沒帶 `--send`）時也會寫入對照表。
- 只設 `SHORT_NAMES_BUCKET`、不設 `GEMINI_MODEL` 也可以：這時只查表，表完全由你手動維護。
- 表讀不到或格式壞掉時，這次全部用原名而且不會寫回，既有的表不會被蓋掉。
- Job 執行期間（讀表之後、寫回之前）表被改動時，Job 不會覆蓋你的修改：這次新加的項目不寫回，
  原因記在 `reminders.shortNameError` 與統整信，信照寄，下次執行會重新濃縮那些商品。

```powershell
gcloud storage buckets create gs://你的bucket名稱 --location=asia-east1 --uniform-bucket-level-access
gcloud storage buckets add-iam-policy-binding gs://你的bucket名稱 `
  --member="serviceAccount:line-gift-pending-address-job@你的專案ID.iam.gserviceaccount.com" `
  --role="roles/storage.objectUser"
```

再替 Job 新增環境變數 `SHORT_NAMES_BUCKET`，值填 bucket 名稱（不含 `gs://`）。

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
| `{products}` | 尚未填地址的商品（有設 `GEMINI_MODEL` 或 `SHORT_NAMES_BUCKET` 時是濃縮後的名稱） | 禮盒（紅 / L） ×2、卡片 |
| `{deadline}` | 最後填寫地址日期 | 11/14（六） |
| `{ordered}` | 下單日 | 11/5（四） |
| `{days}` | 今天是下單後第幾天 | 8 |
| `{sender_name}` | 送禮人（發票姓名） | |
| `{recipients}` | 收禮人 | |
| `{order_id}` | 訂單編號 | |

內文裡的代碼可以放在文字或**有引號**（單、雙引號皆可）的屬性值內。換進去的內容只做 HTML 跳脫，
保證不會跳出文字或引號；不要放在沒有引號的屬性，或 `href`、`style`、`<script>` 這類需要網址／CSS／程式語意的位置。

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

用個人 Gmail 帳號（`MAIL_SENDER`）寄信。個人帳號沒有「全網域委派」可用，所以改由帳號本人做一次 OAuth 授權，
拿到只有 `gmail.send`（只能寄信、不能讀信）權限的更新權杖，存在 Secret Manager；
Job 每次執行時用它向 Google 換一張短效權杖，再呼叫 Gmail API。

- 個人 Gmail 每天約可寄給 500 位收件人，超過會被暫時停寄。
- **更改 Google 帳號密碼會讓更新權杖失效**（Job 會以 `invalid_grant` 失敗），需重做下方第 3、4 步。
  在 [Google 帳戶的第三方存取權](https://myaccount.google.com/connections)移除這個應用程式、或權杖連續 6 個月沒用到，也會失效。

### 一次性設定

#### 1. 啟用 Gmail API、設定 OAuth 同意畫面

GCP 主控台（專案可以屬於任何帳號，不必是寄信的那個 Gmail）：

1. 「API 和服務」→「程式庫」→ 啟用 **Gmail API**。
2. 「API 和服務」→「OAuth 同意畫面」（Google Auth Platform）：使用者類型選**外部**，填應用程式名稱與聯絡 email。
3. 「目標對象」→ 把發布狀態按成**正式版**（In production）。
   **留在「測試中」的話，更新權杖 7 天就會過期。** 不必送審；沒送審只是授權時多一個「Google 尚未驗證這個應用程式」的警告畫面。

#### 2. 建立 OAuth 用戶端

「API 和服務」→「憑證」→「建立憑證」→「OAuth 用戶端 ID」：

- 應用程式類型：**網頁應用程式**
- 已授權的重新導向 URI：`https://developers.google.com/oauthplayground`

記下用戶端 ID 和用戶端密鑰。

#### 3. 用寄信的 Gmail 帳號授權，取得更新權杖

1. 開 [OAuth 2.0 Playground](https://developers.google.com/oauthplayground)，右上角齒輪 → 勾「Use your own OAuth credentials」，填入上一步的用戶端 ID 和密鑰。
2. 左側 Step 1 最下方的輸入框填 `https://www.googleapis.com/auth/gmail.send` →「Authorize APIs」。
3. 登入**要用來寄信的那個 Gmail 帳號**。出現未驗證警告時按「進階」→「前往…（不安全）」，再按允許。
4. Step 2 按「Exchange authorization code for tokens」，複製 **Refresh token**。

#### 4. 把密鑰放進 Secret Manager、建立 Job 的服務帳號

```powershell
$project = "你的專案ID"
$sa = "line-gift-pending-address-job@$project.iam.gserviceaccount.com"

# Job 專用的服務帳號
gcloud iam service-accounts create line-gift-pending-address-job --display-name="LINE gift pending address job"

# 兩個寄信用的密鑰（名稱要和 cloudbuild.yaml 一致）
Set-Content -Path "$env:TEMP\s.txt" -Value "用戶端密鑰" -NoNewline -Encoding ascii
gcloud secrets create mail-oauth-client-secret --data-file="$env:TEMP\s.txt"
Set-Content -Path "$env:TEMP\s.txt" -Value "更新權杖" -NoNewline -Encoding ascii
gcloud secrets create mail-oauth-refresh-token --data-file="$env:TEMP\s.txt"
Remove-Item "$env:TEMP\s.txt"

# 讓 Job 能讀三個密鑰（line-gift-gateway-token 見下方「LINE token 放 Secret Manager」）
foreach ($secret in "line-gift-gateway-token", "mail-oauth-client-secret", "mail-oauth-refresh-token") {
  gcloud secrets add-iam-policy-binding $secret `
    --member="serviceAccount:$sa" --role="roles/secretmanager.secretAccessor"
}
```

之後要換更新權杖（例如改過密碼），重做第 3 步後新增一個版本即可，不用重新部署：

```powershell
Set-Content -Path "$env:TEMP\s.txt" -Value "新的更新權杖" -NoNewline -Encoding ascii
gcloud secrets versions add mail-oauth-refresh-token --data-file="$env:TEMP\s.txt"
Remove-Item "$env:TEMP\s.txt"
```

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

第一次 push 之前，這個密鑰、上面「一次性設定」的兩個寄信密鑰，以及服務帳戶 `line-gift-pending-address-job`（含讀取三個密鑰的權限）都要先建好，否則部署那一步會失敗。

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
| `MAIL_SENDER` | 必填。授權時登入的那個 Gmail 信箱 |
| `MAIL_OAUTH_CLIENT_ID` | 必填。OAuth 用戶端 ID（不是機密，直接填） |
| `MAIL_FROM_NAME` | 選填 |
| `MAIL_REPLY_TO` | 選填 |
| `MAIL_SUMMARY_TO` | 選填 |
| `GEMINI_MODEL` | 選填。要先完成「濃縮商品名稱」的啟用步驟 |
| `SHORT_NAMES_BUCKET` | 選填。要先完成「對照表」的啟用步驟 |

`MAIL_OAUTH_CLIENT_SECRET` 和 `MAIL_OAUTH_REFRESH_TOKEN` 由 `cloudbuild.yaml` 從 Secret Manager 帶入，不用在這裡填。

`MAIL_SENDER` 和 `MAIL_OAUTH_CLIENT_ID` 設定之前執行 Job 會直接失敗，不會寄出任何信，所以排程請等這一步完成後再建立。

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
商品名稱濃縮失敗不算寄送失敗：信以原名寄出，Job 仍以成功狀態結束。

輸出與統整信都含收禮人姓名與送禮人 email，請留意 log 的存取權限與保存期限。
