//! Account-wide limits reported by the providers' own clients.

use serde_json::Value;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

#[derive(Clone, Debug, PartialEq)]
pub struct Window {
    pub remaining: f64,
    pub resets_at: i64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Usage {
    pub session: Option<Window>,
    pub weekly: Option<Window>,
    pub observed_at: i64,
    pub plan: Option<String>,
}

impl Usage {
    pub fn claude(payload: &Value, observed_at: i64) -> Self {
        let limits = &payload["rate_limits"];
        Self {
            session: window(&limits["five_hour"], "used_percentage"),
            weekly: window(&limits["seven_day"], "used_percentage"),
            observed_at,
            plan: None,
        }
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
        Some(usage)
    }

    pub fn update(&mut self, latest: Self) -> bool {
        if latest.observed_at < self.observed_at || *self == latest {
            return false;
        }
        *self = latest;
        true
    }
}

fn window(value: &Value, used_key: &str) -> Option<Window> {
    let used = value[used_key].as_f64()?;
    if !used.is_finite() || !(0.0..=100.0).contains(&used) {
        return None;
    }
    let resets_at = value["resets_at"].as_i64()?;
    (resets_at > 0).then_some(Window { remaining: 100.0 - used, resets_at })
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
    fn supplied_provider_samples_parse_without_requiring_unrelated_fields() {
        // https://code.claude.com/docs/en/statusline (rate_limits excerpt).
        let claude: Value = serde_json::from_str(include_str!("../tests/fixtures/claude-statusline.json")).unwrap();
        let claude = Usage::claude(&claude, 1);
        assert_eq!(claude.session.unwrap(), Window { remaining: 76.5, resets_at: 1738425600 });
        assert_eq!(claude.weekly.unwrap(), Window { remaining: 58.8, resets_at: 1738857600 });
        let codex = Usage::codex(include_str!("../tests/fixtures/codex-token-count-weekly-only.json")).unwrap();
        assert!(codex.session.is_none());
        assert_eq!(codex.weekly.unwrap(), Window { remaining: 90.0, resets_at: 1793783673 });
        assert_eq!(codex.plan.as_deref(), Some("self_serve_business_prolite"));
        let codex = Usage::codex(include_str!("../tests/fixtures/codex-token-count-5h-and-weekly.json")).unwrap();
        assert_eq!(codex.session.unwrap().remaining, 63.0);
        assert_eq!(codex.weekly.unwrap().remaining, 87.5);
    }

    #[test]
    fn claude_statusline_limits_and_missing_windows() {
        let payload = json!({"rate_limits": {
            "five_hour": {"used_percentage": 72.5, "resets_at": 1793207640},
            "seven_day": {"used_percentage": 94, "resets_at": 1793462400}
        }});
        let usage = Usage::claude(&payload, 1793199600);
        assert_eq!(usage.session.unwrap().remaining, 27.5);
        assert_eq!(usage.weekly.unwrap().remaining, 6.0);
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
        assert_eq!(usage.session.unwrap().remaining, 65.0);
        assert_eq!(usage.weekly.unwrap().remaining, 9.0);
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
        let window = Window { remaining: 20.0, resets_at: 1793207640 };
        assert_eq!(reset_label(&window, false, 1793199600), "resets in 2h 14m");
        assert_eq!(reset_label(&window, true, 1793199600), "resets Wed");
        assert_eq!(reset_label(&window, false, window.resets_at), "no data yet");
        assert_eq!(age_label(100, 99), "just now");
        assert_eq!(age_label(100, 220), "2m ago");
        assert_eq!(age_label(100, 7300), "2h ago");
        assert_eq!(age_label(100, 172900), "2d ago");
    }
}
