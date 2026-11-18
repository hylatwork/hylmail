//! SOAP 1.1 request envelopes, XML escaping and fault extraction.
//!
//! Requests are assembled from strings rather than from a serialiser: the two
//! protocols need a handful of hand-tuned fragments (`RequestServerVersion`,
//! `ExchangeImpersonation`, the GroupWise session header) that are clearer to
//! write out than to model as types.  Everything that goes in from user data
//! goes through [`escape_text`] or [`escape_attribute`], which is what keeps a
//! subject line containing `&` or `<` from producing invalid XML.

use super::dom::Node;

/// Namespace of the SOAP 1.1 envelope, which both protocols use.
pub const SOAP_ENVELOPE_NAMESPACE: &str = "http://schemas.xmlsoap.org/soap/envelope/";
/// Namespace of the EWS message definitions.
pub const EWS_NAMESPACE: &str = "http://schemas.microsoft.com/exchange/services/2006/messages";
/// Namespace of the EWS type definitions.
pub const EWS_TYPES_NAMESPACE: &str = "http://schemas.microsoft.com/exchange/services/2006/types";
/// Prefix of every EWS SOAP action URI.
pub const EWS_ACTION_PREFIX: &str = "http://schemas.microsoft.com/exchange/services/2006/messages/";

/// Escapes a value that ends up between two tags.
#[must_use]
pub fn escape_text(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            // `\r` is not allowed in XML 1.0 content at all; Exchange rejects a
            // request that carries one, so it is dropped rather than escaped.
            '\r' => {}
            other => escaped.push(other),
        }
    }
    escaped
}

/// Escapes a value that ends up inside double quotes.
#[must_use]
pub fn escape_attribute(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            '\n' | '\t' => escaped.push(' '),
            '\r' => {}
            other => escaped.push(other),
        }
    }
    escaped
}

/// A SOAP request under construction.
#[derive(Debug, Clone)]
pub struct Envelope {
    action: String,
    header: Vec<String>,
    body: String,
    namespaces: Vec<(String, String)>,
}

impl Envelope {
    /// Starts an envelope with the EWS namespaces declared.
    #[must_use]
    pub fn new(action: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            action: action.into(),
            header: Vec::new(),
            body: body.into(),
            namespaces: vec![
                ("t".to_string(), EWS_TYPES_NAMESPACE.to_string()),
                ("m".to_string(), EWS_NAMESPACE.to_string()),
            ],
        }
    }

    /// Starts an envelope without any namespace declaration of its own.
    #[must_use]
    pub fn bare(action: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            action: action.into(),
            header: Vec::new(),
            body: body.into(),
            namespaces: Vec::new(),
        }
    }

    /// Declares an additional namespace on the `Envelope` element.
    #[must_use]
    pub fn namespace(mut self, prefix: impl Into<String>, uri: impl Into<String>) -> Self {
        self.namespaces.push((prefix.into(), uri.into()));
        self
    }

    /// Adds a fragment to the SOAP header.
    ///
    /// The fragment is inserted as is; callers escape the values they put into
    /// it themselves.
    #[must_use]
    pub fn header(mut self, fragment: impl Into<String>) -> Self {
        self.header.push(fragment.into());
        self
    }

    /// The fully qualified SOAP action of this request.
    #[must_use]
    pub fn action(&self) -> &str {
        &self.action
    }

    /// The SOAP action prefixed with the EWS namespace, ready for the
    /// `SOAPAction` HTTP header.
    #[must_use]
    pub fn action_uri(&self) -> String {
        if self.action.starts_with("http://") || self.action.starts_with("https://") {
            self.action.clone()
        } else {
            format!("{EWS_ACTION_PREFIX}{}", self.action)
        }
    }

    /// Renders the request document.
    #[must_use]
    pub fn render(&self) -> String {
        let mut namespaces = format!("xmlns:soap=\"{SOAP_ENVELOPE_NAMESPACE}\"");
        for (prefix, uri) in &self.namespaces {
            namespaces.push_str(&format!(" xmlns:{prefix}=\"{}\"", escape_attribute(uri)));
        }

        let mut document = String::with_capacity(self.body.len() + 512);
        document.push_str("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n");
        document.push_str(&format!("<soap:Envelope {namespaces}>\n"));
        if !self.header.is_empty() {
            document.push_str("  <soap:Header>\n");
            for fragment in &self.header {
                document.push_str("    ");
                document.push_str(fragment);
                document.push('\n');
            }
            document.push_str("  </soap:Header>\n");
        }
        document.push_str("  <soap:Body>\n");
        document.push_str("    ");
        document.push_str(&self.body);
        document.push('\n');
        document.push_str("  </soap:Body>\n");
        document.push_str("</soap:Envelope>\n");
        document
    }
}

/// Extracts a human readable message from a SOAP fault.
///
/// Handles both spellings: SOAP 1.1 (`faultcode`/`faultstring`, what Exchange
/// and the GroupWise proxy send) and SOAP 1.2 (`Code`/`Reason`).  The EWS
/// `ResponseCode` from the fault detail is appended when the server sent one,
/// because that is the part a support case needs.
#[must_use]
pub fn fault_message(root: &Node) -> Option<String> {
    let fault = root.find(&["Body", "Fault"])?;
    let mut message = fault
        .find_text(&["faultstring"])
        .or_else(|| fault.find_text(&["Reason", "Text"]))
        .unwrap_or("the server reported a SOAP fault")
        .to_string();
    if let Some(code) = fault
        .find_text(&["faultcode"])
        .or_else(|| fault.find_text(&["Code", "Value"]))
    {
        message.push_str(&format!(" ({code})"));
    }
    if let Some(detail) = fault
        .find_text(&["detail", "ResponseCode"])
        .or_else(|| fault.find_text(&["detail", "MessageText"]))
    {
        message.push_str(&format!(": {detail}"));
    }
    Some(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_data_is_escaped() {
        assert_eq!(escape_text("Tom & Jerry <3"), "Tom &amp; Jerry &lt;3");
        assert_eq!(escape_attribute("a\"b'c<d"), "a&quot;b&apos;c&lt;d");
        // Carriage returns are dropped: XML 1.0 has no way to express them.
        assert_eq!(escape_text("a\rb"), "ab");
        assert_eq!(escape_attribute("a\nb"), "a b");
    }

    #[test]
    fn a_request_carries_the_ews_namespaces_and_the_action() {
        let envelope = Envelope::new("FindFolder", "<m:FindFolder/>");
        assert_eq!(
            envelope.action_uri(),
            "http://schemas.microsoft.com/exchange/services/2006/messages/FindFolder"
        );
        let document = envelope.render();
        assert!(document.starts_with("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<soap:Envelope"));
        assert!(document.contains(&format!("xmlns:soap=\"{SOAP_ENVELOPE_NAMESPACE}\"")));
        assert!(document.contains(&format!("xmlns:t=\"{EWS_TYPES_NAMESPACE}\"")));
        assert!(document.contains(&format!("xmlns:m=\"{EWS_NAMESPACE}\"")));
        assert!(document.contains("<m:FindFolder/>"));
        assert!(document.contains("<soap:Body>"));
        // No header means no empty `<soap:Header>` element either.
        assert!(!document.contains("soap:Header"));
        // And the document parses back.
        assert!(Node::parse(&document).is_ok());
    }

    #[test]
    fn headers_are_rendered_in_order() {
        let document = Envelope::new("SyncFolderItems", "<m:SyncFolderItems/>")
            .header("<t:RequestServerVersion Version=\"Exchange2013\"/>")
            .header(
                "<t:TimeZoneContext><t:TimeZoneDefinition Id=\"W. Europe\"/></t:TimeZoneContext>",
            )
            .namespace("xsi", "http://www.w3.org/2001/XMLSchema-instance")
            .render();
        let root = Node::parse(&document).unwrap();
        let header = root.child("Header").unwrap();
        assert_eq!(
            header
                .child("RequestServerVersion")
                .unwrap()
                .attr("Version"),
            Some("Exchange2013")
        );
        assert_eq!(
            header
                .find(&["TimeZoneContext", "TimeZoneDefinition"])
                .unwrap()
                .attr("Id"),
            Some("W. Europe")
        );
        assert!(document.contains("xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\""));
    }

    #[test]
    fn a_soap_11_fault_becomes_a_readable_message() {
        let fault = r#"<soap:Envelope xmlns:soap="http://schemas.xmlsoap.org/soap/envelope/"
            xmlns:m="http://schemas.microsoft.com/exchange/services/2006/messages">
          <soap:Body>
            <soap:Fault>
              <faultcode>soap:Client</faultcode>
              <faultstring>An error occurred when verifying security for the message.</faultstring>
              <detail>
                <m:ResponseCode>ErrorInvalidCredentials</m:ResponseCode>
              </detail>
            </soap:Fault>
          </soap:Body>
        </soap:Envelope>"#;
        let root = Node::parse(fault).unwrap();
        let message = fault_message(&root).unwrap();
        assert!(message.contains("verifying security"), "{message}");
        assert!(message.contains("soap:Client"), "{message}");
        assert!(message.contains("ErrorInvalidCredentials"), "{message}");
    }

    #[test]
    fn a_soap_12_fault_and_a_plain_document_are_handled() {
        let fault = r#"<Envelope><Body><Fault><Code><Value>soap:Sender</Value></Code>
            <Reason><Text>Bad request</Text></Reason></Fault></Body></Envelope>"#;
        let root = Node::parse(fault).unwrap();
        let message = fault_message(&root).unwrap();
        assert!(message.contains("Bad request"), "{message}");
        assert!(message.contains("soap:Sender"), "{message}");

        // A successful response has no fault at all.
        let ok = Node::parse("<Envelope><Body><Response/></Body></Envelope>").unwrap();
        assert!(fault_message(&ok).is_none());
    }
}
