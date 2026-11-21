use crate::model::{
    error_text, grouped, merged, tool_parameters, Call, ChatMessage, Finish, ModelError,
    OpenAIModel, Purpose, Stream, StreamDelta, TokenUsage,
};
use crate::prompt::{self, TOOL};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::mem;

/// Media goes in the tool output that shows it, which this API takes
pub(crate) fn body(
    model: &OpenAIModel,
    conversation: &[ChatMessage],
    purpose: Purpose,
) -> Map<String, Value> {
    let mut input = Vec::new();
    // Calls that go back under the ids their provider gave
    let mut renamed = HashMap::new();
    for group in grouped(conversation) {
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
                let (items, ends) = match model.opens(sealed.as_ref()) {
                    Some(sealed) => (&sealed.items[..], &sealed.ends[..]),
                    None => (&[][..], &[][..]),
                };
                let last = items.iter().rposition(|item| item["type"] == "message");
                let mut placed = 0;
                let mut calls = calls.iter();
                for (index, kept) in items.iter().enumerate() {
                    let mut item = kept.clone();
                    match kept["type"].as_str() {
                        Some("message") => {
                            // Items saved without ends give the first message all the text
                            let end = match ends.get(index) {
                                Some(&end) if Some(index) != last => end.clamp(placed, text.len()),
                                _ => text.len(),
                            };
                            let said = text.get(placed..end).unwrap_or_default().trim();
                            placed = end;
                            if said.is_empty() {
                                continue;
                            }

                            let part =
                                json!({ "type": "output_text", "text": said, "annotations": [] });
                            item["content"] = json!([part]);
                        }
                        Some("function_call") => {
                            let Some(call) = calls.next() else {
                                continue;
                            };
                            // Items saved before kept no id
                            match kept["call_id"].as_str() {
                                Some(id) => {
                                    renamed.insert(call.id.as_str(), id);
                                }
                                None => item["call_id"] = json!(call.id),
                            }
                            item["name"] = json!(call.name);
                            item["arguments"] = json!(call.arguments);
                        }
                        _ => {}
                    }
                    input.push(item);
                }

                let rest = text.get(placed..).unwrap_or_default().trim();
                if !rest.is_empty() {
                    let message = json!({ "role": "assistant", "content": rest });
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
                let id = renamed.get(id.as_str()).copied().unwrap_or(id);
                let item =
                    json!({ "type": "function_call_output", "call_id": id, "output": output });
                input.push(item);
            }
        }
    }

    let mut reasoning = Map::new();
    if let Some(effort) = model.effort_for(purpose) {
        reasoning.insert("effort".into(), json!(effort));
    }
    // Only steps show the reasoning
    let summary = model.spec.summary.as_ref();
    if let Some(summary) = summary.filter(|_| purpose == Purpose::Step) {
        reasoning.insert("summary".into(), json!(summary));
    }

    let mut body = Map::new();
    body.insert("model".into(), json!(model.spec.id));
    if !reasoning.is_empty() {
        body.insert("reasoning".into(), Value::Object(reasoning));
    }
    body.insert("input".into(), Value::Array(input));
    if let Some(max_tokens) = model.spec.max_tokens {
        body.insert("max_output_tokens".into(), json!(max_tokens));
    }
    body.insert("stream".into(), json!(true));
    body.insert("store".into(), json!(false));
    if purpose == Purpose::Step {
        // With nothing stored, a server reads the reasoning items it gets back only from their
        // encrypted content
        let sealed = json!(["reasoning.encrypted_content"]);
        body.insert("include".into(), sealed);

        let parameters = tool_parameters();
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
                // The provider finds what led to a call by the id it gave, which goes back
                Some("function_call") => &["name", "arguments"],
                _ => &[],
            };
            let mut kept = item.clone();
            if let Some(fields) = kept.as_object_mut() {
                for field in held {
                    fields.remove(*field);
                }
            }
            self.seal_item(kept, on_delta);
        }

        let paragraphs = &mut self.paragraphs;
        match kind {
            // Each message, reasoning item and summary part gets a paragraph of its own
            "response.output_item.added" if item["type"] == "message" => paragraphs.opened = true,
            "response.output_text.delta" | "response.refusal.delta" if !delta.is_empty() => {
                let parted = mem::take(&mut paragraphs.opened) && paragraphs.said;
                paragraphs.said = true;
                if parted {
                    self.say("\n\n", on_delta);
                }
                self.say(delta, on_delta);
            }
            "response.reasoning_summary_part.added" => paragraphs.parted = true,
            "response.output_item.added" if item["type"] == "reasoning" => paragraphs.parted = true,
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta"
                if !delta.is_empty() =>
            {
                if mem::take(&mut paragraphs.parted) && paragraphs.reasoned {
                    on_delta(StreamDelta::Reasoning("\n\n"));
                }
                paragraphs.reasoned = true;
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
    use crate::prompt::{self, TOOL_DOC};
    use crate::tests::{temp_dir, write_file};
    use serde_json::{json, Value};
    use std::fs;

    #[test]
    fn events_parsed() {
        let encrypted = json!({ "type": "reasoning", "id": "rs_1", "summary": [], "encrypted_content": "gAAAA-café" });
        let bare = json!({ "type": "reasoning", "id": "rs_2", "summary": [] });
        let said = json!({ "type": "message", "id": "msg_1", "role": "assistant", "status": "completed", "phase": "commentary", "content": [{ "type": "output_text", "text": "Hello, world 🍕" }] });
        let plain = json!({ "type": "reasoning", "id": "rs_3", "summary": [], "content": [{ "type": "reasoning_text", "text": "Check the résumé" }] });
        let answered = json!({ "type": "message", "id": "msg_2", "role": "assistant", "status": "completed", "phase": "final_answer", "content": [{ "type": "output_text", "text": "Bon appétit" }] });
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
                r#"data: {"type":"response.output_item.added","item":{"type":"message","id":"msg_2","role":"assistant","content":[]}}"#,
                r#"data: {"type":"response.output_text.delta","delta":"Bon appétit"}"#,
                &done(&answered),
                r#"data: {"type":"response.completed","response":{"status":"completed","usage":{"input_tokens":10,"output_tokens":2,"input_tokens_details":{"cached_tokens":8}}}}"#,
            ],
        );

        let mut kept = said.clone();
        kept.as_object_mut().expect("item").remove("content");
        let mut answer = answered.clone();
        answer.as_object_mut().expect("item").remove("content");
        let items = format!("[mock {encrypted}][mock {bare}]Hello, world 🍕[mock {kept}][mock {plain}]\n\nBon appétit[mock {answer}]");
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
    fn request_sent() {
        let dir = temp_dir("responses");
        let picture = dir.join("chart.png");
        write_file(&picture, b"png");
        let media = Media {
            file: picture,
            ..png("chart.png")
        };
        let tag = prompt::media_tag("chart.png", &media);
        let viewed = Part::Media("chart.png".to_owned(), media);

        let call = |id: &str, code: &str| ToolCall::eval(id.to_owned(), code);
        let theirs = Sealed {
            provider: "other".to_owned(),
            kind: Kind::Responses,
            items: vec![
                json!({ "type": "reasoning", "id": "rs_1", "encrypted_content": "gAAAA-naïve" }),
            ],
            ends: Vec::new(),
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
                parts: vec![Part::Text(prompt::NO_OUTPUT.to_owned()), viewed],
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
                { "type": "input_text", "text": format!("{}\n{tag}", prompt::NO_OUTPUT) },
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
        assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
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
        assert!(summary.get("include").is_none(), "{summary}");
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
        // The first call kept the id its provider gave, as items saved before kept none
        let mut given = called("fc_1");
        given["call_id"] = json!("call_00_xy");
        let items = vec![
            thought("rs_1"),
            said("msg_1"),
            given,
            thought("rs_2"),
            said("msg_2"),
            called("fc_2"),
            thought("rs_3"),
        ];
        let first = "Counting both.";
        let text = format!("{first}\n\nThen the naïve way.");
        let (spoken, whole) = (first.len(), text.len());
        let ends = vec![0, spoken, spoken, spoken, whole, whole, whole];
        // A reply interrupted before its message and call finished
        let interrupted = vec![thought("rs_4")];
        let conversation = [
            ChatMessage::user("Count the cafés"),
            ChatMessage::Assistant {
                text: text.clone(),
                reasoning: None,
                sealed: Some(Sealed {
                    provider: "mock".to_owned(),
                    kind: Kind::Responses,
                    items,
                    ends,
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
                    kind: Kind::Responses,
                    items: interrupted,
                    ends: vec![0],
                }),
                calls: vec![call("call_3", "print(3)")],
            },
            printed("call_3"),
        ];

        let spec = ModelSpec::default();
        let (_, body) = served(Kind::Responses, DONE, &spec, &conversation, Purpose::Step);
        let said_first = json!([{ "type": "output_text", "text": first, "annotations": [] }]);
        let said_then =
            json!([{ "type": "output_text", "text": "Then the naïve way.", "annotations": [] }]);
        let output =
            |id: &str| json!({ "type": "function_call_output", "call_id": id, "output": "café" });
        let expected = json!([
            { "role": "user", "content": "Count the cafés" },
            thought("rs_1"),
            { "type": "message", "id": "msg_1", "role": "assistant", "status": "completed", "phase": "commentary", "content": said_first },
            { "type": "function_call", "id": "fc_1", "status": "completed", "call_id": "call_00_xy", "name": "eval", "arguments": "{\"code\":\"print(1)\"}" },
            thought("rs_2"),
            { "type": "message", "id": "msg_2", "role": "assistant", "status": "completed", "phase": "commentary", "content": said_then },
            { "type": "function_call", "id": "fc_2", "status": "completed", "call_id": "call_2", "name": "eval", "arguments": "{\"code\":\"print(2)\"}" },
            output("call_00_xy"),
            output("call_2"),
            thought("rs_4"),
            { "role": "assistant", "content": "Once more for the résumé." },
            { "type": "function_call", "call_id": "call_3", "name": "eval", "arguments": "{\"code\":\"print(3)\"}" },
            output("call_3"),
        ]);
        assert_eq!(body["input"], expected);
    }
}
