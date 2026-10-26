pub(crate) fn unserializable(key: &str, kind: &str) -> String {
    format!("{key}: {kind} is not JSON-serializable")
}
