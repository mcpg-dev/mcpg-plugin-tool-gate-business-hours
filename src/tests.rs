use chrono::{DateTime, TimeZone, Utc};
use chrono_tz::Tz;
use mcpg_plugin_protocol::{GateDecision, PluginContext, PluginIdentity};
use mcpg_plugin_sdk::ffi::SyncToolGate;
use serde_json::json;

use super::BusinessHoursPlugin;

// Reference weekdays (UTC-stable anchor): 2024-01-01 was a Monday, so
//   Mon 2024-01-01, Wed 2024-01-03, Fri 2024-01-05, Sat 2024-01-06, Sun 2024-01-07.

fn ctx() -> PluginContext {
    PluginContext {
        request_id: "t".into(),
        session_id: None,
        tool_name: "x".into(),
        surface: "tool".into(),
        identity: PluginIdentity {
            kind: "anonymous".into(),
            trust_level: "unauthenticated".into(),
            subject_id: None,
            auth_provider: None,
            issuer: None,
            roles: Vec::new(),
            groups: Vec::new(),
            scopes: Vec::new(),
            attributes: Default::default(),
        },
        transport: "http".into(),
    }
}

fn plugin(cfg: serde_json::Value) -> BusinessHoursPlugin {
    BusinessHoursPlugin::from_config_json(&cfg.to_string())
}

/// Standard mon–fri 09:00–17:00 America/New_York gate.
fn mon_fri_9_5() -> BusinessHoursPlugin {
    plugin(json!({
        "timezone": "America/New_York",
        "windows": [
            { "days": ["mon", "tue", "wed", "thu", "fri"], "start": "09:00", "end": "17:00" }
        ]
    }))
}

/// A `DateTime` in the plugin's own timezone.
fn at(p: &BusinessHoursPlugin, y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Tz> {
    p.tz.with_ymd_and_hms(y, mo, d, h, mi, 0).single().unwrap()
}

fn deny_of(d: GateDecision) -> (u16, i32, String) {
    match d {
        GateDecision::Deny {
            http_status,
            code,
            message,
            ..
        } => (http_status, code, message),
        other => panic!("expected Deny, got {other:?}"),
    }
}

#[test]
fn weekday_inside_window_is_open() {
    let p = mon_fri_9_5();
    // Wed 2024-01-03 12:00 EST.
    assert!(p.is_open(at(&p, 2024, 1, 3, 12, 0)));
}

#[test]
fn before_window_is_closed() {
    let p = mon_fri_9_5();
    assert!(!p.is_open(at(&p, 2024, 1, 3, 8, 59)));
}

#[test]
fn start_is_inclusive_end_is_exclusive() {
    let p = mon_fri_9_5();
    assert!(p.is_open(at(&p, 2024, 1, 3, 9, 0)), "09:00 is inclusive");
    assert!(!p.is_open(at(&p, 2024, 1, 3, 17, 0)), "17:00 is exclusive");
    assert!(p.is_open(at(&p, 2024, 1, 3, 16, 59)));
}

#[test]
fn weekend_is_closed() {
    let p = mon_fri_9_5();
    // Sat 2024-01-06 and Sun 2024-01-07, both midday.
    assert!(!p.is_open(at(&p, 2024, 1, 6, 12, 0)));
    assert!(!p.is_open(at(&p, 2024, 1, 7, 12, 0)));
}

#[test]
fn end_of_day_window_covers_last_minute() {
    let p = plugin(json!({
        "timezone": "UTC",
        "windows": [{ "days": ["mon"], "start": "00:00", "end": "24:00" }]
    }));
    // Mon 2024-01-01: both the first and last minute are open.
    assert!(p.is_open(at(&p, 2024, 1, 1, 0, 0)));
    assert!(p.is_open(at(&p, 2024, 1, 1, 23, 59)));
    // Tue is still closed (not in the day set).
    assert!(!p.is_open(at(&p, 2024, 1, 2, 12, 0)));
}

#[test]
fn blackout_date_denies_inside_window() {
    let p = plugin(json!({
        "timezone": "America/New_York",
        "windows": [{ "days": ["wed"], "start": "09:00", "end": "17:00" }],
        "blackout_dates": ["2024-01-03"]
    }));
    // Inside the window by time/weekday, but the date is blacked out.
    assert!(!p.is_open(at(&p, 2024, 1, 3, 12, 0)));
    // The following Wednesday is fine.
    assert!(p.is_open(at(&p, 2024, 1, 10, 12, 0)));
}

#[test]
fn multiple_windows_model_a_lunch_gap() {
    let p = plugin(json!({
        "timezone": "UTC",
        "windows": [
            { "days": ["mon"], "start": "09:00", "end": "12:00" },
            { "days": ["mon"], "start": "13:00", "end": "17:00" }
        ]
    }));
    assert!(p.is_open(at(&p, 2024, 1, 1, 10, 0)), "morning");
    assert!(!p.is_open(at(&p, 2024, 1, 1, 12, 30)), "lunch gap");
    assert!(p.is_open(at(&p, 2024, 1, 1, 14, 0)), "afternoon");
}

#[test]
fn timezone_is_respected() {
    let p = mon_fri_9_5();
    // 2024-01-03 14:00 UTC == 09:00 EST → open; 13:00 UTC == 08:00 EST → closed.
    let open = Utc.with_ymd_and_hms(2024, 1, 3, 14, 0, 0).single().unwrap();
    let closed = Utc.with_ymd_and_hms(2024, 1, 3, 13, 0, 0).single().unwrap();
    assert!(p.is_open(open.with_timezone(&p.tz)));
    assert!(!p.is_open(closed.with_timezone(&p.tz)));
}

#[test]
fn case_insensitive_days() {
    let p = plugin(json!({
        "timezone": "UTC",
        "windows": [{ "days": ["WED", "Thu"], "start": "09:00", "end": "17:00" }]
    }));
    assert!(p.is_open(at(&p, 2024, 1, 3, 12, 0)), "Wed");
    assert!(p.is_open(at(&p, 2024, 1, 4, 12, 0)), "Thu");
    assert!(!p.is_open(at(&p, 2024, 1, 5, 12, 0)), "Fri not listed");
}

#[test]
fn default_deny_carries_status_code_and_tz() {
    let p = mon_fri_9_5();
    let (status, code, msg) = deny_of(p.deny());
    assert_eq!(status, 403);
    assert_eq!(code, -32030);
    assert!(msg.contains("America/New_York"), "{msg}");
}

#[test]
fn custom_deny_fields_are_honoured() {
    let p = plugin(json!({
        "timezone": "UTC",
        "windows": [{ "days": ["mon"], "start": "09:00", "end": "17:00" }],
        "deny_code": -32000,
        "deny_http_status": 429,
        "deny_message": "Closed for the day"
    }));
    let (status, code, msg) = deny_of(p.deny());
    assert_eq!(status, 429);
    assert_eq!(code, -32000);
    assert_eq!(msg, "Closed for the day");
}

#[test]
fn always_open_config_allows_via_evaluate_pre() {
    // Every day, all day → evaluate_pre (which reads the wall clock) must Allow
    // regardless of when the suite runs.
    let p = plugin(json!({
        "timezone": "UTC",
        "windows": [
            { "days": ["mon", "tue", "wed", "thu", "fri", "sat", "sun"], "start": "00:00", "end": "24:00" }
        ]
    }));
    assert!(matches!(
        p.evaluate_pre(&ctx(), &json!({}), None, &json!({})),
        GateDecision::Allow { .. }
    ));
}

#[test]
fn post_dispatch_always_allows() {
    let p = mon_fri_9_5();
    assert!(matches!(
        p.evaluate_post(&ctx(), &json!({}), &json!({}), 1, &json!({})),
        GateDecision::Allow { .. }
    ));
}

#[test]
#[should_panic(expected = "unknown IANA timezone")]
fn bad_timezone_fails_closed() {
    plugin(json!({
        "timezone": "Mars/Olympus_Mons",
        "windows": [{ "days": ["mon"], "start": "09:00", "end": "17:00" }]
    }));
}

#[test]
#[should_panic(expected = "at least one window is required")]
fn empty_windows_fails_closed() {
    plugin(json!({ "timezone": "UTC", "windows": [] }));
}

#[test]
#[should_panic(expected = "a window has no days")]
fn empty_days_fails_closed() {
    plugin(json!({
        "timezone": "UTC",
        "windows": [{ "days": [], "start": "09:00", "end": "17:00" }]
    }));
}

#[test]
#[should_panic(expected = "invalid day")]
fn invalid_day_fails_closed() {
    plugin(json!({
        "timezone": "UTC",
        "windows": [{ "days": ["funday"], "start": "09:00", "end": "17:00" }]
    }));
}

#[test]
#[should_panic(expected = "invalid start time")]
fn invalid_time_fails_closed() {
    plugin(json!({
        "timezone": "UTC",
        "windows": [{ "days": ["mon"], "start": "25:00", "end": "26:00" }]
    }));
}

#[test]
#[should_panic(expected = "is not before end")]
fn start_not_before_end_fails_closed() {
    plugin(json!({
        "timezone": "UTC",
        "windows": [{ "days": ["mon"], "start": "17:00", "end": "09:00" }]
    }));
}

#[test]
#[should_panic(expected = "invalid blackout date")]
fn invalid_blackout_date_fails_closed() {
    plugin(json!({
        "timezone": "UTC",
        "windows": [{ "days": ["mon"], "start": "09:00", "end": "17:00" }],
        "blackout_dates": ["01/03/2024"]
    }));
}

#[test]
#[should_panic(expected = "config JSON failed to parse")]
fn unknown_config_field_fails_closed() {
    plugin(json!({
        "timezone": "UTC",
        "windows": [{ "days": ["mon"], "start": "09:00", "end": "17:00" }],
        "bogus": true
    }));
}
