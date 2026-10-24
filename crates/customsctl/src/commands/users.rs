//! `customsctl users ...`: quota and suspension management through the admin API.
//!
//! Unlike the audit commands these are online: the gateway owns the quota
//! counters, so the CLI asks it rather than editing `state.json` behind its
//! back.

use std::error::Error;
use std::time::Duration;

use chrono::{DateTime, Utc};
use clap::{Args, Subcommand};
use customapi_core::config;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::Value;

use crate::commands::emit;
use crate::ui::TabWriter;
use crate::util::{go_floats, sort_keys};

/// The admin client's timeout, matching the Go `http.Client{Timeout: 15s}`.
const TIMEOUT: Duration = Duration::from_secs(15);

/// The default admin base URL.
const DEFAULT_ADMIN: &str = "http://127.0.0.1:8081";

/// `customsctl users <command>`.
#[derive(Subcommand, Debug)]
pub enum Command {
    /// List users and their quota.
    List(ListArgs),
    /// Set a user's quota to an absolute value.
    Grant(ActionArgs),
    /// Add to a user's quota.
    Add(ActionArgs),
    /// Suspend a user.
    Suspend(ActionArgs),
    /// Resume a suspended user.
    Resume(ActionArgs),
}

/// Flags accepted by `users list`.
#[derive(Args, Debug, Clone)]
pub struct ListArgs {
    /// admin base URL
    #[arg(long, value_name = "URL", default_value = DEFAULT_ADMIN)]
    pub admin: String,
}

/// Flags accepted by `users grant|add|suspend|resume`.
#[derive(Args, Debug, Clone)]
pub struct ActionArgs {
    /// admin base URL
    #[arg(long, value_name = "URL", default_value = DEFAULT_ADMIN)]
    pub admin: String,
    /// user name
    #[arg(long, value_name = "NAME")]
    pub user: Option<String>,
    /// quota amount (units of 1e-6 currency)
    #[arg(long, value_name = "N", default_value_t = 0)]
    pub quota: i64,
    /// reset period for set_period
    #[arg(long, value_name = "PERIOD")]
    pub period: Option<String>,
}

/// One row of `GET /admin/users`.
///
/// Fields are defaulted the way Go's decoder leaves them when the gateway omits
/// them, so an older gateway still lists.
#[derive(Debug, Deserialize)]
struct AdminUser {
    #[serde(default)]
    name: String,
    #[serde(default)]
    trust: String,
    #[serde(default)]
    period: String,
    #[serde(default)]
    quota: i64,
    #[serde(default)]
    consumed: i64,
    #[serde(default)]
    requests: i64,
    #[serde(default)]
    spend: f64,
    #[serde(default = "config::zero_time")]
    last_seen: DateTime<Utc>,
}

impl Default for AdminUser {
    /// The Go zero value: every string empty and `LastSeen` at year 1, not at
    /// the Unix epoch a derived `Default` would produce.
    fn default() -> Self {
        AdminUser {
            name: String::new(),
            trust: String::new(),
            period: String::new(),
            quota: 0,
            consumed: 0,
            requests: 0,
            spend: 0.0,
            last_seen: config::zero_time(),
        }
    }
}

/// The response envelope of `GET /admin/users`.
#[derive(Debug, Default, Deserialize)]
struct UsersResponse {
    #[serde(default)]
    users: Vec<AdminUser>,
}

/// Builds the HTTP client used by every admin call.
fn client() -> Result<reqwest::Client, Box<dyn Error>> {
    Ok(reqwest::Client::builder().timeout(TIMEOUT).build()?)
}

/// `GET url` decoded into `T`, with the Go error texts for transport and
/// non-200 responses.
async fn get_json<T: DeserializeOwned>(url: &str) -> Result<T, Box<dyn Error>> {
    let resp = client()?
        .get(url)
        .send()
        .await
        .map_err(|e| format!("GET {url}: {e}"))?;
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    if status != 200 {
        return Err(format!("GET {url}: HTTP {status}: {}", body.trim()).into());
    }
    Ok(serde_json::from_str(&body)?)
}

/// `POST url` with a JSON body, decoded into `T`.
async fn post_json<T: DeserializeOwned>(url: &str, body: &Value) -> Result<T, Box<dyn Error>> {
    let payload = serde_json::to_vec(body)?;
    let resp = client()?
        .post(url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(payload)
        .send()
        .await
        .map_err(|e| format!("POST {url}: {e}"))?;
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    if status != 200 {
        return Err(format!("POST {url}: HTTP {status}: {}", text.trim()).into());
    }
    Ok(serde_json::from_str(&text)?)
}

/// Runs `users list`.
pub async fn list(args: &ListArgs) -> Result<(), Box<dyn Error>> {
    let url = format!("{}/admin/users", args.admin);
    let out: UsersResponse = get_json(&url).await?;
    let mut table = TabWriter::new();
    table.line("USER\tTRUST\tPERIOD\tQUOTA\tCONSUMED\tREMAINING\tREQUESTS\tSPEND\tLAST SEEN");
    for user in &out.users {
        let remaining = if user.quota > 0 {
            (user.quota - user.consumed).to_string()
        } else {
            "unlimited".to_string()
        };
        let last = if user.last_seen == config::zero_time() {
            "-".to_string()
        } else {
            user.last_seen.format("%m-%d %H:%M").to_string()
        };
        table.line(&format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{:.6}\t{}",
            user.name,
            user.trust,
            user.period,
            user.quota,
            user.consumed,
            remaining,
            user.requests,
            user.spend,
            last
        ));
    }
    emit(&table.finish());
    Ok(())
}

/// Runs one of the `users` mutations.
///
/// `action` is the gateway's action name, which is also what the command prints:
/// `add` posts `add_quota`, exactly like the Go CLI.
pub async fn action(args: &ActionArgs, action: &str) -> Result<(), Box<dyn Error>> {
    let user = args
        .user
        .clone()
        .filter(|u| !u.is_empty())
        .ok_or("--user is required")?;
    let mut body = serde_json::Map::new();
    body.insert("user".to_string(), Value::String(user));
    body.insert("action".to_string(), Value::String(action.to_string()));
    body.insert("quota".to_string(), Value::from(args.quota));
    if let Some(period) = args.period.clone().filter(|p| !p.is_empty()) {
        body.insert("period".to_string(), Value::String(period));
    }
    let mut body = Value::Object(body);
    // Go marshals a map, which sorts the keys; the request body is compared by
    // operators reading gateway logs.
    sort_keys(&mut body);

    let out: Value = post_json(&format!("{}/admin/users", args.admin), &body).await?;
    let mut user_value = out.get("user").cloned().unwrap_or(Value::Null);
    // Go decodes the response into `map[string]any` and re-marshals `out["user"]`,
    // so the printed object has sorted keys and no trailing `.0` on floats.
    sort_keys(&mut user_value);
    go_floats(&mut user_value);
    println!("{action}: {}", serde_json::to_string(&user_value)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn user_rows_keep_the_go_layout() {
        let user = AdminUser {
            name: "alice".to_string(),
            trust: "trusted".to_string(),
            period: "monthly".to_string(),
            quota: 0,
            consumed: 0,
            requests: 5,
            spend: 0.1234,
            last_seen: Utc.with_ymd_and_hms(2026, 3, 4, 13, 5, 6).unwrap(),
        };
        let remaining = if user.quota > 0 {
            (user.quota - user.consumed).to_string()
        } else {
            "unlimited".to_string()
        };
        let last = if user.last_seen == config::zero_time() {
            "-".to_string()
        } else {
            user.last_seen.format("%m-%d %H:%M").to_string()
        };
        assert_eq!(remaining, "unlimited");
        assert_eq!(last, "03-04 13:05");
        assert_eq!(
            format!(
                "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{:.6}\t{}",
                user.name,
                user.trust,
                user.period,
                user.quota,
                user.consumed,
                remaining,
                user.requests,
                user.spend,
                last
            ),
            "alice\ttrusted\tmonthly\t0\t0\tunlimited\t5\t0.123400\t03-04 13:05"
        );
    }

    #[test]
    fn a_missing_last_seen_prints_a_dash() {
        let user = AdminUser::default();
        assert_eq!(user.last_seen, config::zero_time());
    }

    #[test]
    fn action_body_is_sorted_and_drops_empty_period() {
        let mut body = serde_json::Map::new();
        body.insert("user".to_string(), Value::String("bob".to_string()));
        body.insert("action".to_string(), Value::String("add_quota".to_string()));
        body.insert("quota".to_string(), Value::from(500i64));
        let mut body = Value::Object(body);
        sort_keys(&mut body);
        assert_eq!(
            serde_json::to_string(&body).unwrap(),
            r#"{"action":"add_quota","quota":500,"user":"bob"}"#
        );
    }
}
