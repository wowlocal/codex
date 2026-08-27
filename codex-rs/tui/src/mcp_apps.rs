//! Companion-browser host for the small MCP Apps surface supported by the TUI.

use crate::app_event::AppEvent;
use crate::app_event_sender::AppEventSender;
use crate::history_cell::WebHyperlinkHistoryCell;
use crate::terminal_hyperlinks::HyperlinkLine;
use crate::version::CODEX_CLI_VERSION;
use axum::Router;
use axum::body::Body;
use axum::body::to_bytes;
use axum::extract::Path;
use axum::extract::State;
use axum::http::Response;
use axum::http::StatusCode;
use axum::routing::get;
use axum::routing::post;
use codex_app_server_client::AppServerRequestHandle;
use codex_app_server_protocol::AdditionalContextEntry;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::McpServerToolCallParams;
use codex_app_server_protocol::McpServerToolCallResponse;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ServerNotification;
use codex_protocol::ThreadId;
use ratatui::style::Stylize;
use serde::Deserialize;
use serde_json::Value;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use uuid::Uuid;

const MCP_APPS_PROTOCOL_VERSION: &str = "2026-01-26";
const MAX_RPC_BODY_BYTES: usize = 4 * 1024 * 1024;
const MAX_VIEW_BOOTSTRAP_BYTES: usize = 2 * 1024 * 1024;

mod context;
mod resource;
mod view;

use context::ContextStore;
use context::UiMessageParams;
use context::UpdateModelContextParams;
use resource::ResourceStore;
use resource::ensure_app_tool_visible;
use resource::load_app_resource;
use resource::resource_csp;
use view::ViewDescriptor;
use view::ViewRegistry;

#[cfg(test)]
#[path = "mcp_apps_tests.rs"]
mod tests;

pub(crate) struct McpAppsBrowser {
    state: Arc<BridgeState>,
    tasks: Vec<JoinHandle<()>>,
}

impl McpAppsBrowser {
    pub(crate) async fn start(
        request_handle: AppServerRequestHandle,
        app_event_tx: AppEventSender,
    ) -> std::io::Result<Self> {
        let host_listener = TcpListener::bind("127.0.0.1:0").await?;
        let sandbox_listener = TcpListener::bind("127.0.0.1:0").await?;
        let host_origin = format!("http://{}", host_listener.local_addr()?);
        let sandbox_origin = format!("http://{}", sandbox_listener.local_addr()?);
        let state = Arc::new(BridgeState {
            request_handle,
            app_event_tx,
            host_origin,
            sandbox_origin,
            token: Uuid::new_v4().to_string(),
            registry: Mutex::new(ViewRegistry::default()),
            contexts: Mutex::new(ContextStore::default()),
            resources: Mutex::new(ResourceStore::default()),
        });

        let host_router = Router::new()
            .route("/view/{view_id}/{token}", get(host_view))
            .route("/alive/{view_id}/{token}", get(view_alive))
            .route("/bootstrap/{view_id}/{token}", get(view_bootstrap))
            .route("/resource/{view_id}/{token}", get(view_resource))
            .route("/rpc/{view_id}/{instance_id}/{token}", post(view_rpc))
            .route(
                "/lifecycle/{view_id}/{instance_id}/{token}",
                post(view_closed),
            )
            .with_state(state.clone());
        let sandbox_router = Router::new()
            .route("/sandbox/{view_id}/{token}", get(sandbox_view))
            .with_state(state.clone());
        let host_task = tokio::spawn(serve(host_listener, host_router, "MCP Apps browser host"));
        let sandbox_task = tokio::spawn(serve(
            sandbox_listener,
            sandbox_router,
            "MCP Apps sandbox host",
        ));

        Ok(Self {
            state,
            tasks: vec![host_task, sandbox_task],
        })
    }

    pub(crate) fn register_notification(
        &self,
        notification: &ServerNotification,
    ) -> Option<McpAppViewLink> {
        let descriptor = ViewDescriptor::from_notification(notification)?;
        if serde_json::to_vec(&descriptor.bootstrap)
            .is_ok_and(|value| value.len() > MAX_VIEW_BOOTSTRAP_BYTES)
        {
            tracing::warn!(
                call_id = descriptor.call_id,
                "MCP App result is too large to render"
            );
            return None;
        }
        let title = descriptor.bootstrap.title.clone();
        let view_id = lock(&self.state.registry).insert(descriptor);
        lock(&self.state.resources).remove(&view_id);
        Some(McpAppViewLink {
            title,
            url: format!(
                "{}/view/{view_id}/{}",
                self.state.host_origin, self.state.token
            ),
        })
    }

    pub(crate) fn additional_context(
        &self,
        thread_id: ThreadId,
    ) -> HashMap<String, AdditionalContextEntry> {
        lock(&self.state.contexts).snapshot(thread_id)
    }
}

impl Drop for McpAppsBrowser {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

async fn serve(listener: TcpListener, router: Router, component: &'static str) {
    if let Err(error) = axum::serve(listener, router).await {
        tracing::warn!(%error, component, "companion-browser server stopped");
    }
}

pub(crate) struct McpAppViewLink {
    title: String,
    url: String,
}

impl McpAppViewLink {
    pub(crate) fn history_cell(self) -> WebHyperlinkHistoryCell {
        let mut line = HyperlinkLine::new(vec!["  └ ".dim(), "MCP App: ".dim()].into());
        line.push_span(self.title.into(), /*destination*/ None);
        line.push_span(" — ".dim(), /*destination*/ None);
        line.push_span("Open in browser".cyan().underlined(), Some(&self.url));
        WebHyperlinkHistoryCell::new_hyperlink_lines(vec![line])
    }
}

struct BridgeState {
    request_handle: AppServerRequestHandle,
    app_event_tx: AppEventSender,
    host_origin: String,
    sandbox_origin: String,
    token: String,
    registry: Mutex<ViewRegistry>,
    contexts: Mutex<ContextStore>,
    resources: Mutex<ResourceStore>,
}

async fn host_view(
    State(state): State<Arc<BridgeState>>,
    Path((view_id, token)): Path<(String, String)>,
) -> Response<Body> {
    if descriptor(&state, &view_id, &token).is_none() {
        return not_found();
    }
    let instance_id = Uuid::new_v4().to_string();
    let config = json!({
        "bootstrapUrl": format!("{}/bootstrap/{view_id}/{token}", state.host_origin),
        "aliveUrl": format!("{}/alive/{view_id}/{token}", state.host_origin),
        "channel": token,
        "closeUrl": format!("{}/lifecycle/{view_id}/{instance_id}/{token}", state.host_origin),
        "hostOrigin": state.host_origin,
        "protocolVersion": MCP_APPS_PROTOCOL_VERSION,
        "resourceUrl": format!("{}/resource/{view_id}/{token}", state.host_origin),
        "rpcUrl": format!("{}/rpc/{view_id}/{instance_id}/{token}", state.host_origin),
        "sandboxOrigin": state.sandbox_origin,
        "sandboxUrl": format!("{}/sandbox/{view_id}/{token}", state.sandbox_origin),
        "version": CODEX_CLI_VERSION,
    });
    html_response(
        include_str!("mcp_apps/host.html").replace("__CODEX_CONFIG__", &inline_json(&config)),
        format!(
            "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; connect-src 'self'; frame-src {}; base-uri 'none'; form-action 'none'; frame-ancestors 'none'",
            state.sandbox_origin
        ),
    )
}

async fn view_alive(
    State(state): State<Arc<BridgeState>>,
    Path((view_id, token)): Path<(String, String)>,
) -> Response<Body> {
    if descriptor(&state, &view_id, &token).is_none() {
        return not_found();
    }
    response(
        StatusCode::NO_CONTENT,
        "text/plain; charset=utf-8",
        Body::empty(),
        /*csp*/ None,
    )
}

async fn sandbox_view(
    State(state): State<Arc<BridgeState>>,
    Path((view_id, token)): Path<(String, String)>,
) -> Response<Body> {
    if descriptor(&state, &view_id, &token).is_none() {
        return not_found();
    }
    let Some(resource) = lock(&state.resources).get(&view_id) else {
        return text_error(
            "MCP App resource must be loaded before its sandbox".to_string(),
            &state.host_origin,
        );
    };
    let config = json!({
        "channel": token,
        "hostOrigin": state.host_origin,
    });
    html_response(
        include_str!("mcp_apps/sandbox.html").replace("__CODEX_CONFIG__", &inline_json(&config)),
        resource_csp(&resource.csp, &state.host_origin),
    )
}

async fn view_bootstrap(
    State(state): State<Arc<BridgeState>>,
    Path((view_id, token)): Path<(String, String)>,
) -> Response<Body> {
    let Some(descriptor) = descriptor(&state, &view_id, &token) else {
        return not_found();
    };
    json_response(
        StatusCode::OK,
        serde_json::to_value(descriptor.bootstrap).unwrap_or_default(),
    )
}

async fn view_resource(
    State(state): State<Arc<BridgeState>>,
    Path((view_id, token)): Path<(String, String)>,
) -> Response<Body> {
    let Some(descriptor) = descriptor(&state, &view_id, &token) else {
        return not_found();
    };
    match load_app_resource(&state.request_handle, &descriptor).await {
        Ok(resource) => {
            lock(&state.resources).insert(&view_id, resource.clone());
            json_response(
                StatusCode::OK,
                serde_json::to_value(resource).unwrap_or_default(),
            )
        }
        Err(error) => text_error(error, &state.host_origin),
    }
}

async fn view_rpc(
    State(state): State<Arc<BridgeState>>,
    Path((view_id, instance_id, token)): Path<(String, String, String)>,
    body: Body,
) -> Response<Body> {
    let Some(descriptor) = descriptor(&state, &view_id, &token) else {
        return not_found();
    };
    let request = match to_bytes(body, MAX_RPC_BODY_BYTES)
        .await
        .ok()
        .and_then(|body| serde_json::from_slice::<JsonRpcRequest>(&body).ok())
    {
        Some(request) if request.jsonrpc == "2.0" => request,
        _ => {
            return json_response(
                StatusCode::OK,
                rpc_error(Value::Null, -32600, "Invalid request"),
            );
        }
    };
    let id = request.id.unwrap_or(Value::Null);
    let allow_missing_message_role = lock(&state.resources)
        .get(&view_id)
        .is_some_and(|resource| resource.legacy_mime_type);
    let result = match request.method.as_str() {
        "ui/update-model-context" => update_model_context(
            &state,
            &descriptor,
            &view_id,
            &context_key(&view_id, &instance_id),
            request.params.unwrap_or_else(|| json!({})),
        )
        .map(|()| json!({})),
        "ui/message" => send_message(
            &state,
            descriptor.thread_id,
            request.params.unwrap_or(Value::Null),
            allow_missing_message_role,
        )
        .await
        .map(|()| json!({})),
        "tools/call" => {
            call_server_tool(
                &state,
                &descriptor,
                request.params.unwrap_or_else(|| json!({})),
            )
            .await
        }
        _ => Err((-32601, "Method not found".to_string())),
    };
    let response = match result {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err((code, message)) => rpc_error(id, code, &message),
    };
    json_response(StatusCode::OK, response)
}

async fn view_closed(
    State(state): State<Arc<BridgeState>>,
    Path((view_id, instance_id, token)): Path<(String, String, String)>,
) -> Response<Body> {
    if token != state.token {
        return not_found();
    }
    lock(&state.contexts).clear_source(&context_key(&view_id, &instance_id));
    response(
        StatusCode::NO_CONTENT,
        "text/plain; charset=utf-8",
        Body::empty(),
        /*csp*/ None,
    )
}

fn update_model_context(
    state: &BridgeState,
    descriptor: &ViewDescriptor,
    registered_view_id: &str,
    source_id: &str,
    params: Value,
) -> Result<(), (i64, String)> {
    let params = serde_json::from_value::<UpdateModelContextParams>(params)
        .map_err(|error| (-32602, format!("Invalid context update: {error}")))?;
    let value = params
        .into_context(&descriptor.source)
        .map_err(|error| (-32602, error))?;
    let mut contexts = lock(&state.contexts);
    contexts.clear_view(descriptor.thread_id, registered_view_id);
    contexts.update(descriptor.thread_id, source_id, value);
    Ok(())
}

async fn send_message(
    state: &BridgeState,
    thread_id: ThreadId,
    params: Value,
    allow_missing_role: bool,
) -> Result<(), (i64, String)> {
    let text = serde_json::from_value::<UiMessageParams>(params)
        .map_err(|error| (-32602, format!("Invalid message: {error}")))?
        .into_text(allow_missing_role)
        .map_err(|error| (-32602, error))?;
    let (response_tx, response_rx) = oneshot::channel();
    state.app_event_tx.send(AppEvent::McpAppMessage {
        thread_id,
        text,
        response_tx,
    });
    match tokio::time::timeout(Duration::from_secs(30), response_rx).await {
        Ok(Ok(Ok(()))) => Ok(()),
        Ok(Ok(Err(error))) => Err((-32603, error)),
        Ok(Err(_)) => Err((-32603, "Codex closed the browser bridge".to_string())),
        Err(_) => Err((
            -32603,
            "Codex did not accept the message in time".to_string(),
        )),
    }
}

async fn call_server_tool(
    state: &BridgeState,
    descriptor: &ViewDescriptor,
    params: Value,
) -> Result<Value, (i64, String)> {
    let params = serde_json::from_value::<ViewToolCallParams>(params)
        .map_err(|error| (-32602, format!("Invalid tool call: {error}")))?;
    ensure_app_tool_visible(&state.request_handle, descriptor, &params.name)
        .await
        .map_err(|error| (-32601, error))?;
    let request_id = RequestId::String(format!("mcp-app-tool-{}", Uuid::new_v4()));
    let response: McpServerToolCallResponse = state
        .request_handle
        .request_typed(ClientRequest::McpServerToolCall {
            request_id,
            params: McpServerToolCallParams {
                thread_id: descriptor.thread_id.to_string(),
                server: descriptor.server.clone(),
                tool: params.name,
                arguments: params.arguments.map(Value::Object),
                meta: params.meta,
            },
        })
        .await
        .map_err(|error| (-32603, format!("MCP tool call failed: {error}")))?;
    serde_json::to_value(response).map_err(|error| {
        (
            -32603,
            format!("Could not serialize MCP tool result: {error}"),
        )
    })
}

#[derive(Deserialize)]
struct JsonRpcRequest {
    jsonrpc: String,
    id: Option<Value>,
    method: String,
    params: Option<Value>,
}

#[derive(Deserialize)]
struct ViewToolCallParams {
    name: String,
    arguments: Option<serde_json::Map<String, Value>>,
    #[serde(rename = "_meta")]
    meta: Option<Value>,
}

fn descriptor(state: &BridgeState, view_id: &str, token: &str) -> Option<ViewDescriptor> {
    (token == state.token)
        .then(|| lock(&state.registry).get(view_id))
        .flatten()
}

fn context_key(view_id: &str, instance_id: &str) -> String {
    format!("{view_id}-{instance_id}")
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn inline_json(value: &Value) -> String {
    serde_json::to_string(value)
        .unwrap_or_else(|_| "{}".to_string())
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026")
}

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

fn html_response(html: String, csp: String) -> Response<Body> {
    response(
        StatusCode::OK,
        "text/html; charset=utf-8",
        Body::from(html),
        Some(csp),
    )
}

fn json_response(status: StatusCode, value: Value) -> Response<Body> {
    response(
        status,
        "application/json; charset=utf-8",
        Body::from(value.to_string()),
        /*csp*/ None,
    )
}

fn text_error(message: String, sandbox_origin: &str) -> Response<Body> {
    response(
        StatusCode::BAD_GATEWAY,
        "text/plain; charset=utf-8",
        Body::from(message),
        Some(format!(
            "default-src 'none'; frame-ancestors {sandbox_origin}"
        )),
    )
}

fn not_found() -> Response<Body> {
    response(
        StatusCode::NOT_FOUND,
        "text/plain; charset=utf-8",
        Body::from("Not found"),
        Some("default-src 'none'; frame-ancestors 'none'".to_string()),
    )
}

fn response(
    status: StatusCode,
    content_type: &'static str,
    body: Body,
    csp: Option<String>,
) -> Response<Body> {
    let mut builder = Response::builder()
        .status(status)
        .header("content-type", content_type)
        .header("cache-control", "no-store")
        .header("x-content-type-options", "nosniff")
        .header("referrer-policy", "no-referrer")
        .header(
            "permissions-policy",
            "camera=(), microphone=(), geolocation=(), clipboard-write=()",
        );
    if let Some(csp) = csp {
        builder = builder.header("content-security-policy", csp);
    }
    builder
        .body(body)
        .unwrap_or_else(|_| Response::new(Body::empty()))
}
