//! The HTTP transport of the SOAP protocols.
//!
//! `reqwest` is configured from the very same [`TlsPolicy`] the IMAP and SMTP
//! clients use: the rustls `ClientConfig` is built by
//! [`TlsPolicy::client_config`], which is where the platform trust store, the
//! extra roots of an on-premises installation and the `require_tls13` switch
//! live.  Doing it that way means "trust this certificate" is one per-account
//! setting for all four protocols instead of four separate code paths.
//!
//! Redirects are **not** followed automatically: the GroupWise proxy answers
//! with a redirect to the post office that owns the mailbox, and that decision
//! has to consult [`GroupWiseProxyConfig::allow_certificate_cn_fallback`](crate::models::account::GroupWiseProxyConfig).
//!
//! One transport detail deserves attention: NTLM authenticates a *connection*,
//! and the answer to a challenge is only valid on the connection that challenge
//! arrived on.  A pooled client would hand the second request to whichever
//! connection happens to be idle, so NTLM gets its own client with exactly one
//! connection and HTTP/1.1 - see [`HttpClient::ntlm`].

use crate::error::{Error, Result};
use crate::net::ntlm::Ntlm;
use crate::net::{Timeouts, TlsPolicy};

/// The HTTP status and headers of one response.
#[derive(Debug, Clone)]
pub struct HttpResponse {
    /// HTTP status code.
    pub status: u16,
    /// Response body, decoded as UTF-8 (lossy).
    pub body: String,
    /// `Location` header, for the GroupWise redirect handshake.
    pub location: Option<String>,
    /// `WWW-Authenticate` header of a `401`, used to pick an auth mechanism.
    pub www_authenticate: Option<String>,
}

impl HttpResponse {
    /// `true` for a `2xx` status.
    #[must_use]
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// A configured HTTP client.
#[derive(Debug, Clone)]
pub struct HttpClient {
    client: reqwest::Client,
}

impl HttpClient {
    /// Builds a client for the given certificate policy and deadlines.
    ///
    /// # Errors
    ///
    /// Returns [`Error::TlsConfig`] when the policy cannot be turned into a
    /// rustls configuration and [`Error::Connect`] when the client itself cannot
    /// be created.
    pub fn new(policy: &TlsPolicy, timeouts: &Timeouts) -> Result<Self> {
        let client = build_client(policy, timeouts)?;
        Ok(Self { client })
    }

    /// Builds a client that can perform an NTLM handshake.
    ///
    /// NTLM authenticates a connection rather than a request, so this client
    /// keeps **one** idle connection and speaks HTTP/1.1, where each request runs
    /// to completion before the next one starts.  Together that guarantees the
    /// type 3 message travels on the connection the type 2 challenge arrived on.
    /// TLS, deadlines and the cookie store are configured exactly as in
    /// [`Self::new`].
    ///
    /// # Errors
    ///
    /// See [`Self::new`].
    pub fn ntlm(policy: &TlsPolicy, timeouts: &Timeouts) -> Result<Self> {
        let client = build_ntlm_client(policy, timeouts)?;
        Ok(Self { client })
    }

    /// The underlying client, for the rare call that needs a custom request.
    #[must_use]
    pub fn raw(&self) -> &reqwest::Client {
        &self.client
    }

    /// Sends a SOAP request.
    ///
    /// `authorization` is the ready made `Authorization` header value; passing
    /// `None` sends the request anonymously, which is how the NTLM handshake
    /// starts and how Exchange is asked which mechanisms it accepts.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Timeout`], [`Error::Tls`], [`Error::Connect`] or
    /// [`Error::Protocol`]; a `4xx`/`5xx` answer is *not* an error, because both
    /// protocols need to look at the status and the headers themselves.
    pub async fn post_soap(
        &self,
        url: &str,
        action: &str,
        body: String,
        authorization: Option<&str>,
        extra_headers: &[(&str, String)],
    ) -> Result<HttpResponse> {
        let mut request = self
            .client
            .post(url)
            .header(reqwest::header::CONTENT_TYPE, "text/xml; charset=utf-8")
            .header("SOAPAction", format!("\"{action}\""))
            .body(body);
        if let Some(value) = authorization {
            request = request.header(reqwest::header::AUTHORIZATION, value);
        }
        for (name, value) in extra_headers {
            request = request.header(*name, value);
        }
        send(request, url).await
    }

    /// Sends a SOAP request as one NTLM authenticated [`Ntlm`] identity.
    ///
    /// The handshake is the one `MS-NLMP` describes for HTTP: the request goes
    /// out with the type 1 message, the server answers `401` with a type 2
    /// challenge, and the *same* request is repeated with the type 3 answer.
    /// Both requests travel over the same connection, which is why this method
    /// belongs to a client built by [`HttpClient::ntlm`].
    ///
    /// A response without an NTLM challenge (or with a challenge for a mechanism
    /// this client cannot answer) is returned unchanged, so the caller reports
    /// the server's own verdict instead of a handshake error.
    ///
    /// # Errors
    ///
    /// Adds [`Error::Auth`] to the errors of [`HttpClient::post_soap`]: a
    /// challenge that is not a well formed type 2 message, or one that does not
    /// offer what NTLMv2 needs.
    pub async fn post_soap_ntlm(
        &self,
        url: &str,
        action: &str,
        body: String,
        identity: &Ntlm,
        extra_headers: &[(&str, String)],
    ) -> Result<HttpResponse> {
        let negotiate = format!("NTLM {}", identity.negotiate());
        let response = self
            .post_soap(url, action, body.clone(), Some(&negotiate), extra_headers)
            .await?;
        if response.status != 401 {
            return Ok(response);
        }
        let Some((scheme, token)) = response
            .www_authenticate
            .as_deref()
            .and_then(challenge_token)
        else {
            return Ok(response);
        };
        // The scheme is echoed back: a server that offers only `Negotiate`
        // still receives an NTLMSSP message, which is what makes NTLM work
        // against installations that do not advertise it separately.
        let authenticate = format!("{scheme} {}", identity.authenticate(token)?);
        self.post_soap(url, action, body, Some(&authenticate), extra_headers)
            .await
    }

    /// Sends a plain request body, used by the OAuth2 token exchange.
    ///
    /// # Errors
    ///
    /// See [`HttpClient::post_soap`].
    pub async fn post_form(
        &self,
        url: &str,
        form: &[(&str, &str)],
        headers: &[(&str, String)],
    ) -> Result<HttpResponse> {
        let mut request = self.client.post(url).form(form);
        for (name, value) in headers {
            request = request.header(*name, value);
        }
        send(request, url).await
    }
}

/// Builds the `reqwest` client used by every SOAP caller.
///
/// # Errors
///
/// See [`HttpClient::new`].
pub fn build_client(policy: &TlsPolicy, timeouts: &Timeouts) -> Result<reqwest::Client> {
    finish(builder(policy, timeouts)?, "the HTTP client")
}

/// Builds the `reqwest` client an NTLM handshake needs.
///
/// On top of [`build_client`] this pins the client to a single connection and to
/// HTTP/1.1: a second connection could be handed the type 3 message while the
/// server waits for it on the first one, and HTTP/2 multiplexing has no notion
/// of "the connection a challenge arrived on" at all.
///
/// # Errors
///
/// See [`HttpClient::new`].
pub fn build_ntlm_client(policy: &TlsPolicy, timeouts: &Timeouts) -> Result<reqwest::Client> {
    finish(
        builder(policy, timeouts)?
            .http1_only()
            .pool_max_idle_per_host(1),
        "the NTLM HTTP client",
    )
}

/// The client configuration every SOAP caller shares.
fn builder(policy: &TlsPolicy, timeouts: &Timeouts) -> Result<reqwest::ClientBuilder> {
    // `reqwest` would otherwise install its own rustls configuration; handing it
    // ours keeps the trust decisions in one place.  ALPN is added because an
    // on-premises Exchange speaks HTTP/2 when the client offers it.
    let mut tls: rustls::ClientConfig = (*policy.client_config()?).clone();
    tls.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];

    Ok(reqwest::Client::builder()
        .use_preconfigured_tls(tls)
        .user_agent(concat!("HylMail/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(timeouts.connect)
        .timeout(timeouts.command)
        // A `Set-Cookie` from the first request must come back with the second
        // one: some Exchange installations hand out a session cookie next to the
        // challenge.
        .cookie_store(true)
        // The GroupWise redirect is handled by the protocol client, which has to
        // check the target against the account's certificate policy.
        .redirect(reqwest::redirect::Policy::none()))
}

/// Completes a client and names it in the error message.
fn finish(builder: reqwest::ClientBuilder, what: &str) -> Result<reqwest::Client> {
    builder.build().map_err(|error| Error::Connect {
        endpoint: what.to_string(),
        source: std::io::Error::other(error.to_string()),
    })
}

/// Splits a `WWW-Authenticate` header into the scheme and its base64 token.
///
/// The header may list several mechanisms (`Basic realm="x", NTLM TlRMTV…`),
/// and the comma is not guaranteed to be followed by a space.  Both `NTLM` and
/// `Negotiate` are accepted, because an on-premises Exchange frequently
/// advertises only the latter while still answering with an NTLMSSP message.
fn challenge_token(header: &str) -> Option<(&str, &str)> {
    for challenge in header.split(',') {
        let mut parts = challenge.split_whitespace();
        let Some(scheme) = parts.next() else {
            continue;
        };
        if scheme.eq_ignore_ascii_case("NTLM") || scheme.eq_ignore_ascii_case("Negotiate") {
            return parts.next().map(|token| (scheme, token));
        }
    }
    None
}

/// Maps a `reqwest` failure onto the structured error type.
async fn send(request: reqwest::RequestBuilder, url: &str) -> Result<HttpResponse> {
    let response = request
        .send()
        .await
        .map_err(|error| classify(&error, url))?;
    let status = response.status().as_u16();
    let header = |name: reqwest::header::HeaderName| -> Option<String> {
        response
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(ToString::to_string)
    };
    let location = header(reqwest::header::LOCATION);
    let www_authenticate = header(reqwest::header::WWW_AUTHENTICATE);
    let body = response
        .text()
        .await
        .map_err(|error| classify(&error, url))?;
    Ok(HttpResponse {
        status,
        body,
        location,
        www_authenticate,
    })
}

/// Turns a transport failure into the error the caller has to show.
fn classify(error: &reqwest::Error, url: &str) -> Error {
    if error.is_timeout() {
        return Error::timeout("HTTP request", 60);
    }
    // `reqwest` hides the rustls error inside the source chain, so the text is
    // the only reliable hint that the handshake failed rather than the socket.
    let mut chain = String::new();
    let mut source: Option<&dyn std::error::Error> = Some(error);
    while let Some(current) = source {
        chain.push_str(&current.to_string());
        chain.push(' ');
        source = current.source();
    }
    let tls = chain.contains("certificate")
        || chain.contains("Certificate")
        || chain.contains("rustls")
        || chain.contains("handshake")
        || chain.contains("InvalidCertificate");
    if tls {
        return Error::Tls {
            host: host_of(url),
            message: chain.trim().to_string(),
        };
    }
    if error.is_connect() {
        return Error::Connect {
            endpoint: host_of(url),
            source: std::io::Error::other(error.to_string()),
        };
    }
    Error::ConnectionLost {
        protocol: "HTTP",
        host: host_of(url),
    }
}

/// Extracts `host:port` from a URL for the error messages and for the
/// GroupWise redirect policy.
#[must_use]
pub fn host_of(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|parsed| {
            parsed
                .host_str()
                .map(|host| (host.to_string(), parsed.port()))
        })
        .map(|(host, port)| match port {
            Some(port) => format!("{host}:{port}"),
            None => host,
        })
        .unwrap_or_else(|| url.to_string())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn timeouts() -> Timeouts {
        Timeouts {
            connect: Duration::from_secs(2),
            handshake: Duration::from_secs(2),
            command: Duration::from_secs(5),
            greeting: Duration::from_secs(5),
        }
    }

    #[test]
    fn the_client_is_built_from_the_tls_policy() {
        let client = build_client(&TlsPolicy::strict(), &timeouts());
        assert!(client.is_ok(), "{client:?}");
        let client = build_client(&TlsPolicy::insecure(), &timeouts());
        assert!(client.is_ok(), "{client:?}");
        let mut policy = TlsPolicy::strict().require_tls13();
        policy.extra_roots_pem.push("not a pem".to_string());
        // An unreadable extra root is a configuration error, not a panic.
        let error = build_client(&policy, &timeouts()).unwrap_err();
        assert!(matches!(error, Error::TlsConfig(_)), "{error:?}");
    }

    #[test]
    fn urls_are_reduced_to_host_and_port() {
        assert_eq!(
            host_of("https://mail.example.org/EWS/Exchange.asmx"),
            "mail.example.org"
        );
        assert_eq!(host_of("http://127.0.0.1:8080/soap"), "127.0.0.1:8080");
        assert_eq!(host_of("not a url"), "not a url");
    }

    #[test]
    fn the_challenge_token_is_extracted_from_the_www_authenticate_header() {
        assert_eq!(
            challenge_token("NTLM TlRMTVNTUAAC"),
            Some(("NTLM", "TlRMTVNTUAAC"))
        );
        assert_eq!(
            challenge_token("Negotiate TlRMTVNTUAAC"),
            Some(("Negotiate", "TlRMTVNTUAAC"))
        );
        // Several mechanisms in one header, and the scheme name is not case
        // sensitive.
        assert_eq!(
            challenge_token("Basic realm=\"mail\", ntlm TlRMTVNTUAAC"),
            Some(("ntlm", "TlRMTVNTUAAC"))
        );
        // A comma without a following space, and a realm that mentions NTLM.
        assert_eq!(
            challenge_token("Basic realm=\"NTLM portal\",NTLM TlRMTVNTUAAC"),
            Some(("NTLM", "TlRMTVNTUAAC"))
        );
        // A bare scheme name is an offer, not a challenge.
        assert_eq!(challenge_token("NTLM"), None);
        assert_eq!(challenge_token("Basic realm=\"mail\""), None);
        assert_eq!(challenge_token(""), None);
    }

    #[test]
    fn the_ntlm_client_is_built_like_the_shared_one() {
        let client = build_ntlm_client(&TlsPolicy::strict(), &timeouts());
        assert!(client.is_ok(), "{client:?}");
        let mut policy = TlsPolicy::strict().require_tls13();
        policy.extra_roots_pem.push("not a pem".to_string());
        let error = build_ntlm_client(&policy, &timeouts()).unwrap_err();
        assert!(matches!(error, Error::TlsConfig(_)), "{error:?}");
    }
}
