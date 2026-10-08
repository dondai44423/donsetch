//! Main-frame navigation and response identity. No page-script injection.

use serde_json::Value;
use std::collections::VecDeque;

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Document {
    pub generation: u64,
    pub frame: String,
    pub loader: String,
    pub url: String,
    pub status: Option<u16>,
}

pub(super) fn navigation_committed(
    before: &Document,
    current: &Document,
    expected_loader: Option<&str>,
    requested: &str,
) -> bool {
    if current.generation <= before.generation
        || current.loader.is_empty()
        || current.url.is_empty()
        || current.url.starts_with("about:blank")
    {
        return false;
    }
    if let Some(loader) = expected_loader {
        return current.loader == loader;
    }
    current.loader != before.loader || (current.url != before.url && current.url == requested)
}

#[derive(Default)]
pub(super) struct Tracker {
    session: String,
    pub current: Document,
    responses: VecDeque<Document>,
    commits: VecDeque<(u64, String, String)>,
}

impl Tracker {
    pub fn new(session: String) -> Self {
        Self {
            session,
            ..Self::default()
        }
    }

    pub fn navigation_committed(
        &self,
        before: &Document,
        expected_loader: Option<&str>,
        requested: &str,
    ) -> bool {
        if navigation_committed(before, &self.current, expected_loader, requested) {
            return true;
        }
        // A client redirect may commit another loader before the caller wakes.
        // Accept it only if the requested loader actually committed after the
        // baseline, on the current main frame. An unrelated loader is no proof.
        expected_loader.is_some_and(|expected| {
            self.current.generation > before.generation
                && !self.current.url.is_empty()
                && !self.current.url.starts_with("about:blank")
                && self.commits.iter().any(|(generation, frame, loader)| {
                    *generation > before.generation
                        && frame == &self.current.frame
                        && loader == expected
                })
        })
    }

    // Called synchronously by the CDP demux, before another reply can wake
    // a DOM reader. Only main-document metadata is retained, never bodies.
    pub fn observe(&mut self, event: &Value) {
        if event.get("sessionId").and_then(Value::as_str) != Some(&self.session) {
            return;
        }
        let Some(params) = event.get("params") else {
            return;
        };
        match event.get("method").and_then(Value::as_str) {
            Some("Page.frameNavigated") => {
                let Some(frame) = params.get("frame") else {
                    return;
                };
                if frame.get("parentId").is_some() {
                    return;
                }
                let (Some(id), Some(loader), Some(url)) = (
                    bounded_text(frame.get("id"), 1024),
                    bounded_text(frame.get("loaderId"), 1024),
                    bounded_text(frame.get("url"), 64 * 1024),
                ) else {
                    return;
                };
                let status = self
                    .responses
                    .iter()
                    .rev()
                    .find(|r| r.frame == id && r.loader == loader)
                    .and_then(|r| r.status);
                if self.current.frame != id || self.current.loader != loader {
                    self.current.generation += 1;
                    if self.commits.len() == 8 {
                        self.commits.pop_front();
                    }
                    self.commits
                        .push_back((self.current.generation, id.into(), loader.into()));
                }
                self.current.frame = id.into();
                self.current.loader = loader.into();
                self.current.url = url.into();
                self.current.status = status;
            }
            Some("Page.navigatedWithinDocument") => {
                if params.get("frameId").and_then(Value::as_str) == Some(&self.current.frame)
                    && let Some(url) = bounded_text(params.get("url"), 64 * 1024)
                    && url != self.current.url
                {
                    self.current.url = url.into();
                    self.current.generation += 1;
                }
            }
            Some("Network.responseReceived")
                if params.get("type").and_then(Value::as_str) == Some("Document") =>
            {
                let (Some(frame), Some(loader), Some(url), Some(status)) = (
                    bounded_text(params.get("frameId"), 1024),
                    bounded_text(params.get("loaderId"), 1024),
                    bounded_text(params.pointer("/response/url"), 64 * 1024),
                    params.pointer("/response/status").and_then(Value::as_f64),
                ) else {
                    return;
                };
                let status = ((url.starts_with("https://") || url.starts_with("http://"))
                    && (100.0..600.0).contains(&status)
                    && status.fract() == 0.0)
                    .then_some(status as u16);
                if self.current.frame == frame && self.current.loader == loader {
                    self.current.status = status;
                }
                if self.responses.len() == 8 {
                    self.responses.pop_front();
                }
                self.responses.push_back(Document {
                    frame: frame.into(),
                    loader: loader.into(),
                    url: url.into(),
                    status,
                    generation: 0,
                });
            }
            _ => {}
        }
    }
}

fn bounded_text(value: Option<&Value>, max: usize) -> Option<&str> {
    value
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty() && s.len() <= max)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn commit(tracker: &mut Tracker, frame: &str, loader: &str, url: &str) {
        tracker.observe(&json!({"sessionId":"owned", "method":"Page.frameNavigated",
            "params":{"frame":{"id":frame, "loaderId":loader, "url":url}}}));
    }

    fn response(tracker: &mut Tracker, frame: &str, loader: &str, status: u16) {
        tracker.observe(
            &json!({"sessionId":"owned", "method":"Network.responseReceived",
            "params":{"type":"Document", "frameId":frame, "loaderId":loader,
                "response":{"url":"https://owned.test/response", "status":status}}}),
        );
    }

    #[test]
    fn stealth_v3_document_status_stays_with_its_frame_and_loader() {
        let mut tracker = Tracker::new("owned".into());
        response(&mut tracker, "main", "A", 403);
        commit(&mut tracker, "main", "A", "https://owned.test/a");
        assert_eq!(tracker.current.status, Some(403));
        commit(&mut tracker, "main", "B", "https://owned.test/b");
        assert_eq!(
            tracker.current.status, None,
            "new document cannot inherit old 403"
        );
        response(&mut tracker, "iframe", "B", 429);
        assert_eq!(tracker.current.status, None);
        response(&mut tracker, "main", "A", 404);
        assert_eq!(
            tracker.current.status, None,
            "late previous-loader response"
        );
        response(&mut tracker, "main", "B", 201);
        assert_eq!(tracker.current.status, Some(201));
        tracker.observe(
            &json!({"sessionId":"owned", "method":"Page.navigatedWithinDocument",
            "params":{"frameId":"main", "url":"https://owned.test/b#part"}}),
        );
        assert_eq!(tracker.current.status, Some(201));
        assert_eq!(tracker.current.loader, "B");
        assert_eq!(tracker.current.generation, 3);
        let saved = tracker.current.clone();
        tracker.observe(&json!({"sessionId":"other", "method":"Page.frameNavigated",
            "params":{"frame":{"id":"main", "loaderId":"C", "url":"https://other.test"}}}));
        tracker.observe(&json!({"sessionId":"owned", "method":"Page.frameNavigated",
            "params":{"frame":{"parentId":"main", "id":"iframe", "loaderId":"C", "url":"https://other.test"}}}));
        assert_eq!(
            tracker.current, saved,
            "other sessions/subframes must not replace main document"
        );
    }

    #[test]
    fn stealth_v3_response_before_commit_is_bounded_and_missing_stays_unknown() {
        let mut tracker = Tracker::new("owned".into());
        for id in 0..40 {
            response(&mut tracker, "main", &format!("loader-{id}"), 200);
        }
        assert_eq!(tracker.responses.len(), 8);
        commit(&mut tracker, "main", "loader-0", "https://owned.test/old");
        assert_eq!(tracker.current.status, None);
        commit(
            &mut tracker,
            "main",
            "loader-39",
            "https://owned.test/current",
        );
        assert_eq!(tracker.current.status, Some(200));
        tracker.observe(
            &json!({"sessionId":"owned", "method":"Network.responseReceived",
            "params":{"type":"Document", "frameId":"main", "loaderId":"loader-39",
                "response":{"url":"https://owned.test/current", "status":200.5}}}),
        );
        assert_eq!(
            tracker.current.status, None,
            "invalid status cannot become an invented 200"
        );
    }

    #[test]
    fn stealth_v3_previous_document_cannot_complete_delayed_navigation() {
        let before = Document {
            generation: 7,
            frame: "main".into(),
            loader: "A".into(),
            url: "https://owned.test/a".into(),
            status: Some(200),
        };
        assert!(!navigation_committed(
            &before,
            &before,
            Some("B"),
            "https://owned.test/b"
        ));
        let mut after = before.clone();
        after.generation += 1;
        after.loader = "B".into();
        after.url = "https://owned.test/redirected".into();
        assert!(navigation_committed(
            &before,
            &after,
            Some("B"),
            "https://owned.test/b"
        ));
        assert!(!navigation_committed(
            &before,
            &after,
            Some("C"),
            "https://owned.test/b"
        ));
    }

    #[test]
    fn stealth_v3_reload_requires_new_loader_but_fragment_keeps_document() {
        let before = Document {
            generation: 4,
            frame: "main".into(),
            loader: "A".into(),
            url: "https://owned.test/a".into(),
            status: Some(200),
        };
        let mut after = before.clone();
        after.generation += 1;
        assert!(!navigation_committed(&before, &after, None, &before.url));
        after.url.push_str("#section");
        assert!(navigation_committed(&before, &after, None, &after.url));
        after.loader = "B".into();
        after.url = before.url.clone();
        assert!(navigation_committed(&before, &after, None, &before.url));
    }

    #[test]
    fn stealth_v3_fast_client_redirect_requires_requested_loader_ancestry() {
        let mut tracker = Tracker::new("owned".into());
        commit(&mut tracker, "main", "old", "https://owned.test/old");
        let before = tracker.current.clone();
        commit(
            &mut tracker,
            "main",
            "requested",
            "https://owned.test/start",
        );
        commit(&mut tracker, "main", "redirected", "https://owned.test/end");
        assert!(tracker.navigation_committed(
            &before,
            Some("requested"),
            "https://owned.test/start"
        ));
        assert!(!tracker.navigation_committed(
            &before,
            Some("unrelated"),
            "https://owned.test/start"
        ));
        assert!(!tracker.navigation_committed(&before, Some("old"), "https://owned.test/start"));
        let after = tracker.current.clone();
        assert!(!tracker.navigation_committed(
            &after,
            Some("requested"),
            "https://owned.test/start"
        ));
        for n in 0..16 {
            commit(
                &mut tracker,
                "main",
                &format!("next-{n}"),
                "https://owned.test/next",
            );
        }
        assert_eq!(tracker.commits.len(), 8);
        assert!(
            !tracker.navigation_committed(&before, Some("requested"), "https://owned.test/start"),
            "evicted ancestry must fail closed"
        );
    }
}
