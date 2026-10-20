//! Per-account request rewrite used only after an upstream explicitly rejects
//! the request by its content policy.

use crate::error::AppError;
use serde_json::{json, Map, Value};

pub(crate) const CONFIG_KEY: &str = "content_rejection_compat";
pub(crate) const ELIDED_TOOL_OUTPUT: &str =
    "[ai-switch: older tool output omitted after upstream content filter; re-run the command if needed]";
const KEEP_LATEST_TOOL_OUTPUTS: usize = 6;
const MIN_ELIDED_BYTES: usize = 128;

pub(crate) fn validate(mode: Option<&Value>) -> Result<(), AppError> {
    if matches!(mode, None | Some(Value::Null))
        || mode
            .and_then(Value::as_str)
            .is_some_and(|mode| matches!(mode, "off" | "trim_old_tool_outputs"))
    {
        return Ok(());
    }
    Err(AppError::Validation {
        code: "validation.content_rejection_compat",
        message: "内容过滤兼容仅支持 off 或 trim_old_tool_outputs。".to_string(),
        details: None,
        recoverable: true,
    })
}

pub(crate) fn trim_enabled(config: &Value) -> bool {
    config.get(CONFIG_KEY).and_then(Value::as_str) == Some("trim_old_tool_outputs")
}

#[derive(Debug, Clone, Copy)]
enum ToolOutputSlot {
    Responses(usize),
    Chat(usize),
}

pub(crate) fn elide_old_tool_outputs(body: &[u8]) -> Option<Vec<u8>> {
    let mut value = serde_json::from_slice::<Value>(body).ok()?;
    let object = value.as_object_mut()?;
    let slots = collect_tool_output_slots(object);
    let elide_until = slots.len().saturating_sub(KEEP_LATEST_TOOL_OUTPUTS);
    if elide_until == 0 {
        return None;
    }

    let mut changed = false;
    for slot in slots.into_iter().take(elide_until) {
        changed |= match slot {
            ToolOutputSlot::Responses(index) => object
                .get_mut("input")
                .and_then(Value::as_array_mut)
                .and_then(|items| items.get_mut(index))
                .is_some_and(|item| elide_responses_output(item)),
            ToolOutputSlot::Chat(index) => object
                .get_mut("messages")
                .and_then(Value::as_array_mut)
                .and_then(|messages| messages.get_mut(index))
                .is_some_and(|message| elide_chat_output(message)),
        };
    }
    changed.then(|| serde_json::to_vec(&value).ok()).flatten()
}

fn collect_tool_output_slots(object: &Map<String, Value>) -> Vec<ToolOutputSlot> {
    let mut slots = Vec::new();
    if let Some(items) = object.get("input").and_then(Value::as_array) {
        for (index, item) in items.iter().enumerate() {
            if item
                .get("type")
                .and_then(Value::as_str)
                .is_some_and(|kind| {
                    matches!(
                        kind,
                        "function_call_output"
                            | "custom_tool_call_output"
                            | "local_shell_call_output"
                    )
                })
            {
                slots.push(ToolOutputSlot::Responses(index));
            }
        }
    }
    if let Some(messages) = object.get("messages").and_then(Value::as_array) {
        for (index, message) in messages.iter().enumerate() {
            if message.get("role").and_then(Value::as_str) == Some("tool") {
                slots.push(ToolOutputSlot::Chat(index));
            }
        }
    }
    slots
}

fn elide_responses_output(item: &mut Value) -> bool {
    let Some(object) = item.as_object_mut() else {
        return false;
    };
    let Some(output) = object.get_mut("output") else {
        return false;
    };
    elide_value(output, true)
}

fn elide_chat_output(message: &mut Value) -> bool {
    let Some(content) = message
        .as_object_mut()
        .and_then(|object| object.get_mut("content"))
    else {
        return false;
    };
    elide_value(content, false)
}

fn elide_value(output: &mut Value, responses_shape: bool) -> bool {
    match output {
        Value::String(text) if text.len() > MIN_ELIDED_BYTES => {
            *text = ELIDED_TOOL_OUTPUT.to_string();
            true
        }
        Value::Array(parts) => {
            let mut total = 0usize;
            let mut all_text = !parts.is_empty();
            for part in parts.iter() {
                let Some(object) = part.as_object() else {
                    all_text = false;
                    break;
                };
                let kind = object.get("type").and_then(Value::as_str);
                let text = object.get("text").and_then(Value::as_str);
                let accepted_kind =
                    kind.is_some_and(|kind| matches!(kind, "text" | "input_text" | "output_text"));
                let Some(text) = text.filter(|_| accepted_kind) else {
                    all_text = false;
                    break;
                };
                total = total.saturating_add(text.len());
            }
            if !all_text || total <= MIN_ELIDED_BYTES {
                return false;
            }
            *parts = vec![if responses_shape {
                json!({"type":"input_text","text":ELIDED_TOOL_OUTPUT})
            } else {
                json!({"type":"text","text":ELIDED_TOOL_OUTPUT})
            }];
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    fn long(label: &str) -> String {
        format!("{label}-{}", "x".repeat(160))
    }

    #[test]
    fn keeps_the_latest_six_responses_outputs_and_elides_the_oldest() {
        let input = (0..7)
            .map(|index| {
                json!({
                    "type": "function_call_output",
                    "call_id": format!("call-{index}"),
                    "output": long(&index.to_string())
                })
            })
            .collect::<Vec<_>>();
        let body = serde_json::to_vec(&json!({"model":"gpt-5","input":input})).unwrap();

        let trimmed = elide_old_tool_outputs(&body).expect("one old output trimmed");
        let value: Value = serde_json::from_slice(&trimmed).unwrap();
        assert_eq!(value["input"][0]["output"], ELIDED_TOOL_OUTPUT);
        assert_eq!(value["input"][0]["call_id"], "call-0");
        for index in 1..7 {
            assert_eq!(value["input"][index]["output"], long(&index.to_string()));
        }
        assert_eq!(elide_old_tool_outputs(&trimmed), None);
    }

    #[test]
    fn six_outputs_need_no_rewrite() {
        let input = (0..6)
            .map(|index| {
                json!({
                    "type": "function_call_output",
                    "call_id": format!("call-{index}"),
                    "output": long(&index.to_string())
                })
            })
            .collect::<Vec<_>>();
        let body = serde_json::to_vec(&json!({"input":input})).unwrap();
        assert_eq!(elide_old_tool_outputs(&body), None);
    }

    #[test]
    fn only_tool_output_bodies_change() {
        let body = serde_json::to_vec(&json!({
            "model": "gpt-5",
            "instructions": long("instructions"),
            "tools": [{"type":"function","name":"Bash","parameters":{"type":"object"}}],
            "input": [
                {"type":"message","role":"user","content":[{"type":"input_text","text":long("user")}]},
                {"type":"function_call","call_id":"call-0","name":"Bash","arguments":"{\"command\":\"pwd\"}"},
                {"type":"function_call_output","call_id":"call-0","output":long("old")},
                {"type":"custom_tool_call_output","call_id":"call-1","output":long("old-custom")},
                {"type":"local_shell_call_output","call_id":"call-2","output":long("old-shell")},
                {"type":"function_call_output","call_id":"call-3","output":long("new-3")},
                {"type":"function_call_output","call_id":"call-4","output":long("new-4")},
                {"type":"function_call_output","call_id":"call-5","output":long("new-5")},
                {"type":"function_call_output","call_id":"call-6","output":long("new-6")},
                {"type":"function_call_output","call_id":"call-7","output":long("new-7")},
                {"type":"function_call_output","call_id":"call-8","output":long("new-8")}
            ]
        }))
        .unwrap();
        let before: Value = serde_json::from_slice(&body).unwrap();
        let after: Value = serde_json::from_slice(&elide_old_tool_outputs(&body).unwrap()).unwrap();

        assert_eq!(after["instructions"], before["instructions"]);
        assert_eq!(after["tools"], before["tools"]);
        assert_eq!(after["input"][0], before["input"][0]);
        assert_eq!(after["input"][1], before["input"][1]);
        assert_eq!(after["input"][2]["output"], ELIDED_TOOL_OUTPUT);
        assert_eq!(after["input"][3]["output"], ELIDED_TOOL_OUTPUT);
        assert_eq!(after["input"][4]["output"], ELIDED_TOOL_OUTPUT);
        for index in 5..11 {
            assert_eq!(after["input"][index], before["input"][index]);
        }
    }

    #[test]
    fn chat_tool_messages_use_a_single_text_part() {
        let messages = (0..7)
            .map(|index| {
                json!({
                    "role": "tool",
                    "tool_call_id": format!("call-{index}"),
                    "content": [{"type":"text","text":long(&index.to_string())}]
                })
            })
            .collect::<Vec<_>>();
        let body = serde_json::to_vec(&json!({"model":"gpt-5","messages":messages})).unwrap();
        let value: Value = serde_json::from_slice(&elide_old_tool_outputs(&body).unwrap()).unwrap();
        assert_eq!(
            value["messages"][0]["content"],
            json!([{"type":"text","text":ELIDED_TOOL_OUTPUT}])
        );
        assert_eq!(value["messages"][0]["tool_call_id"], "call-0");
    }

    #[test]
    fn non_text_parts_are_skipped_while_other_old_text_outputs_are_trimmed() {
        let with_image = serde_json::to_vec(&json!({
            "input": [
                {"type":"function_call_output","call_id":"0","output":[
                    {"type":"input_text","text":long("old")},
                    {"type":"input_image","image_url":"data:image/png;base64,AA=="}
                ]},
                {"type":"function_call_output","call_id":"1","output":long("1")},
                {"type":"function_call_output","call_id":"2","output":long("2")},
                {"type":"function_call_output","call_id":"3","output":long("3")},
                {"type":"function_call_output","call_id":"4","output":long("4")},
                {"type":"function_call_output","call_id":"5","output":"short"},
                {"type":"function_call_output","call_id":"6","output":long("6")},
                {"type":"function_call_output","call_id":"7","output":long("7")}
            ]
        }))
        .unwrap();
        let value: Value =
            serde_json::from_slice(&elide_old_tool_outputs(&with_image).unwrap()).unwrap();
        assert_eq!(
            value["input"][0]["output"],
            json!([
                {"type":"input_text","text":long("old")},
                {"type":"input_image","image_url":"data:image/png;base64,AA=="}
            ])
        );
        assert_eq!(value["input"][1]["output"], ELIDED_TOOL_OUTPUT);
        assert_eq!(value["input"][2]["output"], long("2"));
        assert_eq!(value["input"][7]["output"], long("7"));
        assert_eq!(elide_old_tool_outputs(b"not json"), None);
        assert!(!trim_enabled(&json!({})));
        assert!(!trim_enabled(&json!({CONFIG_KEY:"off"})));
        assert!(trim_enabled(&json!({CONFIG_KEY:"trim_old_tool_outputs"})));
    }

    #[test]
    fn validation_accepts_only_the_supported_modes() {
        for mode in [
            None,
            Some(Value::Null),
            Some(json!("off")),
            Some(json!("trim_old_tool_outputs")),
        ] {
            assert!(validate(mode.as_ref()).is_ok());
        }
        for mode in [json!("on"), json!(true), json!("trim_all")] {
            assert!(validate(Some(&mode)).is_err());
        }
    }
}
