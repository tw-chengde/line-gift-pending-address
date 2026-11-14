//! 列出 LINE 禮物「已付款、收禮人尚未填寫地址」的訂單，並對下單滿 5 天、滿 8 天（可調整）的訂單
//! 寄 email 提醒送禮人。對 LINE 是唯讀的（只呼叫訂單清單與訂單詳情）。
//!
//! 環境變數：
//!   LINE_GIFT_GATEWAY_TOKEN  必填。
//!   LINE_GIFT_ENVIRONMENT    設為 beta 時改打測試環境。
//!   MAIL_SENDER              --send 時必填。用來寄信的 Gmail 信箱（就是授權時登入的那個帳號）。
//!   MAIL_OAUTH_CLIENT_ID     --send 時必填。OAuth 用戶端 ID。
//!   MAIL_OAUTH_CLIENT_SECRET --send 時必填。OAuth 用戶端密鑰。
//!   MAIL_OAUTH_REFRESH_TOKEN --send 時必填。MAIL_SENDER 授權 gmail.send 後拿到的更新權杖。
//!   MAIL_SUMMARY_TO          統整信收件人，可用逗號分隔多個；未設定時寄給 MAIL_SENDER 自己。
//!   MAIL_FROM_NAME           寄件人顯示名稱（選填）。
//!   MAIL_REPLY_TO            收件人按「回覆」時的收件地址（選填）；未設定時回到 MAIL_SENDER。
//!   REMINDER_DAYS            下單滿幾天要提醒，逗號分隔（預設 5,8）。
//!   DEADLINE_DAYS            最後填寫地址日期 = 下單日 + 這個天數（預設 9）。
//!   MAIL_SUBJECT / MAIL_BODY 提醒信的主旨與內文（HTML）範本，可用 {products} {deadline} 等代碼。
//!   GEMINI_MODEL             設定後（例如 gemini-3.1-flash-lite）用 Vertex AI 濃縮信裡的商品名稱，
//!                            去掉促銷字眼；未設定時用原名。只送商品名稱，失敗時退回原名、信照寄。
//!   SHORT_NAMES_BUCKET       設定後把「商品編號 → 短名」對照表存在這個 Cloud Storage bucket 的 short-names.json：
//!                            表裡有的直接用，沒有的才問 Gemini 並寫回。可以手動修改這個檔案。
//!   GOOGLE_CLOUD_PROJECT     Vertex AI 用的專案 ID；未設定時向 metadata server 查（Cloud Run 內不必設）。
//!   GOOGLE_ACCESS_TOKEN      本機測試上面兩項用：gcloud auth print-access-token 的輸出。
//!
//! 參數：
//!   --from / --to  台北時間的日期（YYYY-MM-DD），以訂單成立時間篩選；未指定時查最近 DEADLINE_DAYS + 1 天（有帶 --today 時從那一天往回算）。
//!   --max          最多檢查幾筆已付款訂單（預設 200；每筆需呼叫一次訂單詳情）。
//!   --send         真的寄出提醒信與統整信；沒帶時只在輸出的 reminders 預覽，不寄任何信。
//!   --today        把「今天」當成指定日期（YYYY-MM-DD）來算滿幾天，補寄漏掉的那一天時用。
//!   --raw          額外印出每筆待填地址群組的原始 deliveryForm（含個資，確認回傳格式用）。
//!   --compact      輸出單行 JSON（在 Cloud Run Job 內會自動啟用，讓 Cloud Logging 視為一筆紀錄）。
//!
//! 程式不記錄寄過誰：是否提醒完全由「今天 − 下單日」決定，所以同一天執行兩次會寄兩次，
//! 某天沒執行則那天該提醒的訂單會被跳過（可用 --today 補寄）。

use std::{
    collections::{BTreeMap, HashMap},
    env,
    process::exit,
    time::Duration,
};

use base64::{
    engine::general_purpose::{STANDARD, URL_SAFE},
    Engine,
};
use chrono::{DateTime, Datelike, FixedOffset, NaiveDate, TimeDelta, Utc};
use futures::future::try_join_all;
use reqwest::{Client, Method, RequestBuilder, StatusCode};
use serde_json::{json, Value};

const ORDER_PATH: &str = "/order2/v1/order/orders";
const PAGE_SIZE: usize = 100;
const DETAIL_CONCURRENCY: usize = 5;
const DEFAULT_REMINDER_DAYS: &str = "5,8";
const DEFAULT_DEADLINE_DAYS: i64 = 9;
const DEFAULT_MAIL_SUBJECT: &str = "【LINE 禮物】收件地址填寫提醒（最後日期 {deadline}）";
const DEFAULT_MAIL_BODY: &str = "您贈送給朋友的 <strong>{products}</strong>，<br>\
    <span style=\"color:#d93025;\"><strong>{deadline}為最後填寫地址日期</strong></span><br>\
    再請朋友留意，<br>\
    避免訂單取消～<br>\
    <br>\
    謝謝您❤️<br>\
    祝順心。";
/// MAIL_SUBJECT、MAIL_BODY 裡可以用的 {代碼}。
const PLACEHOLDERS: [&str; 7] = [
    "products",
    "deadline",
    "ordered",
    "days",
    "sender_name",
    "recipients",
    "order_id",
];

fn fail(message: &str) -> ! {
    eprintln!("{message}");
    exit(1)
}

/// 去掉頭尾空白後的環境變數；未設定或空白時為 None。
fn env_text(key: &str) -> Option<String> {
    env::var(key)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// 把範本裡的 {代碼} 換成 value 回傳的內容；value 回傳 None 的 {…} 原樣保留。
/// 只掃一次，所以換進去的內容（例如商品名稱）就算含有 {代碼} 也不會再被替換。
fn render(template: &str, mut value: impl FnMut(&str) -> Option<String>) -> String {
    let mut output = String::new();
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        output.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let replaced = after
            .find('}')
            .and_then(|end| value(&after[..end]).map(|text| (text, end)));
        match replaced {
            Some((text, end)) => {
                output.push_str(&text);
                rest = &after[end + 1..];
            }
            None => {
                output.push('{');
                rest = after;
            }
        }
    }
    output.push_str(rest);
    output
}

/// 範本裡看起來像代碼（小寫英文與底線）卻不認得的 {…}，多半是打錯字，啟動時就擋下來。
fn check_template(key: &str, template: &str) -> Result<(), String> {
    let mut unknown = Vec::new();
    render(template, |name| {
        let looks_like_code = !name.is_empty()
            && name
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte == b'_');
        if looks_like_code && !PLACEHOLDERS.contains(&name) {
            unknown.push(format!("{{{name}}}"));
        }
        None
    });
    if unknown.is_empty() {
        return Ok(());
    }
    Err(format!(
        "{key} 內有不認得的代碼 {}。可用的代碼：{}",
        unknown.join("、"),
        PLACEHOLDERS.map(|name| format!("{{{name}}}")).join("、")
    ))
}

fn parse_reminder_days(value: &str) -> Result<Vec<i64>, String> {
    let mut days = Vec::new();
    for part in value
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
    {
        match part.parse::<i64>() {
            Ok(day) if day >= 0 => days.push(day),
            _ => return Err(format!("REMINDER_DAYS 內的 {part} 不是 0 以上的整數。")),
        }
    }
    if days.is_empty() {
        return Err("REMINDER_DAYS 至少要有一個天數，例如 5,8。".to_string());
    }
    days.sort_unstable();
    days.dedup();
    Ok(days)
}

/// 不必改程式就能調整的設定，全部來自環境變數，未設定時用預設值。
struct Config {
    /// 下單後滿幾天（以台北時間的日曆天計）要提醒送禮人。
    reminder_days: Vec<i64>,
    /// 最後填寫地址日期 = 下單日 + 這個天數。
    deadline_days: i64,
    subject: String,
    body: String,
}

impl Config {
    fn from_env() -> Result<Self, String> {
        let reminder_days = parse_reminder_days(
            &env_text("REMINDER_DAYS").unwrap_or_else(|| DEFAULT_REMINDER_DAYS.to_string()),
        )?;
        let deadline_days = match env_text("DEADLINE_DAYS").map(|value| value.parse::<i64>()) {
            None => DEFAULT_DEADLINE_DAYS,
            Some(Ok(days)) if days >= 1 => days,
            Some(_) => return Err("DEADLINE_DAYS 必須是正整數。".to_string()),
        };
        if let Some(late) = reminder_days.iter().find(|day| **day > deadline_days) {
            return Err(format!(
                "REMINDER_DAYS 的 {late} 晚於 DEADLINE_DAYS（{deadline_days}），提醒時已經過了最後填寫日期。"
            ));
        }
        let subject = env_text("MAIL_SUBJECT").unwrap_or_else(|| DEFAULT_MAIL_SUBJECT.to_string());
        let body = env_text("MAIL_BODY").unwrap_or_else(|| DEFAULT_MAIL_BODY.to_string());
        check_template("MAIL_SUBJECT", &subject)?;
        check_template("MAIL_BODY", &body)?;
        Ok(Self {
            reminder_days,
            deadline_days,
            subject,
            body,
        })
    }

    /// 例如「滿 5 天或滿 8 天」。
    fn days_text(&self) -> String {
        self.reminder_days
            .iter()
            .map(|day| format!("滿 {day} 天"))
            .collect::<Vec<_>>()
            .join("或")
    }
}

fn taipei() -> FixedOffset {
    FixedOffset::east_opt(8 * 60 * 60).unwrap()
}

fn taipei_day(value: &str, end_of_day: bool) -> DateTime<Utc> {
    let well_formed = value.len() == 10
        && value.bytes().enumerate().all(|(index, byte)| {
            if index == 4 || index == 7 {
                byte == b'-'
            } else {
                byte.is_ascii_digit()
            }
        });
    if !well_formed {
        fail(&format!("日期 {value} 格式錯誤，請用 YYYY-MM-DD。"));
    }
    let Ok(date) = NaiveDate::parse_from_str(value, "%Y-%m-%d") else {
        fail(&format!("日期 {value} 無效。"));
    };
    let time = if end_of_day {
        date.and_hms_milli_opt(23, 59, 59, 999)
    } else {
        date.and_hms_milli_opt(0, 0, 0, 0)
    };
    time.unwrap()
        .and_local_timezone(taipei())
        .unwrap()
        .with_timezone(&Utc)
}

/// ISO 8601 in Taipei time, e.g. 2026-11-01T00:00:00.000+08:00.
fn taipei_date_time(date: DateTime<Utc>) -> String {
    date.with_timezone(&taipei())
        .format("%Y-%m-%dT%H:%M:%S%.3f+08:00")
        .to_string()
}

/// 字串原樣回傳、null 用預設值、其餘型別（例如數字 id）轉成文字。
fn text(value: &Value, default: &str) -> String {
    match value {
        Value::Null => default.to_string(),
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

fn encode_path_segment(segment: &str) -> String {
    segment
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || b"-_.~".contains(&byte) {
                (byte as char).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}

struct Api {
    client: Client,
    endpoint: &'static str,
    token: String,
}

impl Api {
    async fn request(
        &self,
        path: &str,
        method: Method,
        body: Option<&Value>,
    ) -> Result<Value, String> {
        let mut request = self
            .client
            .request(method, format!("{}{path}", self.endpoint))
            .header("Authorization", &self.token)
            .header("Accept", "application/json")
            .header("Content-Type", "application/json");
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request
            .send()
            .await
            .map_err(|error| format!("LINE Gift 連線失敗 ({path}): {error}"))?;
        if !response.status().is_success() {
            return Err(format!(
                "LINE Gift HTTP {} ({path})",
                response.status().as_u16()
            ));
        }
        let mut envelope: Value = response
            .json()
            .await
            .map_err(|error| format!("LINE Gift 回應不是 JSON ({path}): {error}"))?;
        if envelope["success"] != true {
            let error = match &envelope["errors"][0] {
                Value::Null => &envelope["error"],
                first => first,
            };
            return Err(format!(
                "LINE Gift API 失敗 ({path}): {} {}",
                text(&error["code"], "UNKNOWN"),
                text(&error["message"], "")
            ));
        }
        Ok(envelope["result"].take())
    }
}

fn has_text(value: &Value) -> bool {
    value.as_str().is_some_and(|text| !text.trim().is_empty())
}

fn has_address(form: &Value) -> bool {
    let any_text = |object: &Value, keys: &[&str]| keys.iter().any(|key| has_text(&object[*key]));
    any_text(
        &form["generalAddress"],
        &["province", "district", "baseAddress", "detailAddress"],
    ) || any_text(&form["cvsAddress"], &["storeId", "address"])
}

fn pending_groups(order: &Value, raw: bool) -> Vec<Value> {
    let items = order["items"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    order["deliveryGroups"].as_array().into_iter().flatten().filter_map(|group| {
        let form = &group["deliveryForm"];
        // 訂單是因為有 PAYED 品項才被查到，但其他群組可能已出貨或取消。
        let paid_items: Vec<&Value> = group["itemIds"].as_array().into_iter().flatten()
            .filter_map(|id| items.iter().find(|item| item["id"] == *id))
            .filter(|item| item["status"] == "PAYED")
            .collect();
        if paid_items.is_empty() || has_address(form) {
            return None;
        }
        let mut pending = json!({
            "groupId": group["id"],
            "recipient": text(&form["name"], ""),
            // 只列欄位名稱，方便確認 LINE 在未填地址時實際回傳了哪些欄位。
            "deliveryFormFields": form.as_object().map(|fields| fields.keys().collect::<Vec<_>>()).unwrap_or_default(),
            "items": paid_items.iter().map(|item| json!({
                "id": item["id"],
                "productId": item["productId"],
                "name": item["productName"],
                "spec": item["optionNames"].as_array().into_iter().flatten().map(|name| text(name, "")).collect::<Vec<_>>().join(" / "),
                "qty": item["quantity"],
            })).collect::<Vec<_>>(),
        });
        if raw {
            pending["deliveryForm"] = form.clone();
        }
        Some(pending)
    }).collect()
}

/// 訂單成立時間換成台北時間的日期。接受 ISO 8601 字串或 epoch（秒或毫秒）。
fn ordered_date(value: &Value) -> Option<NaiveDate> {
    let utc = match value {
        Value::String(text) => DateTime::parse_from_rfc3339(text)
            .or_else(|_| DateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S%.f%z"))
            .ok()?
            .with_timezone(&Utc),
        Value::Number(number) => {
            let number = number.as_i64()?;
            let millis = if number < 100_000_000_000 {
                number * 1000
            } else {
                number
            };
            DateTime::from_timestamp_millis(millis)?
        }
        _ => return None,
    };
    Some(utc.with_timezone(&taipei()).date_naive())
}

/// 例如 11/21（六）。
fn short_date(date: NaiveDate) -> String {
    const WEEKDAYS: [&str; 7] = ["一", "二", "三", "四", "五", "六", "日"];
    format!(
        "{}/{}（{}）",
        date.month(),
        date.day(),
        WEEKDAYS[date.weekday().num_days_from_monday() as usize]
    )
}

/// 對照表的鍵：LINE 的商品編號（productId）。賣場改了商品名稱也還是同一個鍵，
/// 所以信裡的名稱不會跟著變。LINE 沒給編號時退而用商品名稱。
fn product_key(item: &Value) -> String {
    match &item["productId"] {
        Value::Null => text(&item["name"], ""),
        id => text(id, ""),
    }
}

/// short_names 是「商品鍵（見 product_key）→ 信裡要用的名稱」；查不到的用原名。
fn product_text(groups: &[Value], short_names: &BTreeMap<String, ShortName>) -> String {
    groups
        .iter()
        .flat_map(|group| group["items"].as_array().into_iter().flatten())
        .map(|item| {
            let mut label = match short_names.get(&product_key(item)) {
                Some(entry) => entry.short.clone(),
                None => text(&item["name"], "商品"),
            };
            let spec = text(&item["spec"], "");
            if !spec.is_empty() {
                label.push_str(&format!("（{spec}）"));
            }
            if item["qty"].as_u64().is_some_and(|qty| qty > 1) {
                label.push_str(&format!(" ×{}", item["qty"]));
            }
            label
        })
        .collect::<Vec<_>>()
        .join("、")
}

fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// 收件地址來自送禮人自己填的發票 email，會直接放進信件標頭，所以只接受單純的 ASCII 地址。
fn valid_email(address: &str) -> bool {
    let Some((local, domain)) = address.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !domain.contains('@')
        && address
            .chars()
            .all(|ch| ch.is_ascii_graphic() && !"<>(),;:\\\"[]".contains(ch))
}

/// RFC 2047 encoded-word，過長時折行。每段最多 39 bytes（編碼後 64 字元），
/// 加上「Subject: 」這類標頭名稱後整行仍在 76 字元內。
fn encoded_word(text: &str) -> String {
    let mut words = Vec::new();
    let mut chunk = String::new();
    for ch in text.chars() {
        if chunk.len() + ch.len_utf8() > 39 {
            words.push(format!("=?UTF-8?B?{}?=", STANDARD.encode(&chunk)));
            chunk.clear();
        }
        chunk.push(ch);
    }
    if !chunk.is_empty() {
        words.push(format!("=?UTF-8?B?{}?=", STANDARD.encode(&chunk)));
    }
    words.join("\r\n ")
}

fn mime_message(
    from_name: Option<&str>,
    from: &str,
    reply_to: Option<&str>,
    to: &str,
    subject: &str,
    html: &str,
) -> String {
    let from = match from_name {
        Some(name) => format!("{} <{from}>", encoded_word(name)),
        None => from.to_string(),
    };
    let reply_to = reply_to.map_or_else(String::new, |address| format!("Reply-To: {address}\r\n"));
    let body = STANDARD.encode(html);
    let body = body
        .as_bytes()
        .chunks(76)
        .map(|line| std::str::from_utf8(line).unwrap())
        .collect::<Vec<_>>()
        .join("\r\n");
    format!(
        "From: {from}\r\n{reply_to}To: {to}\r\nSubject: {}\r\nMIME-Version: 1.0\r\n\
         Content-Type: text/html; charset=UTF-8\r\nContent-Transfer-Encoding: base64\r\n\r\n{body}\r\n",
        encoded_word(subject)
    )
}

async fn google_text(request: RequestBuilder, what: &str) -> Result<String, String> {
    let response = request
        .send()
        .await
        .map_err(|error| format!("{what}連線失敗: {error}"))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|error| format!("{what}讀取回應失敗: {error}"))?;
    if !status.is_success() {
        let detail: String = body.chars().take(300).collect();
        return Err(format!("{what}失敗 HTTP {}: {detail}", status.as_u16()));
    }
    Ok(body)
}

async fn google_json(request: RequestBuilder, what: &str) -> Result<Value, String> {
    serde_json::from_str(&google_text(request, what).await?)
        .map_err(|error| format!("{what}回應不是 JSON: {error}"))
}

const SHORTEN_INSTRUCTION: &str = "你會收到一個商品名稱的 JSON 陣列。\
    請去掉促銷、聳動、SEO 用的字眼，只保留品牌、品名與必要規格，不可新增原文沒有的資訊。\
    回傳同樣長度、同樣順序的 JSON 字串陣列。";

/// 從 generateContent 的回應取出「原名 → 濃縮後名稱」，只留真的有變短的。
/// 筆數對不上就整批不用：對錯位置會讓信裡出現別的商品。
fn short_name_map(names: &[String], response: &Value) -> Result<BTreeMap<String, String>, String> {
    // 被內容過濾擋下時沒有 parts；思考片段（thought）可能排在答案前面。
    let answer: String = response["candidates"][0]["content"]["parts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|part| part["thought"] != true)
        .filter_map(|part| part["text"].as_str())
        .collect();
    let short: Vec<String> = serde_json::from_str(&answer).map_err(|error| {
        format!(
            "Gemini 沒有回傳字串陣列（finishReason {}）: {error}",
            text(&response["candidates"][0]["finishReason"], "無")
        )
    })?;
    if short.len() != names.len() {
        return Err(format!(
            "Gemini 回傳 {} 筆，和送出的 {} 筆對不上。",
            short.len(),
            names.len()
        ));
    }
    Ok(names
        .iter()
        .zip(short)
        .map(|(name, short)| (name.clone(), short.trim().to_string()))
        .filter(|(name, short)| !short.is_empty() && short.chars().count() < name.chars().count())
        .collect())
}

async fn gcp_metadata(client: &Client, path: &str) -> Result<String, String> {
    google_text(
        client
            .get(format!(
                "http://metadata.google.internal/computeMetadata/v1/{path}"
            ))
            .header("Metadata-Flavor", "Google"),
        "讀取 GCP metadata（本機請設 GOOGLE_CLOUD_PROJECT 與 GOOGLE_ACCESS_TOKEN）",
    )
    .await
}

/// 執行身分（Cloud Run 的服務帳號）的存取權杖，Vertex AI 和 Cloud Storage 共用。
async fn gcp_token(client: &Client) -> Result<String, String> {
    if let Some(token) = env_text("GOOGLE_ACCESS_TOKEN") {
        return Ok(token);
    }
    let token: Value = serde_json::from_str(
        &gcp_metadata(client, "instance/service-accounts/default/token").await?,
    )
    .map_err(|error| format!("GCP 權杖回應不是 JSON: {error}"))?;
    Ok(token["access_token"]
        .as_str()
        .ok_or("GCP 權杖回應缺少 access_token。")?
        .to_string())
}

/// 對照表在 bucket 裡的物件名稱，格式見 parse_short_names。
const SHORT_NAMES_OBJECT: &str = "short-names.json";

/// 讀 Cloud Storage 上的對照表；物件還不存在時是空表。
async fn load_short_names(
    client: &Client,
    token: &str,
    bucket: &str,
) -> Result<BTreeMap<String, ShortName>, String> {
    let response = client
        .get(format!(
            "https://storage.googleapis.com/storage/v1/b/{}/o/{SHORT_NAMES_OBJECT}?alt=media",
            encode_path_segment(bucket)
        ))
        .bearer_auth(token)
        .send()
        .await
        .map_err(|error| format!("讀取商品名稱對照表連線失敗: {error}"))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|error| format!("讀取商品名稱對照表失敗: {error}"))?;
    // bucket 不存在也是 404，那種情況之後寫回時會失敗並回報。
    if status == StatusCode::NOT_FOUND {
        return Ok(BTreeMap::new());
    }
    if !status.is_success() {
        let detail: String = body.chars().take(300).collect();
        return Err(format!(
            "讀取商品名稱對照表失敗 HTTP {}: {detail}",
            status.as_u16()
        ));
    }
    parse_short_names(&body)
}

/// 對照表檔案的格式：{"商品編號": {"name": "加入時的原始名稱", "short": "信裡要用的名稱"}}。
/// name 只是給人對照用的；有一筆格式不對就整份不用，免得之後寫回時把手動改的內容弄丟。
fn parse_short_names(body: &str) -> Result<BTreeMap<String, ShortName>, String> {
    let bad = |detail: String| format!("商品名稱對照表 {SHORT_NAMES_OBJECT} 格式不對: {detail}");
    let entries: BTreeMap<String, Value> =
        serde_json::from_str(body).map_err(|error| bad(error.to_string()))?;
    entries
        .into_iter()
        .map(|(key, entry)| {
            let short = entry["short"]
                .as_str()
                .ok_or_else(|| bad(format!("{key} 缺少文字欄位 short")))?
                .to_string();
            let name = text(&entry["name"], "");
            Ok((key, ShortName { name, short }))
        })
        .collect()
}

async fn save_short_names(
    client: &Client,
    token: &str,
    bucket: &str,
    table: &BTreeMap<String, ShortName>,
) -> Result<(), String> {
    let entries: BTreeMap<&String, Value> = table
        .iter()
        .map(|(key, entry)| (key, entry.json()))
        .collect();
    let body = serde_json::to_string_pretty(&entries).map_err(|error| error.to_string())?;
    google_text(
        client
            .post(format!(
                "https://storage.googleapis.com/upload/storage/v1/b/{}/o?uploadType=media&name={SHORT_NAMES_OBJECT}",
                encode_path_segment(bucket)
            ))
            .bearer_auth(token)
            .header("Content-Type", "application/json; charset=utf-8")
            .body(body),
        "寫入商品名稱對照表",
    )
    .await
    .map(|_| ())
}

/// 一個商品的原始名稱（name）和信裡要用的名稱（short）。
#[derive(Clone, Debug, PartialEq)]
struct ShortName {
    name: String,
    short: String,
}

impl ShortName {
    fn json(&self) -> Value {
        json!({ "name": self.name, "short": self.short })
    }
}

fn short_names_json(entries: &BTreeMap<String, ShortName>) -> Value {
    entries
        .iter()
        .map(|(key, entry)| (key.clone(), entry.json()))
        .collect::<serde_json::Map<_, _>>()
        .into()
}

/// 這次執行的商品名稱濃縮結果，鍵都是 product_key。
#[derive(Default)]
struct ShortNames {
    /// 信裡實際換掉名稱的商品（name 是訂單上現在的名稱）。
    used: BTreeMap<String, ShortName>,
    /// 這次新加進對照表的商品（沒變短的 short 會和 name 一樣）。
    added: BTreeMap<String, ShortName>,
    error: Option<String>,
}

/// products 是今天要提醒的商品（鍵、訂單上的名稱）。對照表裡 short 和現在的名稱不同的才算有換名稱。
fn used_short_names(
    products: &[(String, String)],
    table: &BTreeMap<String, ShortName>,
) -> BTreeMap<String, ShortName> {
    products
        .iter()
        .filter_map(|(key, name)| {
            let short = table.get(key)?.short.trim().to_string();
            let name = name.clone();
            (!short.is_empty() && short != name).then(|| (key.clone(), ShortName { name, short }))
        })
        .collect()
}

/// 先查對照表（有設 bucket 時），表裡沒有的才問 Gemini（有設 model 時），問到的結果寫回對照表。
/// 對照表讀不到時整批用原名而且不寫回，避免把既有的表蓋掉。
async fn resolve_short_names(
    client: &Client,
    model: Option<&str>,
    bucket: Option<&str>,
    products: &[(String, String)],
) -> ShortNames {
    let mut result = ShortNames::default();
    let token = match gcp_token(client).await {
        Ok(token) => token,
        Err(error) => {
            result.error = Some(error);
            return result;
        }
    };
    let mut table = BTreeMap::new();
    if let Some(bucket) = bucket {
        match load_short_names(client, &token, bucket).await {
            Ok(loaded) => table = loaded,
            Err(error) => {
                result.error = Some(error);
                return result;
            }
        }
    }
    let unknown: Vec<&(String, String)> = products
        .iter()
        .filter(|(key, _)| !table.contains_key(key))
        .collect();
    if let (Some(model), false) = (model, unknown.is_empty()) {
        let names: Vec<String> = unknown.iter().map(|(_, name)| name.clone()).collect();
        match shorten_names(client, &token, model, &names).await {
            Err(error) => result.error = Some(error),
            Ok(short) => {
                for (key, name) in unknown {
                    let entry = ShortName {
                        name: name.clone(),
                        short: short.get(name).unwrap_or(name).clone(),
                    };
                    table.insert(key.clone(), entry.clone());
                    result.added.insert(key.clone(), entry);
                }
                if let Some(bucket) = bucket {
                    result.error = save_short_names(client, &token, bucket, &table).await.err();
                }
            }
        }
    }
    result.used = used_short_names(products, &table);
    result
}

/// 用 Vertex AI 的 Gemini 濃縮商品名稱，需要 Vertex AI 使用者角色。
async fn shorten_names(
    client: &Client,
    token: &str,
    model: &str,
    names: &[String],
) -> Result<BTreeMap<String, String>, String> {
    let project = match env_text("GOOGLE_CLOUD_PROJECT") {
        Some(project) => project,
        None => gcp_metadata(client, "project/project-id")
            .await?
            .trim()
            .to_string(),
    };
    let response = google_json(
        client
            .post(format!(
                "https://aiplatform.googleapis.com/v1/projects/{}/locations/global/publishers/google/models/{}:generateContent",
                encode_path_segment(&project),
                encode_path_segment(model)
            ))
            .bearer_auth(token)
            .json(&json!({
                "systemInstruction": { "parts": [{ "text": SHORTEN_INSTRUCTION }] },
                "contents": [{ "role": "user", "parts": [{ "text": json!(names).to_string() }] }],
                "generationConfig": {
                    "temperature": 0,
                    "responseMimeType": "application/json",
                    "responseSchema": { "type": "ARRAY", "items": { "type": "STRING" } },
                },
            })),
        "Gemini 濃縮商品名稱",
    )
    .await?;
    short_name_map(names, &response)
}

/// 以 MAIL_SENDER 本人授權的 OAuth 更新權杖（只有 gmail.send 權限）透過 Gmail API 寄信。
struct Mailer {
    client: Client,
    sender: String,
    from_name: Option<String>,
    reply_to: Option<String>,
    summary_to: String,
    token: String,
}

impl Mailer {
    async fn connect(client: Client) -> Result<Self, String> {
        let var = env_text;
        let sender =
            var("MAIL_SENDER").ok_or("缺少 MAIL_SENDER 環境變數（用來寄信的 Gmail 信箱）。")?;
        if !valid_email(&sender) {
            return Err(format!("MAIL_SENDER {sender} 不是有效的 email。"));
        }
        let reply_to = var("MAIL_REPLY_TO");
        if let Some(bad) = reply_to.as_deref().filter(|address| !valid_email(address)) {
            return Err(format!("MAIL_REPLY_TO {bad} 不是有效的 email。"));
        }
        let summary_to = match var("MAIL_SUMMARY_TO") {
            None => sender.clone(),
            Some(list) => {
                let addresses: Vec<&str> = list.split(',').map(str::trim).collect();
                if let Some(bad) = addresses.iter().find(|address| !valid_email(address)) {
                    return Err(format!("MAIL_SUMMARY_TO 內的 {bad} 不是有效的 email。"));
                }
                addresses.join(", ")
            }
        };

        let required = |key: &str| var(key).ok_or(format!("缺少 {key} 環境變數。"));
        let client_id = required("MAIL_OAUTH_CLIENT_ID")?;
        let client_secret = required("MAIL_OAUTH_CLIENT_SECRET")?;
        let refresh_token = required("MAIL_OAUTH_REFRESH_TOKEN")?;
        let token = google_json(
            client.post("https://oauth2.googleapis.com/token").form(&[
                ("grant_type", "refresh_token"),
                ("client_id", &client_id),
                ("client_secret", &client_secret),
                ("refresh_token", &refresh_token),
            ]),
            "換取 Gmail 權杖（invalid_grant 表示更新權杖已失效，需重新授權）",
        )
        .await?;
        let token = token["access_token"]
            .as_str()
            .ok_or("Gmail 權杖回應缺少 access_token。")?
            .to_string();

        Ok(Self {
            client,
            sender,
            from_name: var("MAIL_FROM_NAME"),
            reply_to,
            summary_to,
            token,
        })
    }

    async fn send(&self, to: &str, subject: &str, html: &str) -> Result<(), String> {
        let message = mime_message(
            self.from_name.as_deref(),
            &self.sender,
            self.reply_to.as_deref(),
            to,
            subject,
            html,
        );
        google_json(
            self.client
                .post("https://gmail.googleapis.com/gmail/v1/users/me/messages/send")
                .bearer_auth(&self.token)
                .json(&json!({ "raw": URL_SAFE.encode(message) })),
            "Gmail 寄信",
        )
        .await
        .map(|_| ())
    }
}

/// 一筆今天該提醒的訂單。result 為 pending（待寄／預覽）、sent、failed 或 skipped。
struct Reminder {
    order_id: String,
    ordered: NaiveDate,
    days: i64,
    deadline: NaiveDate,
    sender_name: String,
    email: String,
    recipients: String,
    products: String,
    /// 尚未填地址的群組；商品名稱濃縮後用來重組 products。
    groups: Vec<Value>,
    result: &'static str,
    note: String,
}

impl Reminder {
    /// 範本代碼對應的內容；清單要和 PLACEHOLDERS 一致。
    fn value(&self, name: &str) -> Option<String> {
        Some(match name {
            "products" => self.products.clone(),
            "deadline" => short_date(self.deadline),
            "ordered" => short_date(self.ordered),
            "days" => self.days.to_string(),
            "sender_name" => self.sender_name.clone(),
            "recipients" => self.recipients.clone(),
            "order_id" => self.order_id.clone(),
            _ => return None,
        })
    }

    fn subject(&self, config: &Config) -> String {
        render(&config.subject, |name| self.value(name))
    }

    /// MAIL_BODY 本身是 HTML；換進去的內容來自 LINE，一律跳脫。
    fn html(&self, config: &Config) -> String {
        format!(
            "<div style=\"font-size:15px;line-height:1.8;\">{}</div>",
            render(&config.body, |name| self
                .value(name)
                .map(|text| escape_html(&text)))
        )
    }

    fn json(&self) -> Value {
        json!({
            "orderId": self.order_id,
            "orderedDate": self.ordered.to_string(),
            "daysSinceOrder": self.days,
            "deadline": self.deadline.to_string(),
            "to": self.email,
            "senderName": self.sender_name,
            "recipients": self.recipients,
            "products": self.products,
            "result": self.result,
            "note": self.note,
        })
    }
}

fn summary_html(
    config: &Config,
    today: NaiveDate,
    reminders: &[Reminder],
    undated: &[String],
    pending_orders: usize,
    truncated: bool,
    short_names: &ShortNames,
) -> String {
    let count = |result: &str| reminders.iter().filter(|r| r.result == result).count();
    let mut html = format!(
        "<div style=\"font-size:14px;line-height:1.7;\">\
         <p><strong>{} LINE 禮物地址提醒</strong></p>\
         <p>寄出 {} 封、失敗 {} 封、無法寄送 {} 筆。<br>\
         目前共有 {pending_orders} 筆已付款訂單尚未填寫地址。</p>",
        short_date(today),
        count("sent"),
        count("failed"),
        count("skipped"),
    );
    if truncated {
        html.push_str(
            "<p style=\"color:#d93025;\"><strong>注意：已付款訂單數超過 --max 上限，\
             較早的訂單沒有被檢查，可能有漏寄，請調高 --max。</strong></p>",
        );
    }
    if !undated.is_empty() {
        html.push_str(&format!(
            "<p style=\"color:#d93025;\"><strong>注意：以下訂單的下單時間無法解析，沒有判斷是否提醒：{}</strong></p>",
            escape_html(&undated.join("、"))
        ));
    }
    if let Some(error) = &short_names.error {
        html.push_str(&format!(
            "<p style=\"color:#d93025;\"><strong>注意：商品名稱濃縮出了問題，這次部分或全部商品用的是原始名稱：{}</strong></p>",
            escape_html(error)
        ));
    }
    if !short_names.added.is_empty() {
        html.push_str(&format!(
            "<p>這次有新的商品名稱加進對照表，不滿意的話請修改 {SHORT_NAMES_OBJECT}：<br>{}</p>",
            short_names
                .added
                .values()
                .map(|ShortName { name, short }| if name == short {
                    format!("{}（未更動）", escape_html(name))
                } else {
                    format!(
                        "{} → <strong>{}</strong>",
                        escape_html(name),
                        escape_html(short)
                    )
                })
                .collect::<Vec<_>>()
                .join("<br>")
        ));
    }
    if reminders.is_empty() {
        html.push_str(&format!(
            "<p>今天沒有下單{}的待填地址訂單。</p>",
            config.days_text()
        ));
    } else {
        let cell =
            "style=\"border:1px solid #ccc;padding:4px 8px;text-align:left;vertical-align:top;\"";
        html.push_str("<table style=\"border-collapse:collapse;font-size:13px;\"><tr>");
        for title in [
            "結果",
            "滿幾天",
            "最後日期",
            "送禮人",
            "收禮人",
            "商品",
            "下單日",
            "訂單編號",
        ] {
            html.push_str(&format!("<th {cell}>{title}</th>"));
        }
        html.push_str("</tr>");
        for reminder in reminders {
            let result = match reminder.result {
                "sent" => "已寄出".to_string(),
                "failed" => format!(
                    "<span style=\"color:#d93025;\">寄送失敗：{}</span>",
                    escape_html(&reminder.note)
                ),
                _ => format!(
                    "<span style=\"color:#d93025;\">未寄：{}</span>",
                    escape_html(&reminder.note)
                ),
            };
            let sender = format!(
                "{}<br>{}",
                escape_html(&reminder.sender_name),
                escape_html(&reminder.email)
            );
            for value in [
                result,
                format!("滿 {} 天", reminder.days),
                short_date(reminder.deadline),
                sender,
                escape_html(&reminder.recipients),
                escape_html(&reminder.products),
                short_date(reminder.ordered),
                escape_html(&reminder.order_id),
            ] {
                html.push_str(&format!("<td {cell}>{value}</td>"));
            }
            html.push_str("</tr>");
        }
        html.push_str("</table>");
        if count("failed") + count("skipped") > 0 {
            html.push_str("<p>紅字的訂單沒有通知到送禮人，需要的話請改用其他方式聯絡。</p>");
        }
    }
    html.push_str("</div>");
    html
}

/// 訂單查詢範圍。沒指定 --to 時以 --today（補寄的那一天）為準，否則用現在；
/// 這樣補寄幾天前的提醒時，當時該提醒的訂單仍在範圍內。
fn query_range(
    from: Option<&str>,
    to: Option<&str>,
    today: Option<&str>,
    deadline_days: i64,
) -> (DateTime<Utc>, DateTime<Utc>) {
    let to = to
        .or(today)
        .map_or_else(Utc::now, |value| taipei_day(value, true));
    // 預設查詢範圍要涵蓋到最後填寫日期為止的訂單，多留一天給「今天」的時分秒。
    let from = from.map_or_else(
        || to - TimeDelta::days(deadline_days + 1),
        |value| taipei_day(value, false),
    );
    (from, to)
}

async fn run() -> Result<(), String> {
    let args: HashMap<String, String> = env::args()
        .skip(1)
        .map(|arg| {
            let arg = arg.strip_prefix("--").unwrap_or(&arg);
            match arg.split_once('=') {
                Some((key, value)) => (key.to_string(), value.to_string()),
                None => (arg.to_string(), "true".to_string()),
            }
        })
        .collect();
    let arg = |key: &str| {
        args.get(key)
            .map(String::as_str)
            .filter(|value| !value.is_empty())
    };
    let flag = |key: &str| arg(key).is_some_and(|value| value != "false");

    // 設定有錯就在呼叫 LINE、寄任何信之前停下來。
    let config = Config::from_env()?;
    let Some(token) = env_text("LINE_GIFT_GATEWAY_TOKEN") else {
        fail("缺少 LINE_GIFT_GATEWAY_TOKEN 環境變數。");
    };
    let beta = env::var("LINE_GIFT_ENVIRONMENT").is_ok_and(|value| value.trim() == "beta");
    let api = &Api {
        client: Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|error| error.to_string())?,
        endpoint: if beta {
            "https://lcp-apis.line-beta.me"
        } else {
            "https://lcp-apis.line.me"
        },
        token,
    };

    let (from, to) = query_range(arg("from"), arg("to"), arg("today"), config.deadline_days);
    if from > to {
        fail("--from 不可晚於 --to。");
    }
    let max_orders = match arg("max").map(str::parse::<usize>) {
        None => 200,
        Some(Ok(max)) if max >= 1 => max,
        Some(_) => fail("--max 必須是正整數。"),
    };
    let today = arg("today")
        .map_or_else(Utc::now, |value| taipei_day(value, false))
        .with_timezone(&taipei())
        .date_naive();

    let mut ids: Vec<String> = Vec::new();
    let mut total_paid_orders = 0;
    let mut page = 0;
    while ids.len() < max_orders {
        let result = api.request(ORDER_PATH, Method::POST, Some(&json!({
            "orderItemStatuses": ["PAYED"],
            "range": { "type": "ORDERED_AT", "from": taipei_date_time(from), "fromInclusive": true, "to": taipei_date_time(to), "toInclusive": true },
            "paging": { "page": page, "size": PAGE_SIZE },
            "sort": "ORDERED_AT_DESC",
        }))).await?;
        total_paid_orders = result["paginate"]["totalCount"]
            .as_u64()
            .ok_or("LINE Gift 訂單清單缺少 paginate.totalCount。")?
            as usize;
        let list = result["list"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default();
        ids.extend(list.iter().map(|summary| text(&summary["id"], "")));
        if list.is_empty() || (page + 1) * PAGE_SIZE >= total_paid_orders {
            break;
        }
        page += 1;
    }
    ids.truncate(max_orders);

    let mut orders = Vec::new();
    let mut reminders: Vec<Reminder> = Vec::new();
    let mut undated: Vec<String> = Vec::new();
    for batch in ids.chunks(DETAIL_CONCURRENCY) {
        let details = try_join_all(batch.iter().map(|id| async move {
            api.request(
                &format!("{ORDER_PATH}/{}", encode_path_segment(id)),
                Method::GET,
                None,
            )
            .await
        }))
        .await?;
        for detail in details {
            let groups = pending_groups(&detail, flag("raw"));
            if !groups.is_empty() {
                match ordered_date(&detail["orderedAt"]) {
                    None => undated.push(text(&detail["id"], "")),
                    Some(ordered) => {
                        let days = (today - ordered).num_days();
                        if config.reminder_days.contains(&days) {
                            let email = text(&detail["invoice"]["form"]["email"], "")
                                .trim()
                                .to_string();
                            let (result, note) = if email.is_empty() {
                                ("skipped", "訂單沒有送禮人 email")
                            } else if !valid_email(&email) {
                                ("skipped", "email 格式不正確")
                            } else {
                                ("pending", "")
                            };
                            reminders.push(Reminder {
                                order_id: text(&detail["id"], ""),
                                ordered,
                                days,
                                deadline: ordered + TimeDelta::days(config.deadline_days),
                                sender_name: text(&detail["invoice"]["form"]["name"], ""),
                                email,
                                recipients: groups
                                    .iter()
                                    .map(|group| text(&group["recipient"], ""))
                                    .filter(|name| !name.is_empty())
                                    .collect::<Vec<_>>()
                                    .join("、"),
                                products: product_text(&groups, &BTreeMap::new()),
                                groups: groups.clone(),
                                result,
                                note: note.to_string(),
                            });
                        }
                    }
                }
                orders.push(json!({
                    "orderId": detail["id"],
                    "orderedAt": detail["orderedAt"],
                    // 送禮人（付款者）的聯絡資訊：發票 email 與賣場會員 email，LINE 未提供時為 null。
                    "sender": {
                        "invoiceName": detail["invoice"]["form"]["name"],
                        "invoiceEmail": detail["invoice"]["form"]["email"],
                    },
                    "groups": groups,
                }));
            }
        }
    }

    // 只處理今天要提醒的訂單裡的商品名稱；預覽時也會做（也會寫對照表），方便先確認結果。
    let gemini_model = env_text("GEMINI_MODEL");
    let short_names_bucket = env_text("SHORT_NAMES_BUCKET");
    let mut short_names = ShortNames::default();
    if gemini_model.is_some() || short_names_bucket.is_some() {
        // 同一個商品出現在多筆訂單時只算一次。
        let products: Vec<(String, String)> = reminders
            .iter()
            .flat_map(|reminder| &reminder.groups)
            .flat_map(|group| group["items"].as_array().into_iter().flatten())
            .filter_map(|item| Some((product_key(item), item["name"].as_str()?.to_string())))
            .filter(|(_, name)| !name.trim().is_empty())
            .collect::<BTreeMap<_, _>>()
            .into_iter()
            .collect();
        if !products.is_empty() {
            short_names = resolve_short_names(
                &api.client,
                gemini_model.as_deref(),
                short_names_bucket.as_deref(),
                &products,
            )
            .await;
            if let Some(error) = &short_names.error {
                eprintln!("商品名稱濃縮出了問題，查不到的用原名：{error}");
            }
        }
        for reminder in &mut reminders {
            reminder.products = product_text(&reminder.groups, &short_names.used);
        }
    }

    let truncated = ids.len() < total_paid_orders;
    let send = flag("send");
    let mut mail_error: Option<String> = None;
    if send {
        match Mailer::connect(api.client.clone()).await {
            Err(error) => {
                for reminder in reminders.iter_mut().filter(|r| r.result == "pending") {
                    reminder.result = "failed";
                    reminder.note = error.clone();
                }
                mail_error = Some(error);
            }
            Ok(mailer) => {
                for reminder in reminders.iter_mut().filter(|r| r.result == "pending") {
                    match mailer
                        .send(
                            &reminder.email,
                            &reminder.subject(&config),
                            &reminder.html(&config),
                        )
                        .await
                    {
                        Ok(()) => reminder.result = "sent",
                        Err(error) => {
                            reminder.result = "failed";
                            reminder.note = error;
                        }
                    }
                }
                let sent = reminders.iter().filter(|r| r.result == "sent").count();
                let unsent = reminders.len() - sent;
                let mut subject = format!(
                    "【LINE 禮物】{} 地址提醒統整：寄出 {sent} 封",
                    short_date(today)
                );
                if unsent > 0 {
                    subject.push_str(&format!("、未寄出 {unsent} 筆"));
                }
                let html = summary_html(
                    &config,
                    today,
                    &reminders,
                    &undated,
                    orders.len(),
                    truncated,
                    &short_names,
                );
                if let Err(error) = mailer.send(&mailer.summary_to, &subject, &html).await {
                    mail_error = Some(format!("統整信寄送失敗: {error}"));
                }
            }
        }
    }
    let failed = reminders.iter().filter(|r| r.result == "failed").count();

    let report = json!({
        "range": { "from": taipei_date_time(from), "to": taipei_date_time(to) },
        "totalPaidOrders": total_paid_orders,
        "scannedOrders": ids.len(),
        "truncated": truncated,
        "pendingOrders": orders.len(),
        "reminders": {
            "today": today.to_string(),
            // false 表示只是預覽，沒有寄出任何信。
            "send": send,
            // 這次執行實際生效的設定，改完環境變數後可以用預覽確認。
            "config": {
                "reminderDays": config.reminder_days,
                "deadlineDays": config.deadline_days,
                "mailSubject": config.subject,
                "mailBody": config.body,
                "geminiModel": gemini_model,
                "shortNamesBucket": short_names_bucket,
            },
            // 這次信裡換掉名稱的商品（鍵是 productId）；newShortNames 是這次新加進對照表的，
            // 出問題時原因在 shortNameError。
            "shortNames": short_names_json(&short_names.used),
            "newShortNames": short_names_json(&short_names.added),
            "shortNameError": short_names.error,
            "undatedOrderIds": undated,
            "items": reminders.iter().map(Reminder::json).collect::<Vec<_>>(),
        },
        "orders": orders,
    });
    // Cloud Logging 以行為單位收 stdout；單行 JSON 才會被當成一筆結構化紀錄。
    let compact = flag("compact") || env::var_os("CLOUD_RUN_JOB").is_some();
    let output = if compact {
        serde_json::to_string(&report)
    } else {
        serde_json::to_string_pretty(&report)
    };
    println!("{}", output.map_err(|error| error.to_string())?);
    // 報表先印出來再回報錯誤，這樣失敗時 log 裡仍看得到每一筆的結果。
    if let Some(error) = mail_error {
        return Err(error);
    }
    if failed > 0 {
        return Err(format!("有 {failed} 封提醒信寄送失敗。"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordered_date_uses_taipei_calendar_day() {
        let date = |y, m, d| NaiveDate::from_ymd_opt(y, m, d);
        // UTC 16:30 已經是台北時間的隔天。
        assert_eq!(
            ordered_date(&json!("2026-11-01T16:30:00.000Z")),
            date(2026, 11, 2)
        );
        assert_eq!(
            ordered_date(&json!("2026-11-01T23:59:59.999+08:00")),
            date(2026, 11, 1)
        );
        assert_eq!(
            ordered_date(&json!("2026-11-01T10:00:00+0800")),
            date(2026, 11, 1)
        );
        assert_eq!(ordered_date(&json!(1793550600000_i64)), date(2026, 11, 2));
        assert_eq!(ordered_date(&json!(1793550600_i64)), date(2026, 11, 2));
        assert_eq!(ordered_date(&json!("昨天")), None);
        assert_eq!(ordered_date(&Value::Null), None);
    }

    #[test]
    fn deadline_and_weekday() {
        let ordered = NaiveDate::from_ymd_opt(2026, 11, 5).unwrap();
        let today = NaiveDate::from_ymd_opt(2026, 11, 13).unwrap();
        assert_eq!((today - ordered).num_days(), 8);
        assert_eq!(
            short_date(ordered + TimeDelta::days(DEFAULT_DEADLINE_DAYS)),
            "11/14（六）"
        );
    }

    fn default_config() -> Config {
        Config {
            reminder_days: parse_reminder_days(DEFAULT_REMINDER_DAYS).unwrap(),
            deadline_days: DEFAULT_DEADLINE_DAYS,
            subject: DEFAULT_MAIL_SUBJECT.to_string(),
            body: DEFAULT_MAIL_BODY.to_string(),
        }
    }

    #[test]
    fn query_range_follows_today_when_backfilling() {
        // 補寄 11/10：滿 8 天的 11/2 訂單、最後填寫日當天的 11/1 訂單都要查得到。
        let (from, to) = query_range(None, None, Some("2026-11-10"), DEFAULT_DEADLINE_DAYS);
        assert_eq!(to, taipei_day("2026-11-10", true));
        assert!(from < taipei_day("2026-11-01", false));

        // 明確指定的 --from / --to 不受 --today 影響。
        let (from, to) = query_range(
            Some("2026-11-05"),
            Some("2026-11-12"),
            Some("2026-11-10"),
            DEFAULT_DEADLINE_DAYS,
        );
        assert_eq!(from, taipei_day("2026-11-05", false));
        assert_eq!(to, taipei_day("2026-11-12", true));
    }

    #[test]
    fn reminder_days_parsing() {
        assert_eq!(parse_reminder_days("5,8"), Ok(vec![5, 8]));
        assert_eq!(parse_reminder_days(" 8, 3 ,8,"), Ok(vec![3, 8]));
        assert!(parse_reminder_days("").is_err());
        assert!(parse_reminder_days("5,x").is_err());
        assert!(parse_reminder_days("-1").is_err());
        assert_eq!(default_config().days_text(), "滿 5 天或滿 8 天");
    }

    #[test]
    fn templates() {
        assert!(check_template("X", DEFAULT_MAIL_SUBJECT).is_ok());
        assert!(check_template("X", DEFAULT_MAIL_BODY).is_ok());
        // CSS 之類不像代碼的大括號不算錯；像代碼卻不認得的才擋。
        assert!(check_template("X", "<style>p { color: red }</style>{days}").is_ok());
        let error = check_template("MAIL_BODY", "到 {deadine} 為止").unwrap_err();
        assert!(
            error.contains("MAIL_BODY") && error.contains("{deadine}"),
            "{error}"
        );

        let value = |name: &str| match name {
            "days" => Some("5".to_string()),
            // 換進去的內容含有代碼也不會被再次替換。
            "products" => Some("{days}".to_string()),
            _ => None,
        };
        assert_eq!(
            render("滿 {days} 天：{products} {x} { {days}", value),
            "滿 5 天：{days} {x} { 5"
        );
    }

    #[test]
    fn rejects_addresses_that_could_inject_headers() {
        assert!(valid_email("someone@example.com"));
        assert!(valid_email("a.b+c@mail.example.com.tw"));
        for bad in [
            "",
            "no-at-sign",
            "a@b",
            "a@@b.com",
            "a@b.com\r\nBcc: x@y.com",
            "a b@c.com",
            "a@b.com, x@y.com",
            "<a@b.com>",
            "王@example.com",
        ] {
            assert!(!valid_email(bad), "{bad:?}");
        }
    }

    #[test]
    fn mime_headers_are_folded_and_body_is_wrapped() {
        let subject = "【LINE 禮物】收件地址填寫提醒（最後日期 11/14（六））";
        let message = mime_message(
            Some("小店"),
            "shop@example.com",
            Some("reply@example.com"),
            "to@example.com",
            subject,
            &"好".repeat(200),
        );
        assert!(message.lines().all(|line| line.len() <= 76), "{message}");
        let (headers, body) = message.split_once("\r\n\r\n").unwrap();
        assert!(headers.contains("To: to@example.com\r\n"));
        assert!(headers.contains("Reply-To: reply@example.com\r\n"));
        // 沒設定回覆地址時不該出現這個標頭。
        let plain = mime_message(None, "shop@example.com", None, "to@example.com", "hi", "x");
        assert!(!plain.contains("Reply-To"));
        let decoded = STANDARD.decode(body.replace("\r\n", "")).unwrap();
        assert_eq!(String::from_utf8(decoded).unwrap(), "好".repeat(200));
        // 每段 encoded-word 解回來要能拼回原本的主旨。
        let subject_header = headers
            .split("Subject: ")
            .nth(1)
            .unwrap()
            .split("\r\nMIME")
            .next()
            .unwrap();
        let rebuilt: Vec<u8> = subject_header
            .split("\r\n ")
            .flat_map(|word| {
                STANDARD
                    .decode(&word["=?UTF-8?B?".len()..word.len() - 2])
                    .unwrap()
            })
            .collect();
        assert_eq!(String::from_utf8(rebuilt).unwrap(), subject);
    }

    #[test]
    fn short_names_from_gemini_response() {
        let names = vec![
            "【限時免運】鳳梨酥禮盒 12入".to_string(),
            "卡片".to_string(),
            "茶葉".to_string(),
        ];
        let response = |answer: &str| {
            json!({ "candidates": [{ "content": { "parts": [
                { "thought": true, "text": "想一下" },
                { "text": answer },
            ]}}]})
        };
        // 沒變短（含變長、空白）的不採用，留原名。
        assert_eq!(
            short_name_map(&names, &response(r#"["鳳梨酥禮盒 12入", "精美卡片", " "]"#)),
            Ok(BTreeMap::from([(
                names[0].clone(),
                "鳳梨酥禮盒 12入".to_string()
            )]))
        );
        // 筆數對不上、不是 JSON、被過濾而沒有內容，都算失敗。
        assert!(short_name_map(&names, &response(r#"["鳳梨酥禮盒"]"#)).is_err());
        assert!(short_name_map(&names, &response("好的，以下是結果")).is_err());
        let blocked = json!({ "candidates": [{ "finishReason": "SAFETY" }] });
        let error = short_name_map(&names, &blocked).unwrap_err();
        assert!(error.contains("SAFETY"), "{error}");
    }

    #[test]
    fn short_name_table() {
        let entry = |name: &str, short: &str| ShortName {
            name: name.to_string(),
            short: short.to_string(),
        };
        let table = parse_short_names(
            r#"{
                "101": { "name": "【限時免運】鳳梨酥禮盒", "short": "鳳梨酥禮盒" },
                "102": { "name": "卡片", "short": "卡片" },
                "103": { "name": "茶葉", "short": "高山烏龍茶葉" },
                "104": { "short": " " },
                "999": { "name": "沒出現的商品", "short": "x" }
            }"#,
        )
        .unwrap();
        // 101 在賣場改過名稱，仍沿用表裡的短名；和現在名稱相同（102）或空白（104）的不算換名稱；
        // 手動改的（103）可以比原名長；表裡沒有的（105）用原名。
        let products: Vec<(String, String)> = [
            ("101", "★新春特賣★鳳梨酥禮盒"),
            ("102", "卡片"),
            ("103", "茶葉"),
            ("104", "杯子"),
            ("105", "新商品"),
        ]
        .map(|(key, name)| (key.to_string(), name.to_string()))
        .into();
        assert_eq!(
            used_short_names(&products, &table),
            BTreeMap::from([
                (
                    "101".to_string(),
                    entry("★新春特賣★鳳梨酥禮盒", "鳳梨酥禮盒")
                ),
                ("103".to_string(), entry("茶葉", "高山烏龍茶葉")),
            ])
        );
        // 有一筆格式不對就整份不用。
        let error = parse_short_names(r#"{ "101": "鳳梨酥禮盒" }"#).unwrap_err();
        assert!(error.contains("101"), "{error}");
        assert!(parse_short_names("[]").is_err());

        // LINE 的 productId 是數字；沒有編號時用名稱當鍵。
        assert_eq!(
            product_key(&json!({ "productId": 322533113, "name": "a" })),
            "322533113"
        );
        assert_eq!(product_key(&json!({ "productId": null, "name": "a" })), "a");
    }

    #[test]
    fn product_text_and_html_escaping() {
        let groups = vec![json!({ "items": [
            { "productId": 7, "name": "禮盒 <A&B>", "spec": "紅 / L", "qty": 2 },
            { "productId": 8, "name": "卡片", "spec": "", "qty": 1 },
        ]})];
        let products = product_text(&groups, &BTreeMap::new());
        assert_eq!(products, "禮盒 <A&B>（紅 / L） ×2、卡片");
        // 有濃縮名稱的換掉，規格與數量照舊；沒有的用原名。
        let short = BTreeMap::from([(
            "7".to_string(),
            ShortName {
                name: "禮盒 <A&B>".to_string(),
                short: "禮盒".to_string(),
            },
        )]);
        assert_eq!(product_text(&groups, &short), "禮盒（紅 / L） ×2、卡片");
        let reminder = Reminder {
            order_id: "1".into(),
            ordered: NaiveDate::from_ymd_opt(2026, 11, 5).unwrap(),
            days: 8,
            deadline: NaiveDate::from_ymd_opt(2026, 11, 14).unwrap(),
            sender_name: String::new(),
            email: "a@b.com".into(),
            recipients: String::new(),
            products,
            groups,
            result: "pending",
            note: String::new(),
        };
        let mut config = default_config();
        let html = reminder.html(&config);
        assert!(html.contains("<strong>禮盒 &lt;A&amp;B&gt;（紅 / L） ×2、卡片</strong>"));
        assert!(html.contains("<strong>11/14（六）為最後填寫地址日期</strong>"));
        assert_eq!(
            reminder.subject(&config),
            "【LINE 禮物】收件地址填寫提醒（最後日期 11/14（六））"
        );

        config.subject = "滿 {days} 天提醒：{products}".to_string();
        config.body = "<b>{ordered}</b> 下單，訂單 {order_id}".to_string();
        // 主旨是純文字不跳脫，內文才跳脫。
        assert_eq!(
            reminder.subject(&config),
            "滿 8 天提醒：禮盒 <A&B>（紅 / L） ×2、卡片"
        );
        assert!(reminder
            .html(&config)
            .contains("<b>11/5（四）</b> 下單，訂單 1</div>"));
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    if let Err(message) = run().await {
        fail(&message);
    }
}
