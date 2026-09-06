//! Dispatch local settings requests in the same event loop as human TUI input.
use super::App;
use crate::app_server_session::AppServerSession;
use crate::local_control::EffortRequest;
use crate::local_control::LocalControl;
use crate::tui::Tui;
use codex_app_server_protocol::ThreadSettingsUpdateParams;
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
            && !self.startup_protected_input_boundary
            && thread.is_some_and(|id| !self.agent_navigation.is_parent_owned(id));
        state["threadId"] = json!(thread.map(|id| id.to_string()));
        state["ready"] = ready.into();
        state["focused"] = tui
            .terminal_focused
            .load(std::sync::atomic::Ordering::Relaxed)
            .into();
        state
    }

    pub(super) async fn apply_local_effort(
        &mut self,
        tui: &Tui,
        app_server: &mut AppServerSession,
        control: &mut LocalControl,
        request: EffortRequest,
    ) {
        let state = self.local_control_snapshot(tui);
        control.publish(state.clone());
        let error = if state["ready"] != true || state["focused"] != true {
            Some("TUI is not ready and focused")
        } else if request.expected_revision != control.revision
            || state["threadId"] != request.expected_thread_id
        {
            Some("selection or settings changed")
        } else if !state["supportedEfforts"]
            .as_array()
            .is_some_and(|levels| levels.contains(&json!(request.effort)))
        {
            Some("effort is not supported by the selected model")
        } else {
            None
        };
        if let Some(error) = error {
            control.finish(request.request_id, json!({"error":error}));
            return;
        }
        if state["effort"] == json!(request.effort) {
            control.finish(request.request_id, json!({"threadId":state["threadId"],"model":state["model"],"effort":state["effort"]}));
            return;
        }
        let mut mode = self.chat_widget.effective_collaboration_mode();
        mode.settings.reasoning_effort = Some(request.effort.clone());
        let params = ThreadSettingsUpdateParams {
            thread_id: request.expected_thread_id.clone(),
            effort: Some(request.effort.clone()),
            collaboration_mode: Some(mode),
            ..ThreadSettingsUpdateParams::default()
        };
        control.pending = Some((
            request.clone(),
            self.chat_widget.current_model().to_string(),
        ));
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
