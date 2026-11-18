//! Secret redaction for log output.
//!
//! The specification demands that passwords, bearer tokens and base64 SASL
//! payloads can never reach a log file.  Redaction is therefore implemented as
//! a *formatting* layer: every event is rendered into a buffer, the buffer is
//! scrubbed by [`Redactor`] and only the scrubbed text is handed to the writer.
//!
//! The redactor fails **closed**: if a regular expression ever failed to
//! compile (which the unit tests below rule out for the shipped rule set) the
//! formatter replaces the complete event with a marker instead of leaking it.

use regex_lite::Regex;
use tracing::Subscriber;
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields};
use tracing_subscriber::registry::LookupSpan;

/// Marker that replaces a redacted value.
pub const REDACTED: &str = "[REDACTED]";

/// One search-and-replace rule.
#[derive(Debug)]
struct RedactionRule {
    /// Name used in diagnostics and tests.
    name: &'static str,
    /// Pattern to search for.
    pattern: Regex,
    /// Replacement, may reference capture groups.
    replacement: &'static str,
}

/// Stateless collection of redaction rules.
#[derive(Debug)]
pub struct Redactor {
    rules: Vec<RedactionRule>,
    fail_closed: bool,
}

impl Redactor {
    /// A redactor that keeps the text unchanged.
    ///
    /// Only useful for tests; production code always uses
    /// [`Redactor::standard`] or [`Redactor::default`].
    #[must_use]
    pub fn empty() -> Self {
        Self {
            rules: Vec::new(),
            fail_closed: false,
        }
    }

    /// A redactor that removes everything.
    ///
    /// Used as the fail-closed fallback when a rule cannot be compiled: it is
    /// always better to lose a log line than to leak a credential.
    #[must_use]
    pub fn fail_closed() -> Self {
        Self {
            rules: Vec::new(),
            fail_closed: true,
        }
    }

    /// Compiles the shipped rule set.
    pub fn standard() -> crate::error::Result<Self> {
        let mut rules = Vec::new();
        for (name, pattern, replacement) in STANDARD_RULES {
            let pattern = Regex::new(pattern).map_err(|error| {
                crate::error::Error::config(format!("redaction rule `{name}` is invalid: {error}"))
            })?;
            rules.push(RedactionRule {
                name,
                pattern,
                replacement,
            });
        }
        Ok(Self {
            rules,
            fail_closed: false,
        })
    }

    /// Number of active rules.
    #[must_use]
    pub fn rule_count(&self) -> usize {
        self.rules.len()
    }

    /// `true` when the redactor would not change anything.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty() && !self.fail_closed
    }

    /// `true` when the redactor removes every event entirely.
    #[must_use]
    pub fn is_fail_closed(&self) -> bool {
        self.fail_closed
    }

    /// Names of the active rules, in application order.
    #[must_use]
    pub fn rule_names(&self) -> Vec<&'static str> {
        self.rules.iter().map(|rule| rule.name).collect()
    }

    /// Scrubs every secret from `input`.
    #[must_use]
    pub fn redact(&self, input: &str) -> String {
        if self.fail_closed {
            return "[REDACTED: the redaction rule set failed to compile]".to_string();
        }
        let mut current = input.to_string();
        for rule in &self.rules {
            if rule.pattern.is_match(&current) {
                current = rule
                    .pattern
                    .replace_all(&current, rule.replacement)
                    .into_owned();
            }
        }
        current
    }
}

impl Default for Redactor {
    fn default() -> Self {
        match Self::standard() {
            Ok(redactor) => redactor,
            Err(error) => {
                // The rule set is covered by unit tests, so this branch is
                // unreachable in practice - but if it is ever reached we must
                // not log anything unredacted.
                // A redactor is built before any global subscriber exists, so
                // this cannot be routed through `tracing`.
                #[allow(clippy::print_stderr)]
                {
                    eprintln!("HylMail: redaction rule set is broken ({error}); failing closed");
                }
                Self::fail_closed()
            }
        }
    }
}

/// The shipped redaction rules, in application order.
///
/// Order matters: the specific, context bearing rules run first so that the
/// generic `key=value` rule only has to clean up what is left.
const STANDARD_RULES: &[(&str, &str, &str)] = &[
    // 1. Authorization headers with a scheme and a credential.
    (
        "authorization-header",
        r"(?i)\b(authorization|proxy-authorization)\s*:\s*(basic|bearer|ntlm|negotiate|digest)\s+\S+",
        "${1}: ${2} [REDACTED]",
    ),
    // 2. Standalone bearer tokens.
    (
        "bearer-token",
        r"(?i)\bbearer\s+[A-Za-z0-9\-._~+/=]{8,}",
        "Bearer [REDACTED]",
    ),
    // 3. SASL exchanges, e.g. `AUTH PLAIN AGpvZQBiYXI=`.
    (
        "sasl-payload",
        r"(?i)\b(AUTH(?:ENTICATE)?\s+[A-Z0-9-]*\s*)[A-Za-z0-9+/]{8,}=*",
        "${1}[REDACTED]",
    ),
    // 4a. IMAP `LOGIN user password`, preceded by a command tag (`1`, `A001`,
    //     `*`, `+`).  Anchoring on the tag keeps ordinary log prose such as
    //     `login attempt with a retry` readable, while every protocol line the
    //     client actually emits carries a tag.
    (
        "imap-login-tagged",
        r"(?i)\b([A-Za-z0-9*+.]{1,20})\s+(LOGIN)\s+(\S+)\s+(\S+)",
        "${1} ${2} ${3} [REDACTED]",
    ),
    // 4b. Safety net for an untagged `LOGIN user password` line: only fires when
    //     the command occupies a whole line by itself.
    (
        "imap-login-bare",
        r"(?im)^\s*(LOGIN)\s+(\S+)\s+(\S+)\s*$",
        "${1} ${2} [REDACTED]",
    ),
    // 5. SOAP / XML request bodies of the EWS and GroupWise clients.
    (
        "soap-field",
        r"(?i)<(password|passwd|token|sessionid|sessiontoken|session|clientsecret|authorization)>[^<]*</",
        "<$1>[REDACTED]</",
    ),
    // 6. Credentials embedded in a URL query string.
    (
        "url-query",
        r"(?i)([?&](?:password|passwd|pass|pwd|token|access_token|refresh_token|code|client_secret)=)[^&\s]+",
        "${1}[REDACTED]",
    ),
    // 7. Cookie headers (session cookies are credentials).
    (
        "cookie-header",
        r"(?i)\b(set-cookie|cookie)\s*:\s*[^\r\n]+",
        "${1}: [REDACTED]",
    ),
    // 8. Generic `key = value` pairs for every credential-ish key.
    (
        "key-value",
        r#"(?i)\b(password|passwd|pwd|passphrase|client_secret|secret|api_key|apikey|access_token|refresh_token|id_token|code_verifier|token)\b(\s*[:=]\s*)("(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*'|[^\s,;&)\]}]+)"#,
        "${1}${2}[REDACTED]",
    ),
    // 9. Long base64 blobs that are not already attributed to a scheme.
    ("base64-blob", r"\b[A-Za-z0-9+/]{40,}={0,2}", "[REDACTED]"),
];

/// [`FormatEvent`] wrapper that scrubs every rendered event.
#[derive(Debug)]
pub struct RedactingFormat<F> {
    inner: F,
    redactor: std::sync::Arc<Redactor>,
}

impl<F> RedactingFormat<F> {
    /// Wraps an inner formatter.
    #[must_use]
    pub fn new(inner: F, redactor: std::sync::Arc<Redactor>) -> Self {
        Self { inner, redactor }
    }
}

impl<S, N, F> FormatEvent<S, N> for RedactingFormat<F>
where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
    N: for<'writer> FormatFields<'writer> + 'static,
    F: FormatEvent<S, N>,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &tracing::Event<'_>,
    ) -> std::fmt::Result {
        let mut buffer = String::with_capacity(256);
        self.inner
            .format_event(ctx, Writer::new(&mut buffer), event)?;
        let redacted = self.redactor.redact(&buffer);
        std::fmt::Write::write_str(&mut writer, &redacted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn redactor() -> Redactor {
        Redactor::standard().unwrap()
    }

    fn assert_redacted(input: &str, needle: &str) {
        let output = redactor().redact(input);
        assert!(
            !output.contains(needle),
            "`{needle}` survived redaction in: {output}"
        );
        assert!(output.contains(REDACTED), "no marker in: {output}");
    }

    #[test]
    fn standard_rules_all_compile() {
        let redactor = Redactor::standard().unwrap();
        assert_eq!(redactor.rule_count(), STANDARD_RULES.len());
        assert!(!redactor.is_empty());
        assert_eq!(redactor.rule_names().len(), STANDARD_RULES.len());
    }

    #[test]
    fn passwords_in_key_value_pairs_are_removed() {
        for input in [
            "password=hunter2",
            "password: hunter2",
            "PASSWORD = hunter2",
            "passwd='hunter2'",
            r#"password="hunter2""#,
            "pwd=hunter2",
            "passphrase=hunter2",
        ] {
            assert_redacted(input, "hunter2");
        }
    }

    #[test]
    fn tokens_are_removed() {
        for (input, needle) in [
            ("access_token=ya29.a0AfH6SMBx", "ya29.a0AfH6SMBx"),
            (
                "refresh_token=1//0gExampleRefreshToken",
                "1//0gExampleRefreshToken",
            ),
            (
                "api_key=AIzaSyD-ExampleKey1234567890",
                "AIzaSyD-ExampleKey1234567890",
            ),
            ("client_secret=abc123", "abc123"),
            (
                "code_verifier=dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk",
                "dBjftJeZ4CVP",
            ),
        ] {
            assert_redacted(input, needle);
        }
    }

    #[test]
    fn authorization_headers_keep_the_scheme_only() {
        let output = redactor().redact("Authorization: Bearer eyJhbGciOiJIUzI1NiJ9.payload.sig");
        assert!(output.contains("Bearer"));
        assert!(!output.contains("eyJhbGciOiJIUzI1NiJ9"));
        assert!(output.contains(REDACTED));
    }

    #[test]
    fn basic_auth_is_removed() {
        assert_redacted("Authorization: Basic YWRhOmh1bnRlcjI=", "YWRhOmh1bnRlcjI=");
    }

    #[test]
    fn sasl_payloads_are_removed() {
        assert_redacted("S: AUTH PLAIN AGpvZQBiYXI=", "AGpvZQBiYXI=");
        assert_redacted(
            "C: AUTHENTICATE XOAUTH2 dXNlcj1hZGFAdGVzdA==",
            "dXNlcj1hZGFAdGVzdA==",
        );
    }

    #[test]
    fn imap_login_lines_keep_the_user_name() {
        let output = redactor().redact("1 LOGIN ada@example.org s3cr3t-pw");
        assert!(output.contains("ada@example.org"));
        assert!(!output.contains("s3cr3t-pw"));

        let output = redactor().redact("A002 LOGIN ada@example.org s3cr3t-pw");
        assert_eq!(output, "A002 LOGIN ada@example.org [REDACTED]");
    }

    #[test]
    fn an_untagged_login_command_is_still_cleaned() {
        let output = redactor().redact("LOGIN ada@example.org s3cr3t-pw");
        assert!(!output.contains("s3cr3t-pw"));
        assert!(output.contains("ada@example.org"));
    }

    #[test]
    fn the_async_imap_encode_trace_is_cleaned() {
        // `async-imap` traces every command it writes with `log::trace!`.  The
        // line is rendered by the log bridge and then passed through the
        // redactor, so this exact shape has to be covered.
        let output = redactor().redact(
            r#"TRACE async_imap::imap_stream: encode: input: Some("A001"), Some("A001 LOGIN ada@example.org s3cr3t-pw\r\n")"#,
        );
        assert!(!output.contains("s3cr3t-pw"), "{output}");
        assert!(output.contains("ada@example.org"), "{output}");

        // The same line as an `AUTHENTICATE PLAIN` exchange.
        let output = redactor().redact(
            r#"TRACE async_imap::imap_stream: encode: input: Some("A002"), Some("A002 AUTHENTICATE PLAIN AGpvZQBiYXI=\r\n")"#,
        );
        assert!(!output.contains("AGpvZQBiYXI="), "{output}");
    }

    #[test]
    fn plain_prose_about_logging_in_survives() {
        // Regression guard: the previous IMAP rule matched "login attempt with"
        // and redacted the word "with".
        let input = "2026-11-12T14:00:00Z ERROR hylmail: login attempt with a retry";
        assert_eq!(redactor().redact(input), input);
        let input = "login failed for ada@example.org: bad credentials";
        assert_eq!(redactor().redact(input), input);
    }

    #[test]
    fn soap_bodies_are_cleaned() {
        assert_redacted("<Password>hunter2</Password>", "hunter2");
        assert_redacted("<SessionId>abc-def</SessionId>", "abc-def");
        assert_redacted("<Token>xyz</Token>", "xyz");
    }

    #[test]
    fn query_string_credentials_are_cleaned() {
        assert_redacted(
            "GET /oauth2/callback?code=4/0AY0e-g7&state=abc",
            "4/0AY0e-g7",
        );
        assert_redacted("https://x/?access_token=secretValue&x=1", "secretValue");
    }

    #[test]
    fn cookies_are_cleaned() {
        assert_redacted(
            "Set-Cookie: SESSION=abcdef123456; Path=/; HttpOnly",
            "abcdef123456",
        );
    }

    #[test]
    fn standalone_base64_blobs_are_cleaned() {
        assert_redacted(
            "decoded chunk: dGhpcyBpcyBhIHZlcnkgbG9uZyBiYXNlNjQgYmxvYg==",
            "dGhpcyBpcyBh",
        );
        assert!(!redactor().redact("short=YWJj").contains("[REDACTED]"));
    }

    #[test]
    fn harmless_lines_are_untouched() {
        let redactor = redactor();
        for input in [
            "Connected to imap.example.org:993",
            "Found 12 new messages in INBOX",
            "id=3f1c0d5e-1b2a-4c3d-8e9f-0a1b2c3d4e5f",
            "subject: Re: Quarterly report",
            "size=2048 bytes",
        ] {
            assert_eq!(redactor.redact(input), input, "line was modified: {input}");
        }
    }

    #[test]
    fn empty_redactor_changes_nothing() {
        let redactor = Redactor::empty();
        assert!(redactor.is_empty());
        assert_eq!(redactor.redact("password=hunter2"), "password=hunter2");
    }

    #[test]
    fn fail_closed_redactor_removes_everything() {
        let redactor = Redactor::fail_closed();
        assert!(redactor.is_fail_closed());
        assert!(!redactor.is_empty());
        let output = redactor.redact("password=hunter2\nline two");
        assert!(!output.contains("hunter2"));
        assert!(!output.contains("line two"));
        assert!(output.contains("failed to compile"));
    }

    #[test]
    fn default_redactor_uses_the_standard_rules() {
        let redactor = Redactor::default();
        assert_eq!(redactor.rule_count(), STANDARD_RULES.len());
    }

    #[test]
    fn redaction_is_idempotent() {
        let redactor = redactor();
        let once = redactor.redact("Authorization: Bearer abcdefghijklmnop");
        let twice = redactor.redact(&once);
        assert_eq!(once, twice);
    }
}
