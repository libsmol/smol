use crate::model::{
    error_text, merged, retryable, tool_parameters, Call, ChatMessage, Finish, ModelError,
    OpenAIModel, Purpose, Stream, StreamDelta, TokenUsage,
};
use crate::prompt::{self, TOOL};
use serde_json::{json, Map, Value};
use std::mem;

pub(crate) fn body(
    model: &OpenAIModel,
    conversation: &[ChatMessage],
    purpose: Purpose,
) -> Map<String, Value> {
    let mut sent = Vec::new();
    // Tool messages can't hold images, which go in a user message after all the step's replies
    let mut attached = Vec::new();
    let together = |a: &ChatMessage, b: &ChatMessage| {
        matches!((a, b), (ChatMessage::User(_), ChatMessage::User(_)))
    };
    for group in conversation.chunk_by(together) {
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
                calls,
                ..
            } => {
                let content = match text.is_empty() && !calls.is_empty() {
                    true => Value::Null,
                    false => json!(text),
                };
                let mut message = json!({ "role": "assistant", "content": content });
                if let Some(reasoning) = reasoning {
                    message["reasoning_content"] = json!(reasoning);
                }

                let mut called = Vec::new();
                for call in calls {
                    let function = json!({ "name": call.name, "arguments": call.arguments });
                    let call = json!({ "id": call.id, "type": "function", "function": function });
                    called.push(call);
                }
                if !called.is_empty() {
                    message["tool_calls"] = Value::Array(called);
                }
                message
            }
            ChatMessage::Tool { id, parts } => {
                let content = model.content(&[parts.as_slice()], Some(&mut attached));
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
    body.insert("model".into(), json!(model.id));
    body.insert("messages".into(), Value::Array(sent));
    if let Some(max_tokens) = model.max_tokens {
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
        let content = delta["content"].as_str().filter(|text| !text.is_empty());
        let deltas = [
            reasoning.map(StreamDelta::Reasoning),
            content.map(StreamDelta::Content),
        ];
        for streamed in deltas.into_iter().flatten() {
            on_delta(streamed);
        }

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
                self.add_arguments(arguments, on_delta);
            }
        }

        if self.finish.is_some() {
            self.hand_over(on_delta);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::config::{Kind, ModelSpec};
    use crate::model::tests::{hello, parsed, png, served, DONE};
    use crate::model::{
        ChatMessage, Finish, Media, MediaKind, Part, Purpose, Stream, StreamDelta, ToolCall,
    };
    use crate::tests::{temp_dir, write_file};
    use serde_json::json;
    use std::fs;
    use std::path::PathBuf;

    #[test]
    fn media_parts() {
        let dir = temp_dir("parts");
        let picture = dir.join("chart.png");
        write_file(&picture, b"png");
        let media = |label: &str, kind, mime: &str, file: PathBuf| {
            let mime = mime.to_owned();
            let media = Media {
                kind,
                mime,
                file,
                from_call: false,
            };
            Part::Media(label.to_owned(), media)
        };
        let image = media("chart.png", MediaKind::Image, "image/png", picture);
        let video = media(
            "clip.mp4",
            MediaKind::Video,
            "video/mp4",
            dir.join("clip.mp4"),
        );
        let gone = media(
            "old.png",
            MediaKind::Image,
            "image/png",
            dir.join("gone.png"),
        );

        let first = ChatMessage::User(vec![Part::Text("Look at this".to_owned()), image]);
        let second = ChatMessage::User(vec![Part::Text("And a video".to_owned()), video, gone]);
        let conversation = [first, second];

        let image = ModelSpec {
            image: true,
            ..ModelSpec::default()
        };
        let (finished, body) = served(
            Kind::Completions,
            DONE,
            &image,
            &conversation,
            Purpose::Step,
        );
        finished.expect("finished");
        let expected = json!([
            {"type": "text", "text": "Look at this\n<image path=\"chart.png\"/>"},
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,cG5n"}},
            {"type": "text", "text": "And a video\n[video clip.mp4: not shown, the model takes no videos]\n[image old.png: not shown, its copy was deleted]"},
        ]);
        assert_eq!(body["messages"][0]["content"], expected);

        let text = ModelSpec::default();
        let (_, unseen) = served(Kind::Completions, DONE, &text, &conversation, Purpose::Step);
        let expected = "Look at this\n[image chart.png: not shown, the model takes no images]\n\n\
            And a video\n[video clip.mp4: not shown, the model takes no videos]\n\
            [image old.png: not shown, the model takes no images]";
        assert_eq!(unseen["messages"][0]["content"], expected);

        let both = ModelSpec {
            video: true,
            ..image
        };
        let greeting = hello();
        let (_, plain) = served(Kind::Completions, DONE, &both, &greeting, Purpose::Step);
        assert_eq!(plain["messages"][0]["content"], "Hello");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn request_options() {
        let conversation = hello();
        let spec = ModelSpec {
            effort: Some("max".to_owned()),
            ..ModelSpec::default()
        };
        let (_, step) = served(Kind::Completions, DONE, &spec, &conversation, Purpose::Step);
        assert_eq!(step["reasoning_effort"], "max");
        assert!(step.get("max_tokens").is_none(), "{step}");
        let function = &step["tools"][0]["function"];
        assert_eq!(function["name"], "eval");
        assert_eq!(function["parameters"]["required"][0], "code");

        let (_, summary) = served(
            Kind::Completions,
            DONE,
            &spec,
            &conversation,
            Purpose::Summary,
        );
        assert_eq!(summary["reasoning_effort"], "max");
        assert!(summary.get("tools").is_none(), "{summary}");

        let (_, title) = served(
            Kind::Completions,
            DONE,
            &spec,
            &conversation,
            Purpose::Title,
        );
        assert!(title.get("reasoning_effort").is_none(), "{title}");
        assert!(title.get("tools").is_none(), "{title}");

        let capped = ModelSpec {
            max_tokens: Some(65536),
            ..spec
        };
        let (_, capped) = served(
            Kind::Completions,
            DONE,
            &capped,
            &conversation,
            Purpose::Step,
        );
        assert_eq!(capped["max_tokens"], 65536);
    }

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
        let viewed = Part::Media("chart.png".to_owned(), media);
        let quiet = Part::Text("[no output]".to_owned());
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
            {"role": "tool", "content": "[no output]\n<image path=\"chart.png\"/>", "tool_call_id": "call_2"},
            {"role": "tool", "content": "<image path=\"chart.png\"/>", "tool_call_id": "call_3"},
            {"role": "tool", "content": "No such tool", "tool_call_id": "call_4"},
            {"role": "tool", "content": "No such tool", "tool_call_id": "call_5"},
            {"role": "user", "content": [
                {"type": "text", "text": "<image path=\"chart.png\"/>"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,cG5n"}},
                {"type": "text", "text": "<image path=\"chart.png\"/>"},
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
    fn stream_parsing() {
        let (finished, content, reasoning) = parsed(
            Kind::Completions,
            &[
                r#"data: {"choices":[{"delta":{"role":"assistant","reasoning_content":"Think about the façade"}}]}"#,
                r#"data: {"choices":[{"delta":{"reasoning_content":"","reasoning":", then the piñata"}}]}"#,
                r#"data: {"choices":[{"delta":{"content":"Hello"}}],"usage":null,"error":null}"#,
                r#"data: {"choices":[{"delta":{"content":", world 🍕"}}]}"#,
                r#"data: {"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":2,"prompt_tokens_details":{"cached_tokens":8}}}"#,
                "data: [DONE]\r",
            ],
        );

        assert_eq!(content, "Hello, world 🍕");
        assert_eq!(reasoning, "Think about the façade, then the piñata");

        let finished = finished.expect("parse");
        let usage = finished.usage.expect("usage");
        assert_eq!((usage.input_tokens, usage.output_tokens), (10, 2));
        assert_eq!(usage.cached_tokens, Some(8));
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
        let filtered = Finish::Other("content_filter".to_owned());
        assert_eq!(finished.expect("parse").finish, filtered);
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
    fn stream_endings() {
        let partial = r#"data: {"choices":[{"delta":{"content":"Half a sentence"}}]}"#;
        let cut = parsed(Kind::Completions, &[partial]).0.expect_err("cut");
        assert_eq!(cut.message, "stream: ended early");
        assert!(cut.transient);
    }

    #[test]
    fn stream_errors() {
        let failed = |line: &str| parsed(Kind::Completions, &[line]).0.expect_err("error");

        let upstream = failed(r#"data: {"error":{"message":"Upstream\n  overloaded","code":502}}"#);
        assert_eq!(upstream.message, "stream: Upstream overloaded");
        assert!(upstream.transient);

        let bare = failed(r#"data: {"error":"overloaded"}"#);
        assert_eq!(bare.message, "stream: overloaded");
        assert!(bare.transient);

        let refused = failed(r#"data: {"error":{"message":"bad","code":400}}"#);
        assert!(!refused.transient);

        let cut = failed(r#"data: {"choices":[{"delta":{"content":"Hal"#);
        assert_eq!(cut.message, "stream: ended mid-event");
        assert!(cut.transient);

        let garbled = failed("data: {proxy notice: try again later}");
        assert_eq!(
            garbled.message,
            "stream: not JSON: {proxy notice: try again later}"
        );
        assert!(garbled.transient);
    }

    #[test]
    fn empty_reasoning_kept() {
        let lines = [
            r#"data: {"choices":[{"delta":{"reasoning_content":"","reasoning":"","content":"Sure"}}]}"#,
            r#"data: {"choices":[{"delta":{"reasoning_content":"","reasoning":"Hmm"}}]}"#,
            r#"data: {"choices":[{"delta":{"reasoning_content":null,"content":" thing"}}]}"#,
        ];
        let lines = lines.join("\n") + "\n";
        let mut thoughts = Vec::new();
        let mut on_delta = |delta: StreamDelta| {
            if let StreamDelta::Reasoning(text) = delta {
                thoughts.push(text.to_owned());
            }
        };

        let mut stream = Stream::new(Kind::Completions, "mock/m1");
        stream.feed(lines.as_bytes(), &mut on_delta).expect("feed");
        assert_eq!(thoughts, ["", "Hmm"]);
    }
}
