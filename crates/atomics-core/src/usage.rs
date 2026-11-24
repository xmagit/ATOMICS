//! Account-wide limits reported by the providers' own clients.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use time::{Date, OffsetDateTime, format_description::well_known::Rfc3339};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Window {
    /// Quota left, counting down from 100 to 0.
    // Keep the existing snapshot field for clients running an older version.
    #[serde(rename = "remaining", alias = "left_percent")]
    pub left_percent: f64,
    pub resets_at: i64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    /// Signed-in user login or email, e.g. `octocat` or `alex@acme.example`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// Team or organization name, e.g. `Acme Bank`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub org: Option<String>,
}

impl Account {
    pub fn is_empty(&self) -> bool {
        self.user.is_none() && self.org.is_none()
    }

    pub fn label(&self) -> Option<String> {
        match (self.user.as_deref(), self.org.as_deref()) {
            (Some(user), Some(org)) => Some(format!("{user} · {org}")),
            (Some(user), None) => Some(user.to_owned()),
            (None, Some(org)) => Some(org.to_owned()),
            (None, None) => None,
        }
    }
}

#[derive(Clone, Debug, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetUnit {
    #[default]
    AiCredits,
    PremiumRequests,
    Usd,
    #[serde(other)]
    Percent,
}

fn default_budget_label() -> String {
    "Plan".into()
}

/// Period budget for providers whose quota is a plan or enterprise allowance (Copilot, Cursor).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Budget {
    #[serde(default = "default_budget_label")]
    pub label: String,
    /// Quota left, counting down from 100 to 0.
    pub left_percent: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<f64>,
    #[serde(default)]
    pub unit: BudgetUnit,
    /// Unix timestamp in seconds when this budget resets, or 0 if not reported.
    #[serde(default)]
    pub resets_at: i64,
}

impl Budget {
    pub fn is_current(&self, now: i64) -> bool {
        self.left_percent.is_finite()
            && (0.0..=100.0).contains(&self.left_percent)
            && (self.resets_at == 0 || self.resets_at > now)
    }

    pub fn used_percent(&self) -> f64 {
        (100.0 - self.left_percent).clamp(0.0, 100.0)
    }

    pub fn amount_label(&self) -> Option<String> {
        let (used, limit) = (self.used?, self.limit?);
        if !used.is_finite() || !limit.is_finite() || limit <= 0.0 {
            return None;
        }
        match self.unit {
            BudgetUnit::AiCredits => Some(format!("{} / {} AIC", format_count(used), format_count(limit))),
            BudgetUnit::PremiumRequests => Some(format!("{} / {} requests", format_count(used), format_count(limit))),
            BudgetUnit::Usd => Some(format!("{} / {}", format_usd(used), format_usd(limit))),
            BudgetUnit::Percent => None,
        }
    }

    pub fn summary(&self, now: i64) -> String {
        let mut parts = vec![format!("{:.0}% used", self.used_percent())];
        if let Some(amount) = self.amount_label() {
            parts.push(amount);
        }
        if let Some(reset) = budget_reset_label(self.resets_at, now) {
            parts.push(reset);
        }
        format!("{}  {}", self.label, parts.join(" · "))
    }
}

pub fn provider_display_name(provider: &str) -> &str {
    match provider {
        "claude" => "Claude",
        "codex" => "Codex",
        "copilot" => "Copilot",
        "cursor" => "Cursor",
        other => other,
    }
}

pub fn is_budget_provider(provider: &str) -> bool {
    matches!(provider, "copilot" | "cursor")
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    pub session: Option<Window>,
    pub weekly: Option<Window>,
    pub observed_at: i64,
    pub plan: Option<String>,
    /// Provider-reported model/limit buckets. Never inferred from the model an agent happens to use.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub models: BTreeMap<String, ModelUsage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<Account>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<Budget>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelUsage {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub session: Option<Window>,
    pub weekly: Option<Window>,
    pub observed_at: i64,
}

impl Usage {
    /// Last provider reading for the chief. Expired windows remain visible as historical
    /// readings; an absent reading has no age, rather than appearing fresh or exhausted.
    pub fn chief_json(&self, now: i64) -> Value {
        let window = |window: &Option<Window>| {
            window.as_ref().map(|window| {
                serde_json::json!({
                    "left_percent": window.left_percent,
                    "resets_at": window.resets_at,
                    "expired": window.resets_at <= now,
                })
            })
        };
        let observed_at = (self.observed_at > 0).then_some(self.observed_at);
        let models: BTreeMap<_, _> = self
            .models
            .iter()
            .map(|(id, usage)| {
                (
                    id,
                    serde_json::json!({
                        "label":usage.label, "session":window(&usage.session), "weekly":window(&usage.weekly),
                        "observed_at":usage.observed_at, "age_secs":now.saturating_sub(usage.observed_at).max(0),
                    }),
                )
            })
            .collect();
        let budget = self.budget.as_ref().map(|budget| {
            serde_json::json!({
                "label": budget.label,
                "left_percent": budget.left_percent,
                "used": budget.used,
                "limit": budget.limit,
                "unit": budget.unit,
                "resets_at": (budget.resets_at > 0).then_some(budget.resets_at),
                "expired": budget.resets_at > 0 && budget.resets_at <= now,
            })
        });
        let account = self.account.as_ref().filter(|account| !account.is_empty()).map(|account| {
            serde_json::json!({
                "user": account.user,
                "org": account.org,
            })
        });
        serde_json::json!({
            "session": window(&self.session),
            "weekly": window(&self.weekly),
            "budget": budget,
            "account": account,
            "observed_at": observed_at,
            "age_secs": observed_at.map(|at| now.saturating_sub(at).max(0)),
            "plan": self.plan,
            "models": models,
        })
    }

    pub fn claude(payload: &Value, observed_at: i64) -> Self {
        let limits = &payload["rate_limits"];
        Self {
            session: window(&limits["five_hour"], "used_percentage"),
            weekly: window(&limits["seven_day"], "used_percentage"),
            observed_at,
            plan: None,
            models: BTreeMap::new(),
            account: None,
            budget: None,
        }
    }

    /// Parse GitHub Copilot's `GET /copilot_internal/user` payload.
    pub fn copilot_user(payload: &Value, observed_at: i64) -> Option<Self> {
        let user = non_empty_str(&payload["login"]).map(str::to_owned);
        let org = payload["organization_login_list"]
            .as_array()
            .and_then(|list| list.iter().find_map(non_empty_str))
            .or_else(|| {
                payload["organization_list"].as_array().and_then(|list| {
                    list.iter().find_map(|item| non_empty_str(&item["login"]).or_else(|| non_empty_str(&item["name"])))
                })
            })
            .map(str::to_owned);
        let account = Account { user, org };
        let account = (!account.is_empty()).then_some(account);
        let plan = non_empty_str(&payload["copilot_plan"]).map(str::to_owned);
        let resets_at = parse_copilot_reset(payload).unwrap_or(0);
        let budget = payload
            .get("quota_snapshots")
            .and_then(|snapshots| snapshots.get("premium_interactions"))
            .and_then(|snapshot| parse_copilot_budget(snapshot, resets_at));
        if account.is_none() && budget.is_none() && plan.is_none() {
            return None;
        }
        Some(Self { observed_at, plan, account, budget, ..Self::default() })
    }

    /// Parse Cursor's `DashboardService/GetMe` and `DashboardService/GetCurrentPeriodUsage` payloads.
    pub fn cursor(me: Option<&Value>, period: Option<&Value>, observed_at: i64) -> Option<Self> {
        let account = me.and_then(|me| {
            let user = non_empty_str(&me["email"]).map(str::to_owned);
            let org = non_empty_str(&me["teamName"]).map(str::to_owned);
            let account = Account { user, org };
            (!account.is_empty()).then_some(account)
        });
        let plan = me.and_then(|me| {
            non_empty_str(&me["membershipType"])
                .map(str::to_owned)
                .or_else(|| (me["isEnterpriseUser"].as_bool() == Some(true)).then(|| "enterprise".into()))
        });
        let budget = period.and_then(parse_cursor_budget);
        if account.is_none() && budget.is_none() && plan.is_none() {
            return None;
        }
        Some(Self { observed_at, plan, account, budget, ..Self::default() })
    }

    /// Parse `agent status --format json` and `agent about --format json` (account metadata without budget).
    pub fn cursor_cli(status: Option<&Value>, about: Option<&Value>, observed_at: i64) -> Option<Self> {
        let info = status.and_then(|s| s.get("userInfo"));
        let user = info
            .and_then(|u| non_empty_str(&u["email"]))
            .or_else(|| about.and_then(|a| non_empty_str(&a["userEmail"])))
            .map(str::to_owned);
        let org = info.and_then(|u| non_empty_str(&u["teamName"])).map(str::to_owned);
        let account = Account { user, org };
        let account = (!account.is_empty()).then_some(account);
        let plan = about.and_then(|a| non_empty_str(&a["subscriptionTier"])).map(str::to_owned);
        if account.is_none() && plan.is_none() {
            return None;
        }
        Some(Self { observed_at, plan, account, budget: None, ..Self::default() })
    }

    pub fn codex(line: &str) -> Option<Self> {
        let event: Value = serde_json::from_str(line).ok()?;
        if event["type"] != "event_msg" || event["payload"]["type"] != "token_count" {
            return None;
        }
        let limits = event["payload"].get("rate_limits")?.as_object()?;
        let observed_at = OffsetDateTime::parse(event["timestamp"].as_str()?, &Rfc3339).ok()?.unix_timestamp();
        let mut usage = Self {
            observed_at,
            plan: limits.get("plan_type").and_then(Value::as_str).map(str::to_owned),
            ..Self::default()
        };
        for name in ["primary", "secondary"] {
            let Some(value) = limits.get(name) else { continue };
            match value["window_minutes"].as_u64() {
                Some(300) => usage.session = window(value, "used_percent"),
                Some(10080) => usage.weekly = window(value, "used_percent"),
                _ => {}
            }
        }
        let id = limits.get("limit_id").and_then(Value::as_str).unwrap_or("codex");
        let model = limits.get("normal_model_slug").and_then(Value::as_str).filter(|model| !model.is_empty());
        if id != "codex" || model.is_some() {
            let label = ["normal_model_slug", "limit_name"]
                .into_iter()
                .find_map(|key| limits.get(key).and_then(Value::as_str).filter(|name| !name.is_empty()))
                .unwrap_or(id);
            usage.models.insert(
                id.into(),
                ModelUsage {
                    label: label.into(),
                    model: model.map(str::to_owned),
                    session: usage.session.clone(),
                    weekly: usage.weekly.clone(),
                    observed_at,
                },
            );
            // A non-default model bucket is not a new account-wide reading.
            if id != "codex" {
                usage.session = None;
                usage.weekly = None;
                usage.observed_at = 0;
            }
        }
        Some(usage)
    }

    pub fn update(&mut self, mut latest: Self) -> bool {
        let mut changed = false;
        for (id, reading) in std::mem::take(&mut latest.models) {
            if !self.models.contains_key(&id) && self.models.len() >= 32 {
                continue;
            }
            if self.models.get(&id).is_none_or(|old| reading.observed_at >= old.observed_at && *old != reading) {
                self.models.insert(id, reading);
                changed = true;
            }
        }
        if latest.observed_at >= self.observed_at {
            latest.models = std::mem::take(&mut self.models);
            changed |= *self != latest;
            *self = latest;
        }
        changed
    }
}

fn non_empty_str(value: &Value) -> Option<&str> {
    value.as_str().map(str::trim).filter(|s| !s.is_empty())
}

fn json_f64(value: &Value) -> Option<f64> {
    value.as_f64().or_else(|| value.as_str()?.trim().parse::<f64>().ok()).filter(|n| n.is_finite())
}

fn json_i64(value: &Value) -> Option<i64> {
    value.as_i64().or_else(|| value.as_str()?.trim().parse::<i64>().ok())
}

fn parse_copilot_reset(payload: &Value) -> Option<i64> {
    if let Some(rfc) = non_empty_str(&payload["quota_reset_date_utc"])
        && let Ok(dt) = OffsetDateTime::parse(rfc, &Rfc3339)
    {
        return Some(dt.unix_timestamp());
    }
    let ymd = non_empty_str(&payload["quota_reset_date"])?;
    let mut parts = ymd.split('-');
    let year: i32 = parts.next()?.parse().ok()?;
    let month: u8 = parts.next()?.parse().ok()?;
    let day: u8 = parts.next()?.parse().ok()?;
    let month = time::Month::try_from(month).ok()?;
    let date = Date::from_calendar_date(year, month, day).ok()?;
    Some(date.midnight().assume_utc().unix_timestamp())
}

fn parse_copilot_budget(snapshot: &Value, resets_at: i64) -> Option<Budget> {
    if snapshot["unlimited"].as_bool() == Some(true) {
        return None;
    }
    let entitlement = json_f64(&snapshot["entitlement"]).filter(|&e| e > 0.0)?;
    let token_based = snapshot["token_based_billing"].as_bool() == Some(true);
    let unit = if token_based { BudgetUnit::AiCredits } else { BudgetUnit::PremiumRequests };
    let remaining = json_f64(&snapshot["quota_remaining"]).or_else(|| json_f64(&snapshot["remaining"]));
    let used = if token_based {
        json_f64(&snapshot["credits_used"])
            .filter(|&u| u >= 0.0)
            .or_else(|| remaining.map(|r| (entitlement - r).max(0.0)))
    } else {
        remaining.map(|r| (entitlement - r).max(0.0))
    };
    let left_percent = json_f64(&snapshot["percent_remaining"])
        .filter(|p| (0.0..=100.0).contains(p))
        .or_else(|| used.map(|u| ((entitlement - u) / entitlement * 100.0).clamp(0.0, 100.0)))
        .or_else(|| remaining.map(|r| (r / entitlement * 100.0).clamp(0.0, 100.0)))?;
    Some(Budget { label: default_budget_label(), left_percent, used, limit: Some(entitlement), unit, resets_at })
}

fn parse_cursor_budget(period: &Value) -> Option<Budget> {
    let resets_at = json_i64(&period["billingCycleEnd"]).filter(|&ms| ms > 0).map_or(0, |ms| ms / 1000);
    if let Some(spend) = period.get("spendLimitUsage") {
        for (limit_key, used_key, rem_key) in [
            ("overallLimit", "overallUsed", "overallRemaining"),
            ("individualLimit", "individualUsed", "individualRemaining"),
        ] {
            if let Some(limit_cents) = json_f64(&spend[limit_key]).filter(|&v| v > 0.0) {
                let used_cents = json_f64(&spend[used_key])
                    .filter(|&v| v >= 0.0)
                    .or_else(|| json_f64(&spend[rem_key]).map(|r| (limit_cents - r).max(0.0)));
                let rem_cents = json_f64(&spend[rem_key])
                    .filter(|&v| v >= 0.0)
                    .or_else(|| used_cents.map(|u| (limit_cents - u).max(0.0)));
                let Some(rem_cents) = rem_cents else { continue };
                let left_percent = (rem_cents / limit_cents * 100.0).clamp(0.0, 100.0);
                return Some(Budget {
                    label: default_budget_label(),
                    left_percent,
                    used: used_cents.map(|c| c / 100.0),
                    limit: Some(limit_cents / 100.0),
                    unit: BudgetUnit::Usd,
                    resets_at,
                });
            }
        }
    }
    if let Some(plan) = period.get("planUsage")
        && let Some(limit_cents) = json_f64(&plan["limit"]).filter(|&v| v > 0.0)
    {
        let used_cents =
            json_f64(&plan["includedSpend"]).or_else(|| json_f64(&plan["totalSpend"])).filter(|&v| v >= 0.0);
        let left_percent = json_f64(&plan["totalPercentUsed"])
            .filter(|p| (0.0..=100.0).contains(p))
            .map(|used_pct| (100.0 - used_pct).clamp(0.0, 100.0))
            .or_else(|| used_cents.map(|u| ((limit_cents - u) / limit_cents * 100.0).clamp(0.0, 100.0)))?;
        return Some(Budget {
            label: default_budget_label(),
            left_percent,
            used: used_cents.map(|c| c / 100.0),
            limit: Some(limit_cents / 100.0),
            unit: BudgetUnit::Usd,
            resets_at,
        });
    }
    None
}

pub fn budget_reset_label(resets_at: i64, now: i64) -> Option<String> {
    if resets_at <= 0 || resets_at <= now {
        return None;
    }
    let remaining = resets_at - now;
    if remaining < 86_400 {
        let minutes = (remaining + 59) / 60;
        return Some(format!("resets in {}h {}m", minutes / 60, minutes % 60));
    }
    let date = OffsetDateTime::from_unix_timestamp(resets_at).ok()?;
    let month = &date.month().to_string()[..3];
    Some(format!("resets {month} {}", date.day()))
}

fn format_grouped_int(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

fn format_count(value: f64) -> String {
    let rounded = value.round();
    if (value - rounded).abs() < 0.05 && rounded >= 0.0 {
        format_grouped_int(rounded as u64)
    } else {
        format!("{value:.1}")
    }
}

fn format_usd(dollars: f64) -> String {
    let cents = (dollars * 100.0).round() as i64;
    let neg = cents < 0;
    let abs_cents = cents.unsigned_abs();
    let whole = abs_cents / 100;
    let rem = abs_cents % 100;
    let prefix = if neg { "-$" } else { "$" };
    if rem == 0 {
        format!("{prefix}{}", format_grouped_int(whole))
    } else {
        format!("{prefix}{}.{rem:02}", format_grouped_int(whole))
    }
}

fn window(value: &Value, used_key: &str) -> Option<Window> {
    let used = value[used_key].as_f64()?;
    if !used.is_finite() || !(0.0..=100.0).contains(&used) {
        return None;
    }
    let resets_at = value["resets_at"].as_i64()?;
    (resets_at > 0).then_some(Window { left_percent: 100.0 - used, resets_at })
}

pub fn reset_label(window: &Window, weekly: bool, now: i64) -> String {
    if window.resets_at <= now {
        return "no data yet".into();
    }
    if weekly && let Ok(date) = OffsetDateTime::from_unix_timestamp(window.resets_at) {
        return format!("resets {}", &date.weekday().to_string()[..3]);
    }
    let minutes = (window.resets_at - now + 59) / 60;
    format!("resets in {}h {}m", minutes / 60, minutes % 60)
}

pub fn age_label(observed_at: i64, now: i64) -> String {
    let seconds = now.saturating_sub(observed_at).max(0);
    match seconds {
        0..60 => "just now".into(),
        60..3600 => format!("{}m ago", seconds / 60),
        3600..86400 => format!("{}h ago", seconds / 3600),
        _ => format!("{}d ago", seconds / 86400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn chief_reading_exposes_quota_left_source_age_and_expiry_without_inventing_missing_data() {
        let usage = Usage::claude(
            &json!({"rate_limits": {
                "five_hour": {"used_percentage": 72.5, "resets_at": 150},
                "seven_day": {"used_percentage": 10, "resets_at": 300}
            }}),
            100,
        );
        let reading = usage.chief_json(200);
        assert_eq!(reading["session"], json!({"left_percent":27.5, "resets_at":150, "expired":true}));
        assert_eq!(reading["weekly"], json!({"left_percent":90.0, "resets_at":300, "expired":false}));
        assert_eq!(reading["observed_at"], 100);
        assert_eq!(reading["age_secs"], 100);
        assert_eq!(usage.chief_json(99)["age_secs"], 0, "clock skew cannot make a negative age");
        let missing = Usage::default().chief_json(200);
        for key in ["session", "weekly", "observed_at", "age_secs"] {
            assert!(missing[key].is_null(), "{key}: {missing}");
        }
        let legacy: Window = serde_json::from_value(json!({"remaining":27.5,"resets_at":150})).unwrap();
        assert_eq!(legacy.left_percent, 27.5);
        assert_eq!(serde_json::to_value(legacy).unwrap(), json!({"remaining":27.5,"resets_at":150}));
        let named: Window = serde_json::from_value(json!({"left_percent":27.5,"resets_at":150})).unwrap();
        assert_eq!(named.left_percent, 27.5);
    }

    #[test]
    fn model_limits_stay_separate_from_the_account_and_each_other() {
        let event = |id: &str, model: &str, used: f64, timestamp: &str| {
            json!({
                "timestamp":timestamp, "type":"event_msg", "payload": {"type":"token_count", "rate_limits": {
                    "limit_id":id, "normal_model_slug":model,
                    "primary":{"used_percent":used, "window_minutes":300, "resets_at":2000000000}
                }}
            })
            .to_string()
        };
        let mut usage = Usage::codex(&event("codex", "main-model", 10.0, "2026-10-28T15:00:00Z")).unwrap();
        usage.update(Usage::codex(&event("codex_other", "cheap-model", 20.0, "2026-10-28T15:00:02Z")).unwrap());
        usage.update(Usage::codex(&event("codex_premium", "expensive-model", 80.0, "2026-10-28T15:00:01Z")).unwrap());
        let reading = usage.chief_json(1793199610);
        assert_eq!(reading["session"]["left_percent"], 90.0);
        assert_eq!(reading["models"]["codex"]["label"], "main-model");
        assert_eq!(reading["models"]["codex_other"]["label"], "cheap-model");
        assert_eq!(reading["models"]["codex_other"]["session"]["left_percent"], 80.0);
        assert_eq!(reading["models"]["codex_premium"]["session"]["left_percent"], 20.0);
        assert_eq!(reading["models"]["codex_premium"]["age_secs"], 9);
        assert!(
            !usage.update(Usage::codex(&event("codex_other", "cheap-model", 99.0, "2026-10-28T15:00:00Z")).unwrap()),
            "an older model reading cannot replace its newer reading"
        );
        let claude = Usage::claude(
            &json!({"model":{"id":"claude-opus"}, "rate_limits": {
                "seven_day":{"used_percentage":75,"resets_at":2000000000}
            }}),
            100,
        );
        assert!(claude.models.is_empty(), "a current model does not turn shared quotas into model quotas");
        assert_eq!(Usage::default().chief_json(0)["models"], json!({}));
    }

    #[test]
    fn documented_claude_and_captured_codex_samples_parse_without_unrelated_fields() {
        // https://code.claude.com/docs/en/statusline (rate_limits excerpt).
        let claude: Value = serde_json::from_str(include_str!("../tests/fixtures/claude-statusline.json")).unwrap();
        let claude = Usage::claude(&claude, 1);
        assert_eq!(claude.session.unwrap(), Window { left_percent: 76.5, resets_at: 1738425600 });
        assert_eq!(claude.weekly.unwrap(), Window { left_percent: 58.8, resets_at: 1738857600 });
        let codex = Usage::codex(include_str!("../tests/fixtures/codex-token-count-weekly-only.json")).unwrap();
        assert!(codex.session.is_none());
        assert_eq!(codex.weekly.unwrap(), Window { left_percent: 90.0, resets_at: 1793783673 });
        assert_eq!(codex.plan.as_deref(), Some("self_serve_business_prolite"));
        let codex = Usage::codex(include_str!("../tests/fixtures/codex-token-count-5h-and-weekly.json")).unwrap();
        assert_eq!(codex.session.unwrap().left_percent, 63.0);
        assert_eq!(codex.weekly.unwrap().left_percent, 87.5);
    }

    #[test]
    fn claude_statusline_limits_and_missing_windows() {
        let payload = json!({"rate_limits": {
            "five_hour": {"used_percentage": 72.5, "resets_at": 1793207640},
            "seven_day": {"used_percentage": 94, "resets_at": 1793462400}
        }});
        let usage = Usage::claude(&payload, 1793199600);
        assert_eq!(usage.session.unwrap().left_percent, 27.5);
        assert_eq!(usage.weekly.unwrap().left_percent, 6.0);
        assert!(Usage::claude(&json!({}), 1).session.is_none());
        assert!(Usage::claude(&json!({"rate_limits":{"five_hour":null}}), 1).session.is_none());
        assert!(
            Usage::claude(&json!({"rate_limits":{"five_hour":{"used_percentage":101,"resets_at":5}}}), 1)
                .session
                .is_none()
        );
        assert!(Usage::claude(&json!({"rate_limits":{"five_hour":{"used_percentage":20}}}), 1).session.is_none());
    }

    #[test]
    fn codex_token_count_maps_window_minutes_and_preserves_source_age() {
        let event = json!({"timestamp":"2026-10-28T15:00:00.123Z", "type":"event_msg", "payload": {
            "type":"token_count", "info":null, "rate_limits": {
                "primary":{"used_percent":91,"window_minutes":10080,"resets_at":1793462400},
                "secondary":{"used_percent":35,"window_minutes":300,"resets_at":1793207640},
                "plan_type":"plus"
            }
        }});
        let usage = Usage::codex(&event.to_string()).unwrap();
        assert_eq!(usage.session.unwrap().left_percent, 65.0);
        assert_eq!(usage.weekly.unwrap().left_percent, 9.0);
        assert_eq!(usage.observed_at, 1793199600);
        assert_eq!(usage.plan.as_deref(), Some("plus"));
        assert!(Usage::codex(r#"{"type":"event_msg","payload":{"type":"token_count","rate_limits":null}}"#).is_none());
        assert!(Usage::codex("partial json").is_none());
        let mut unknown = event;
        unknown["payload"]["rate_limits"]["primary"]["window_minutes"] = json!(60);
        unknown["payload"]["rate_limits"]["secondary"] = Value::Null;
        assert!(Usage::codex(&unknown.to_string()).unwrap().weekly.is_none());
    }

    #[test]
    fn latest_account_report_wins_and_can_remove_a_window() {
        let mut usage = Usage::claude(&json!({"rate_limits":{"five_hour":{"used_percentage":20,"resets_at":100}}}), 50);
        assert!(!usage.update(Usage { observed_at: 49, ..Usage::default() }));
        assert!(usage.update(Usage { observed_at: 51, ..Usage::default() }));
        assert!(usage.session.is_none());
        assert!(!usage.update(usage.clone()));
    }

    #[test]
    fn reset_and_age_labels_handle_expiry_weekdays_and_clock_skew() {
        let window = Window { left_percent: 20.0, resets_at: 1793207640 };
        assert_eq!(reset_label(&window, false, 1793199600), "resets in 2h 14m");
        assert_eq!(reset_label(&window, true, 1793199600), "resets Wed");
        assert_eq!(reset_label(&window, false, window.resets_at), "no data yet");
        assert_eq!(age_label(100, 99), "just now");
        assert_eq!(age_label(100, 220), "2m ago");
        assert_eq!(age_label(100, 7300), "2h ago");
        assert_eq!(age_label(100, 172900), "2d ago");
    }

    #[test]
    fn copilot_fixture_parses_account_and_aic_plan_budget() {
        let payload: Value = serde_json::from_str(include_str!("../tests/fixtures/copilot-internal-user-aic.json"))
            .expect("copilot aic fixture");
        let observed = 1_795_520_000;
        let usage = Usage::copilot_user(&payload, observed).expect("copilot usage");
        assert_eq!(usage.account, Some(Account { user: Some("octocat_acme".into()), org: Some("acme-org".into()) }));
        assert_eq!(usage.plan.as_deref(), Some("business"));
        assert_eq!(usage.observed_at, observed);
        let budget = usage.budget.as_ref().expect("budget");
        assert_eq!(budget.label, "Plan");
        assert_eq!(budget.left_percent, 47.0);
        assert_eq!(budget.used, Some(15884.0));
        assert_eq!(budget.limit, Some(30000.0));
        assert_eq!(budget.unit, BudgetUnit::AiCredits);
        assert_eq!(budget.resets_at, 1_796_083_200);
        assert_eq!(budget.summary(observed), "Plan  53% used · 15,884 / 30,000 AIC · resets Dec 1");

        let chief = usage.chief_json(observed + 60);
        assert_eq!(chief["budget"]["left_percent"], 47.0);
        assert_eq!(chief["budget"]["used"], 15884.0);
        assert_eq!(chief["budget"]["limit"], 30000.0);
        assert_eq!(chief["budget"]["expired"], false);
        assert_eq!(chief["account"], json!({"user": "octocat_acme", "org": "acme-org"}));
    }

    #[test]
    fn copilot_fixture_with_no_reported_budget_keeps_account_without_inventing_quota() {
        let payload: Value =
            serde_json::from_str(include_str!("../tests/fixtures/copilot-internal-user-no-budget.json"))
                .expect("copilot no-budget fixture");
        let usage = Usage::copilot_user(&payload, 1_795_520_000).expect("account-only usage");
        assert_eq!(
            usage.account,
            Some(Account { user: Some("octocat_unlimited".into()), org: Some("acme-org".into()) })
        );
        assert_eq!(usage.plan.as_deref(), Some("enterprise"));
        assert!(usage.budget.is_none(), "unlimited/unreported account must not invent a budget");
        assert!(usage.chief_json(1_795_520_010)["budget"].is_null());
    }

    #[test]
    fn copilot_legacy_premium_requests_and_date_only_reset_parse() {
        let payload = json!({
            "login": "legacy_user",
            "copilot_plan": "individual",
            "quota_reset_date": "2026-12-01",
            "quota_snapshots": {
                "premium_interactions": {
                    "entitlement": 300,
                    "quota_remaining": 180.0,
                    "percent_remaining": 60.0,
                    "unlimited": false,
                    "token_based_billing": false
                }
            }
        });
        let usage = Usage::copilot_user(&payload, 1_795_520_000).unwrap();
        let budget = usage.budget.unwrap();
        assert_eq!(budget.left_percent, 60.0);
        assert_eq!(budget.unit, BudgetUnit::PremiumRequests);
        assert_eq!(budget.summary(1_795_520_000), "Plan  40% used · 120 / 300 requests · resets Dec 1");
    }

    #[test]
    fn cursor_enterprise_fixture_parses_account_team_and_usd_budget() {
        let fixture: Value =
            serde_json::from_str(include_str!("../tests/fixtures/cursor-enterprise-usage.json")).unwrap();
        let observed = 1_795_520_000;
        let usage = Usage::cursor(Some(&fixture["me"]), Some(&fixture["usage"]), observed).expect("cursor usage");
        assert_eq!(
            usage.account,
            Some(Account { user: Some("alex@acme.example".into()), org: Some("Acme Bank".into()) })
        );
        assert_eq!(usage.plan.as_deref(), Some("enterprise"));
        let budget = usage.budget.as_ref().expect("enterprise budget");
        assert!((budget.left_percent - (28057.0 / 30000.0 * 100.0)).abs() < 1e-6);
        assert_eq!(budget.used, Some(19.43));
        assert_eq!(budget.limit, Some(300.0));
        assert_eq!(budget.unit, BudgetUnit::Usd);
        assert_eq!(budget.resets_at, 1_796_083_200);
        assert_eq!(budget.summary(observed), "Plan  6% used · $19.43 / $300 · resets Dec 1");

        let chief = usage.chief_json(observed);
        assert!((chief["budget"]["left_percent"].as_f64().unwrap() - budget.left_percent).abs() < 1e-6);
        assert_eq!(chief["account"], json!({"user": "alex@acme.example", "org": "Acme Bank"}));
    }

    #[test]
    fn cursor_cli_and_missing_budget_fixtures_preserve_account_without_guessing() {
        let fixture: Value =
            serde_json::from_str(include_str!("../tests/fixtures/cursor-status-no-budget.json")).unwrap();
        let observed = 1_795_520_000;
        let cli_only =
            Usage::cursor_cli(Some(&fixture["status"]), Some(&fixture["about"]), observed).expect("cursor cli");
        assert_eq!(
            cli_only.account,
            Some(Account { user: Some("alex@acme.example".into()), org: Some("Acme Bank".into()) })
        );
        assert_eq!(cli_only.plan.as_deref(), Some("Enterprise"));
        assert!(cli_only.budget.is_none());

        // Enterprise payload with zeroed planUsage and no spendLimitUsage overall/individual limit must not guess.
        let empty_period = json!({
            "billingCycleEnd": "1796083200000",
            "planUsage": { "autoPercentUsed": 0, "apiPercentUsed": 0, "totalPercentUsed": 0 },
            "spendLimitUsage": { "limitType": "team" }
        });
        let api_no_budget = Usage::cursor(
            Some(&json!({"email": "alex@acme.example", "teamName": "Acme Bank", "membershipType": "enterprise"})),
            Some(&empty_period),
            observed,
        )
        .unwrap();
        assert_eq!(api_no_budget.account.as_ref().unwrap().label().as_deref(), Some("alex@acme.example · Acme Bank"));
        assert!(api_no_budget.budget.is_none(), "missing spend limit must not invent a budget from planUsage");
    }
}
