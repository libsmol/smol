use crate::model::{
    error_text, grouped, merged, retryable, tool_parameters, Call, ChatMessage, Finish, ModelError,
    OpenAIModel, Purpose, Stream, StreamDelta, TokenUsage,
};
use crate::prompt::{self, TOOL};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::mem;

/// Fields the protocol defines, which the request rebuilds from the step
const MESSAGE_FIELDS: [&str; 5] = [
    "role",
    "content",
    "reasoning_content",
    "reasoning",
    "tool_calls",
];
const CALL_FIELDS: [&str; 4] = ["index", "id", "type", "function"];

pub(crate) fn body(
    model: &OpenAIModel,
    conversation: &[ChatMessage],
    purpose: Purpose,
) -> Map<String, Value> {
    let mut sent = Vec::new();
    // Tool messages can't hold images, which go in a user message after all the step's replies
    let mut attached = Vec::new();
    // Calls that go back under the ids their provider gave
    let mut renamed = HashMap::new();
    for group in grouped(conversation) {
        let message = match &group[0] {
            ChatMessage::System(text) => json!({ "role": "system", "content": text }),
            ChatMessage::User(_) => {
                let parts = merged(group);
                let mut content = model.content(&parts, None);
                // Images open the user message that follows the replies instead of a new one
                if !attached.is_empty() {
                    let mut blocks = mem::take(&mut attached);
                    match content {
                        Value::Array(rest) => blocks.extend(rest),
                        text => blocks.push(json!({ "type": "text", "text": text })),
                    }
                    content = Value::Array(blocks);
                }
                json!({ "role": "user", "content": content })
            }
            ChatMessage::Assistant {
                text,
                reasoning,
                sealed,
                calls,
            } => {
                let content = match text.is_empty() && !calls.is_empty() {
                    true => Value::Null,
                    false => json!(text),
                };
                let mut message = json!({ "role": "assistant", "content": content });
                if let Some(reasoning) = reasoning {
                    message["reasoning_content"] = json!(reasoning);
                }

                // The provider's own fields go back on the message and calls they came with
                let sealed = model.opens(sealed.as_ref());
                let mut own = match sealed.and_then(|sealed| sealed.items.first()) {
                    Some(Value::Object(fields)) => fields.clone(),
                    _ => Map::new(),
                };
                let theirs = own.shift_remove("tool_calls").unwrap_or_default();
                for (key, value) in own {
                    message[key] = value;
                }

                let mut called = Vec::new();
                for (index, call) in calls.iter().enumerate() {
                    let function = json!({ "name": call.name, "arguments": call.arguments });
                    let mut item =
                        json!({ "id": call.id, "type": "function", "function": function });
                    // Their fields may name the id the provider gave, which replaces ours
                    if let Some(Value::Object(fields)) = theirs.get(index) {
                        for (key, value) in fields {
                            item[key] = value.clone();
                        }
                    }
                    if item["id"] != call.id.as_str() {
                        renamed.insert(call.id.as_str(), item["id"].clone());
                    }
                    called.push(item);
                }
                if !called.is_empty() {
                    message["tool_calls"] = Value::Array(called);
                }
                message
            }
            ChatMessage::Tool { id, parts } => {
                let content = model.content(&[parts.as_slice()], Some(&mut attached));
                let id = renamed
                    .get(id.as_str())
                    .cloned()
                    .unwrap_or_else(|| json!(id));
                json!({ "role": "tool", "content": content, "tool_call_id": id })
            }
        };

        let answering = matches!(group[0], ChatMessage::Tool { .. });
        if !answering && !attached.is_empty() {
            let images = mem::take(&mut attached);
            sent.push(json!({ "role": "user", "content": images }));
        }
        sent.push(message);
    }
    if !attached.is_empty() {
        sent.push(json!({ "role": "user", "content": attached }));
    }

    let mut body = Map::new();
    if let Some(effort) = model.effort_for(purpose) {
        body.insert("reasoning_effort".into(), json!(effort));
    }
    body.insert("model".into(), json!(model.spec.id));
    body.insert("messages".into(), Value::Array(sent));
    if let Some(max_tokens) = model.spec.max_tokens {
        body.insert("max_tokens".into(), json!(max_tokens));
    }
    body.insert("stream".into(), json!(true));
    body.insert("stream_options".into(), json!({ "include_usage": true }));
    if purpose == Purpose::Step {
        let parameters = tool_parameters();
        let function =
            json!({ "name": TOOL, "description": prompt::TOOL_DOC, "parameters": parameters });
        let tool = json!({ "type": "function", "function": function });
        body.insert("tools".into(), json!([tool]));
    }
    body
}

impl Stream {
    pub(crate) fn chunk(
        &mut self,
        chunk: &Value,
        data: &[u8],
        on_delta: &mut dyn FnMut(StreamDelta),
    ) -> Result<(), ModelError> {
        // After a 200 it's a transient upstream error unless `code` blames the request
        let error = &chunk["error"];
        if !error.is_null() {
            let raw = String::from_utf8_lossy(data);
            let said = error_text(chunk, &raw);
            let message = format!("stream: {said}");

            let code = error["code"].as_u64();
            let code = code.and_then(|code| u16::try_from(code).ok());
            let transient = code.is_none_or(retryable);
            return Err(ModelError {
                message,
                transient,
                retry_after: None,
            });
        }

        let choice = &chunk["choices"][0];
        if let Some(reason) = choice["finish_reason"].as_str() {
            let reason = match reason {
                "stop" | "end" | "tool_calls" | "function_call" => Finish::Stop,
                "length" => Finish::Length,
                "content_filter" => Finish::Filtered,
                other => Finish::Other(other.to_owned()),
            };
            self.finish = Some(reason);
        }

        let reported = &chunk["usage"];
        if reported.is_object() {
            let input = reported["prompt_tokens"].as_u64().unwrap_or(0);
            let output = reported["completion_tokens"].as_u64().unwrap_or(0);
            let mut usage = TokenUsage::new(input, output);
            usage.cached_tokens = reported["prompt_tokens_details"]["cached_tokens"].as_u64();
            self.usage = Some(usage);
        }

        let delta = &choice["delta"];
        let thinking = delta["reasoning_content"].as_str();
        let fallback = delta["reasoning"].as_str();
        // Keep empty reasoning since DeepSeek returns 400
        // if this turn's calls come back without it
        let reasoning = match thinking {
            Some("") | None => fallback.or(thinking),
            thinking => thinking,
        };
        if let Some(reasoning) = reasoning {
            on_delta(StreamDelta::Reasoning(reasoning));
        }
        let content = delta["content"].as_str().filter(|text| !text.is_empty());
        if let Some(content) = content {
            self.say(content, on_delta);
        }
        add_fields(&mut self.gathered.extras, delta, &MESSAGE_FIELDS);

        if let Some(parts) = delta["tool_calls"].as_array() {
            for part in parts {
                let index = part["index"].as_u64().unwrap_or(0);
                let id = part["id"].as_str().unwrap_or_default();
                let function = &part["function"];
                let name = function["name"].as_str().unwrap_or_default();
                let arguments = function["arguments"].as_str().unwrap_or_default();

                // Calls arrive in sequence, so the previous one is done once the next starts
                // Some providers keep `index` at 0 and some proxies change ids per chunk,
                // so only a new id with a name starts a call
                let current = match &self.call {
                    Some(call) if call.index == index => {
                        let opens = !id.is_empty() && !name.is_empty();
                        !opens || call.id.is_empty() || call.id == id
                    }
                    _ => false,
                };
                if !current {
                    self.hand_over(on_delta);
                }

                let started = Call {
                    index,
                    ..Call::default()
                };
                let call = self.call.get_or_insert(started);
                if call.id.is_empty() {
                    call.id.push_str(id);
                }
                // Some providers repeat the full name in every chunk
                if call.name != name {
                    call.name.push_str(name);
                }
                add_fields(&mut call.extras, part, &CALL_FIELDS);
                self.add_arguments(arguments, on_delta);
            }
        }

        if self.finish.is_some() {
            self.hand_over(on_delta);
        }
        Ok(())
    }
}

/// Adds the fields that only this provider reads,
/// whose pieces a stream sends over several chunks
fn add_fields(held: &mut Map<String, Value>, piece: &Value, known: &[&str]) {
    let Some(fields) = piece.as_object() else {
        return;
    };

    for (key, value) in fields {
        if known.contains(&key.as_str()) || value.is_null() {
            continue;
        }

        let entry = held.entry(key.as_str());
        match (key.as_str(), value) {
            ("reasoning_details", Value::Array(pieces)) => {
                let details = entry.or_insert_with(|| json!([]));
                let Value::Array(details) = details else {
                    continue;
                };
                for piece in pieces {
                    add_detail(details, piece);
                }
            }
            _ => {
                let field = entry.or_insert(Value::Null);
                merge(field, value);
            }
        }
    }
}

/// Strings and arrays add up, objects merge key by key and anything else is replaced
fn merge(held: &mut Value, piece: &Value) {
    match (held, piece) {
        (Value::Object(held), Value::Object(piece)) => {
            for (key, value) in piece {
                let field = held.entry(key.as_str()).or_insert(Value::Null);
                merge(field, value);
            }
        }
        (Value::String(held), Value::String(piece)) => held.push_str(piece),
        (Value::Array(held), Value::Array(piece)) => held.extend(piece.iter().cloned()),
        (held, piece) => *held = piece.clone(),
    }
}

/// Builds `reasoning_details` as OpenRouter's own client does, since its `index` doesn't tell
/// the pieces of one detail apart and a detail sent back in pieces breaks the next request
fn add_detail(details: &mut Vec<Value>, piece: &Value) {
    let unset = |value: &Value| value.as_str().is_none_or(str::is_empty);
    let field = match piece["type"].as_str() {
        Some("reasoning.text") => Some("text"),
        Some("reasoning.summary") => Some("summary"),
        _ => None,
    };
    // Text and summaries join the last detail of their type until a signature closes it
    let (field, last) = match (field, details.last_mut()) {
        (Some(field), Some(last)) if last["type"] == piece["type"] && unset(&last["signature"]) => {
            (field, last)
        }
        _ => {
            details.push(piece.clone());
            return;
        }
    };

    let held = last[field].as_str().unwrap_or_default();
    let added = piece[field].as_str().unwrap_or_default();
    let joined = format!("{held}{added}");
    last[field] = json!(joined);
    for key in ["signature", "format"] {
        if unset(&last[key]) && !unset(&piece[key]) {
            last[key] = piece[key].clone();
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::config::{Kind, ModelSpec};
    use crate::model::tests::{parsed, png, served, DONE};
    use crate::model::{ChatMessage, Finish, Media, Part, Purpose, Sealed, ToolCall};
    use crate::prompt;
    use crate::tests::{temp_dir, write_file};
    use serde_json::json;
    use std::fs;

    #[test]
    fn calls_sent() {
        let dir = temp_dir("calls");
        let picture = dir.join("chart.png");
        write_file(&picture, b"png");
        let media = Media {
            file: picture,
            ..png("chart.png")
        };

        let call = |id: &str, code: &str| ToolCall::eval(id.to_owned(), code);
        let asked = ChatMessage::Assistant {
            text: "Let me look.".to_owned(),
            reasoning: Some("Run it first".to_owned()),
            sealed: None,
            calls: vec![call("call_1", "print('café')")],
        };
        let printed = ChatMessage::Tool {
            id: "call_1".to_owned(),
            parts: vec![Part::Text("café".to_owned())],
        };
        let silent = ChatMessage::Assistant {
            text: String::new(),
            reasoning: None,
            sealed: None,
            calls: vec![
                call("call_2", "view('chart.png')"),
                call("call_3", "view('chart.png')"),
                ToolCall::unread("call_4".to_owned(), "functions.résumé"),
                ToolCall::unread("call_5".to_owned(), ""),
            ],
        };
        let tag = prompt::media_tag("chart.png", &media);
        let viewed = Part::Media("chart.png".to_owned(), media);
        let quiet = Part::Text(prompt::NO_OUTPUT.to_owned());
        let first = ChatMessage::Tool {
            id: "call_2".to_owned(),
            parts: vec![quiet, viewed.clone()],
        };
        let second = ChatMessage::Tool {
            id: "call_3".to_owned(),
            parts: vec![viewed],
        };
        let refused = |id: &str| ChatMessage::Tool {
            id: id.to_owned(),
            parts: vec![Part::Text("No such tool".to_owned())],
        };
        let update = ChatMessage::user("Task update:\nUse the café colors");
        let conversation = [
            asked,
            printed,
            silent,
            first,
            second,
            refused("call_4"),
            refused("call_5"),
            update,
        ];

        let spec = ModelSpec {
            image: true,
            ..ModelSpec::default()
        };
        let (_, body) = served(Kind::Completions, DONE, &spec, &conversation, Purpose::Step);
        let expected = json!([
            {
                "role": "assistant",
                "content": "Let me look.",
                "reasoning_content": "Run it first",
                "tool_calls": [{"id": "call_1", "type": "function", "function": {"name": "eval", "arguments": "{\"code\":\"print('café')\"}"}}],
            },
            {"role": "tool", "content": "café", "tool_call_id": "call_1"},
            {
                "role": "assistant",
                "content": null,
                "tool_calls": [
                    {"id": "call_2", "type": "function", "function": {"name": "eval", "arguments": "{\"code\":\"view('chart.png')\"}"}},
                    {"id": "call_3", "type": "function", "function": {"name": "eval", "arguments": "{\"code\":\"view('chart.png')\"}"}},
                    {"id": "call_4", "type": "function", "function": {"name": "functions_r_sum_", "arguments": "{}"}},
                    {"id": "call_5", "type": "function", "function": {"name": "_", "arguments": "{}"}},
                ],
            },
            {"role": "tool", "content": format!("{}\n{tag}", prompt::NO_OUTPUT), "tool_call_id": "call_2"},
            {"role": "tool", "content": tag, "tool_call_id": "call_3"},
            {"role": "tool", "content": "No such tool", "tool_call_id": "call_4"},
            {"role": "tool", "content": "No such tool", "tool_call_id": "call_5"},
            {"role": "user", "content": [
                {"type": "text", "text": tag},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,cG5n"}},
                {"type": "text", "text": tag},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,cG5n"}},
                {"type": "text", "text": "Task update:\nUse the café colors"},
            ]},
        ]);
        assert_eq!(body["messages"], expected);

        // Without a following user message the images get one of their own
        let (_, ended) = served(
            Kind::Completions,
            DONE,
            &spec,
            &conversation[..7],
            Purpose::Step,
        );
        let images = &expected[7]["content"].as_array().expect("blocks")[..4];
        let alone = json!({ "role": "user", "content": images });
        assert_eq!(ended["messages"][7], alone);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn tool_calls_collected() {
        let (finished, content, _) = parsed(
            Kind::Completions,
            &[
                r#"data: {"choices":[{"delta":{"content":"Let me work it out."}}]}"#,
                r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","type":"function","function":{"name":"eval","arguments":""}}]}}]}"#,
                r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"code\": \"print('caf"}}]}}]}"#,
                r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"é')\"}"}}]}}]}"#,
                r#"data: {"choices":[{"delta":{"tool_calls":[{"index":1,"function":{"name":"ls","arguments":"{}"}}]},"finish_reason":"tool_calls"}]}"#,
                "data: [DONE]",
            ],
        );
        assert_eq!(finished.expect("parse").finish, Finish::Stop);
        assert_eq!(content, "Let me work it out.print('café')[end][ls {}]");

        let (_, content, _) = parsed(
            Kind::Completions,
            &[
                r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"eval","arguments":"{\"code\": \"a = 'naïve'\"}"}}]}}]}"#,
                r#"data: {"choices":[{"delta":{"tool_calls":[{"index":1,"function":{"name":"eval","arguments":"{\"code\": \"print("}}]}}]}"#,
                r#"data: {"choices":[{"delta":{"tool_calls":[{"index":1,"function":{"arguments":"a)\"}"}}]},"finish_reason":"tool_calls"}]}"#,
                "data: [DONE]",
            ],
        );
        assert_eq!(content, "a = 'naïve'[end]print(a)[end]");

        let (_, content, _) = parsed(
            Kind::Completions,
            &[
                r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"eval","arguments":"{\"code\": \"print('jalapeño')\"}"}}]}}]}"#,
                r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c2","function":{"name":"eval","arguments":"{\"code\": \"print('piñata')\"}"}}]},"finish_reason":"tool_calls"}]}"#,
                "data: [DONE]",
            ],
        );
        assert_eq!(content, "print('jalapeño')[end]print('piñata')[end]");

        let (_, content, _) = parsed(
            Kind::Completions,
            &[
                r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"eval","arguments":"{\"code\": \"pri"}}]}}]}"#,
                r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"eval","arguments":"nt('crème')\"}"}}]},"finish_reason":"tool_calls"}]}"#,
                "data: [DONE]",
            ],
        );
        assert_eq!(content, "print('crème')[end]");

        let (_, content, _) = parsed(
            Kind::Completions,
            &[
                r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"eval","arguments":"{\"code\": \"pri"}}]}}]}"#,
                r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c2","function":{"arguments":"nt('brûlée')\"}"}}]},"finish_reason":"tool_calls"}]}"#,
                "data: [DONE]",
            ],
        );
        assert_eq!(content, "print('brûlée')[end]");

        let (finished, content, _) = parsed(
            Kind::Completions,
            &[
                r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"eval","arguments":"{\"code\": \"pri"}}]},"finish_reason":"length"}]}"#,
            ],
        );
        assert_eq!(finished.expect("parse").finish, Finish::Length);
        assert_eq!(content, "pri");

        let (finished, content, _) = parsed(
            Kind::Completions,
            &[
                r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"eval","arguments":"{\"code\": \"print('café"}}]},"finish_reason":"content_filter"}]}"#,
            ],
        );
        assert_eq!(finished.expect("parse").finish, Finish::Filtered);
        assert_eq!(content, "print('café");
    }

    #[test]
    fn loose_arguments() {
        let called = |arguments: &str| {
            let call =
                json!({ "index": 0, "function": { "name": "eval", "arguments": arguments } });
            let delta = json!({ "tool_calls": [call] });
            let chunk = json!({ "choices": [{ "delta": delta, "finish_reason": "tool_calls" }] });
            let line = format!("data: {chunk}");
            let (_, content, _) = parsed(Kind::Completions, &[&line, "data: [DONE]"]);
            content
        };

        let sloppy = "{\"code\": \"import re\\nprint(re.findall(r'\\d+', 'café1\tpiñata22'))\"}";
        let code = "import re\nprint(re.findall(r'\\d+', 'café1\tpiñata22'))[end]";
        assert_eq!(called(sloppy), code);

        let renamed =
            "{ \"content\" : \"import re\\nprint(re.findall(r'\\d+', 'café1\tpiñata22'))\" }";
        assert_eq!(called(renamed), code);
        let several = r#"{"command": "ls résumés", "cwd": "."}"#;
        assert_eq!(called(several), format!("[eval {several}]"));
        let number = r#"{"code": 1}"#;
        assert_eq!(called(number), format!("[eval {number}]"));
    }

    #[test]
    fn fields_replayed() {
        let call = |id: &str, code: &str| ToolCall::eval(id.to_owned(), code);
        let printed = |id: &str| ChatMessage::Tool {
            id: id.to_owned(),
            parts: vec![Part::Text("café".to_owned())],
        };
        let details = json!([{ "type": "reasoning.encrypted", "data": "gAAAA-naïve", "id": "tool_1", "index": 0 }]);
        let signed = json!({ "google": { "thought_signature": "c2ln" } });
        let items = vec![json!({
            "reasoning_details": details,
            "tool_calls": [{ "extra_content": signed, "id": "tool_1" }, { "id": "tool_2" }],
        })];
        let replied = |provider: &str, kind| {
            let sealed = Sealed {
                provider: provider.to_owned(),
                kind,
                items: items.clone(),
                ends: Vec::new(),
            };
            let asked = ChatMessage::Assistant {
                text: String::new(),
                reasoning: Some("Weigh the café".to_owned()),
                sealed: Some(sealed),
                calls: vec![call("call_1", "print('crème')"), call("call_2", "print(1)")],
            };
            let conversation = [asked, printed("call_1"), printed("call_2")];
            let spec = ModelSpec::default();
            let (_, body) = served(Kind::Completions, DONE, &spec, &conversation, Purpose::Step);
            body["messages"].clone()
        };

        let ours = replied("mock", Kind::Completions);
        let function =
            |code: &str| json!({ "name": "eval", "arguments": format!("{{\"code\":\"{code}\"}}") });
        let expected = json!([
            {
                "role": "assistant",
                "content": null,
                "reasoning_content": "Weigh the café",
                "reasoning_details": details,
                "tool_calls": [
                    { "id": "tool_1", "type": "function", "function": function("print('crème')"), "extra_content": signed },
                    { "id": "tool_2", "type": "function", "function": function("print(1)") },
                ],
            },
            { "role": "tool", "content": "café", "tool_call_id": "tool_1" },
            { "role": "tool", "content": "café", "tool_call_id": "tool_2" },
        ]);
        assert_eq!(ours, expected);

        for theirs in [
            replied("other", Kind::Completions),
            replied("mock", Kind::Responses),
        ] {
            assert!(theirs[0].get("reasoning_details").is_none(), "{theirs}");
            assert_eq!(theirs[0]["tool_calls"][0]["id"], "call_1");
            assert_eq!(theirs[2]["tool_call_id"], "call_2");
        }
    }
}
