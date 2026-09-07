//! Typed commands keep wire compatibility with the original effort endpoint.
use codex_protocol::openai_models::ReasoningEffort;
use serde::Deserialize;
use serde::Serialize;
use uuid::Uuid;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct EffortRequest {
    pub request_id: Uuid,
    pub expected_revision: u64,
    pub expected_thread_id: String,
    pub effort: ReasoningEffort,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct FastRequest {
    pub request_id: Uuid,
    pub expected_revision: u64,
    pub expected_thread_id: String,
    pub enabled: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ModelRequest {
    pub request_id: Uuid,
    pub expected_revision: u64,
    pub expected_thread_id: String,
    pub model: String,
    pub effort: ReasoningEffort,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum SettingsChange {
    Effort(ReasoningEffort),
    Model {
        model: String,
        effort: ReasoningEffort,
    },
    Fast {
        enabled: bool,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SettingsRequest {
    pub request_id: Uuid,
    pub expected_revision: u64,
    pub expected_thread_id: String,
    pub change: SettingsChange,
}

impl From<EffortRequest> for SettingsRequest {
    fn from(request: EffortRequest) -> Self {
        Self {
            request_id: request.request_id,
            expected_revision: request.expected_revision,
            expected_thread_id: request.expected_thread_id,
            change: SettingsChange::Effort(request.effort),
        }
    }
}

impl From<FastRequest> for SettingsRequest {
    fn from(request: FastRequest) -> Self {
        Self {
            request_id: request.request_id,
            expected_revision: request.expected_revision,
            expected_thread_id: request.expected_thread_id,
            change: SettingsChange::Fast {
                enabled: request.enabled,
            },
        }
    }
}

impl From<ModelRequest> for SettingsRequest {
    fn from(request: ModelRequest) -> Self {
        Self {
            request_id: request.request_id,
            expected_revision: request.expected_revision,
            expected_thread_id: request.expected_thread_id,
            change: SettingsChange::Model {
                model: request.model,
                effort: request.effort,
            },
        }
    }
}
