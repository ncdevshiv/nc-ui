//! Per-window page-target binding for the process-wide `WebView2` CDP endpoint.

#[cfg(target_os = "windows")]
use std::collections::BTreeSet;
#[cfg(target_os = "windows")]
use std::path::Path;
#[cfg(target_os = "windows")]
use std::thread;
#[cfg(target_os = "windows")]
use std::time::{Duration, Instant};

use crate::cdp::{CdpError, DevToolsEndpoint};

/// The page target bound to one engine window during serialized creation.
#[cfg(feature = "wry-engine")]
#[derive(Debug, Clone)]
pub(super) enum PageTargetBinding {
    #[cfg_attr(
        not(target_os = "windows"),
        allow(
            dead_code,
            reason = "only Windows WebView2 target discovery constructs a ready page binding"
        )
    )]
    Ready {
        endpoint: DevToolsEndpoint,
        target_id: String,
    },
    Failed(CdpError),
}

#[cfg(feature = "wry-engine")]
impl PageTargetBinding {
    pub(super) fn send(&self, command: &serde_json::Value) -> Result<serde_json::Value, CdpError> {
        match self {
            Self::Failed(error) => Err(error.clone()),
            Self::Ready {
                endpoint,
                target_id,
            } => {
                // Only this browser-domain discovery command is allowed to bypass page scope.
                if command.get("method").and_then(serde_json::Value::as_str)
                    == Some("Browser.getVersion")
                {
                    crate::cdp::send_command(endpoint, command)
                } else {
                    crate::cdp::send_page_command(endpoint, target_id, command)
                }
            }
        }
    }
}

/// How long a newly built Windows webview may take to publish its page target.
#[cfg(all(feature = "wry-engine", target_os = "windows"))]
const TARGET_BIND_TIMEOUT: Duration = Duration::from_secs(90);

/// Snapshots page targets before `WebView` construction.
///
/// On the first window there is no endpoint yet. A missing port file or a stale endpoint that
/// refuses connections therefore supplies an empty baseline; a speaking endpoint returning a
/// malformed snapshot is a real failure and must not be hidden as first-start readiness.
#[cfg(all(feature = "wry-engine", target_os = "windows"))]
pub(super) fn page_targets_before_window(dir: &Path) -> Result<BTreeSet<String>, CdpError> {
    let endpoint = match crate::cdp::read_devtools_endpoint(dir) {
        Ok(endpoint) => endpoint,
        Err(CdpError::PortUnavailable { .. }) => return Ok(BTreeSet::new()),
        Err(error) => return Err(error),
    };
    match page_target_snapshot(&endpoint) {
        Err(CdpError::ConnectFailed { .. }) => Ok(BTreeSet::new()),
        result => result,
    }
}

#[cfg(all(feature = "wry-engine", target_os = "windows"))]
fn page_target_snapshot(endpoint: &DevToolsEndpoint) -> Result<BTreeSet<String>, CdpError> {
    crate::cdp::send_command(
        endpoint,
        &serde_json::json!({ "id": 1, "method": "Target.getTargets" }),
    )
    .and_then(|answer| crate::cdp::page_target_ids(&answer))
}

/// Waits for the just-built page to appear and binds exactly one new page target.
///
/// Window creation is serialized by the engine loop, so no other NC-UI window can contribute to
/// this delta. Zero targets are retried only while the engine is starting; multiple targets fail
/// immediately. URL and title are never consulted.
#[cfg(all(feature = "wry-engine", target_os = "windows"))]
pub(super) fn bind_page_target_after_window(
    dir: &Path,
    before: &BTreeSet<String>,
) -> PageTargetBinding {
    let deadline = Instant::now() + TARGET_BIND_TIMEOUT;
    loop {
        let endpoint = match crate::cdp::read_devtools_endpoint(dir) {
            Ok(endpoint) => endpoint,
            Err(error) => {
                if Instant::now() >= deadline {
                    return PageTargetBinding::Failed(error);
                }
                thread::sleep(Duration::from_millis(100));
                continue;
            }
        };
        let after = match page_target_snapshot(&endpoint) {
            Ok(after) => after,
            Err(CdpError::ConnectFailed { .. }) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(100));
                continue;
            }
            Err(error) => return PageTargetBinding::Failed(error),
        };
        if after.difference(before).next().is_none() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(100));
            continue;
        }
        return match crate::cdp::bind_new_page_target(before, &after) {
            Ok(target_id) => PageTargetBinding::Ready {
                endpoint,
                target_id,
            },
            Err(error) => PageTargetBinding::Failed(error),
        };
    }
}
