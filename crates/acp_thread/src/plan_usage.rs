//! What is left of the plan, as Claude reports it.
//!
//! The Claude adapter forwards Claude Code's rate-limit events as
//! `usage_update` session updates carrying `_meta["_claude/rateLimit"]` — the
//! SDK's `SDKRateLimitInfo`. One event carries one window, so the latest
//! report for each is kept, across every Claude thread: the account is not on
//! the wire, and one Zed talks to one of them.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use collections::HashMap;
use gpui::{App, AppContext as _, Context, Entity, Global};
/// `_meta["_claude/rateLimit"]`, the adapter's own namespace.
const RATE_LIMIT_META_KEY: &str = "_claude/rateLimit";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum UsageWindow {
    FiveHour,
    SevenDay,
    SevenDayOpus,
    SevenDaySonnet,
    Overage,
}

impl UsageWindow {
    fn from_wire(kind: &str) -> Option<Self> {
        match kind {
            "five_hour" => Some(Self::FiveHour),
            "seven_day" | "seven_day_overage_included" => Some(Self::SevenDay),
            "seven_day_opus" => Some(Self::SevenDayOpus),
            "seven_day_sonnet" => Some(Self::SevenDaySonnet),
            "overage" => Some(Self::Overage),
            _ => None,
        }
    }

    /// Short enough for one muted line.
    pub fn short_label(&self) -> &'static str {
        match self {
            Self::FiveHour => "5h",
            Self::SevenDay => "wk",
            Self::SevenDayOpus => "opus",
            Self::SevenDaySonnet => "sonnet",
            Self::Overage => "overage",
        }
    }

    pub fn full_label(&self) -> &'static str {
        match self {
            Self::FiveHour => "5-hour limit",
            Self::SevenDay => "Weekly limit",
            Self::SevenDayOpus => "Weekly limit, Opus",
            Self::SevenDaySonnet => "Weekly limit, Sonnet",
            Self::Overage => "Overage",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UsageStatus {
    Allowed,
    Warning,
    Rejected,
}

impl UsageStatus {
    fn from_wire(status: &str) -> Self {
        match status {
            "rejected" => Self::Rejected,
            "allowed_warning" => Self::Warning,
            _ => Self::Allowed,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WindowUsage {
    pub status: UsageStatus,
    /// Percent used, where the report named one.
    pub utilization: Option<f64>,
    pub resets_at: Option<SystemTime>,
}

#[derive(Default)]
pub struct PlanUsageStore {
    windows: HashMap<UsageWindow, WindowUsage>,
}

struct GlobalPlanUsageStore(Entity<PlanUsageStore>);

impl Global for GlobalPlanUsageStore {}

impl PlanUsageStore {
    pub fn global(cx: &mut App) -> Entity<Self> {
        if !cx.has_global::<GlobalPlanUsageStore>() {
            let store = cx.new(|_| PlanUsageStore::default());
            cx.set_global(GlobalPlanUsageStore(store));
        }
        cx.global::<GlobalPlanUsageStore>().0.clone()
    }

    pub fn try_global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalPlanUsageStore>()
            .map(|global| global.0.clone())
    }

    pub fn window(&self, window: UsageWindow) -> Option<&WindowUsage> {
        self.windows.get(&window)
    }

    /// Every window reported so far, in a stable order.
    pub fn windows(&self) -> Vec<(UsageWindow, WindowUsage)> {
        let mut windows: Vec<(UsageWindow, WindowUsage)> = self
            .windows
            .iter()
            .map(|(window, usage)| (*window, *usage))
            .collect();
        windows.sort_by_key(|(window, _)| *window);
        windows
    }

    pub fn is_empty(&self) -> bool {
        self.windows.is_empty()
    }

    fn report(&mut self, window: UsageWindow, usage: WindowUsage, cx: &mut Context<Self>) {
        if self.windows.get(&window) == Some(&usage) {
            return;
        }
        self.windows.insert(window, usage);
        cx.notify();
    }
}

/// Reads a rate-limit report off a `usage_update`'s metadata, where there is
/// one. Called from the thread's own usage handler, which is where the meta
/// arrives.
pub fn note_usage_update(meta: Option<&serde_json::Map<String, serde_json::Value>>, cx: &mut App) {
    let Some((window, usage)) = parse_rate_limit(meta) else {
        return;
    };
    let store = PlanUsageStore::global(cx);
    store.update(cx, |store, cx| store.report(window, usage, cx));
}

fn parse_rate_limit(
    meta: Option<&serde_json::Map<String, serde_json::Value>>,
) -> Option<(UsageWindow, WindowUsage)> {
    let info = meta?.get(RATE_LIMIT_META_KEY)?;
    let window = UsageWindow::from_wire(info.get("rateLimitType")?.as_str()?)?;
    let status = info
        .get("status")
        .and_then(|status| status.as_str())
        .map(UsageStatus::from_wire)
        .unwrap_or(UsageStatus::Allowed);
    let utilization = info
        .get("utilization")
        .and_then(|value| value.as_f64())
        .filter(|value| value.is_finite() && *value >= 0.0);
    // Epoch seconds, as the SDK reports them.
    let resets_at = info
        .get("resetsAt")
        .and_then(|value| value.as_f64())
        .filter(|value| value.is_finite() && *value > 0.0)
        .and_then(|seconds| UNIX_EPOCH.checked_add(Duration::from_secs_f64(seconds)));
    Some((
        window,
        WindowUsage {
            status,
            utilization,
            resets_at,
        },
    ))
}

/// The reset time as a local clock time, for a tooltip.
pub fn reset_label(resets_at: SystemTime) -> Option<String> {
    let at: chrono::DateTime<chrono::Local> = chrono::DateTime::from(resets_at);
    Some(at.format("%H:%M").to_string())
}

/// The footer and the thread both read it, so it exists before either.
pub fn init(cx: &mut App) {
    PlanUsageStore::global(cx);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(value: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
        match value {
            serde_json::Value::Object(map) => map,
            _ => panic!("expected an object"),
        }
    }

    #[test]
    fn test_a_rate_limit_report_is_read_off_the_claude_namespace() {
        let reported = meta(serde_json::json!({
            "_claude/rateLimit": {
                "status": "allowed_warning",
                "rateLimitType": "five_hour",
                "utilization": 83.0,
                "resetsAt": 1_760_000_000.0,
            }
        }));
        let (window, usage) = parse_rate_limit(Some(&reported)).expect("a report");
        assert_eq!(window, UsageWindow::FiveHour);
        assert_eq!(usage.status, UsageStatus::Warning);
        assert_eq!(usage.utilization, Some(83.0));
        assert_eq!(
            usage.resets_at,
            Some(UNIX_EPOCH + Duration::from_secs(1_760_000_000))
        );

        // The overage-included weekly window is the weekly window.
        let reported = meta(serde_json::json!({
            "_claude/rateLimit": {
                "status": "rejected",
                "rateLimitType": "seven_day_overage_included",
            }
        }));
        let (window, usage) = parse_rate_limit(Some(&reported)).expect("a report");
        assert_eq!(window, UsageWindow::SevenDay);
        assert_eq!(usage.status, UsageStatus::Rejected);
        assert_eq!(usage.utilization, None, "a report need not name a percent");

        // A usage update with no rate limit behind it, which is most of them.
        assert!(parse_rate_limit(Some(&meta(serde_json::json!({})))).is_none());
        assert!(parse_rate_limit(None).is_none());
        // A window this side has never heard of is not guessed at.
        assert!(
            parse_rate_limit(Some(&meta(serde_json::json!({
                "_claude/rateLimit": { "status": "allowed", "rateLimitType": "fortnightly" }
            }))))
            .is_none()
        );
    }

    #[gpui::test]
    fn test_the_latest_report_for_each_window_is_what_is_kept(cx: &mut gpui::App) {
        let store = cx.new(|_| PlanUsageStore::default());
        assert!(store.read(cx).is_empty());

        let report = |utilization| WindowUsage {
            status: UsageStatus::Allowed,
            utilization: Some(utilization),
            resets_at: None,
        };
        store.update(cx, |store, cx| {
            store.report(UsageWindow::FiveHour, report(42.0), cx);
            store.report(UsageWindow::SevenDay, report(18.0), cx);
            store.report(UsageWindow::FiveHour, report(55.0), cx);
        });
        assert_eq!(
            store
                .read(cx)
                .windows()
                .into_iter()
                .map(|(window, usage)| (window.short_label(), usage.utilization))
                .collect::<Vec<_>>(),
            vec![("5h", Some(55.0)), ("wk", Some(18.0))],
            "one line per window, each at its latest report"
        );
    }
}
