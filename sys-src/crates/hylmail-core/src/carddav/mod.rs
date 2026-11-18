//! CardDAV address book synchronisation.
//!
//! The client speaks the subset of RFC 6352 that an address book needs:
//!
//! ```text
//!   PROPFIND Depth: 1   ──►  every vCard resource with its ETag
//!   GET <href>           ─►  the raw vCard (only when the ETag changed)
//!   PUT / DELETE         ──►  written back later, in phase 2
//! ```
//!
//! ## Lossless round trip
//!
//! The **whole** vCard text is stored in [`Contact::vcard_raw`], and the
//! parsed fields are only a convenience view of it.  A synchronisation that
//! updates one field therefore cannot drop a property HylMail does not
//! understand - the requirement the specification calls a "verlustfreier
//! vCard-Roundtrip".
//!
//! ## Change detection
//!
//! A resource whose `getetag` matches the stored one is not fetched again, so a
//! second sync of an unchanged collection transfers nothing but the PROPFIND.
//! A resource that disappeared from the collection is deleted locally.

use base64::prelude::{Engine as _, BASE64_STANDARD};
use reqwest::Method;
use secrecy::{ExposeSecret, SecretString};

use crate::db::{contacts, Database};
use crate::error::{Error, Result};
use crate::models::{utc_now, AccountId, Contact, ContactEmail, ContactId, ContactSource};
use crate::net::{Timeouts, TlsPolicy};

/// What one synchronisation pass did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SyncReport {
    /// Resources the server listed.
    pub listed: usize,
    /// Resources whose vCard was downloaded because the ETag changed.
    pub fetched: usize,
    /// Contacts removed because the server no longer lists them.
    pub removed: usize,
}

impl SyncReport {
    /// `true` when the pass changed nothing on disk.
    #[must_use]
    pub fn is_noop(&self) -> bool {
        self.fetched == 0 && self.removed == 0
    }
}

/// One `<response>` of a `PROPFIND` multistatus body.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Resource {
    href: String,
    etag: Option<String>,
    is_collection: bool,
}

/// Synchronises the address book of `account_id` from `base_url`.
///
/// `username` and `password` authenticate with HTTP Basic, which is what the
/// overwhelming majority of CardDAV servers (and the corporate installations
/// this client targets) accept over TLS.
///
/// # Errors
///
/// [`Error::Connect`], [`Error::Tls`] or [`Error::Timeout`] from the transport;
/// [`Error::Protocol`] when the server answers with a status the client cannot
/// work with; [`Error::MalformedResponse`] when the multistatus body cannot be
/// read.
pub async fn sync(
    database: &Database,
    account_id: AccountId,
    base_url: &str,
    username: &str,
    password: &SecretString,
    tls: &TlsPolicy,
) -> Result<SyncReport> {
    let base_url = base_url.trim();
    if base_url.is_empty() {
        return Err(Error::config(
            "für dieses Konto ist keine CardDAV-Adresse hinterlegt",
        ));
    }
    let client = crate::soap::http::build_client(tls, &Timeouts::default())?;
    let authorization = basic_auth(username, password);

    let listing = list_resources(&client, base_url, &authorization).await?;
    let mut report = SyncReport {
        listed: listing.len(),
        ..SyncReport::default()
    };

    // The stored contacts of this account, keyed by their `href`, so an
    // unchanged ETag can be detected and a vanished resource removed.
    let existing =
        database.read(|connection| contacts::list_contacts(connection, Some(account_id)))?;
    let mut seen: Vec<String> = Vec::new();

    for resource in &listing {
        if resource.is_collection || !looks_like_vcard(&resource.href) {
            continue;
        }
        let absolute = absolute_url(base_url, &resource.href);
        seen.push(absolute.clone());
        let known = existing.iter().find(|contact| {
            contact
                .href
                .as_deref()
                .is_some_and(|href| same_resource(href, &absolute))
        });
        if let (Some(known), Some(etag)) = (known, resource.etag.as_deref()) {
            if known.etag.as_deref() == Some(etag) {
                continue;
            }
        }
        let raw = fetch_resource(&client, &absolute, &authorization).await?;
        report.fetched += 1;
        let parsed = parse_vcard(&raw);
        let mut contact = Contact {
            id: known.map_or_else(ContactId::new, |contact| contact.id),
            account_id: Some(account_id),
            display_name: parsed.display_name,
            family_name: parsed.family_name,
            given_name: parsed.given_name,
            emails: parsed.emails,
            organization: parsed.organization,
            vcard_raw: Some(raw),
            etag: resource.etag.clone(),
            href: Some(absolute.clone()),
            source: ContactSource::CardDav,
            last_seen: Some(utc_now()),
        };
        database.write(|connection| {
            contacts::upsert_contact_by_href(connection, &mut contact)?;
            Ok(())
        })?;
    }

    // Everything the server no longer lists is gone from the collection.
    for contact in &existing {
        let Some(href) = contact.href.as_deref() else {
            continue;
        };
        if !seen.iter().any(|kept| same_resource(kept, href)) {
            database.write(|connection| contacts::delete_contact(connection, contact.id))?;
            report.removed += 1;
        }
    }

    Ok(report)
}

/// The `Authorization` header of the request.
fn basic_auth(username: &str, password: &SecretString) -> String {
    let raw = format!("{username}:{}", password.expose_secret());
    format!("Basic {}", BASE64_STANDARD.encode(raw))
}

/// Sends the `PROPFIND` and parses the multistatus body.
async fn list_resources(
    client: &reqwest::Client,
    url: &str,
    authorization: &str,
) -> Result<Vec<Resource>> {
    const BODY: &str = "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
        <d:propfind xmlns:d=\"DAV:\"><d:prop>\
        <d:getetag/><d:resourcetype/></d:prop></d:propfind>";
    let method = Method::from_bytes(b"PROPFIND")
        .map_err(|_| Error::config("the PROPFIND method could not be constructed"))?;
    let response = client
        .request(method, url)
        .header(reqwest::header::AUTHORIZATION, authorization)
        .header(
            reqwest::header::CONTENT_TYPE,
            "application/xml; charset=utf-8",
        )
        .header("Depth", "1")
        .body(BODY)
        .send()
        .await
        .map_err(|error| transport_error(&error, url))?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(Error::Protocol {
            protocol: "CardDAV",
            command: "PROPFIND".to_string(),
            message: format!("answered with HTTP {status}"),
        });
    }
    let body = response
        .text()
        .await
        .map_err(|error| transport_error(&error, url))?;
    Ok(parse_multistatus(&body))
}

/// `GET`s one vCard resource.
async fn fetch_resource(
    client: &reqwest::Client,
    url: &str,
    authorization: &str,
) -> Result<String> {
    let response = client
        .get(url)
        .header(reqwest::header::AUTHORIZATION, authorization)
        .send()
        .await
        .map_err(|error| transport_error(&error, url))?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(Error::Protocol {
            protocol: "CardDAV",
            command: "GET".to_string(),
            message: format!("answered with HTTP {status}"),
        });
    }
    response
        .text()
        .await
        .map_err(|error| transport_error(&error, url))
}

/// Parses the `<response>` elements of a DAV multistatus body.
fn parse_multistatus(body: &str) -> Vec<Resource> {
    let mut resources = Vec::new();
    let mut rest = body;
    while let Some(start) = find_tag(rest, "response") {
        let block = &rest[start..];
        let end = find_close(block, "response").unwrap_or(block.len());
        let block = &block[..end];
        let href = match tag_value(block, "href") {
            Some(value) => value,
            None => {
                rest = &rest[start + end.min(rest.len() - start)..];
                continue;
            }
        };
        let etag = tag_value(block, "getetag").filter(|value| !value.trim().is_empty());
        let is_collection = block.contains(":collection") || block.contains("<collection");
        resources.push(Resource {
            href,
            etag,
            is_collection,
        });
        let consumed = start + end;
        if consumed >= rest.len() {
            break;
        }
        rest = &rest[consumed..];
    }
    resources
}

/// The index just past the opening tag of `name`, ignoring its namespace prefix.
fn find_tag(haystack: &str, name: &str) -> Option<usize> {
    let needle = format!(":{name}>");
    let bare = format!("<{name}>");
    let paren = format!("{name}>");
    if let Some(index) = haystack.find(&needle) {
        return Some(index + needle.len());
    }
    if let Some(index) = haystack.find(&bare) {
        return Some(index + bare.len());
    }
    let _ = paren;
    None
}

/// The index of the closing tag of `name`.
fn find_close(haystack: &str, name: &str) -> Option<usize> {
    let prefixed = format!(":{name}>");
    let bare = format!("</{name}>");
    let mut best: Option<usize> = None;
    if let Some(index) = haystack.find(&prefixed) {
        best = Some(index + prefixed.len());
    }
    if let Some(index) = haystack.find(&bare) {
        let candidate = index + bare.len();
        best = Some(best.map_or(candidate, |current| current.min(candidate)));
    }
    best
}

/// The text between the opening and closing tag of `name`, if present.
fn tag_value(block: &str, name: &str) -> Option<String> {
    let open = find_tag(block, name)?;
    let tail = &block[open..];
    let end = tail.find('<')?;
    let value = tail[..end].trim().to_string();
    (!value.is_empty()).then_some(value)
}

/// `true` when the resource looks like a vCard rather than a binary body.
fn looks_like_vcard(href: &str) -> bool {
    let lowered = href.to_ascii_lowercase();
    lowered.ends_with(".vcf") || lowered.contains(".vcf?") || !lowered.contains('.')
}

/// Joins a possibly relative `href` with the collection URL.
fn absolute_url(base_url: &str, href: &str) -> String {
    if href.starts_with("http://") || href.starts_with("https://") {
        return href.to_string();
    }
    let base = base_url.trim_end_matches('/');
    if let Some(rest) = href.strip_prefix('/') {
        // A root-relative href keeps only the origin of the base URL.
        if let Some(origin_end) = base
            .find("://")
            .and_then(|scheme| base[scheme + 3..].find('/').map(|slash| scheme + 3 + slash))
        {
            return format!("{}/{rest}", &base[..origin_end]);
        }
        return format!("{base}/{rest}");
    }
    format!("{base}/{href}")
}

/// `true` when two hrefs address the same resource.
fn same_resource(left: &str, right: &str) -> bool {
    normalise_href(left) == normalise_href(right)
}

/// Removes a trailing slash so `a/b` and `a/b/` compare equal.
fn normalise_href(href: &str) -> String {
    href.trim_end_matches('/').to_ascii_lowercase()
}

/// Maps a `reqwest` failure onto the structured error type.
fn transport_error(error: &reqwest::Error, url: &str) -> Error {
    if error.is_timeout() {
        return Error::timeout("CardDAV request", 60);
    }
    let mut chain = String::new();
    let mut source: Option<&dyn std::error::Error> = Some(error);
    while let Some(current) = source {
        chain.push_str(&current.to_string());
        chain.push(' ');
        source = current.source();
    }
    if chain.contains("certificate") || chain.contains("rustls") || chain.contains("handshake") {
        return Error::Tls {
            host: crate::soap::http::host_of(url),
            message: chain.trim().to_string(),
        };
    }
    Error::Connect {
        endpoint: crate::soap::http::host_of(url),
        source: std::io::Error::other(chain.trim().to_string()),
    }
}

/// The fields extracted from a vCard; the raw text is kept separately.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct VCardFields {
    display_name: Option<String>,
    family_name: Option<String>,
    given_name: Option<String>,
    organization: Option<String>,
    emails: Vec<ContactEmail>,
}

/// Parses the handful of properties the address book shows.
///
/// Anything else is deliberately ignored here and preserved in
/// [`Contact::vcard_raw`], which is what keeps the round trip lossless.
fn parse_vcard(raw: &str) -> VCardFields {
    let mut fields = VCardFields::default();
    for line in unfold(raw) {
        let (name, value) = match line.split_once(':') {
            Some((left, right)) => (left, right),
            None => continue,
        };
        let (property, params) = match name.split_once(';') {
            Some((property, params)) => (property.to_ascii_uppercase(), params),
            None => (name.to_ascii_uppercase(), ""),
        };
        match property.as_str() {
            "FN" => fields.display_name = non_empty(value),
            "ORG" => fields.organization = non_empty(value.split(';').next().unwrap_or(value)),
            "N" => {
                let mut parts = value.split(';');
                fields.family_name = parts.next().and_then(non_empty);
                fields.given_name = parts.next().and_then(non_empty);
            }
            "EMAIL" => {
                if let Some(address) = non_empty(value) {
                    fields.emails.push(ContactEmail {
                        address,
                        label: type_label(params),
                        preferred: params.to_ascii_uppercase().contains("PREF"),
                    });
                }
            }
            _ => {}
        }
    }
    if fields.emails.iter().filter(|email| email.preferred).count() > 1 {
        let mut seen = false;
        for email in &mut fields.emails {
            if email.preferred {
                if seen {
                    email.preferred = false;
                }
                seen = true;
            }
        }
    }
    if fields.emails.is_empty() {
        fields.emails.clear();
    }
    fields
}

/// The `TYPE` parameter of an `EMAIL` property, lowercased.
fn type_label(params: &str) -> Option<String> {
    for part in params.split(';') {
        let lowered = part.to_ascii_lowercase();
        let value = lowered.strip_prefix("type=").unwrap_or(&lowered);
        for candidate in value.split(',') {
            let candidate = candidate.trim();
            if matches!(candidate, "work" | "home" | "cell" | "other") {
                return Some(candidate.to_string());
            }
        }
    }
    None
}

/// Splits a vCard into logical lines, undoing RFC 6350 line folding.
fn unfold(raw: &str) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for physical in raw.split(['\n', '\r']).filter(|line| !line.is_empty()) {
        if let Some(continuation) = physical.strip_prefix([' ', '\t']) {
            if let Some(last) = lines.last_mut() {
                last.push_str(continuation);
                continue;
            }
        }
        lines.push(physical.to_string());
    }
    lines
}

/// `Some(trimmed)` unless the value is empty.
fn non_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const VCF: &str = "BEGIN:VCARD\r\nVERSION:3.0\r\nFN:Ada Lovelace\r\n\
        N:Lovelace;Ada;;;\r\nEMAIL;TYPE=WORK;PREF:ada@example.org\r\n\
        ORG:Example University\r\nX-CUSTOM;P=1:keep me\r\nEND:VCARD\r\n";

    #[test]
    fn a_vcard_is_parsed_into_the_display_fields() {
        let fields = parse_vcard(VCF);
        assert_eq!(fields.display_name.as_deref(), Some("Ada Lovelace"));
        assert_eq!(fields.family_name.as_deref(), Some("Lovelace"));
        assert_eq!(fields.given_name.as_deref(), Some("Ada"));
        assert_eq!(fields.organization.as_deref(), Some("Example University"));
        assert_eq!(fields.emails.len(), 1);
        assert_eq!(fields.emails[0].address, "ada@example.org");
        assert_eq!(fields.emails[0].label.as_deref(), Some("work"));
        assert!(fields.emails[0].preferred);
    }

    #[test]
    fn folded_lines_are_rejoined() {
        let folded = "BEGIN:VCARD\r\nFN:Sehr langer\r\n Name\r\nEND:VCARD\r\n";
        let fields = parse_vcard(folded);
        assert_eq!(fields.display_name.as_deref(), Some("Sehr langerName"));
    }

    #[test]
    fn the_multistatus_lists_every_resource() {
        let body = "<?xml version=\"1.0\"?><d:multistatus xmlns:d=\"DAV:\">\
            <d:response><d:href>/addr/</d:href><d:propstat><d:prop>\
            <d:resourcetype><d:collection/></d:resourcetype></d:prop></d:propstat></d:response>\
            <d:response><d:href>/addr/1.vcf</d:href><d:propstat><d:prop>\
            <d:getetag>\"v1\"</d:getetag></d:prop></d:propstat></d:response></d:multistatus>";
        let resources = parse_multistatus(body);
        assert_eq!(resources.len(), 2);
        assert!(resources[0].is_collection);
        assert_eq!(resources[1].href, "/addr/1.vcf");
        assert_eq!(resources[1].etag.as_deref(), Some("\"v1\""));
        assert!(!resources[1].is_collection);
    }

    #[test]
    fn hrefs_are_resolved_against_the_collection() {
        assert_eq!(
            absolute_url("https://dav.example.org/addr/", "1.vcf"),
            "https://dav.example.org/addr/1.vcf"
        );
        assert_eq!(
            absolute_url("https://dav.example.org/addr/", "/addr/2.vcf"),
            "https://dav.example.org/addr/2.vcf"
        );
        assert_eq!(
            absolute_url("https://dav.example.org/addr/", "https://other/3.vcf"),
            "https://other/3.vcf"
        );
    }

    #[test]
    fn resource_comparison_ignores_case_and_a_trailing_slash() {
        assert!(same_resource(
            "https://D/addr/1.vcf",
            "https://d/addr/1.vcf/"
        ));
        assert!(!same_resource(
            "https://d/addr/1.vcf",
            "https://d/addr/2.vcf"
        ));
    }

    #[test]
    fn basic_auth_encodes_the_credentials() {
        let secret = crate::secrets::secret("s3cret");
        let header = basic_auth("ada", &secret);
        let encoded = header.strip_prefix("Basic ").unwrap();
        let decoded = BASE64_STANDARD.decode(encoded).unwrap();
        assert_eq!(String::from_utf8(decoded).unwrap(), "ada:s3cret");
    }

    #[test]
    fn the_etag_round_trips_through_a_contact() {
        let fields = parse_vcard(VCF);
        let contact = Contact {
            id: ContactId::new(),
            account_id: Some(AccountId::new()),
            display_name: fields.display_name,
            family_name: fields.family_name,
            given_name: fields.given_name,
            emails: fields.emails,
            organization: fields.organization,
            vcard_raw: Some(VCF.to_string()),
            etag: Some("\"v1\"".to_string()),
            href: Some("https://dav.example.org/addr/1.vcf".to_string()),
            source: ContactSource::CardDav,
            last_seen: Some(utc_now()),
        };
        assert!(contact.is_remote());
        assert_eq!(contact.primary_email(), Some("ada@example.org"));
        assert_eq!(contact.vcard_raw.as_deref(), Some(VCF));
    }
}
