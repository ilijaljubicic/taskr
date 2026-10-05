use serde_json::Value;

/// A rejection reported by the herdr CLI (JSON error envelope or usage text).
#[derive(Debug, Clone)]
pub struct HerdrCliError {
    pub code: Option<String>,
    pub message: String,
}

impl HerdrCliError {
    pub fn new(code: Option<String>, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for HerdrCliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.code {
            Some(code) => write!(f, "herdr error {code}: {}", self.message),
            None => write!(f, "herdr error: {}", self.message),
        }
    }
}

/// Parse herdr command output.
///
/// Socket-API commands print a JSON envelope (`{"id":..,"result":{..}}` on
/// success, `{"error":{"code":..,"message":..}}` on failure). Some
/// client-side commands (`machine list --json`) print bare JSON. Text reads
/// print raw terminal text and are handled by the caller.
pub fn parse_envelope(stdout: &str, stderr: &str, success: bool) -> Result<Value, HerdrCliError> {
    // Herdr's CLI writes API rejection envelopes to stderr on a failed exit.
    // Preserve their codes so missing resources are distinguished from an
    // unavailable endpoint or an uncertain command outcome.
    if !success {
        for text in [stdout, stderr] {
            if let Ok(value) = serde_json::from_str::<Value>(text.trim()) {
                if let Some(error) = value.get("error") {
                    return Err(HerdrCliError::new(
                        error.get("code").and_then(Value::as_str).map(str::to_owned),
                        error
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("herdr reported an error"),
                    ));
                }
            }
        }
        return Err(HerdrCliError::new(
            None,
            format!(
                "herdr command failed: {} {}",
                truncate_for_message(stdout.trim()),
                truncate_for_message(stderr.trim())
            ),
        ));
    }
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Err(HerdrCliError::new(
            None,
            format!(
                "herdr produced no JSON output{}",
                if stderr.trim().is_empty() {
                    String::new()
                } else {
                    format!("; stderr: {}", stderr.trim())
                }
            ),
        ));
    }
    match serde_json::from_str::<Value>(trimmed) {
        Ok(Value::Object(object)) => {
            if let Some(error) = object.get("error") {
                let code = error.get("code").and_then(Value::as_str).map(str::to_owned);
                let message = error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("herdr reported an error")
                    .to_owned();
                return Err(HerdrCliError::new(code, message));
            }
            if let Some(result) = object.get("result") {
                return Ok(result.clone());
            }
            Ok(Value::Object(object))
        }
        Ok(value) => Ok(value),
        Err(_) => Err(HerdrCliError::new(
            None,
            format!(
                "herdr produced unparseable output: {}{}",
                truncate_for_message(trimmed),
                if stderr.trim().is_empty() {
                    String::new()
                } else {
                    format!("; stderr: {}", truncate_for_message(stderr.trim()))
                }
            ),
        )),
    }
    .map_err(|error| {
        if success {
            error
        } else {
            // Non-zero exit without a JSON error envelope (usage errors and
            // similar) still carries the CLI's own text.
            HerdrCliError::new(
                None,
                format!(
                    "{}{}",
                    truncate_for_message(trimmed),
                    if stderr.trim().is_empty() {
                        String::new()
                    } else {
                        format!(
                            "{}; stderr: {}",
                            if trimmed.is_empty() { "" } else { "; " },
                            truncate_for_message(stderr.trim())
                        )
                    }
                ),
            )
        }
    })
}

fn truncate_for_message(text: &str) -> String {
    const LIMIT: usize = 400;
    if text.len() <= LIMIT {
        text.to_owned()
    } else {
        let mut end = LIMIT;
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}...", &text[..end])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_success_envelope() {
        let value = parse_envelope(
            r#"{"id":"cli:pane:get","result":{"type":"pane_info","pane":{"pane_id":"w1:p1"}}}"#,
            "",
            true,
        )
        .unwrap();
        assert_eq!(value["type"], "pane_info");
        assert_eq!(value["pane"]["pane_id"], "w1:p1");
    }

    #[test]
    fn parses_bare_json_results() {
        let value = parse_envelope("[{\"name\":\"box\"}]", "", true).unwrap();
        assert_eq!(value[0]["name"], "box");
    }

    #[test]
    fn parses_error_envelope() {
        let error = parse_envelope(
            r#"{"error":{"code":"pane_not_found","message":"pane w5:p1 not found"},"id":"cli:pane:get"}"#,
            "",
            false,
        )
        .unwrap_err();
        assert_eq!(error.code.as_deref(), Some("pane_not_found"));
        assert!(error.message.contains("w5:p1"));
    }

    #[test]
    fn failed_exit_preserves_native_error_envelopes_written_to_stderr() {
        let stderr = r#"{"error":{"code":"agent_not_found","message":"agent target worker not found"},"id":"cli:agent:get"}"#;
        for stdout in ["", "progress output", r#"{"result":{"stale":true}}"#] {
            let error = parse_envelope(stdout, stderr, false).unwrap_err();
            assert_eq!(error.code.as_deref(), Some("agent_not_found"));
            assert_eq!(error.message, "agent target worker not found");
        }
    }

    #[test]
    fn nonzero_exit_without_error_envelope_is_never_a_successful_result() {
        let error =
            parse_envelope(r#"{"result":{"stale":true}}"#, "connection lost", false).unwrap_err();
        assert!(error.code.is_none());
        assert!(error.message.contains("connection lost"));
    }

    #[test]
    fn usage_errors_fall_back_to_text() {
        let error = parse_envelope("", "usage: herdr workspace list", false).unwrap_err();
        assert!(error.code.is_none());
        assert!(error.message.contains("usage:"));
    }

    #[test]
    fn empty_output_is_an_error() {
        assert!(parse_envelope("", "", true).is_err());
        assert!(parse_envelope("   \n", "", true).is_err());
        let error = parse_envelope("not json", "", true).unwrap_err();
        assert!(error.code.is_none());
        assert!(error.message.contains("unparseable output"), "{error}");
    }
}
