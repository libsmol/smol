use crate::config::{self, parse_url, MIN_CONTEXT, MODEL_FILE};
use crate::model::{listed, ListError, ModelSpec, DEFAULT_API};
use crate::utils::block_on;
use crate::worker::python::{Runtime, PYTHON};
use crate::{Error, Result};
use inquire::error::CustomUserError;
use inquire::validator::Validation;
use inquire::{Confirm, InquireError, Password, PasswordDisplayMode, Select, Text};
use reaper::{cause, tilde};
use reqwest::Url;
use std::fmt;
use std::io::{self, IsTerminal};

pub const SETUP_ARG: &str = "setup";
const PAGE_SIZE: usize = 10;

#[derive(Clone, Copy, PartialEq)]
enum Ask {
    Address,
    Key,
    Id,
}

impl fmt::Display for Ask {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Ask::Address => "Enter the base URL again",
            Ask::Key => "Enter the API key again",
            Ask::Id => "Type the model id",
        };
        f.write_str(text)
    }
}

pub fn run() -> Result<()> {
    // The prompts draw on stderr
    if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
        let problem = "stdin and stderr: not both a terminal".to_owned();
        return Err(Error::Setup(problem));
    }

    let dir = config::dir();
    let path = dir.join(MODEL_FILE);
    let shown = tilde(&path);
    eprintln!("Set up a model in {shown}");

    if path.exists() {
        let question = format!("{shown} exists. Replace it?");
        let replace = Confirm::new(&question)
            .with_default(false)
            .prompt()
            .map_err(failed)?;
        if !replace {
            return Ok(());
        }
    }

    let mut api = String::new();
    let mut key = String::new();
    let mut next = Ask::Address;
    let mut spec = loop {
        match next {
            Ask::Address => {
                let question = Text::new("API base URL")
                    .with_help_message("Any OpenAI-compatible chat completions endpoint")
                    .with_validator(validator(parse_url));
                // A prefilled value can only be erased key by key
                let question = match api.is_empty() {
                    true => question.with_default(DEFAULT_API),
                    false => question.with_initial_value(&api),
                };
                let answer = question.prompt().map_err(failed)?;
                api = answer.trim().to_owned();
                next = Ask::Key;
            }

            Ask::Key => {
                let masked = |key: &str| match key.trim().is_empty() {
                    true => "(none)".to_owned(),
                    false => "********".to_owned(),
                };
                let answer = Password::new("API key")
                    .with_display_mode(PasswordDisplayMode::Masked)
                    .with_display_toggle_enabled()
                    .with_formatter(&masked)
                    .with_help_message(
                        "Ctrl+R shows it, leave it empty if the endpoint takes no key",
                    )
                    .without_confirmation()
                    .prompt()
                    .map_err(failed)?;
                key = answer.trim().to_owned();

                let listing = listed(&api, &key);
                let (message, suggested) = match block_on(listing) {
                    Ok(mut specs) => {
                        let ids: Vec<&str> = specs.iter().map(|spec| spec.id.as_str()).collect();
                        let chosen = Select::new("Model", ids)
                            .with_page_size(PAGE_SIZE)
                            .raw_prompt()
                            .map_err(failed)?;
                        let index = chosen.index;
                        break specs.swap_remove(index);
                    }
                    Err(ListError::Key(message)) => (message, Ask::Key),
                    Err(ListError::Address(message)) => (message, Ask::Address),
                    Err(ListError::Other(message)) => (message, Ask::Id),
                };

                eprintln!("{message}");
                let choices = vec![Ask::Address, Ask::Key, Ask::Id];
                let found = choices.iter().position(|choice| *choice == suggested);
                let cursor = found.unwrap_or_default();
                next = Select::new("What next?", choices)
                    .with_starting_cursor(cursor)
                    .prompt()
                    .map_err(failed)?;
            }

            Ask::Id => {
                let answer = Text::new("Model id")
                    .with_validator(validator(check_id))
                    .prompt()
                    .map_err(failed)?;
                let id = answer.trim().to_owned();
                break ModelSpec {
                    id,
                    ..ModelSpec::default()
                };
            }
        }
    };

    let suggested = default_name(&api);
    let answer = Text::new("Name")
        .with_initial_value(&suggested)
        .with_help_message("Models are named <name>/<model id>")
        .with_validator(validator(check_name))
        .prompt()
        .map_err(failed)?;
    let name = answer.trim().to_owned();

    let reported = spec.context.map(|context| context.to_string());
    let help = match reported {
        Some(_) => "Reported by the endpoint, older steps are summarized before it fills",
        None => "Older steps are summarized before it fills, leave it empty to never summarize",
    };
    let initial = reported.as_deref().unwrap_or_default();
    let answer = Text::new("Context window in tokens")
        .with_initial_value(initial)
        .with_help_message(help)
        .with_validator(validator(parse_context))
        .prompt()
        .map_err(failed)?;
    spec.context = parse_context(answer.trim()).unwrap_or_default();

    config::save_model(&dir, &name, &api, &key, &spec)?;
    eprintln!("Wrote {shown}, edit it to add models or set max_tokens, effort, image and video");

    // A session that installs it opens without a word until it is done, since ACP has no way
    // to tell the client before the reply
    eprintln!(
        "Getting CPython {PYTHON} ready for the code, a download of about 30 MB the first time"
    );
    let installing = Runtime::install(&dir);
    match block_on(installing) {
        Ok(runtime) => {
            let home = tilde(&runtime.home);
            eprintln!("Python is ready in {home}");
        }
        Err(problem) => eprintln!("{problem}, and the first session will try again"),
    }
    Ok(())
}

fn failed(error: InquireError) -> Error {
    let problem = match error {
        InquireError::OperationCanceled | InquireError::OperationInterrupted => {
            "cancelled".to_owned()
        }
        InquireError::IO(e) => cause(&e),
        other => other.to_string(),
    };
    Error::Setup(problem)
}

/// Checks the answer with surrounding whitespace trimmed
fn validator<T: 'static>(
    check: fn(&str) -> Result<T, String>,
) -> impl Fn(&str) -> Result<Validation, CustomUserError> + Clone {
    move |input: &str| {
        let validation = match check(input.trim()) {
            Ok(_) => Validation::Valid,
            Err(problem) => Validation::Invalid(problem.into()),
        };
        Ok(validation)
    }
}

fn check_id(id: &str) -> Result<(), String> {
    match id.is_empty() {
        true => Err("can't be empty".to_owned()),
        false => Ok(()),
    }
}

fn check_name(name: &str) -> Result<(), String> {
    let problem = match name {
        "" => "can't be empty",
        name if name.contains('/') => "can't contain /, which separates it from the model id",
        _ => return Ok(()),
    };
    Err(problem.to_owned())
}

/// Accepts digit separators, since windows are often written as 128,000
fn parse_context(input: &str) -> Result<Option<u64>, String> {
    let digits = input.replace([',', '_'], "");
    if digits.is_empty() {
        return Ok(None);
    }

    let Ok(tokens) = digits.parse::<u64>() else {
        return Err("not a whole number".to_owned());
    };
    if tokens < MIN_CONTEXT {
        return Err(format!("below {MIN_CONTEXT}"));
    }
    Ok(Some(tokens))
}

/// The label before the top-level domain, which is usually the provider's name
fn default_name(api: &str) -> String {
    let url = Url::parse(api).ok();
    let domain = url.as_ref().and_then(Url::domain).unwrap_or("localhost");
    let domain = domain.trim_end_matches('.');
    if domain == "localhost" {
        return "local".to_owned();
    }

    let labels: Vec<&str> = domain.split('.').collect();
    let at = labels.len().saturating_sub(2);
    labels[at].to_owned()
}

#[cfg(test)]
mod tests {
    use crate::config::setup::{check_name, default_name, parse_context};

    #[test]
    fn default_names() {
        let cases = [
            ("https://api.deepseek.com/v1", "deepseek"),
            ("https://openrouter.ai/api/v1", "openrouter"),
            (
                "https://dashscope.aliyuncs.com/compatible-mode/v1",
                "aliyuncs",
            ),
            ("http://localhost:11434/v1", "local"),
            ("http://127.0.0.1:8000/v1", "local"),
            ("http://[::1]:8000/v1", "local"),
            ("http://ollama:11434/v1", "ollama"),
            ("https://open.bigmodel.cn/api/paas/v4", "bigmodel"),
            ("https://example.com./v1", "example"),
            ("https://api.café.com/v1", "xn--caf-dma"),
        ];
        for (api, name) in cases {
            assert_eq!(default_name(api), name, "{api}");
        }
    }

    #[test]
    fn answers_checked() {
        assert_eq!(parse_context(""), Ok(None));
        assert_eq!(parse_context("128,000"), Ok(Some(128_000)));
        assert_eq!(parse_context("65_536"), Ok(Some(65_536)));
        let words = Err("not a whole number".to_owned());
        assert_eq!(parse_context("64k"), words);
        assert_eq!(parse_context("512"), Err("below 1024".to_owned()));

        assert_eq!(check_name("über café"), Ok(()));
        assert_eq!(check_name(""), Err("can't be empty".to_owned()));
        let slashed = check_name("café/v1").expect_err("problem");
        assert!(slashed.starts_with("can't contain /"), "{slashed}");
    }
}
