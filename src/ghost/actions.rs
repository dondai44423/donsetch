//! actions.rs : the fetch-actions executor (v2).
//!
//! `web_fetch(actions=[...])` gives the agent browser control
//! INSIDE fetch: wait, click, type, press, scroll, hover : then
//! the normal DonSift extraction runs on the final DOM, so
//! focus/section/toc all keep working on an interacted-with
//! page. This is hound's browser-tool surface, but composable
//! with fetch instead of a separate tool : one call replaces
//! navigate→act→act→read round-trips.
//!
//! Design rules:
//! - Validate ALL steps up front (parse_errors before any
//!   browser time is spent : a typo in step 5 must not burn a
//!   launch on step 1).
//! - Execute in order; first failure aborts with the step
//!   index, reason, and everything that succeeded : the agent
//!   retries with corrected steps, not blind.
//! - Deterministic waits over blind sleeps where possible
//!   (wait_selector / wait_text poll the live DOM).
//! - Bounded: max 16 steps, 10s per step, enforced by callers.

use serde_json::Value;

use super::Ghost;
use crate::error::FetchError;

/// Maximum steps per fetch call : an action script, not a
/// browsing session (Bladebro exists for sessions).
pub const MAX_STEPS: usize = 16;

/// Default per-wait timeout (ms).
const DEFAULT_WAIT_MS: u64 = 8_000;

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Fixed pause. Prefer wait_selector/wait_text : but some
    /// SPAs animate with no DOM signal, so blind waits stay.
    Wait { ms: u64 },
    /// Poll until the CSS selector matches (element may be
    /// anywhere in the DOM, visibility not required).
    WaitSelector { selector: String, timeout_ms: u64 },
    /// Poll until the needle appears in rendered body text.
    WaitText { text: String, timeout_ms: u64 },
    /// Click an element by CSS selector, or by (visible) text
    /// when no selector is given. Text match targets the
    /// smallest element whose OWN text contains it : the
    /// button/link itself, not its container.
    Click {
        selector: Option<String>,
        text: Option<String>,
    },
    /// Focus (click) an optional selector, then type `text`
    /// with human cadence.
    Type {
        selector: Option<String>,
        text: String,
    },
    /// Press a named key (Enter, Escape, ...).
    Press { key: String },
    /// Scroll: "top" | "bottom" | "down", or pixels via px.
    Scroll { to: String, px: i64 },
    /// Hover an element (dropdown menus).
    Hover {
        selector: Option<String>,
        text: Option<String>,
    },
}

/// One step's execution outcome, surfaced to the agent.
#[derive(Debug, Clone)]
pub struct ActionOutcome {
    /// 0-based step index.
    pub step: usize,
    /// Short description, e.g. `click #load-more`.
    pub action: String,
    /// "ok" or a failure reason.
    pub outcome: String,
    pub ms: u128,
}

/// Parse + validate the `actions` JSON array. All-or-nothing:
/// returns Err(description) naming the first bad step.
pub fn parse(v: &Value) -> Result<Vec<Action>, String> {
    let Some(arr) = v.as_array() else {
        return Err("actions must be an array of step objects".into());
    };
    if arr.is_empty() {
        return Err("actions array is empty".into());
    }
    if arr.len() > MAX_STEPS {
        return Err(format!("actions: {} steps > max {MAX_STEPS}", arr.len()));
    }
    let mut out = Vec::with_capacity(arr.len());
    for (i, step) in arr.iter().enumerate() {
        let obj = step.as_object().ok_or(format!(
            "actions[{i}] must be an object, got {}",
            type_name(step)
        ))?;
        let kind = obj
            .get("do")
            .and_then(Value::as_str)
            .ok_or(format!("actions[{i}] missing \"do\""))?;
        let sel = || {
            obj.get("selector")
                .and_then(Value::as_str)
                .map(String::from)
        };
        let text = || obj.get("text").and_then(Value::as_str).map(String::from);
        // Clamp agent-supplied durations: a typo'd hour-long wait
        // would stall the tool call with no cancellation path.
        // 30s per wait, 60s per selector/text poll ceiling.
        let timeout = || {
            obj.get("timeout_ms")
                .and_then(Value::as_u64)
                .unwrap_or(DEFAULT_WAIT_MS)
                .min(60_000)
        };
        let a = match kind {
            "wait" => Action::Wait {
                ms: obj
                    .get("ms")
                    .and_then(Value::as_u64)
                    .unwrap_or(500)
                    .min(30_000),
            },
            "wait_selector" => Action::WaitSelector {
                selector: sel().ok_or(format!("actions[{i}] wait_selector needs \"selector\""))?,
                timeout_ms: timeout(),
            },
            "wait_text" => Action::WaitText {
                text: text().ok_or(format!("actions[{i}] wait_text needs \"text\""))?,
                timeout_ms: timeout(),
            },
            "click" | "hover" => {
                let (selector, text) = match (sel(), text()) {
                    (None, None) => {
                        return Err(format!(
                            "actions[{i}] {kind} needs \"selector\" or \"text\""
                        ));
                    }
                    (s, t) => (s, t),
                };
                if kind == "click" {
                    Action::Click { selector, text }
                } else {
                    Action::Hover { selector, text }
                }
            }
            "type" => {
                let text = obj
                    .get("text")
                    .or_else(|| obj.get("value"))
                    .and_then(Value::as_str)
                    .ok_or(format!("actions[{i}] type needs \"text\""))?
                    .to_string();
                Action::Type {
                    selector: sel(),
                    text,
                }
            }
            "press" => Action::Press {
                key: obj
                    .get("key")
                    .and_then(Value::as_str)
                    .ok_or(format!("actions[{i}] press needs \"key\" (e.g. \"Enter\")"))?
                    .to_string(),
            },
            "scroll" => {
                let to = obj
                    .get("to")
                    .and_then(Value::as_str)
                    .unwrap_or("down")
                    .to_string();
                let px = obj.get("px").and_then(Value::as_i64).unwrap_or(0);
                if !matches!(to.as_str(), "top" | "bottom" | "down" | "px") {
                    return Err(format!(
                        "actions[{i}] scroll to must be top|bottom|down (or use px)"
                    ));
                }
                Action::Scroll {
                    to: if to == "px" { "px".into() } else { to },
                    px,
                }
            }
            other => {
                return Err(format!(
                    "actions[{i}] unknown do={other:?} : supported: wait, wait_selector, wait_text, click, hover, type, press, scroll"
                ));
            }
        };
        out.push(a);
    }
    Ok(out)
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn describe(a: &Action) -> String {
    match a {
        Action::Wait { ms } => format!("wait {ms}ms"),
        Action::WaitSelector { selector, .. } => format!("wait_selector {selector}"),
        Action::WaitText { text, .. } => {
            format!("wait_text {:?}", text.chars().take(24).collect::<String>())
        }
        Action::Click {
            selector: Some(s), ..
        } => format!("click {s}"),
        Action::Click { text: Some(t), .. } => {
            format!("click text {:?}", t.chars().take(24).collect::<String>())
        }
        Action::Click { .. } => "click".into(),
        Action::Hover {
            selector: Some(s), ..
        } => format!("hover {s}"),
        Action::Hover { text: Some(t), .. } => {
            format!("hover text {:?}", t.chars().take(24).collect::<String>())
        }
        Action::Hover { .. } => "hover".into(),
        Action::Type {
            selector: Some(s),
            text,
            ..
        } => {
            format!(
                "type {:?} into {}",
                text.chars().take(16).collect::<String>(),
                s
            )
        }
        Action::Type { text, .. } => {
            format!("type {:?}", text.chars().take(16).collect::<String>())
        }
        Action::Press { key } => format!("press {key}"),
        Action::Scroll { to, px } => {
            if *px > 0 {
                format!("scroll {px}px")
            } else {
                format!("scroll {to}")
            }
        }
    }
}

/// Resolve an element point for click/hover by selector or text.
async fn resolve_point(
    g: &Ghost,
    selector: &Option<String>,
    text: &Option<String>,
) -> Result<Option<(f64, f64)>, FetchError> {
    if let Some(sel) = selector {
        g.element_center(sel).await
    } else if let Some(t) = text {
        g.element_center_by_text(t).await
    } else {
        Ok(None)
    }
}

/// Execute steps in order. Ok(outcomes) on full success;
/// Err((failed_step_index, reason, outcomes_so_far)) on the
/// first failure. Navigation caused by a click (href link,
/// form submit) is fine : subsequent steps act on the new page.
pub async fn run(
    g: &mut Ghost,
    actions: &[Action],
) -> Result<Vec<ActionOutcome>, (usize, String, Vec<ActionOutcome>)> {
    let mut operation = g.operation();
    let restore = g.unminimize_for_capture().await;
    let result = run_inner(g, actions).await;
    if restore.needs_surface_wait() {
        g.reminimize_after_capture().await;
    }
    operation.finish();
    result
}

async fn run_inner(
    g: &mut Ghost,
    actions: &[Action],
) -> Result<Vec<ActionOutcome>, (usize, String, Vec<ActionOutcome>)> {
    let mut outcomes = Vec::with_capacity(actions.len());
    for (i, a) in actions.iter().enumerate() {
        let t0 = std::time::Instant::now();
        let desc = describe(a);
        let res: Result<String, String> = match a {
            Action::Wait { ms } => {
                tokio::time::sleep(std::time::Duration::from_millis(*ms)).await;
                Ok("ok".into())
            }
            Action::WaitSelector {
                selector,
                timeout_ms,
            } => wait_loop(*timeout_ms, || g.selector_exists(selector)).await,
            Action::WaitText { text, timeout_ms } => {
                wait_loop(*timeout_ms, || g.body_has_text(text)).await
            }
            Action::Click { .. } | Action::Hover { .. } => {
                match resolve_point(g, sel_of(a), text_of(a)).await {
                    Ok(Some((x, y))) => {
                        let r = if matches!(a, Action::Click { .. }) {
                            g.click(x, y).await
                        } else {
                            g.hover(x, y).await
                        };
                        r.map_err(|e| e.to_string()).map(|()| "ok".into())
                    }
                    Ok(None) => Err("element not found".into()),
                    Err(e) => Err(e.to_string()),
                }
            }
            Action::Type { selector, text } => {
                if let Some(sel) = selector {
                    match g.element_center(sel).await {
                        Ok(Some((x, y))) => match g.click(x, y).await {
                            Ok(()) => {}
                            Err(e) => return fail(i, &e.to_string(), outcomes, &desc, t0),
                        },
                        Ok(None) => return fail(i, "selector not found", outcomes, &desc, t0),
                        Err(e) => return fail(i, &e.to_string(), outcomes, &desc, t0),
                    }
                    // Focus takes a moment before keys land.
                    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                }
                g.type_text(text)
                    .await
                    .map_err(|e| e.to_string())
                    .map(|()| "ok".into())
            }
            Action::Press { key } => g
                .press_key(key)
                .await
                .map_err(|e| e.to_string())
                .map(|()| "ok".into()),
            Action::Scroll { to, px } => g
                .scroll(to, *px)
                .await
                .map_err(|e| e.to_string())
                .map(|()| "ok".into()),
        };
        match res {
            Ok(_) => outcomes.push(ActionOutcome {
                step: i,
                action: desc,
                outcome: "ok".into(),
                ms: t0.elapsed().as_millis(),
            }),
            Err(reason) => return fail(i, &reason, outcomes, &desc, t0),
        }
    }
    Ok(outcomes)
}

fn sel_of(a: &Action) -> &Option<String> {
    match a {
        Action::Click { selector, .. }
        | Action::Hover { selector, .. }
        | Action::Type { selector, .. } => selector,
        _ => &None,
    }
}

fn text_of(a: &Action) -> &Option<String> {
    match a {
        Action::Click { text, .. } | Action::Hover { text, .. } => text,
        _ => &None,
    }
}

fn fail(
    i: usize,
    reason: &str,
    mut outcomes: Vec<ActionOutcome>,
    desc: &str,
    t0: std::time::Instant,
) -> Result<Vec<ActionOutcome>, (usize, String, Vec<ActionOutcome>)> {
    outcomes.push(ActionOutcome {
        step: i,
        action: desc.to_string(),
        outcome: reason.to_string(),
        ms: t0.elapsed().as_millis(),
    });
    Err((i, reason.to_string(), outcomes))
}

/// Poll `probe` every 150ms until true or timeout.
async fn wait_loop<F, Fut>(timeout_ms: u64, mut probe: F) -> Result<String, String>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<bool, FetchError>>,
{
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    loop {
        match probe().await {
            Ok(true) => return Ok("ok".into()),
            Ok(false) => {}
            Err(e) => return Err(format!("probe failed: {e}")),
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!("timeout after {timeout_ms}ms"));
        }
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_full_script() {
        let v = json!([
            {"do": "wait_selector", "selector": "input[name=q]", "timeout_ms": 5000},
            {"do": "type", "selector": "input[name=q]", "text": "rust async"},
            {"do": "press", "key": "Enter"},
            {"do": "wait_text", "text": "results"},
            {"do": "scroll", "to": "bottom"},
            {"do": "click", "text": "Next"},
            {"do": "wait", "ms": 300},
            {"do": "hover", "selector": ".menu"}
        ]);
        let out = parse(&v).unwrap();
        assert_eq!(out.len(), 8);
        assert_eq!(
            out[0],
            Action::WaitSelector {
                selector: "input[name=q]".into(),
                timeout_ms: 5000
            }
        );
        assert_eq!(
            out[2],
            Action::Press {
                key: "Enter".into()
            }
        );
        assert_eq!(
            out[5],
            Action::Click {
                selector: None,
                text: Some("Next".into())
            }
        );
    }

    #[test]
    fn parse_rejects_unknown_and_missing() {
        assert!(parse(&json!([{"do": "explode"}])).is_err());
        assert!(parse(&json!([{"selector": "x"}])).is_err()); // no do
        assert!(parse(&json!([{"do": "click"}])).is_err()); // no selector/text
        assert!(parse(&json!([])).is_err()); // empty
        assert!(parse(&json!("not an array")).is_err());
    }

    #[test]
    fn parse_caps_steps() {
        let many: Vec<Value> = (0..17).map(|_| json!({"do": "wait", "ms": 1})).collect();
        assert!(parse(&Value::Array(many)).is_err());
        let ok: Vec<Value> = (0..16).map(|_| json!({"do": "wait", "ms": 1})).collect();
        assert!(parse(&Value::Array(ok)).is_ok());
    }

    #[test]
    fn type_accepts_value_alias() {
        let out = parse(&json!([{"do": "type", "value": "hello"}])).unwrap();
        assert_eq!(
            out[0],
            Action::Type {
                selector: None,
                text: "hello".into()
            }
        );
    }

    #[test]
    fn scroll_px_form() {
        let out = parse(&json!([{"do": "scroll", "px": 1200}])).unwrap();
        assert!(matches!(out[0], Action::Scroll { .. }));
    }
}
