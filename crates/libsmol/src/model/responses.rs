use crate::model::{
    error_text, merged, Call, ChatMessage, Finish, ModelError, OpenAIModel, Purpose, Stream,
    StreamDelta, TokenUsage,
};
use crate::prompt::{self, ARGUMENT, TOOL};
use serde_json::{json, Map, Value};
use std::mem;

/// Media goes in the tool output that shows it, which this API takes
pub(crate) fn body(
    model: &OpenAIModel,
    conversation: &[ChatMessage],
    purpose: Purpose,
) -> Map<String, Value> {
    let mut input = Vec::new();
    let together = |a: &ChatMessage, b: &ChatMessage| {
        matches!((a, b), (ChatMessage::User(_), ChatMessage::User(_)))
    };
    for group in conversation.chunk_by(together) {
        match &group[0] {
            ChatMessage::System(text) => {
                let message = json!({ "role": "system", "content": text });
                input.push(message);
            }
            ChatMessage::User(_) => {
                let parts = merged(group);
                let content = model.content(&parts, None);
                let message = json!({ "role": "user", "content": content });
                input.push(message);
            }
            ChatMessage::Assistant {
                text,
                sealed,
                calls,
                ..
            } => {
                // The provider's own items go back in their order with the text and calls in them,
                // and what they lack follows
                let items = match sealed {
                    Some(sealed) if sealed.provider == model.provider => &sealed.items[..],
                    _ => &[],
                };
                let mut said = text.is_empty();
                let mut calls = calls.iter();
                for kept in items {
                    let mut item = kept.clone();
                    match kept["type"].as_str() {
                        Some("message") if !said => {
                            let part =
                                json!({ "type": "output_text", "text": text, "annotations": [] });
                            item["content"] = json!([part]);
                            said = true;
                        }
                        // The first one holds all the text
                        Some("message") => continue,
                        Some("function_call") => {
                            let Some(call) = calls.next() else {
                                continue;
                            };
                            item["call_id"] = json!(call.id);
                            item["name"] = json!(call.name);
                            item["arguments"] = json!(call.arguments);
                        }
                        _ => {}
                    }
                    input.push(item);
                }
                if !said {
                    let message = json!({ "role": "assistant", "content": text });
                    input.push(message);
                }
                for call in calls {
                    let item = json!({
                        "type": "function_call",
                        "call_id": call.id,
                        "name": call.name,
                        "arguments": call.arguments,
                    });
                    input.push(item);
                }
                // Servers refuse reasoning not followed by the item it led to
                while input.last().is_some_and(|item| item["type"] == "reasoning") {
                    input.pop();
                }
            }
            ChatMessage::Tool { id, parts } => {
                let output = model.content(&[parts.as_slice()], None);
                let item =
                    json!({ "type": "function_call_output", "call_id": id, "output": output });
                input.push(item);
            }
        }
    }

    let mut reasoning = Map::new();
    // Leaving out the effort still has the server reason at its default effort
    if let Some(effort) = model.effort.as_ref().filter(|_| purpose != Purpose::Title) {
        reasoning.insert("effort".into(), json!(effort));
    }
    // Only steps show the reasoning
    if let Some(summary) = model.summary.as_ref().filter(|_| purpose == Purpose::Step) {
        reasoning.insert("summary".into(), json!(summary));
    }

    let mut body = Map::new();
    body.insert("model".into(), json!(model.id));
    if !reasoning.is_empty() {
        body.insert("reasoning".into(), Value::Object(reasoning));
    }
    body.insert("input".into(), Value::Array(input));
    if let Some(max_tokens) = model.max_tokens {
        body.insert("max_output_tokens".into(), json!(max_tokens));
    }
    body.insert("stream".into(), json!(true));
    body.insert("store".into(), json!(false));
    if purpose == Purpose::Step {
        let code = json!({ "type": "string", "description": prompt::ARGUMENT_DOC });
        let parameters = json!({
            "type": "object",
            "properties": { ARGUMENT: code },
            "required": [ARGUMENT],
        });
        // Strict mode would refuse a schema that allows other properties
        let tool = json!({
            "type": "function",
            "name": TOOL,
            "description": prompt::TOOL_DOC,
            "parameters": parameters,
            "strict": false,
        });
        body.insert("tools".into(), json!([tool]));
    }
    body
}

impl Stream {
    /// Returns true once the response has ended
    pub(crate) fn event(
        &mut self,
        event: &Value,
        data: &[u8],
        on_delta: &mut dyn FnMut(StreamDelta),
    ) -> Result<bool, ModelError> {
        let kind = event["type"].as_str().unwrap_or_default();
        let delta = event["delta"].as_str().unwrap_or_default();
        let item = &event["item"];

        if kind == "response.output_item.done" {
            let held: &[&str] = match item["type"].as_str() {
                Some("message") => &["content"],
                Some("function_call") => &["call_id", "name", "arguments"],
                _ => &[],
            };
            let mut kept = item.clone();
            if let Some(fields) = kept.as_object_mut() {
                for field in held {
                    fields.remove(*field);
                }
            }
            let sealed = StreamDelta::Sealed {
                provider: &self.provider,
                item: &kept,
            };
            on_delta(sealed);
        }

        match kind {
            "response.output_text.delta" | "response.refusal.delta" if !delta.is_empty() => {
                on_delta(StreamDelta::Content(delta));
            }
            // Each summary part and reasoning item gets a paragraph of its own
            "response.reasoning_summary_part.added" => self.parted = true,
            "response.output_item.added" if item["type"] == "reasoning" => self.parted = true,
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta"
                if !delta.is_empty() =>
            {
                if mem::take(&mut self.parted) && self.reasoned {
                    on_delta(StreamDelta::Reasoning("\n\n"));
                }
                self.reasoned = true;
                on_delta(StreamDelta::Reasoning(delta));
            }
            "response.output_item.added" | "response.output_item.done"
                if item["type"] == "function_call" =>
            {
                // Servers may send either event alone, so the call opens at whichever comes first
                let id = item["call_id"].as_str().unwrap_or_default();
                let open = self.call.as_ref().is_some_and(|call| call.id == id);
                if !open {
                    self.hand_over(on_delta);
                    let name = item["name"].as_str().unwrap_or_default();
                    let call = Call {
                        id: id.to_owned(),
                        name: name.to_owned(),
                        ..Call::default()
                    };
                    self.call = Some(call);
                }

                // Some servers send the arguments only with the item
                let whole = item["arguments"].as_str().unwrap_or_default();
                let received = self.call.as_ref().map_or("", |call| &call.arguments);
                let rest = whole.strip_prefix(received).unwrap_or_default().to_owned();
                self.add_arguments(&rest, on_delta);

                // A call the output limit cut off ends incomplete and stays open to be withheld
                let done = kind == "response.output_item.done" && item["status"] != "incomplete";
                if done {
                    self.hand_over(on_delta);
                }
            }
            "response.function_call_arguments.delta" => self.add_arguments(delta, on_delta),
            "response.completed" | "response.incomplete" => {
                let response = &event["response"];
                let reported = &response["usage"];
                if reported.is_object() {
                    let input = reported["input_tokens"].as_u64().unwrap_or(0);
                    let output = reported["output_tokens"].as_u64().unwrap_or(0);
                    let mut usage = TokenUsage::new(input, output);
                    usage.cached_tokens =
                        reported["input_tokens_details"]["cached_tokens"].as_u64();
                    self.usage = Some(usage);
                }

                let reason = response["incomplete_details"]["reason"].as_str();
                let finish = match (kind, reason) {
                    ("response.completed", _) => Finish::Stop,
                    (_, Some("max_output_tokens")) => Finish::Length,
                    (_, reason) => {
                        let reason = reason.unwrap_or("incomplete");
                        Finish::Other(reason.to_owned())
                    }
                };
                self.finish = Some(finish);
                self.hand_over(on_delta);
                return Ok(true);
            }
            "response.failed" | "error" => {
                // A failed response holds the error, which an error event holds flat or nested
                let holder = match kind {
                    "response.failed" => &event["response"],
                    _ => event,
                };
                let error = match holder["error"].is_object() {
                    true => &holder["error"],
                    false => holder,
                };
                let raw = String::from_utf8_lossy(data);
                let said = error_text(holder, &raw);
                let message = format!("stream: {said}");

                // Only these codes blame the server rather than the request
                let code = error["code"].as_str();
                let transient = matches!(code, None | Some("server_error" | "rate_limit_exceeded"));
                return Err(ModelError {
                    message,
                    transient,
                    retry_after: None,
                });
            }
            _ => {}
        }

        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use crate::config::{Kind, ModelSpec};
    use crate::model::tests::{parsed, png, served, DONE};
    use crate::model::{ChatMessage, Finish, Media, Part, Purpose, Sealed, ToolCall};
    use crate::prompt::TOOL_DOC;
    use crate::tests::{temp_dir, write_file};
    use serde_json::{json, Value};
    use std::fs;

    #[test]
    fn events_parsed() {
        let encrypted = json!({ "type": "reasoning", "id": "rs_1", "summary": [], "encrypted_content": "gAAAA-café" });
        let bare = json!({ "type": "reasoning", "id": "rs_2", "summary": [] });
        let said = json!({ "type": "message", "id": "msg_1", "role": "assistant", "status": "completed", "phase": "commentary", "content": [{ "type": "output_text", "text": "Hello, world 🍕" }] });
        let plain = json!({ "type": "reasoning", "id": "rs_3", "summary": [], "content": [{ "type": "reasoning_text", "text": "Check the résumé" }] });
        let done =
            |item: &Value| format!(r#"data: {{"type":"response.output_item.done","item":{item}}}"#);
        let (finished, content, reasoning) = parsed(
            Kind::Responses,
            &[
                "event: response.created",
                r#"data: {"type":"response.created","response":{"id":"resp_1","status":"in_progress"}}"#,
                r#"data: {"type":"response.output_item.added","item":{"type":"reasoning","id":"rs_1","summary":[]}}"#,
                r#"data: {"type":"response.reasoning_summary_part.added","summary_index":0}"#,
                r#"data: {"type":"response.reasoning_summary_text.delta","summary_index":0,"delta":"Think about the façade"}"#,
                r#"data: {"type":"response.reasoning_summary_part.added","summary_index":1}"#,
                r#"data: {"type":"response.reasoning_summary_text.delta","summary_index":1,"delta":"Then the piñata"}"#,
                &done(&encrypted),
                &done(&bare),
                r#"data: {"type":"response.output_item.added","item":{"type":"message","id":"msg_1","role":"assistant","content":[]}}"#,
                r#"data: {"type":"response.output_text.delta","delta":"Hello"}"#,
                r#"data: {"type":"response.output_text.delta","delta":", world 🍕"}"#,
                &done(&said),
                r#"data: {"type":"response.output_item.added","item":{"type":"reasoning","id":"rs_3","summary":[]}}"#,
                r#"data: {"type":"response.reasoning_text.delta","delta":"Check the résumé"}"#,
                &done(&plain),
                r#"data: {"type":"response.completed","response":{"status":"completed","usage":{"input_tokens":10,"output_tokens":2,"input_tokens_details":{"cached_tokens":8}}}}"#,
            ],
        );

        let mut kept = said.clone();
        kept.as_object_mut().expect("item").remove("content");
        let items =
            format!("[mock {encrypted}][mock {bare}]Hello, world 🍕[mock {kept}][mock {plain}]");
        assert_eq!(content, items);
        assert_eq!(
            reasoning,
            "Think about the façade\n\nThen the piñata\n\nCheck the résumé"
        );
        let finished = finished.expect("parse");
        assert_eq!(finished.finish, Finish::Stop);
        let usage = finished.usage.expect("usage");
        assert_eq!((usage.input_tokens, usage.output_tokens), (10, 2));
        assert_eq!(usage.cached_tokens, Some(8));
    }

    #[test]
    fn calls_collected() {
        let (finished, content, _) = parsed(
            Kind::Responses,
            &[
                r#"data: {"type":"response.output_text.delta","delta":"Let me work it out."}"#,
                r#"data: {"type":"response.output_item.added","item":{"type":"function_call","call_id":"call_a","name":"eval","arguments":""}}"#,
                r#"data: {"type":"response.function_call_arguments.delta","delta":"{\"code\": \"print('caf"}"#,
                r#"data: {"type":"response.function_call_arguments.delta","delta":"é')\"}"}"#,
                r#"data: {"type":"response.output_item.done","item":{"type":"function_call","call_id":"call_a","name":"eval","arguments":"{\"code\": \"print('café')\"}","status":"completed"}}"#,
                r#"data: {"type":"response.output_item.done","item":{"type":"function_call","call_id":"call_b","name":"eval","arguments":"{\"code\": \"x = 'naïve'\"}","status":"completed"}}"#,
                r#"data: {"type":"response.output_item.added","item":{"type":"function_call","call_id":"call_c","name":"ls","arguments":"{}"}}"#,
                r#"data: {"type":"response.completed","response":{"status":"completed"}}"#,
            ],
        );
        assert_eq!(finished.expect("parse").finish, Finish::Stop);
        let kept = json!({ "type": "function_call", "status": "completed" });
        let calls = format!(
            "Let me work it out.print('café')[mock {kept}][end][mock {kept}]x = 'naïve'[end][ls {{}}]"
        );
        assert_eq!(content, calls);

        let (finished, content, _) = parsed(
            Kind::Responses,
            &[
                r#"data: {"type":"response.output_item.added","item":{"type":"function_call","call_id":"call_a","name":"eval","arguments":""}}"#,
                r#"data: {"type":"response.function_call_arguments.delta","delta":"{\"code\": \"pri"}"#,
                r#"data: {"type":"response.output_item.done","item":{"type":"function_call","call_id":"call_a","name":"eval","arguments":"{\"code\": \"pri","status":"incomplete"}}"#,
                r#"data: {"type":"response.incomplete","response":{"status":"incomplete","incomplete_details":{"reason":"max_output_tokens"}}}"#,
            ],
        );
        assert_eq!(finished.expect("parse").finish, Finish::Length);
        let kept = json!({ "type": "function_call", "status": "incomplete" });
        assert_eq!(content, format!("pri[mock {kept}]"));

        let (finished, content, _) = parsed(
            Kind::Responses,
            &[
                r#"data: {"type":"response.output_text.delta","delta":"Crème"}"#,
                r#"data: {"type":"response.incomplete","response":{"incomplete_details":{"reason":"content_filter"}}}"#,
            ],
        );
        let filtered = Finish::Other("content_filter".to_owned());
        assert_eq!(finished.expect("parse").finish, filtered);
        assert_eq!(content, "Crème");
    }

    #[test]
    fn stream_failures() {
        let failed = |line: &str| parsed(Kind::Responses, &[line]).0.expect_err("error");

        let overloaded = failed(
            r#"data: {"type":"response.failed","response":{"status":"failed","error":{"code":"server_error","message":"The café is\n  overloaded"}}}"#,
        );
        assert_eq!(overloaded.message, "stream: The café is overloaded");
        assert!(overloaded.transient);

        let refused = failed(
            r#"data: {"type":"response.failed","response":{"error":{"code":"invalid_prompt","message":"Bad résumé"}}}"#,
        );
        assert_eq!(refused.message, "stream: Bad résumé");
        assert!(!refused.transient);

        let flat = failed(
            r#"data: {"type":"error","code":"rate_limit_exceeded","message":"Slow down, naïve"}"#,
        );
        assert_eq!(flat.message, "stream: Slow down, naïve");
        assert!(flat.transient);

        let nested = failed(
            r#"data: {"type":"error","error":{"code":"invalid_image","message":"Not a café photo"}}"#,
        );
        assert_eq!(nested.message, "stream: Not a café photo");
        assert!(!nested.transient);

        let partial = r#"data: {"type":"response.output_text.delta","delta":"Half a sentence"}"#;
        let cut = parsed(Kind::Responses, &[partial]).0.expect_err("cut");
        assert_eq!(cut.message, "stream: ended early");
        assert!(cut.transient);
    }

    #[test]
    fn request_sent() {
        let dir = temp_dir("responses");
        let picture = dir.join("chart.png");
        write_file(&picture, b"png");
        let media = Media {
            file: picture,
            ..png("chart.png")
        };
        let viewed = Part::Media("chart.png".to_owned(), media);

        let call = |id: &str, code: &str| ToolCall::eval(id.to_owned(), code);
        let theirs = Sealed {
            provider: "other".to_owned(),
            items: vec![
                json!({ "type": "reasoning", "id": "rs_1", "encrypted_content": "gAAAA-naïve" }),
            ],
        };
        let conversation = [
            ChatMessage::System("Be brief in the café".to_owned()),
            ChatMessage::User(vec![Part::Text("Look at this".to_owned()), viewed.clone()]),
            ChatMessage::user("And the résumé"),
            ChatMessage::Assistant {
                text: "Let me look.".to_owned(),
                reasoning: Some("Run it first".to_owned()),
                sealed: Some(theirs),
                calls: vec![call("call_1", "print('café')")],
            },
            ChatMessage::Tool {
                id: "call_1".to_owned(),
                parts: vec![Part::Text("café".to_owned())],
            },
            ChatMessage::Assistant {
                text: String::new(),
                reasoning: None,
                sealed: None,
                calls: vec![
                    call("call_2", "view('chart.png')"),
                    ToolCall::unread("call_3".to_owned(), "functions.résumé"),
                ],
            },
            ChatMessage::Tool {
                id: "call_2".to_owned(),
                parts: vec![Part::Text("[no output]".to_owned()), viewed],
            },
            ChatMessage::Tool {
                id: "call_3".to_owned(),
                parts: vec![Part::Text("No such tool".to_owned())],
            },
        ];

        let spec = ModelSpec {
            effort: Some("high".to_owned()),
            summary: Some("auto".to_owned()),
            max_tokens: Some(65536),
            image: true,
            ..ModelSpec::default()
        };
        let (finished, body) = served(Kind::Responses, DONE, &spec, &conversation, Purpose::Step);
        finished.expect("finished");

        let tag = "<image path=\"chart.png\"/>";
        let url = "data:image/png;base64,cG5n";
        let image = json!({ "type": "input_image", "image_url": url, "detail": "auto" });
        let expected = json!([
            { "role": "system", "content": "Be brief in the café" },
            { "role": "user", "content": [
                { "type": "input_text", "text": format!("Look at this\n{tag}") },
                image,
                { "type": "input_text", "text": "And the résumé" },
            ]},
            { "role": "assistant", "content": "Let me look." },
            { "type": "function_call", "call_id": "call_1", "name": "eval", "arguments": "{\"code\":\"print('café')\"}" },
            { "type": "function_call_output", "call_id": "call_1", "output": "café" },
            { "type": "function_call", "call_id": "call_2", "name": "eval", "arguments": "{\"code\":\"view('chart.png')\"}" },
            { "type": "function_call", "call_id": "call_3", "name": "functions_r_sum_", "arguments": "{}" },
            { "type": "function_call_output", "call_id": "call_2", "output": [
                { "type": "input_text", "text": format!("[no output]\n{tag}") },
                image,
            ]},
            { "type": "function_call_output", "call_id": "call_3", "output": "No such tool" },
        ]);
        assert_eq!(body["input"], expected);
        assert_eq!(
            body["reasoning"],
            json!({ "effort": "high", "summary": "auto" })
        );
        assert_eq!(body["max_output_tokens"], 65536);
        assert_eq!(body["store"], false);
        assert!(body.get("include").is_none(), "{body}");
        let tool = json!({
            "type": "function",
            "name": "eval",
            "description": TOOL_DOC,
            "parameters": body["tools"][0]["parameters"],
            "strict": false,
        });
        assert_eq!(body["tools"], json!([tool]));
        assert_eq!(body["tools"][0]["parameters"]["required"][0], "code");
        assert!(body.get("messages").is_none(), "{body}");

        let greeting = [ChatMessage::user("Hello")];
        let (_, summary) = served(Kind::Responses, DONE, &spec, &greeting, Purpose::Summary);
        assert_eq!(summary["reasoning"], json!({ "effort": "high" }));
        assert!(summary.get("tools").is_none(), "{summary}");
        let (_, title) = served(Kind::Responses, DONE, &spec, &greeting, Purpose::Title);
        assert!(title.get("reasoning").is_none(), "{title}");
        assert_eq!(
            title["input"],
            json!([{ "role": "user", "content": "Hello" }])
        );
        let plain = ModelSpec::default();
        let (_, step) = served(Kind::Responses, DONE, &plain, &greeting, Purpose::Step);
        assert!(step.get("reasoning").is_none(), "{step}");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn items_replayed() {
        let thought = |id: &str| json!({ "type": "reasoning", "id": id, "summary": [], "encrypted_content": "gAAAA-café" });
        let said = |id: &str| json!({ "type": "message", "id": id, "role": "assistant", "status": "completed", "phase": "commentary" });
        let called = |id: &str| json!({ "type": "function_call", "id": id, "status": "completed" });
        let call = |id: &str, code: &str| ToolCall::eval(id.to_owned(), code);
        let printed = |id: &str| ChatMessage::Tool {
            id: id.to_owned(),
            parts: vec![Part::Text("café".to_owned())],
        };
        let items = vec![
            thought("rs_1"),
            said("msg_1"),
            called("fc_1"),
            thought("rs_2"),
            said("msg_2"),
            called("fc_2"),
            thought("rs_3"),
        ];
        // A reply interrupted before its message and call finished
        let interrupted = vec![thought("rs_4")];
        let conversation = [
            ChatMessage::user("Count the cafés"),
            ChatMessage::Assistant {
                text: "Counting both, then the naïve way.".to_owned(),
                reasoning: None,
                sealed: Some(Sealed {
                    provider: "mock".to_owned(),
                    items,
                }),
                calls: vec![call("call_1", "print(1)"), call("call_2", "print(2)")],
            },
            printed("call_1"),
            printed("call_2"),
            ChatMessage::Assistant {
                text: "Once more for the résumé.".to_owned(),
                reasoning: None,
                sealed: Some(Sealed {
                    provider: "mock".to_owned(),
                    items: interrupted,
                }),
                calls: vec![call("call_3", "print(3)")],
            },
            printed("call_3"),
        ];

        let spec = ModelSpec::default();
        let (_, body) = served(Kind::Responses, DONE, &spec, &conversation, Purpose::Step);
        let text = json!([{ "type": "output_text", "text": "Counting both, then the naïve way.", "annotations": [] }]);
        let output =
            |id: &str| json!({ "type": "function_call_output", "call_id": id, "output": "café" });
        let expected = json!([
            { "role": "user", "content": "Count the cafés" },
            thought("rs_1"),
            { "type": "message", "id": "msg_1", "role": "assistant", "status": "completed", "phase": "commentary", "content": text },
            { "type": "function_call", "id": "fc_1", "status": "completed", "call_id": "call_1", "name": "eval", "arguments": "{\"code\":\"print(1)\"}" },
            thought("rs_2"),
            { "type": "function_call", "id": "fc_2", "status": "completed", "call_id": "call_2", "name": "eval", "arguments": "{\"code\":\"print(2)\"}" },
            output("call_1"),
            output("call_2"),
            thought("rs_4"),
            { "role": "assistant", "content": "Once more for the résumé." },
            { "type": "function_call", "call_id": "call_3", "name": "eval", "arguments": "{\"code\":\"print(3)\"}" },
            output("call_3"),
        ]);
        assert_eq!(body["input"], expected);
    }
}
