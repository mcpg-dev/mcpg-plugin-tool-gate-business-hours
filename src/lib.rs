//! Business-hours ToolGate plugin.
//!
//! Allows tool calls only inside operator-configured weekly time windows,
//! evaluated in a chosen IANA timezone; calls outside every window — or on a
//! configured blackout date — are denied before dispatch. The operator config
//! is parsed ONCE in `from_config_json` (the tool-gate convention — the per-call
//! `config` slot carries request context, not the operator config) into a
//! compiled form: the timezone is resolved, every window's `HH:MM` bounds are
//! reduced to seconds-from-midnight, and blackout dates are parsed. Evaluation
//! is pure time logic, no I/O, fully offline. Fails closed on bad config.

use mcpg_plugin_protocol::{GateDecision, PluginContext, PluginManifest, firstparty_manifest};
use mcpg_plugin_sdk::ffi::SyncToolGate;
use serde::Deserialize;
use serde_json::Value;

use chrono::{DateTime, Datelike, NaiveDate, Timelike, Utc, Weekday};
use chrono_tz::Tz;
use std::collections::BTreeSet;

/// JSON-RPC error code for an outside-business-hours deny. In the mcpg
/// convention the `-3203x` band is the governance/policy family; `-32030` is the
/// time-window denial.
const DEFAULT_DENY_CODE: i32 = -32030;
const DEFAULT_DENY_HTTP_STATUS: u16 = 403;
/// Seconds in a full day; the canonical "end of day" bound (`24:00`).
const SECONDS_PER_DAY: u32 = 86_400;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BusinessHoursConfig {
    /// IANA timezone the windows are evaluated in (e.g. `America/New_York`).
    timezone: String,
    /// Weekly allow-windows. At least one is required.
    windows: Vec<WeeklyWindow>,
    /// Dates (ISO `YYYY-MM-DD`, in `timezone`) on which all calls are denied,
    /// regardless of the windows.
    #[serde(default)]
    blackout_dates: Vec<String>,
    /// JSON-RPC error code for the deny.
    #[serde(default = "default_deny_code")]
    deny_code: i32,
    /// HTTP status for the deny.
    #[serde(default = "default_deny_http_status")]
    deny_http_status: u16,
    /// Optional fixed deny message. When unset a detailed message is produced.
    #[serde(default)]
    deny_message: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WeeklyWindow {
    /// Days the window applies to: any of `mon tue wed thu fri sat sun`
    /// (case-insensitive). At least one is required.
    days: Vec<String>,
    /// Window start, `HH:MM` (24-hour, inclusive).
    start: String,
    /// Window end, `HH:MM` (24-hour, exclusive) or `24:00` for end-of-day. Must
    /// be strictly after `start` — a window that wraps past midnight is
    /// expressed as two windows (see the README).
    end: String,
}

fn default_deny_code() -> i32 {
    DEFAULT_DENY_CODE
}
fn default_deny_http_status() -> u16 {
    DEFAULT_DENY_HTTP_STATUS
}

/// A window reduced to a weekday mask (index = days-from-Monday, 0=Mon..6=Sun)
/// and an `[start, end)` span in seconds-from-midnight (`end` may be
/// [`SECONDS_PER_DAY`] for `24:00`). `chrono::Weekday` is not `Ord`, so a fixed
/// 7-slot mask is used instead of a set.
#[derive(Debug)]
struct CompiledWindow {
    days: [bool; 7],
    start_secs: u32,
    end_secs: u32,
}

pub struct BusinessHoursPlugin {
    manifest: PluginManifest,
    tz: Tz,
    windows: Vec<CompiledWindow>,
    blackouts: BTreeSet<NaiveDate>,
    deny_code: i32,
    deny_http_status: u16,
    deny_message: Option<String>,
}

/// Map a day token (`mon`..`sun`, case-insensitive) to a `Weekday`.
fn parse_weekday(token: &str) -> Option<Weekday> {
    match token.trim().to_ascii_lowercase().as_str() {
        "mon" => Some(Weekday::Mon),
        "tue" => Some(Weekday::Tue),
        "wed" => Some(Weekday::Wed),
        "thu" => Some(Weekday::Thu),
        "fri" => Some(Weekday::Fri),
        "sat" => Some(Weekday::Sat),
        "sun" => Some(Weekday::Sun),
        _ => None,
    }
}

/// Parse an `HH:MM` time (or the literal `24:00`) to seconds-from-midnight.
/// Returns `None` on any malformed input. `24:00` → [`SECONDS_PER_DAY`].
fn parse_hhmm_secs(s: &str) -> Option<u32> {
    let (h, m) = s.split_once(':')?;
    let h: u32 = h.parse().ok()?;
    let m: u32 = m.parse().ok()?;
    if h == 24 && m == 0 {
        return Some(SECONDS_PER_DAY);
    }
    if h >= 24 || m >= 60 {
        return None;
    }
    Some(h * 3_600 + m * 60)
}

impl BusinessHoursPlugin {
    /// SDK factory: parse + compile operator config. A security control FAILS
    /// CLOSED on bad config by refusing to instantiate (panic → null handle →
    /// boot Err), the uniform tool-gate convention (see ip-allowlist).
    pub fn from_config_json(config_json: &str) -> Self {
        let cfg: BusinessHoursConfig = serde_json::from_str(config_json).unwrap_or_else(|err| {
            panic!("tool-gate-business-hours: config JSON failed to parse: {err}")
        });

        let tz: Tz = cfg.timezone.parse().unwrap_or_else(|_| {
            panic!(
                "tool-gate-business-hours: unknown IANA timezone {:?}",
                cfg.timezone
            )
        });

        if cfg.windows.is_empty() {
            panic!("tool-gate-business-hours: at least one window is required");
        }

        let windows: Vec<CompiledWindow> = cfg
            .windows
            .into_iter()
            .map(|w| {
                if w.days.is_empty() {
                    panic!("tool-gate-business-hours: a window has no days");
                }
                let mut days = [false; 7];
                for d in &w.days {
                    let wd = parse_weekday(d).unwrap_or_else(|| {
                        panic!("tool-gate-business-hours: invalid day {d:?} (use mon..sun)")
                    });
                    days[wd.num_days_from_monday() as usize] = true;
                }
                let start_secs = parse_hhmm_secs(&w.start).unwrap_or_else(|| {
                    panic!("tool-gate-business-hours: invalid start time {:?}", w.start)
                });
                let end_secs = parse_hhmm_secs(&w.end).unwrap_or_else(|| {
                    panic!("tool-gate-business-hours: invalid end time {:?}", w.end)
                });
                if start_secs >= end_secs {
                    panic!(
                        "tool-gate-business-hours: window start {:?} is not before end {:?} \
                         (express an overnight window as two windows)",
                        w.start, w.end
                    );
                }
                CompiledWindow {
                    days,
                    start_secs,
                    end_secs,
                }
            })
            .collect();

        let blackouts: BTreeSet<NaiveDate> = cfg
            .blackout_dates
            .iter()
            .map(|d| {
                NaiveDate::parse_from_str(d, "%Y-%m-%d").unwrap_or_else(|_| {
                    panic!("tool-gate-business-hours: invalid blackout date {d:?} (use YYYY-MM-DD)")
                })
            })
            .collect();

        Self {
            manifest: firstparty_manifest! {
                id: "dev.mcpg.tool-gate.business-hours",
                name: "Business Hours Gate",
                class: ToolGate,
            },
            tz,
            windows,
            blackouts,
            deny_code: cfg.deny_code,
            deny_http_status: cfg.deny_http_status,
            deny_message: cfg.deny_message,
        }
    }

    /// Pure decision core: is `now` (already in the configured timezone) inside
    /// an allow-window and not on a blackout date? Directly unit-testable with
    /// any constructed `DateTime<Tz>` — no clock injection needed.
    fn is_open(&self, now: DateTime<Tz>) -> bool {
        if self.blackouts.contains(&now.date_naive()) {
            return false;
        }
        let weekday = now.weekday().num_days_from_monday() as usize;
        let secs = now.time().num_seconds_from_midnight();
        self.windows
            .iter()
            .any(|w| w.days[weekday] && secs >= w.start_secs && secs < w.end_secs)
    }

    fn deny(&self) -> GateDecision {
        let message = self.deny_message.clone().unwrap_or_else(|| {
            format!(
                "tool calls are only permitted during configured business hours ({})",
                self.tz.name()
            )
        });
        GateDecision::Deny {
            http_status: self.deny_http_status,
            code: self.deny_code,
            message,
            error_data: None,
        }
    }
}

impl SyncToolGate for BusinessHoursPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    fn evaluate_pre(
        &self,
        _ctx: &PluginContext,
        _arguments: &Value,
        _meta: Option<&Value>,
        _config: &Value,
    ) -> GateDecision {
        let now = Utc::now().with_timezone(&self.tz);
        if self.is_open(now) {
            GateDecision::allow()
        } else {
            self.deny()
        }
    }

    fn evaluate_post(
        &self,
        _ctx: &PluginContext,
        _arguments: &Value,
        _result: &Value,
        _duration_ms: u64,
        _config: &Value,
    ) -> GateDecision {
        // Pre-dispatch time gate; nothing to enforce post-dispatch.
        GateDecision::allow()
    }
}

#[cfg(any(feature = "cdylib-export", feature = "static-firstparty"))]
mcpg_plugin_sdk::declare_plugin! {
    plugin_id: "dev.mcpg.tool-gate.business-hours",
    plugin_version: env!("CARGO_PKG_VERSION"),
    descriptor_yaml: include_str!("../plugin.yaml"),
    capabilities: &[],
    entities: [
        tool_gate as gate {
            inner_name: "",
            plugin_type: BusinessHoursPlugin,
            factory: |cfg: &str, _host: ::mcpg_plugin_sdk::HostHandle| BusinessHoursPlugin::from_config_json(cfg),
        },
    ],
}

#[cfg(test)]
mod tests;
