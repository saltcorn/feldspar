//! What a server offers and what a call answers (TODO-agent-skills AS4.2):
//! [`McpTool`] from `tools/list`, and [`McpToolResult`], a `tools/call`
//! result turned into what a model can read — text, and images.

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

/// One tool a server offers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpTool {
    /// Its name, as `tools/call` takes it.
    pub name: String,
    /// A human title, when the server gives one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// What it does, for the model.
    pub description: String,
    /// The JSON Schema of its arguments.
    pub input_schema: Json,
    /// Whether the server says it changes nothing (`readOnlyHint`).
    pub read_only: bool,
}

impl McpTool {
    /// The tool an entry of a `tools/list` result describes, or `None` for one
    /// with no name.
    pub(crate) fn from_listing(entry: &Json) -> Option<McpTool> {
        let name = entry.get("name")?.as_str()?.trim();
        if name.is_empty() {
            return None;
        }
        let text = |key: &str| entry.get(key).and_then(Json::as_str).map(str::to_owned);
        let mut schema = entry
            .get("inputSchema")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));
        // A model's tool parameters are an object schema, always.
        if schema.get("type").is_none() && schema.is_object() {
            schema["type"] = Json::from("object");
        }
        Some(McpTool {
            name: name.to_owned(),
            title: text("title").or_else(|| {
                entry
                    .pointer("/annotations/title")
                    .and_then(Json::as_str)
                    .map(str::to_owned)
            }),
            description: text("description").unwrap_or_default(),
            input_schema: schema,
            read_only: entry
                .pointer("/annotations/readOnlyHint")
                .and_then(Json::as_bool)
                .unwrap_or(false),
        })
    }
}

/// An image a tool returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpImage {
    /// Its media type, such as `image/png`.
    pub mime_type: String,
    /// The decoded bytes.
    pub data: Vec<u8>,
}

/// A tool's answer, as a model reads it.
#[derive(Debug, Clone, PartialEq)]
pub struct McpToolResult {
    /// Every text part, and a line for each part that is not text or an image,
    /// joined by blank lines.
    pub text: String,
    /// Every image part.
    pub images: Vec<McpImage>,
    /// The structured result, when the server sent one.
    pub structured: Option<Json>,
    /// Whether the server marked it an error (`isError`): a failure the model
    /// reads, not one of the call.
    pub is_error: bool,
}

impl McpToolResult {
    /// Convert a `tools/call` result.
    pub(crate) fn from_result(result: &Json) -> McpToolResult {
        let mut texts: Vec<String> = Vec::new();
        let mut images = Vec::new();
        for part in result
            .get("content")
            .and_then(Json::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default()
        {
            let field = |key: &str| part.get(key).and_then(Json::as_str).unwrap_or_default();
            match field("type") {
                "text" => texts.push(field("text").to_owned()),
                "image" => {
                    match base64::engine::general_purpose::STANDARD.decode(field("data").trim()) {
                        Ok(data) => images.push(McpImage {
                            mime_type: field("mimeType").to_owned(),
                            data,
                        }),
                        Err(_) => texts.push(format!(
                            "[an image of type {} that could not be decoded]",
                            field("mimeType")
                        )),
                    }
                }
                "audio" => texts.push(format!(
                    "[audio of type {}, which cannot be shown here]",
                    field("mimeType")
                )),
                "resource_link" => {
                    let name = field("name");
                    let uri = field("uri");
                    let description = field("description");
                    texts.push(if description.is_empty() {
                        format!("[resource {name}: {uri}]")
                    } else {
                        format!("[resource {name}: {uri} — {description}]")
                    });
                }
                "resource" => {
                    let resource = part.get("resource").unwrap_or(&Json::Null);
                    let get =
                        |key: &str| resource.get(key).and_then(Json::as_str).unwrap_or_default();
                    texts.push(match resource.get("text").and_then(Json::as_str) {
                        Some(text) => text.to_owned(),
                        None => format!(
                            "[binary resource {} of type {}]",
                            get("uri"),
                            get("mimeType")
                        ),
                    });
                }
                other => texts.push(format!("[a `{other}` part, which cannot be shown here]")),
            }
        }
        let structured = result.get("structuredContent").cloned();
        // A server that answers only with structured content still answers.
        if texts.is_empty()
            && images.is_empty()
            && let Some(structured) = &structured
        {
            texts.push(structured.to_string());
        }
        McpToolResult {
            text: texts.join("\n\n"),
            images,
            structured,
            is_error: result
                .get("isError")
                .and_then(Json::as_bool)
                .unwrap_or(false),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_listing_entry_becomes_a_tool_with_an_object_schema() {
        let tool = McpTool::from_listing(&json!({
            "name": "search_docs",
            "description": "Search the docs",
            "inputSchema": { "properties": { "q": { "type": "string" } } },
            "annotations": { "readOnlyHint": true, "title": "Search" },
        }))
        .unwrap();
        assert_eq!(tool.name, "search_docs");
        assert_eq!(tool.title.as_deref(), Some("Search"));
        assert_eq!(tool.input_schema["type"], "object");
        assert!(tool.read_only);
        assert!(McpTool::from_listing(&json!({ "description": "nameless" })).is_none());
    }

    #[test]
    fn every_kind_of_part_becomes_text_or_an_image() {
        let png = base64::engine::general_purpose::STANDARD.encode(b"\x89PNG");
        let result = McpToolResult::from_result(&json!({
            "content": [
                { "type": "text", "text": "Found 2 pages." },
                { "type": "image", "data": png, "mimeType": "image/png" },
                { "type": "resource_link", "uri": "file:///a.md", "name": "a.md" },
                { "type": "resource", "resource": { "uri": "x", "text": "inline" } },
                { "type": "audio", "data": "", "mimeType": "audio/wav" },
            ],
            "isError": false,
        }));
        assert_eq!(
            result.text,
            "Found 2 pages.\n\n[resource a.md: file:///a.md]\n\ninline\n\n\
             [audio of type audio/wav, which cannot be shown here]"
        );
        assert_eq!(result.images.len(), 1);
        assert_eq!(result.images[0].data, b"\x89PNG");
        assert!(!result.is_error);

        let structured = McpToolResult::from_result(&json!({
            "content": [], "structuredContent": { "n": 1 }, "isError": true,
        }));
        assert_eq!(structured.text, r#"{"n":1}"#);
        assert!(structured.is_error);
    }
}
