use super::McpAppViewLink;
use super::context::ContextStore;
use super::context::MAX_CONTEXT_CHARS;
use super::context::UiMessageParams;
use super::context::UpdateModelContextParams;
use super::resource::ResourceCsp;
use super::resource::ResourcePermissions;
use super::resource::app_tool_is_visible;
use super::resource::extract_resource;
use super::resource::resource_csp;
use crate::history_cell::HistoryCell;
use codex_app_server_protocol::AdditionalContextEntry;
use codex_app_server_protocol::AdditionalContextKind;
use codex_app_server_protocol::McpResourceContent;
use codex_protocol::ThreadId;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::collections::HashMap;

#[test]
fn context_updates_replace_and_clear_one_view() {
    let thread_id = ThreadId::new();
    let mut store = ContextStore::default();
    let first = serde_json::from_value::<UpdateModelContextParams>(json!({
        "content": [{"type": "text", "text": "RevenueChart, May–July"}],
        "structuredContent": {"screen": "Dashboard"}
    }))
    .expect("valid context")
    .into_context("screens.select")
    .expect("bounded context");
    store.update(thread_id, "view-1", first);

    let snapshot = store.snapshot(thread_id);
    assert_eq!(
        snapshot,
        HashMap::from([(
            "mcp_app_view_1".to_string(),
            AdditionalContextEntry {
                value: "MCP App context from screens.select:\nRevenueChart, May–July\n\nStructured content:\n{\n  \"screen\": \"Dashboard\"\n}"
                    .to_string(),
                kind: AdditionalContextKind::Untrusted,
            },
        )])
    );

    store.update(thread_id, "view-1", /*value*/ None);
    assert_eq!(store.snapshot(thread_id), HashMap::new());

    store.update(
        thread_id,
        "view-2-instance-1",
        Some("first instance".to_string()),
    );
    store.update(
        thread_id,
        "view-2-instance-2",
        Some("second instance".to_string()),
    );
    store.clear_view(thread_id, "view-2");
    store.update(
        thread_id,
        "view-2-instance-3",
        Some("latest instance".to_string()),
    );
    assert_eq!(
        store.snapshot(thread_id),
        HashMap::from([(
            "mcp_app_view_2_instance_3".to_string(),
            AdditionalContextEntry {
                value: "latest instance".to_string(),
                kind: AdditionalContextKind::Untrusted,
            },
        )])
    );
    store.clear_source("view-2-instance-1");
    assert_eq!(
        store.snapshot(thread_id),
        HashMap::from([(
            "mcp_app_view_2_instance_3".to_string(),
            AdditionalContextEntry {
                value: "latest instance".to_string(),
                kind: AdditionalContextKind::Untrusted,
            },
        )])
    );
}

#[test]
fn context_and_messages_have_hard_limits_and_text_only_modalities() {
    let oversized = serde_json::from_value::<UpdateModelContextParams>(json!({
        "content": [{"type": "text", "text": "x".repeat(MAX_CONTEXT_CHARS)}]
    }))
    .expect("valid context shape");
    assert!(
        oversized.into_context("screens.select").is_err(),
        "the source label also counts toward the hard context limit"
    );

    assert!(
        serde_json::from_value::<UiMessageParams>(json!({
            "role": "user",
            "content": [{"type": "image", "data": "...", "mimeType": "image/png"}]
        }))
        .is_err()
    );
    let message = serde_json::from_value::<UiMessageParams>(json!({
        "content": [
            {"type": "text", "text": "Fix"},
            {"type": "text", "text": "the selection"}
        ]
    }))
    .expect("text message")
    .into_text(/*allow_missing_role*/ true)
    .expect("bounded message");
    assert_eq!(message, "Fix\nthe selection");

    let missing_role = serde_json::from_value::<UiMessageParams>(json!({
        "content": [{"type": "text", "text": "Fix"}]
    }))
    .expect("message shape");
    assert_eq!(
        missing_role.into_text(/*allow_missing_role*/ false),
        Err("Message role must be user".to_string())
    );
}

#[test]
fn resource_extraction_prefers_profile_mime_and_content_metadata() {
    let resource = extract_resource(
        vec![
            McpResourceContent::Text {
                uri: "ui://demo/widget.html".to_string(),
                mime_type: Some("text/html".to_string()),
                text: "legacy".to_string(),
                meta: None,
            },
            McpResourceContent::Text {
                uri: "ui://demo/widget.html".to_string(),
                mime_type: Some("text/html;profile=mcp-app".to_string()),
                text: "profile".to_string(),
                meta: Some(json!({
                    "ui": {
                        "csp": {"connectDomains": [
                            "wss://content.example",
                            "https://bad.example; script-src *"
                        ]},
                        "permissions": {"microphone": {}},
                    }
                })),
            },
        ],
        "ui://demo/widget.html",
        Some(&json!({
            "ui": {
                "csp": {"connectDomains": ["wss://listing.example"]},
                "permissions": {"camera": {}},
            }
        })),
    )
    .expect("MCP App resource");

    assert_eq!(
        resource,
        super::resource::AppResource {
            html: "profile".to_string(),
            csp: ResourceCsp {
                connect_domains: Some(vec!["wss://content.example".to_string()]),
                ..Default::default()
            },
            permissions: ResourcePermissions {
                microphone: Some(json!({})),
                ..Default::default()
            },
            legacy_mime_type: false,
        }
    );
}

#[test]
fn legacy_html_resource_uses_listing_metadata_fallback() {
    let resource = extract_resource(
        vec![McpResourceContent::Text {
            uri: "ui://demo/widget.html".to_string(),
            mime_type: Some("text/html".to_string()),
            text: "legacy".to_string(),
            meta: None,
        }],
        "ui://demo/widget.html",
        Some(&json!({
            "ui": {
                "csp": {"resourceDomains": ["https://cdn.example"]},
                "permissions": {"clipboardWrite": {}},
            }
        })),
    )
    .expect("legacy MCP App resource");

    assert_eq!(
        resource,
        super::resource::AppResource {
            html: "legacy".to_string(),
            csp: ResourceCsp {
                resource_domains: Some(vec!["https://cdn.example".to_string()]),
                ..Default::default()
            },
            permissions: ResourcePermissions {
                clipboard_write: Some(json!({})),
                ..Default::default()
            },
            legacy_mime_type: true,
        }
    );
}

#[test]
fn resource_csp_keeps_declared_origins_and_drops_injected_sources() {
    let csp = ResourceCsp {
        connect_domains: Some(vec![
            "wss://live.example.com".to_string(),
            "https://bad.example; script-src *".to_string(),
        ]),
        resource_domains: Some(vec!["https://cdn.example.com".to_string()]),
        frame_domains: None,
        base_uri_domains: None,
    };

    assert_eq!(
        resource_csp(&csp, "http://127.0.0.1:43101"),
        "default-src 'none'; script-src 'self' 'unsafe-inline' https://cdn.example.com; style-src 'self' 'unsafe-inline' https://cdn.example.com; img-src 'self' data: https://cdn.example.com; font-src https://cdn.example.com; media-src 'self' data: https://cdn.example.com; connect-src wss://live.example.com; frame-src 'none'; base-uri 'self'; object-src 'none'; form-action 'none'; frame-ancestors http://127.0.0.1:43101"
    );
}

#[test]
fn resource_metadata_omits_absent_optional_capabilities() {
    assert_eq!(
        serde_json::to_value(ResourceCsp {
            connect_domains: Some(vec!["https://api.example.com".to_string()]),
            ..Default::default()
        })
        .expect("serializable CSP"),
        json!({"connectDomains": ["https://api.example.com"]})
    );
    assert_eq!(
        serde_json::to_value(ResourcePermissions::default()).expect("serializable permissions"),
        json!({})
    );
}

#[test]
fn app_tool_visibility_defaults_to_both_and_honors_explicit_targets() {
    assert!(app_tool_is_visible(None));
    assert!(app_tool_is_visible(Some(&json!({
        "ui": {"visibility": ["model", "app"]}
    }))));
    assert!(!app_tool_is_visible(Some(&json!({
        "ui": {"visibility": ["model"]}
    }))));
}

#[test]
fn browser_link_cell_snapshot() {
    let cell = McpAppViewLink {
        title: "Connected iPhone".to_string(),
        url: "http://127.0.0.1:43100/view/id/token".to_string(),
    }
    .history_cell();
    let rendered = cell
        .display_lines(/*width*/ 80)
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");

    insta::assert_snapshot!(rendered, @"  └ MCP App: Connected iPhone — Open in browser");
    assert_eq!(
        cell.display_hyperlink_lines(/*width*/ 80)[0].hyperlinks[0].destination,
        "http://127.0.0.1:43100/view/id/token"
    );
}
