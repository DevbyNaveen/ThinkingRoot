//! Single-slot bus that bridges the desktop's OS-level deep-link
//! handler (lives in `apps/thinkingroot-desktop/src-tauri/src/lib.rs`)
//! to the in-flight `run_browser_login_deeplink` call.
//!
//! Why a singleton: the OS deep-link handler is registered once at
//! startup with no per-call context. There's exactly one in-flight
//! login at a time (enforced by `LOGIN_IN_FLIGHT` in `auth_flow`),
//! so the bus only ever holds one expected state + one oneshot
//! Sender at a time. Trying to deliver when nothing is armed is a
//! no-op (handles the case where the user clicks the
//! `thinkingroot://...` link after cancelling the in-app sign-in).
//!
//! State-nonce check happens here — the bus is the trust boundary
//! between "OS handed us a URL" and "this URL belongs to the
//! in-flight login flow we started". A nonce mismatch returns
//! `Deliver::StateMismatch` so callers can warn-log without
//! aborting the desktop.

use std::sync::Mutex;

use chrono::{DateTime, Utc};
use tokio::sync::oneshot;

#[derive(Debug, Clone)]
pub struct DeepLinkCallback {
    pub token: String,
    pub handle: String,
    pub tier: String,
    pub expires_at: DateTime<Utc>,
    pub azure_endpoint: Option<String>,
    pub azure_api_version: Option<String>,
    pub azure_deployment: Option<String>,
    pub azure_key: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Deliver {
    /// Callback was claimed by an in-flight login. Sender consumed.
    Delivered,
    /// State nonce did not match the armed expectation. Bus stays armed.
    StateMismatch,
    /// No login was in flight when the URL arrived. Drop silently.
    NotArmed,
}

struct Armed {
    expected_state: String,
    tx: oneshot::Sender<DeepLinkCallback>,
}

static BUS: Mutex<Option<Armed>> = Mutex::new(None);

/// Arm the bus before opening the browser. Returns the receiver
/// half — caller awaits this. If a previous arm is still active it
/// is dropped (its Receiver will see `RecvError` and the previous
/// flow exits cleanly).
pub fn arm(expected_state: String) -> oneshot::Receiver<DeepLinkCallback> {
    let (tx, rx) = oneshot::channel();
    let mut guard = BUS.lock().expect("deep_link_bus poisoned");
    *guard = Some(Armed {
        expected_state,
        tx,
    });
    rx
}

/// Disarm the bus (called by `run_browser_login_deeplink` after the
/// receiver completes or its select! arm fires).
pub fn disarm() {
    let mut guard = BUS.lock().expect("deep_link_bus poisoned");
    *guard = None;
}

/// Deliver a callback parsed from a `thinkingroot://` URL. Called
/// by the Tauri deep-link handler. Validates the state nonce
/// against the armed expectation.
pub fn deliver(state: &str, params: DeepLinkCallback) -> Deliver {
    let mut guard = BUS.lock().expect("deep_link_bus poisoned");
    let Some(armed) = guard.take() else {
        return Deliver::NotArmed;
    };
    if armed.expected_state != state {
        // Re-arm with the original Sender so a real (matching) URL
        // arriving later still works.
        *guard = Some(armed);
        return Deliver::StateMismatch;
    }
    let _ = armed.tx.send(params);
    Deliver::Delivered
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn dummy_cb() -> DeepLinkCallback {
        DeepLinkCallback {
            token: "tok".into(),
            handle: "h".into(),
            tier: "free".into(),
            expires_at: Utc.with_ymd_and_hms(2030, 1, 1, 0, 0, 0).unwrap(),
            azure_endpoint: None,
            azure_api_version: None,
            azure_deployment: None,
            azure_key: None,
        }
    }

    #[test]
    fn deliver_without_arm_returns_not_armed() {
        disarm(); // reset
        assert_eq!(deliver("anything", dummy_cb()), Deliver::NotArmed);
    }

    #[tokio::test]
    async fn matching_state_delivers() {
        disarm();
        let rx = arm("S1".into());
        assert_eq!(deliver("S1", dummy_cb()), Deliver::Delivered);
        let got = rx.await.expect("recv");
        assert_eq!(got.token, "tok");
    }

    #[tokio::test]
    async fn mismatched_state_keeps_armed() {
        disarm();
        let rx = arm("S1".into());
        assert_eq!(deliver("WRONG", dummy_cb()), Deliver::StateMismatch);
        // Bus still armed → matching deliver still works.
        assert_eq!(deliver("S1", dummy_cb()), Deliver::Delivered);
        let _ = rx.await.expect("recv");
    }
}
