//! Settings and activity exposed to the optional local controller.
use super::ChatWidget;
use codex_protocol::openai_models::SPEED_TIER_FAST;
use serde_json::Value;
use serde_json::json;

impl ChatWidget {
    pub(crate) fn local_control_snapshot(&self) -> Value {
        let models: Vec<_> = self.model_catalog.try_list_models().unwrap_or_default()
            .into_iter().filter(|preset| preset.show_in_picker)
            .map(|preset| json!({"model":preset.model, "name":preset.display_name,
                "default":preset.default_reasoning_effort,
                "levels":preset.supported_reasoning_efforts.into_iter().map(|e| e.effort).collect::<Vec<_>>()}))
            .collect();
        let model = self.current_model();
        let preset = self
            .model_catalog
            .try_list_models()
            .ok()
            .and_then(|models| models.into_iter().find(|preset| preset.model == model));
        let fast_tier = preset
            .as_ref()
            .filter(|_| self.fast_mode_enabled() && self.has_chatgpt_account)
            .and_then(|p| {
                p.service_tiers
                    .iter()
                    .find(|tier| tier.name.eq_ignore_ascii_case(SPEED_TIER_FAST))
            })
            .map(|tier| tier.id.clone());
        let effort = self
            .current_reasoning_effort()
            .or_else(|| preset.as_ref().map(|p| p.default_reasoning_effort.clone()));
        let levels: Vec<_> = preset
            .map(|p| {
                p.supported_reasoning_efforts
                    .into_iter()
                    .map(|e| e.effort)
                    .collect()
            })
            .unwrap_or_default();
        let context = self.token_info.as_ref().and_then(|info| {
            info.model_context_window
                .filter(|window| *window > 0)
                .map(|window| {
                    (info.last_token_usage.total_tokens as f64 * 100.0 / window as f64)
                        .clamp(/*min*/ 0.0, /*max*/ 100.0)
                })
        });
        json!({"models":models,"model":model,"effort":effort,"supportedEfforts":levels,
            "collaborationMode":self.effective_collaboration_mode().mode,
            "serviceTier":self.effective_service_tier,"fastServiceTier":fast_tier,
            "state":if self.bottom_pane.is_task_running(){"WORKING"}else{"IDLE"},
            "contextPct":context,"badges":self.effective_service_tier.as_ref().map(|tier|vec![tier])})
    }
}
