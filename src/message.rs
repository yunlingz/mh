//! Canonical messages and usage. Presentation never owns conversation state.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    pub input: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub output: u64,
}

impl Usage {
    pub fn add(&mut self, other: &Self) {
        self.input = self.input.saturating_add(other.input);
        self.cache_read = self.cache_read.saturating_add(other.cache_read);
        self.cache_write = self.cache_write.saturating_add(other.cache_write);
        self.output = self.output.saturating_add(other.output);
    }

    pub fn input_total(&self) -> u64 {
        self.input
            .saturating_add(self.cache_read)
            .saturating_add(self.cache_write)
    }

    pub fn context_total(&self) -> u64 {
        self.input_total().saturating_add(self.output)
    }

    pub fn cache_percent(&self) -> Option<f64> {
        let total = self.input_total();
        (total > 0).then(|| self.cache_read as f64 / total as f64 * 100.0)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Content {
    Text { text: String },
    Image { data: String, mime_type: String },
}

impl Content {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResult {
    pub content: Vec<Content>,
    #[serde(default)]
    pub details: Value,
    #[serde(default)]
    pub is_error: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured_content: Option<Value>,
}

impl ToolResult {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content: vec![Content::text(text)],
            details: Value::Null,
            is_error: false,
            structured_content: None,
        }
    }

    pub fn error(text: impl Into<String>) -> Self {
        Self {
            is_error: true,
            ..Self::text(text)
        }
    }

    pub fn plain_text(&self) -> String {
        content_text(&self.content)
    }
}

pub fn content_text(content: &[Content]) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            Content::Text { text } => Some(text.as_str()),
            Content::Image { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OutputText {
    OutputText {
        text: String,
        #[serde(default)]
        annotations: Vec<Value>,
    },
    Refusal {
        refusal: String,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SummaryPart {
    #[serde(rename = "type")]
    pub kind: String,
    pub text: String,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OutputItem {
    Message {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        phase: Option<String>,
        content: Vec<OutputText>,
    },
    FunctionCall {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        call_id: String,
        name: String,
        arguments: String,
    },
    Reasoning {
        id: String,
        #[serde(default)]
        summary: Vec<SummaryPart>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        content: Vec<SummaryPart>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        encrypted_content: Option<String>,
    },
}

// Opaque continuation data is persisted/replayed, never diagnostic text. This
// also protects Assistant/Message/RunOutcome's derived Debug implementations.
impl std::fmt::Debug for OutputItem {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Message { id, phase, content } => formatter
                .debug_struct("Message")
                .field("id", id)
                .field("phase", phase)
                .field("content", content)
                .finish(),
            Self::FunctionCall {
                id,
                call_id,
                name,
                arguments,
            } => formatter
                .debug_struct("FunctionCall")
                .field("id", id)
                .field("call_id", call_id)
                .field("name", name)
                .field("arguments", arguments)
                .finish(),
            Self::Reasoning {
                id,
                summary,
                content,
                encrypted_content,
            } => formatter
                .debug_struct("Reasoning")
                .field("id", id)
                .field("summary", summary)
                .field("content", content)
                .field(
                    "encrypted_content",
                    &encrypted_content.as_ref().map(|_| "[opaque]"),
                )
                .finish(),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    Stop,
    ToolUse,
    Length,
    Error,
    Aborted,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Assistant {
    pub provider: String,
    pub model: String,
    pub items: Vec<OutputItem>,
    pub stop: StopReason,
    #[serde(default)]
    pub usage: Option<Usage>,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Clone, Debug)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

impl Assistant {
    pub fn text(&self) -> String {
        self.items
            .iter()
            .filter_map(|item| match item {
                OutputItem::Message { content, .. } => Some(
                    content
                        .iter()
                        .map(|part| match part {
                            OutputText::OutputText { text, .. } => text.as_str(),
                            OutputText::Refusal { refusal } => refusal.as_str(),
                        })
                        .collect::<Vec<_>>()
                        .join(""),
                ),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn thinking(&self) -> String {
        self.items
            .iter()
            .filter_map(|item| match item {
                OutputItem::Reasoning {
                    summary, content, ..
                } => Some(
                    (if summary.is_empty() { content } else { summary })
                        .iter()
                        .map(|part| part.text.as_str())
                        .collect::<Vec<_>>()
                        .join("\n\n"),
                ),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn tool_calls(&self) -> Vec<ToolCall> {
        self.items
            .iter()
            .filter_map(|item| match item {
                OutputItem::FunctionCall {
                    call_id,
                    name,
                    arguments,
                    ..
                } => Some(ToolCall {
                    id: call_id.clone(),
                    name: name.clone(),
                    arguments: arguments.clone(),
                }),
                _ => None,
            })
            .collect()
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum Message {
    User {
        content: Vec<Content>,
    },
    Assistant {
        response: Assistant,
    },
    ToolResult {
        call_id: String,
        name: String,
        result: ToolResult,
    },
}

impl Message {
    pub fn user(text: impl Into<String>) -> Self {
        Self::User {
            content: vec![Content::text(text)],
        }
    }

    pub fn text(&self) -> String {
        match self {
            Self::User { content } => content_text(content),
            Self::Assistant { response } => response.text(),
            Self::ToolResult { result, .. } => result.plain_text(),
        }
    }

    pub fn estimate_tokens(&self) -> u64 {
        let chars = match self {
            Self::User { content } => content_chars(content),
            Self::ToolResult { result, .. } => content_chars(&result.content),
            Self::Assistant { response } => response
                .items
                .iter()
                .map(|item| match item {
                    OutputItem::Message { content, .. } => content
                        .iter()
                        .map(|part| match part {
                            OutputText::OutputText { text, .. } => text.encode_utf16().count(),
                            OutputText::Refusal { refusal } => refusal.encode_utf16().count(),
                        })
                        .sum(),
                    OutputItem::FunctionCall {
                        name, arguments, ..
                    } => name.len() + arguments.len(),
                    OutputItem::Reasoning {
                        summary, content, ..
                    } => (if summary.is_empty() { content } else { summary })
                        .iter()
                        .map(|part| part.text.encode_utf16().count())
                        .sum(),
                })
                .sum(),
        };
        (chars as u64).div_ceil(4)
    }

    pub fn to_wire(
        &self,
        message_index: usize,
        provider: &str,
        model: &str,
        supports_images: bool,
    ) -> Vec<Value> {
        match self {
            Self::User { content } => {
                vec![json!({"role":"user", "content": wire_content(content, supports_images)})]
            }
            Self::ToolResult {
                call_id, result, ..
            } => vec![json!({
                "type":"function_call_output",
                "call_id":normalize_id(call_id, "call"),
                "output":wire_tool_output(&result.content, supports_images)
            })],
            Self::Assistant { response } => {
                let same = response.provider == provider && response.model == model;
                response
                    .items
                    .iter()
                    .enumerate()
                    .filter_map(|(item_index, item)| {
                        if matches!(item, OutputItem::Reasoning { .. }) && !same {
                            return None;
                        }
                        let mut wire =
                            serde_json::to_value(item).expect("serializable output item");
                        match item {
                            OutputItem::Message { id, .. } => {
                                wire["role"] = json!("assistant");
                                wire["status"] = json!("completed");
                                let fallback = format!("msg_mh_{message_index}_{item_index}");
                                wire["id"] = json!(if same {
                                    id.as_deref()
                                        .filter(|id| valid_id(id, "msg_"))
                                        .unwrap_or(&fallback)
                                } else {
                                    &fallback
                                });
                                if !same {
                                    wire.as_object_mut()
                                        .expect("tagged output item")
                                        .remove("phase");
                                }
                            }
                            OutputItem::FunctionCall { call_id, id, .. } => {
                                wire["call_id"] = json!(normalize_id(call_id, "call"));
                                if !same || id.as_deref().is_none_or(|id| !valid_id(id, "fc_")) {
                                    wire.as_object_mut()
                                        .expect("tagged output item")
                                        .remove("id");
                                }
                            }
                            OutputItem::Reasoning { .. } => {}
                        }
                        Some(wire)
                    })
                    .collect()
            }
        }
    }
}

fn content_chars(content: &[Content]) -> usize {
    content
        .iter()
        .map(|block| match block {
            Content::Text { text } => text.encode_utf16().count(),
            Content::Image { .. } => 4800,
        })
        .sum()
}

fn wire_content(content: &[Content], supports_images: bool) -> Vec<Value> {
    content.iter().map(|block| match block {
        Content::Text { text } => json!({"type":"input_text", "text":text}),
        Content::Image { data, mime_type } if supports_images => json!({
            "type":"input_image", "detail":"auto", "image_url":format!("data:{mime_type};base64,{data}")
        }),
        Content::Image { .. } => json!({"type":"input_text", "text":"[Image omitted: this model does not support image input.]"}),
    }).collect()
}

fn wire_tool_output(content: &[Content], supports_images: bool) -> Value {
    let text = content
        .iter()
        .filter_map(|block| match block {
            Content::Text { text } => Some(text.as_str()),
            Content::Image { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    let images = content
        .iter()
        .filter_map(|block| match block {
            Content::Image { data, mime_type } => Some((data, mime_type)),
            Content::Text { .. } => None,
        })
        .collect::<Vec<_>>();
    if images.is_empty() || !supports_images {
        return Value::String(if !text.trim().is_empty() {
            text
        } else if images.is_empty() {
            "(no tool output)".into()
        } else {
            "(see attached image)".into()
        });
    }
    let mut output = Vec::new();
    if !text.trim().is_empty() {
        output.push(json!({"type":"input_text", "text":text}));
    }
    output.extend(images.into_iter().map(|(data, mime_type)| {
        json!({
            "type":"input_image",
            "detail":"auto",
            "image_url":format!("data:{mime_type};base64,{data}")
        })
    }));
    Value::Array(output)
}

fn valid_id(value: &str, prefix: &str) -> bool {
    value.starts_with(prefix)
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn normalize_id(value: &str, fallback_prefix: &str) -> String {
    if !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return value.to_owned();
    }
    let digest = format!("{:x}", Sha256::digest(value.as_bytes()));
    format!("{fallback_prefix}_{}", &digest[..48])
}
