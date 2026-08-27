use codex_app_server_protocol::AdditionalContextEntry;
use codex_app_server_protocol::AdditionalContextKind;
use codex_protocol::ThreadId;
use serde::Deserialize;
use serde_json::Map;
use serde_json::Value;
use std::collections::HashMap;
use std::collections::VecDeque;

const MAX_CONTEXT_BYTES: usize = 16 * 1024;
pub(super) const MAX_CONTEXT_CHARS: usize = 4_096;
const MAX_MESSAGE_BYTES: usize = 16 * 1024;
const MAX_MESSAGE_CHARS: usize = 4_096;
const MAX_STORED_CONTEXTS: usize = 32;
const MAX_CONTEXTS_PER_TURN: usize = 8;

#[derive(Default)]
pub(super) struct ContextStore {
    values: HashMap<(ThreadId, String), AdditionalContextEntry>,
    order: VecDeque<(ThreadId, String)>,
}

impl ContextStore {
    pub(super) fn update(&mut self, thread_id: ThreadId, view_id: &str, value: Option<String>) {
        let key = (thread_id, view_id.to_string());
        self.values.remove(&key);
        self.order.retain(|existing| existing != &key);
        if let Some(value) = value {
            self.values.insert(
                key.clone(),
                AdditionalContextEntry {
                    value,
                    kind: AdditionalContextKind::Untrusted,
                },
            );
            self.order.push_back(key);
        }
        while self.values.len() > MAX_STORED_CONTEXTS {
            if let Some(expired) = self.order.pop_front() {
                self.values.remove(&expired);
            }
        }
    }

    pub(super) fn snapshot(&self, thread_id: ThreadId) -> HashMap<String, AdditionalContextEntry> {
        self.order
            .iter()
            .rev()
            .filter(|(stored_thread_id, _)| *stored_thread_id == thread_id)
            .take(MAX_CONTEXTS_PER_TURN)
            .filter_map(|key| {
                self.values.get(key).cloned().map(|value| {
                    let source = key.1.replace('-', "_");
                    (format!("mcp_app_{source}"), value)
                })
            })
            .collect()
    }

    pub(super) fn clear_source(&mut self, source: &str) {
        self.values
            .retain(|(_, stored_source), _| stored_source != source);
        self.order
            .retain(|(_, stored_source)| stored_source != source);
    }

    pub(super) fn clear_view(&mut self, thread_id: ThreadId, view_id: &str) {
        let prefix = format!("{view_id}-");
        self.values.retain(|(stored_thread_id, stored_source), _| {
            *stored_thread_id != thread_id || !stored_source.starts_with(&prefix)
        });
        self.order.retain(|(stored_thread_id, stored_source)| {
            *stored_thread_id != thread_id || !stored_source.starts_with(&prefix)
        });
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct UpdateModelContextParams {
    content: Option<Vec<TextContentBlock>>,
    structured_content: Option<Map<String, Value>>,
}

impl UpdateModelContextParams {
    pub(super) fn into_context(self, source: &str) -> Result<Option<String>, String> {
        let mut parts = Vec::new();
        if let Some(content) = self.content {
            let text = content
                .into_iter()
                .map(TextContentBlock::text)
                .filter(|text| !text.is_empty())
                .collect::<Vec<_>>()
                .join("\n");
            if !text.is_empty() {
                parts.push(text);
            }
        }
        if let Some(structured_content) = self.structured_content {
            let structured = serde_json::to_string_pretty(&structured_content)
                .map_err(|error| format!("Could not serialize structured context: {error}"))?;
            parts.push(format!("Structured content:\n{structured}"));
        }
        if parts.is_empty() {
            return Ok(None);
        }
        let value = format!("MCP App context from {source}:\n{}", parts.join("\n\n"));
        ensure_bounded(
            &value,
            MAX_CONTEXT_BYTES,
            MAX_CONTEXT_CHARS,
            "Context update",
        )?;
        Ok(Some(value))
    }
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum TextContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
}

impl TextContentBlock {
    fn text(self) -> String {
        match self {
            Self::Text { text } => text,
        }
    }
}

#[derive(Deserialize)]
pub(super) struct UiMessageParams {
    role: Option<UiMessageRole>,
    content: Vec<TextContentBlock>,
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum UiMessageRole {
    User,
}

impl UiMessageParams {
    pub(super) fn into_text(self, allow_missing_role: bool) -> Result<String, String> {
        match self.role {
            Some(UiMessageRole::User) => {}
            None if allow_missing_role => {}
            None => return Err("Message role must be user".to_string()),
        }
        let text = self
            .content
            .into_iter()
            .map(TextContentBlock::text)
            .collect::<Vec<_>>()
            .join("\n");
        if text.is_empty() {
            return Err("Message must contain text".to_string());
        }
        ensure_bounded(&text, MAX_MESSAGE_BYTES, MAX_MESSAGE_CHARS, "Message")?;
        Ok(text)
    }
}

fn ensure_bounded(
    value: &str,
    max_bytes: usize,
    max_chars: usize,
    label: &str,
) -> Result<(), String> {
    if value.len() > max_bytes || value.chars().count() > max_chars {
        return Err(format!(
            "{label} exceeds the {max_chars}-character / {max_bytes}-byte limit"
        ));
    }
    Ok(())
}
