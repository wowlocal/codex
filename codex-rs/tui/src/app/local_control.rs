//! Dispatch local settings requests in the same event loop as human TUI input.
use super::App;
use crate::app_server_session::AppServerSession;
use crate::local_control::LocalControl;
use crate::local_control::SettingsChange;
use crate::local_control::SettingsRequest;
use crate::tui::Tui;
use codex_app_server_protocol::ThreadSettingsUpdateParams;
use codex_protocol::config_types::SERVICE_TIER_DEFAULT_REQUEST_VALUE;
use serde_json::Value;
use serde_json::json;

impl App {
    pub(super) fn local_control_snapshot(&self, tui: &Tui) -> Value {
        let mut state = self.chat_widget.local_control_snapshot();
        let thread = self.current_displayed_thread_id();
        let ready = thread.is_some()
            && thread == self.chat_widget.thread_id()
            && thread == self.active_thread_id
            && !self.reconnect.offline
            && !self.chat_widget.has_active_view()
            && self.overlay.is_none()
            && !self.startup_pending_protected_request
            && !self.has_queued_startup_protected_request()
            && !self.chat_widget.has_pending_protected_request()
            && thread.is_some_and(|id| !self.agent_navigation.is_parent_owned(id));
        state["threadId"] = json!(thread.map(|id| id.to_string()));
        state["ready"] = ready.into();
        state["focused"] = tui
            .terminal_focused
            .load(std::sync::atomic::Ordering::Relaxed)
            .into();
        state
    }

    pub(super) async fn apply_local_settings(
        &mut self,
        tui: &Tui,
        app_server: &mut AppServerSession,
        control: &mut LocalControl,
        request: SettingsRequest,
    ) {
        let state = self.local_control_snapshot(tui);
        control.publish(state.clone());
        let error = if state["ready"] != true || state["focused"] != true {
            Some("TUI is not ready and focused")
        } else if request.expected_revision != control.revision
            || state["threadId"] != request.expected_thread_id
        {
            Some("selection or settings changed")
        } else {
            match &request.change {
                SettingsChange::Effort(effort)
                    if !state["supportedEfforts"]
                        .as_array()
                        .is_some_and(|levels| levels.contains(&json!(effort))) =>
                {
                    Some("effort is not supported by the selected model")
                }
                SettingsChange::Fast { .. } if !state["fastServiceTier"].is_string() => {
                    Some("Fast mode is not supported by the selected model or account")
                }
                SettingsChange::Effort(_) | SettingsChange::Fast { .. } => None,
            }
        };
        if let Some(error) = error {
            control.finish(request.request_id, json!({"error":error}));
            return;
        }
        let mut expected = json!({"model":state["model"]});
        let mut params = ThreadSettingsUpdateParams {
            thread_id: request.expected_thread_id.clone(),
            ..ThreadSettingsUpdateParams::default()
        };
        match &request.change {
            SettingsChange::Effort(effort) => {
                let mut mode = self.chat_widget.effective_collaboration_mode();
                mode.settings.reasoning_effort = Some(effort.clone());
                params.effort = Some(effort.clone());
                params.collaboration_mode = Some(mode);
                expected["effort"] = json!(effort);
            }
            SettingsChange::Fast { enabled } => {
                let Some(fast_tier) = state["fastServiceTier"].as_str() else {
                    control.finish(
                        request.request_id,
                        json!({"error":"Fast mode is unavailable"}),
                    );
                    return;
                };
                let tier = if *enabled {
                    fast_tier
                } else {
                    SERVICE_TIER_DEFAULT_REQUEST_VALUE
                };
                params.service_tier = Some(Some(tier.to_string()));
                // A selected Plan mask may still be local until the next turn.
                // Preserve that visible mode when the backend confirms Fast.
                params.collaboration_mode = Some(self.chat_widget.effective_collaboration_mode());
                expected["serviceTier"] = json!(tier);
            }
        }
        if expected
            .as_object()
            .is_some_and(|fields| fields.iter().all(|(key, value)| state[key] == *value))
        {
            expected["threadId"] = state["threadId"].clone();
            control.finish(request.request_id, expected);
            return;
        }
        control.pending = Some((request.clone(), expected));
        control.submitted_at = Some(std::time::Instant::now());
        // Do not optimistically change the widget. Its ordinary native settings
        // notification updates the composer, and separately confirms the request.
        match app_server.thread_settings_update(params).await {
            Ok(true) => (),
            Ok(false) => control.finish(
                request.request_id,
                json!({"error":"settings updates unavailable"}),
            ),
            Err(_) => control.finish(
                request.request_id,
                json!({"error":"native settings request failed","uncertain":true}),
            ),
        }
    }
}
