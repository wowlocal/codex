use codex_app_server_protocol::McpToolCallStatus;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ThreadItem;
use codex_protocol::ThreadId;
use serde::Serialize;
use serde_json::Map;
use serde_json::Value;
use serde_json::json;
use std::collections::HashMap;
use std::collections::VecDeque;
use uuid::Uuid;

const MAX_STORED_VIEWS: usize = 32;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct ViewKey {
    thread_id: ThreadId,
    call_id: String,
}

#[derive(Clone, Debug)]
pub(super) struct ViewDescriptor {
    pub(super) thread_id: ThreadId,
    pub(super) call_id: String,
    pub(super) server: String,
    pub(super) resource_uri: String,
    pub(super) connector_id: Option<String>,
    pub(super) source: String,
    pub(super) bootstrap: ViewBootstrap,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ViewBootstrap {
    pub(super) title: String,
    tool_input: Value,
    tool_result: Value,
}

impl ViewDescriptor {
    pub(super) fn from_notification(notification: &ServerNotification) -> Option<Self> {
        let ServerNotification::ItemCompleted(notification) = notification else {
            return None;
        };
        let ThreadItem::McpToolCall {
            id,
            server,
            tool,
            status,
            arguments,
            app_context,
            mcp_app_resource_uri,
            result,
            error,
            ..
        } = &notification.item
        else {
            return None;
        };
        let resource_uri = app_context
            .as_ref()
            .and_then(|context| context.resource_uri.as_ref())
            .or(mcp_app_resource_uri.as_ref())?
            .clone();
        if !resource_uri.starts_with("ui://") {
            return None;
        }
        let thread_id = ThreadId::from_string(&notification.thread_id).ok()?;
        let source = format!("{server}.{tool}");
        let title = app_context
            .as_ref()
            .and_then(|context| context.app_name.clone())
            .unwrap_or_else(|| source.clone());
        Some(Self {
            thread_id,
            call_id: id.clone(),
            server: server.clone(),
            resource_uri,
            connector_id: app_context
                .as_ref()
                .map(|context| context.connector_id.clone()),
            source,
            bootstrap: ViewBootstrap {
                title,
                tool_input: json!({"arguments": arguments}),
                tool_result: tool_result(status, result.as_deref(), error.as_ref()),
            },
        })
    }
}

fn tool_result(
    status: &McpToolCallStatus,
    result: Option<&codex_app_server_protocol::McpToolCallResult>,
    error: Option<&codex_app_server_protocol::McpToolCallError>,
) -> Value {
    if let Some(error) = error {
        return json!({
            "content": [{"type": "text", "text": error.message}],
            "isError": true,
        });
    }
    let Some(result) = result else {
        return json!({"content": [], "isError": true});
    };
    let mut value = Map::from_iter([
        ("content".to_string(), Value::Array(result.content.clone())),
        (
            "isError".to_string(),
            Value::Bool(*status == McpToolCallStatus::Failed),
        ),
    ]);
    if let Some(structured_content) = &result.structured_content {
        value.insert("structuredContent".to_string(), structured_content.clone());
    }
    if let Some(meta) = &result.meta {
        value.insert("_meta".to_string(), meta.clone());
    }
    Value::Object(value)
}

#[derive(Default)]
pub(super) struct ViewRegistry {
    by_key: HashMap<ViewKey, String>,
    order: VecDeque<ViewKey>,
    views: HashMap<String, ViewDescriptor>,
}

impl ViewRegistry {
    pub(super) fn insert(&mut self, descriptor: ViewDescriptor) -> String {
        let key = ViewKey {
            thread_id: descriptor.thread_id,
            call_id: descriptor.call_id.clone(),
        };
        if let Some(view_id) = self.by_key.get(&key).cloned() {
            self.views.insert(view_id.clone(), descriptor);
            self.order.retain(|existing| existing != &key);
            self.order.push_back(key);
            return view_id;
        }
        while self.views.len() >= MAX_STORED_VIEWS {
            let Some(expired) = self.order.pop_front() else {
                break;
            };
            if let Some(view_id) = self.by_key.remove(&expired) {
                self.views.remove(&view_id);
            }
        }
        let view_id = Uuid::new_v4().to_string();
        self.by_key.insert(key.clone(), view_id.clone());
        self.order.push_back(key);
        self.views.insert(view_id.clone(), descriptor);
        view_id
    }

    pub(super) fn get(&self, view_id: &str) -> Option<ViewDescriptor> {
        self.views.get(view_id).cloned()
    }
}
