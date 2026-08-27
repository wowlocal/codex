use base64::Engine;
use codex_app_server_client::AppServerRequestHandle;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::ListMcpServerStatusParams;
use codex_app_server_protocol::ListMcpServerStatusResponse;
use codex_app_server_protocol::McpResourceContent;
use codex_app_server_protocol::McpResourceReadParams;
use codex_app_server_protocol::McpResourceReadResponse;
use codex_app_server_protocol::McpServerStatus;
use codex_app_server_protocol::McpServerStatusDetail;
use codex_app_server_protocol::RequestId;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::collections::VecDeque;
use url::Url;
use uuid::Uuid;

use super::view::ViewDescriptor;

const MCP_APP_MIME_TYPE: &str = "text/html;profile=mcp-app";
const LEGACY_MCP_APP_MIME_TYPE: &str = "text/html";
const MAX_RESOURCE_BYTES: usize = 4 * 1024 * 1024;
const MAX_STORED_RESOURCES: usize = 32;
const MAX_STATUS_PAGES: usize = 20;

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct AppResource {
    pub(super) html: String,
    pub(super) csp: ResourceCsp,
    pub(super) permissions: ResourcePermissions,
    #[serde(skip)]
    pub(super) legacy_mime_type: bool,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ResourceCsp {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) connect_domains: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) resource_domains: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) frame_domains: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) base_uri_domains: Option<Vec<String>>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ResourcePermissions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) camera: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) microphone: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) geolocation: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) clipboard_write: Option<Value>,
}

#[derive(Default, Deserialize)]
struct ResourceMeta {
    csp: Option<ResourceCsp>,
    permissions: Option<ResourcePermissions>,
}

#[derive(Default)]
pub(super) struct ResourceStore {
    resources: HashMap<String, AppResource>,
    order: VecDeque<String>,
}

impl ResourceStore {
    pub(super) fn insert(&mut self, view_id: &str, resource: AppResource) {
        self.resources.insert(view_id.to_string(), resource);
        self.order.retain(|existing| existing != view_id);
        self.order.push_back(view_id.to_string());
        while self.resources.len() > MAX_STORED_RESOURCES {
            if let Some(expired) = self.order.pop_front() {
                self.resources.remove(&expired);
            }
        }
    }

    pub(super) fn get(&self, view_id: &str) -> Option<AppResource> {
        self.resources.get(view_id).cloned()
    }

    pub(super) fn remove(&mut self, view_id: &str) {
        self.resources.remove(view_id);
        self.order.retain(|existing| existing != view_id);
    }
}

pub(super) async fn load_app_resource(
    request_handle: &AppServerRequestHandle,
    descriptor: &ViewDescriptor,
) -> Result<AppResource, String> {
    let request_id = RequestId::String(format!("mcp-app-resource-{}", Uuid::new_v4()));
    let response: McpResourceReadResponse = request_handle
        .request_typed(ClientRequest::McpResourceRead {
            request_id,
            params: McpResourceReadParams {
                thread_id: Some(descriptor.thread_id.to_string()),
                origin_call_id: Some(descriptor.call_id.clone()),
                server: descriptor.server.clone(),
                uri: descriptor.resource_uri.clone(),
                connector_id: descriptor.connector_id.clone(),
            },
        })
        .await
        .map_err(|error| format!("Failed to read MCP App resource: {error}"))?;
    let listing_meta = if content_has_ui_meta(&response.contents, &descriptor.resource_uri) {
        None
    } else {
        listing_resource_meta(request_handle, descriptor).await
    };
    extract_resource(
        response.contents,
        &descriptor.resource_uri,
        listing_meta.as_ref(),
    )
}

fn content_has_ui_meta(contents: &[McpResourceContent], expected_uri: &str) -> bool {
    contents.iter().any(|content| match content {
        McpResourceContent::Text { uri, meta, .. } | McpResourceContent::Blob { uri, meta, .. } => {
            uri == expected_uri && meta.as_ref().is_some_and(|meta| meta.get("ui").is_some())
        }
    })
}

async fn listing_resource_meta(
    request_handle: &AppServerRequestHandle,
    descriptor: &ViewDescriptor,
) -> Option<Value> {
    server_status(request_handle, descriptor)
        .await?
        .resources
        .into_iter()
        .find(|resource| resource.uri == descriptor.resource_uri)
        .and_then(|resource| resource.meta)
}

pub(super) async fn ensure_app_tool_visible(
    request_handle: &AppServerRequestHandle,
    descriptor: &ViewDescriptor,
    tool_name: &str,
) -> Result<(), String> {
    let status = server_status(request_handle, descriptor)
        .await
        .ok_or_else(|| format!("MCP server {} is unavailable", descriptor.server))?;
    let tool = status
        .tools
        .get(tool_name)
        .ok_or_else(|| format!("MCP tool {tool_name} is unavailable"))?;
    if app_tool_is_visible(tool.meta.as_ref()) {
        Ok(())
    } else {
        Err(format!("MCP tool {tool_name} is not visible to Apps"))
    }
}

pub(super) fn app_tool_is_visible(meta: Option<&Value>) -> bool {
    let Some(visibility) = meta
        .and_then(|meta| meta.get("ui"))
        .and_then(|ui| ui.get("visibility"))
        .and_then(Value::as_array)
    else {
        return true;
    };
    visibility
        .iter()
        .any(|target| target.as_str() == Some("app"))
}

async fn server_status(
    request_handle: &AppServerRequestHandle,
    descriptor: &ViewDescriptor,
) -> Option<McpServerStatus> {
    let mut cursor = None;
    for _ in 0..MAX_STATUS_PAGES {
        let request_id = RequestId::String(format!("mcp-app-inventory-{}", Uuid::new_v4()));
        let response: ListMcpServerStatusResponse = match request_handle
            .request_typed(ClientRequest::McpServerStatusList {
                request_id,
                params: ListMcpServerStatusParams {
                    cursor,
                    limit: Some(100),
                    detail: Some(McpServerStatusDetail::Full),
                    thread_id: Some(descriptor.thread_id.to_string()),
                },
            })
            .await
        {
            Ok(response) => response,
            Err(error) => {
                tracing::warn!(%error, server = descriptor.server, "could not read MCP App server inventory");
                return None;
            }
        };
        if let Some(status) = response
            .data
            .into_iter()
            .find(|status| status.name == descriptor.server)
        {
            return Some(status);
        }
        let next_cursor = response.next_cursor?;
        cursor = Some(next_cursor);
    }
    tracing::warn!(
        server = descriptor.server,
        max_pages = MAX_STATUS_PAGES,
        "stopped looking for MCP App server inventory after the page limit"
    );
    None
}

pub(super) fn extract_resource(
    contents: Vec<McpResourceContent>,
    expected_uri: &str,
    listing_meta: Option<&Value>,
) -> Result<AppResource, String> {
    let mut legacy_resource = None;
    for content in contents {
        let (uri, mime_type, bytes, meta) = match content {
            McpResourceContent::Text {
                uri,
                mime_type,
                text,
                meta,
            } => (uri, mime_type, text.into_bytes(), meta),
            McpResourceContent::Blob {
                uri,
                mime_type,
                blob,
                meta,
            } => {
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(blob)
                    .map_err(|error| format!("Invalid MCP App resource encoding: {error}"))?;
                (uri, mime_type, bytes, meta)
            }
        };
        if uri != expected_uri
            || !matches!(
                mime_type.as_deref(),
                Some(MCP_APP_MIME_TYPE | LEGACY_MCP_APP_MIME_TYPE)
            )
        {
            continue;
        }
        if bytes.len() > MAX_RESOURCE_BYTES {
            return Err("MCP App resource exceeds the 4 MiB limit".to_string());
        }
        let html = String::from_utf8(bytes)
            .map_err(|error| format!("MCP App resource is not UTF-8: {error}"))?;
        let content_ui_meta = meta.and_then(|meta| meta.get("ui").cloned());
        let listing_ui_meta = listing_meta.and_then(|meta| meta.get("ui").cloned());
        let resource_meta = content_ui_meta
            .or(listing_ui_meta)
            .map(serde_json::from_value::<ResourceMeta>)
            .transpose()
            .map_err(|error| format!("Invalid MCP App resource metadata: {error}"))?
            .unwrap_or_default();
        let resource = AppResource {
            html,
            csp: resource_meta.csp.unwrap_or_default().sanitized(),
            permissions: resource_meta.permissions.unwrap_or_default(),
            legacy_mime_type: mime_type.as_deref() == Some(LEGACY_MCP_APP_MIME_TYPE),
        };
        if mime_type.as_deref() == Some(MCP_APP_MIME_TYPE) {
            return Ok(resource);
        }
        legacy_resource = Some(resource);
    }
    if let Some(resource) = legacy_resource {
        tracing::warn!(
            uri = expected_uri,
            mime_type = LEGACY_MCP_APP_MIME_TYPE,
            "MCP App resource uses the legacy HTML MIME type"
        );
        return Ok(resource);
    }
    Err(format!(
        "MCP server did not return {expected_uri} as {MCP_APP_MIME_TYPE} or {LEGACY_MCP_APP_MIME_TYPE}"
    ))
}

pub(super) fn resource_csp(csp: &ResourceCsp, sandbox_origin: &str) -> String {
    let resources = csp_sources(csp.resource_domains.as_deref(), &["http", "https"]);
    let connections = csp_sources(
        csp.connect_domains.as_deref(),
        &["http", "https", "ws", "wss"],
    );
    let frames = csp_sources(csp.frame_domains.as_deref(), &["http", "https"]);
    let bases = csp_sources(csp.base_uri_domains.as_deref(), &["http", "https"]);
    let resource_suffix = sources_suffix(&resources);
    let base_suffix = sources_suffix(&bases);
    let font_sources = sources_or_none(&resources);
    let connection_sources = sources_or_none(&connections);
    let frame_sources = sources_or_none(&frames);
    format!(
        "default-src 'none'; script-src 'self' 'unsafe-inline'{resource_suffix}; style-src 'self' 'unsafe-inline'{resource_suffix}; img-src 'self' data:{resource_suffix}; font-src {font_sources}; media-src 'self' data:{resource_suffix}; connect-src {connection_sources}; frame-src {frame_sources}; base-uri 'self'{base_suffix}; object-src 'none'; form-action 'none'; frame-ancestors {sandbox_origin}",
    )
}

impl ResourceCsp {
    fn sanitized(self) -> Self {
        Self {
            connect_domains: sanitize_csp_sources(
                self.connect_domains,
                &["http", "https", "ws", "wss"],
            ),
            resource_domains: sanitize_csp_sources(self.resource_domains, &["http", "https"]),
            frame_domains: sanitize_csp_sources(self.frame_domains, &["http", "https"]),
            base_uri_domains: sanitize_csp_sources(self.base_uri_domains, &["http", "https"]),
        }
    }
}

fn sanitize_csp_sources(values: Option<Vec<String>>, schemes: &[&str]) -> Option<Vec<String>> {
    values.map(|values| csp_sources(Some(&values), schemes))
}

fn csp_sources(values: Option<&[String]>, schemes: &[&str]) -> Vec<String> {
    values
        .unwrap_or_default()
        .iter()
        .filter(|source| valid_csp_source(source, schemes))
        .cloned()
        .collect()
}

fn sources_or_none(values: &[String]) -> String {
    if values.is_empty() {
        "'none'".to_string()
    } else {
        values.join(" ")
    }
}

fn sources_suffix(values: &[String]) -> String {
    if values.is_empty() {
        String::new()
    } else {
        format!(" {}", values.join(" "))
    }
}

fn valid_csp_source(source: &str, schemes: &[&str]) -> bool {
    if source
        .chars()
        .any(|ch| ch.is_whitespace() || ch.is_control() || matches!(ch, ';' | '\'' | '"'))
    {
        return false;
    }
    let normalized = source.replacen("://*.", "://wildcard.", 1);
    Url::parse(&normalized).is_ok_and(|url| {
        schemes.contains(&url.scheme())
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && matches!(url.path(), "" | "/")
            && url.query().is_none()
            && url.fragment().is_none()
    })
}
