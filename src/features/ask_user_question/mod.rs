//! Structured user decisions. The dialog never calls a provider itself.

pub mod view;

use std::collections::BTreeSet;
use std::sync::Arc;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::message::ToolResult;
use crate::tools::{ToolDefinition, definition, parse};

#[derive(Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OptionData {
    #[schemars(length(max = 60))]
    pub label: String,
    pub description: String,
    pub preview: Option<String>,
}

#[derive(Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Question {
    pub question: String,
    #[schemars(length(max = 16))]
    pub header: String,
    #[schemars(length(min = 2, max = 4))]
    pub options: Vec<OptionData>,
    #[serde(default)]
    pub multi_select: bool,
}

#[derive(Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Params {
    #[schemars(length(min = 1, max = 4))]
    pub questions: Vec<Question>,
}

pub fn tool() -> ToolDefinition {
    definition::<Params>(
        "ask_user_question",
        "Ask 1-4 structured questions when requirements need a concrete decision. Each question has 2-4 described options, optionally with previews for single-select questions. Set multiSelect for multiple choices. The UI appends Type something.; never author Other, Type something., or Next as labels. Put a recommended choice first and label it (Recommended).",
    )
}

pub fn prepare(raw: Value) -> Result<Params, ToolResult> {
    let mut raw = raw;
    if let Some(questions) = raw.get_mut("questions") {
        decode_array(questions);
        if let Some(questions) = questions.as_array_mut() {
            for question in questions {
                decode_object(question);
                if let Some(options) = question.get_mut("options") {
                    decode_array(options);
                    if let Some(options) = options.as_array_mut() {
                        for option in options {
                            decode_object(option);
                        }
                    }
                }
            }
        }
    }
    let mut params = parse::<Params>(raw, "ask_user_question")
        .map_err(|error| failure("invalid_parameters", &error))?;
    if !(1..=4).contains(&params.questions.len()) {
        return Err(failure("question_count", "Use one to four questions"));
    }
    let mut questions = BTreeSet::new();
    for question in &mut params.questions {
        question.question = normalize(&question.question);
        question.header = normalize(&question.header);
        if !questions.insert(question.question.clone()) {
            return Err(failure(
                "duplicate_question",
                "Question text must be unique",
            ));
        }
        if question.header.chars().count() > 16 || !(2..=4).contains(&question.options.len()) {
            return Err(failure(
                "question_limits",
                "Use headers up to 16 characters and two to four options",
            ));
        }
        let mut labels = BTreeSet::new();
        for option in &mut question.options {
            option.label = normalize(&option.label);
            option.description = normalize(&option.description);
            if let Some(preview) = &mut option.preview {
                *preview = normalize(preview);
            }
            if ["Other", "Type something.", "Next"].contains(&option.label.as_str()) {
                return Err(failure("reserved_label", "Option label is reserved"));
            }
            if option.label.chars().count() > 60 {
                return Err(failure(
                    "label_limit",
                    "Option labels must not exceed 60 characters",
                ));
            }
            if !labels.insert(option.label.clone()) {
                return Err(failure(
                    "duplicate_option_label",
                    "Option labels must be unique",
                ));
            }
        }
    }
    Ok(params)
}

fn normalize(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "")
}
fn decode_array(value: &mut Value) {
    if let Some(text) = value.as_str()
        && let Ok(parsed @ Value::Array(_)) = serde_json::from_str::<Value>(text)
    {
        *value = parsed;
    }
}
fn decode_object(value: &mut Value) {
    if let Some(text) = value.as_str()
        && let Ok(parsed @ Value::Object(_)) = serde_json::from_str::<Value>(text)
    {
        *value = parsed;
    }
}

pub fn failure(code: &str, message: &str) -> ToolResult {
    ToolResult {
        details: json!({"answers":[],"cancelled":true,"error":code}),
        ..ToolResult::error(format!(
            "Error: {message}. The user did not see these questions; do not treat this as a decline."
        ))
    }
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AnswerKind {
    Option,
    Custom,
    Multi,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Answer {
    pub question_index: usize,
    pub question: String,
    pub kind: AnswerKind,
    pub answer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Answers {
    pub answers: Vec<Answer>,
    pub cancelled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub global_note: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub unanswered: Vec<usize>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub unanswered_questions: Vec<String>,
}

impl Answers {
    pub fn cancelled() -> Self {
        Self {
            answers: Vec::new(),
            cancelled: true,
            global_note: None,
            unanswered: Vec::new(),
            unanswered_questions: Vec::new(),
        }
    }

    pub fn tool_result(&self) -> ToolResult {
        let text = if self.cancelled || (self.answers.is_empty() && self.global_note.is_none()) {
            "User declined to answer questions".into()
        } else {
            let mut parts = self
                .answers
                .iter()
                .map(|answer| {
                    let value = match &answer.selected {
                        Some(selected) => selected
                            .iter()
                            .cloned()
                            .chain(answer.answer.clone())
                            .collect::<Vec<_>>()
                            .join(", "),
                        None => answer.answer.clone().unwrap_or_default(),
                    };
                    let value = if value.trim().is_empty() {
                        "(no input)".into()
                    } else {
                        value
                    };
                    let mut text = format!("\"{}\"=\"{value}\"", answer.question);
                    if let Some(preview) = &answer.preview {
                        text.push_str(&format!(". selected preview: {preview}"));
                    }
                    if let Some(notes) = &answer.notes {
                        text.push_str(&format!(". user notes: {notes}"));
                    }
                    text
                })
                .collect::<Vec<_>>();
            if !self.unanswered_questions.is_empty() {
                parts.push(format!(
                    "unanswered questions: {}",
                    self.unanswered_questions
                        .iter()
                        .map(|question| format!("\"{question}\""))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            if let Some(note) = &self.global_note {
                parts.push(format!("global note: {note}"));
            }
            format!(
                "User has answered your questions: {}. You can now continue with the user's answers in mind.",
                parts.join(". ")
            )
        };
        ToolResult {
            details: serde_json::to_value(self).expect("serializable answers"),
            ..ToolResult::text(text)
        }
    }
}

#[derive(Clone, Copy)]
pub enum Intent {
    Option(usize),
    Custom,
    Multi,
}

#[derive(Default)]
pub struct Draft {
    pub selected: BTreeSet<usize>,
    pub custom: String,
    pub notes: String,
    pub intent: Option<Intent>,
}

pub struct Questionnaire {
    pub params: Arc<Params>,
    pub drafts: Vec<Draft>,
    pub global_note: String,
}

impl Questionnaire {
    pub fn new(params: Arc<Params>) -> Self {
        let drafts = params.questions.iter().map(|_| Draft::default()).collect();
        Self {
            params,
            drafts,
            global_note: String::new(),
        }
    }

    pub fn choose(&mut self, question: usize, option: usize) {
        self.drafts[question].intent = Some(Intent::Option(option));
    }

    pub fn toggle(&mut self, question: usize, option: usize) {
        let draft = &mut self.drafts[question];
        let committed_custom =
            matches!(draft.intent, Some(Intent::Multi)) && !draft.custom.trim().is_empty();
        if !draft.selected.remove(&option) {
            draft.selected.insert(option);
        }
        draft.intent = (committed_custom || !draft.selected.is_empty()).then_some(Intent::Multi);
    }

    pub fn commit_custom(&mut self, question: usize) -> bool {
        let draft = &mut self.drafts[question];
        if self.params.questions[question].multi_select {
            draft.intent = Some(Intent::Multi);
            true
        } else if !draft.custom.trim().is_empty() {
            draft.intent = Some(Intent::Custom);
            true
        } else {
            false
        }
    }

    pub fn finish(&self, cancelled: bool) -> Answers {
        let mut answers = Vec::new();
        let mut unanswered = Vec::new();
        for (index, (question, draft)) in self.params.questions.iter().zip(&self.drafts).enumerate()
        {
            let Some(intent) = draft.intent else {
                unanswered.push(index);
                continue;
            };
            let (kind, answer, selected, preview) = match intent {
                Intent::Option(option) => (
                    AnswerKind::Option,
                    Some(question.options[option].label.clone()),
                    None,
                    question.options[option].preview.clone(),
                ),
                Intent::Custom => (AnswerKind::Custom, Some(draft.custom.clone()), None, None),
                Intent::Multi => (
                    AnswerKind::Multi,
                    (!draft.custom.trim().is_empty()).then(|| draft.custom.clone()),
                    Some(
                        draft
                            .selected
                            .iter()
                            .map(|index| question.options[*index].label.clone())
                            .collect(),
                    ),
                    None,
                ),
            };
            answers.push(Answer {
                question_index: index,
                question: question.question.clone(),
                kind,
                answer,
                selected,
                notes: (!draft.notes.trim().is_empty()).then(|| draft.notes.trim().to_owned()),
                preview,
            });
        }
        let unanswered_questions = unanswered
            .iter()
            .map(|index| self.params.questions[*index].question.clone())
            .collect();
        Answers {
            answers,
            cancelled,
            global_note: (!self.global_note.trim().is_empty())
                .then(|| self.global_note.trim().to_owned()),
            unanswered,
            unanswered_questions,
        }
    }
}
