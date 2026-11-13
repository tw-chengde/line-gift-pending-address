//! 列出 LINE 禮物「已付款、收禮人尚未填寫地址」的訂單，並對下單滿 5 天、滿 8 天（可調整）的訂單
//! 寄 email 提醒送禮人。對 LINE 是唯讀的（只呼叫訂單清單與訂單詳情）。
//!
//! 環境變數：
//!   LINE_GIFT_GATEWAY_TOKEN  必填。
//!   LINE_GIFT_ENVIRONMENT    設為 beta 時改打測試環境。
//!   MAIL_SENDER              --send 時必填。用來寄信的 Workspace 信箱（服務帳號以全網域委派代表它）。
//!   MAIL_SUMMARY_TO          統整信收件人，可用逗號分隔多個；未設定時寄給 MAIL_SENDER 自己。
//!   MAIL_FROM_NAME           寄件人顯示名稱（選填）。
//!   MAIL_SERVICE_ACCOUNT     已設好全網域委派的服務帳號；未設定時用執行環境的服務帳號。
//!   REMINDER_DAYS            下單滿幾天要提醒，逗號分隔（預設 5,8）。
//!   DEADLINE_DAYS            最後填寫地址日期 = 下單日 + 這個天數（預設 9）。
//!   MAIL_SUBJECT / MAIL_BODY 提醒信的主旨與內文（HTML）範本，可用 {products} {deadline} 等代碼。
//!
//! 參數：
//!   --from / --to  台北時間的日期（YYYY-MM-DD），以訂單成立時間篩選；未指定時查最近 DEADLINE_DAYS + 1 天。
//!   --max          最多檢查幾筆已付款訂單（預設 200；每筆需呼叫一次訂單詳情）。
//!   --send         真的寄出提醒信與統整信；沒帶時只在輸出的 reminders 預覽，不寄任何信。
//!   --today        把「今天」當成指定日期（YYYY-MM-DD）來算滿幾天，補寄漏掉的那一天時用。
//!   --raw          額外印出每筆待填地址群組的原始 deliveryForm（含個資，確認回傳格式用）。
//!   --compact      輸出單行 JSON（在 Cloud Run Job 內會自動啟用，讓 Cloud Logging 視為一筆紀錄）。
//!
//! 程式不記錄寄過誰：是否提醒完全由「今天 − 下單日」決定，所以同一天執行兩次會寄兩次，
//! 某天沒執行則那天該提醒的訂單會被跳過（可用 --today 補寄）。

use std::{collections::HashMap, env, process::exit, time::Duration};

use base64::{
    engine::general_purpose::{STANDARD, URL_SAFE},
    Engine,
};
use chrono::{DateTime, Datelike, FixedOffset, NaiveDate, TimeDelta, Utc};
use futures::future::try_join_all;
use reqwest::{Client, Method, RequestBuilder};
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
const GMAIL_SEND_SCOPE: &str = "https://www.googleapis.com/auth/gmail.send";
const METADATA: &str =
    "http://metadata.google.internal/computeMetadata/v1/instance/service-accounts/default";

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

fn product_text(groups: &[Value]) -> String {
    groups
        .iter()
        .flat_map(|group| group["items"].as_array().into_iter().flatten())
        .map(|item| {
            let mut label = text(&item["name"], "商品");
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
    to: &str,
    subject: &str,
    html: &str,
) -> String {
    let from = match from_name {
        Some(name) => format!("{} <{from}>", encoded_word(name)),
        None => from.to_string(),
    };
    let body = STANDARD.encode(html);
    let body = body
        .as_bytes()
        .chunks(76)
        .map(|line| std::str::from_utf8(line).unwrap())
        .collect::<Vec<_>>()
        .join("\r\n");
    format!(
        "From: {from}\r\nTo: {to}\r\nSubject: {}\r\nMIME-Version: 1.0\r\n\
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

/// 以服務帳號的全網域委派代表 MAIL_SENDER，透過 Gmail API 寄信。
/// 不使用金鑰檔：由執行環境的服務帳號呼叫 IAM signJwt 幫自己簽 JWT，再換成 Gmail 權杖。
struct Mailer {
    client: Client,
    sender: String,
    from_name: Option<String>,
    summary_to: String,
    token: String,
}

impl Mailer {
    async fn connect(client: Client) -> Result<Self, String> {
        let var = env_text;
        let sender =
            var("MAIL_SENDER").ok_or("缺少 MAIL_SENDER 環境變數（用來寄信的 Workspace 信箱）。")?;
        if !valid_email(&sender) {
            return Err(format!("MAIL_SENDER {sender} 不是有效的 email。"));
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

        let platform_token = {
            let request = client
                .get(format!("{METADATA}/token"))
                .header("Metadata-Flavor", "Google");
            let token =
                google_json(request, "取得服務帳號權杖（寄信只能在 Cloud Run 上執行）").await?;
            text(&token["access_token"], "")
        };
        let service_account = match var("MAIL_SERVICE_ACCOUNT") {
            Some(account) => account,
            None => {
                let request = client
                    .get(format!("{METADATA}/email"))
                    .header("Metadata-Flavor", "Google");
                google_text(request, "取得服務帳號名稱")
                    .await?
                    .trim()
                    .to_string()
            }
        };

        let now = Utc::now().timestamp();
        let claims = json!({
            "iss": service_account,
            "sub": sender,
            "scope": GMAIL_SEND_SCOPE,
            "aud": "https://oauth2.googleapis.com/token",
            "iat": now,
            "exp": now + 3600,
        });
        let signed = google_json(
            client
                .post(format!(
                    "https://iamcredentials.googleapis.com/v1/projects/-/serviceAccounts/{}:signJwt",
                    encode_path_segment(&service_account)
                ))
                .bearer_auth(&platform_token)
                .json(&json!({ "payload": claims.to_string() })),
            "簽署 JWT（signJwt）",
        )
        .await?;
        let jwt = signed["signedJwt"]
            .as_str()
            .ok_or("signJwt 回應缺少 signedJwt。")?;
        let token = google_json(
            client
                .post("https://oauth2.googleapis.com/token")
                .header("Content-Type", "application/x-www-form-urlencoded")
                .body(format!(
                    "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Ajwt-bearer&assertion={jwt}"
                )),
            "換取 Gmail 權杖（請確認 Workspace 全網域委派設定）",
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
            summary_to,
            token,
        })
    }

    async fn send(&self, to: &str, subject: &str, html: &str) -> Result<(), String> {
        let message = mime_message(self.from_name.as_deref(), &self.sender, to, subject, html);
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

    let to = arg("to").map_or_else(Utc::now, |value| taipei_day(value, true));
    // 預設查詢範圍要涵蓋到最後填寫日期為止的訂單，多留一天給「今天」的時分秒。
    let from = arg("from").map_or_else(
        || to - TimeDelta::days(config.deadline_days + 1),
        |value| taipei_day(value, false),
    );
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
                                products: product_text(&groups),
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
            },
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
            "to@example.com",
            subject,
            &"好".repeat(200),
        );
        assert!(message.lines().all(|line| line.len() <= 76), "{message}");
        let (headers, body) = message.split_once("\r\n\r\n").unwrap();
        assert!(headers.contains("To: to@example.com\r\n"));
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
    fn product_text_and_html_escaping() {
        let groups = vec![json!({ "items": [
            { "name": "禮盒 <A&B>", "spec": "紅 / L", "qty": 2 },
            { "name": "卡片", "spec": "", "qty": 1 },
        ]})];
        let products = product_text(&groups);
        assert_eq!(products, "禮盒 <A&B>（紅 / L） ×2、卡片");
        let reminder = Reminder {
            order_id: "1".into(),
            ordered: NaiveDate::from_ymd_opt(2026, 11, 5).unwrap(),
            days: 8,
            deadline: NaiveDate::from_ymd_opt(2026, 11, 14).unwrap(),
            sender_name: String::new(),
            email: "a@b.com".into(),
            recipients: String::new(),
            products,
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
