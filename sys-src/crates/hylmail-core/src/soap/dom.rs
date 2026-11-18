//! A small read-only DOM for the SOAP responses of EWS and GroupWise.
//!
//! Both protocols answer with deeply nested XML where every interesting value
//! sits under a fixed path (`FindFolderResponse/ResponseMessages/…`).  A full DOM
//! implementation would be overkill, and `serde` deserialisation of these
//! schemas (`xsi:type` attributes, `Choice` wrappers, optional everything) is
//! brittle.  What is needed instead is:
//!
//! * element names **without** their namespace prefix, because Exchange uses
//!   `t:` for the types and `m:` for the messages namespace;
//! * path lookup (`find(&["Fault", "Code", "Value"])`);
//! * text, including `CDATA` — EWS delivers `MimeContent` bodies that way;
//! * resolved entity references, which `quick-xml` 0.42 reports as
//!   [`Event::GeneralRef`] instead of unescaping text itself;
//! * attributes, for `Id`/`ChangeKey`/`DistinguishedFolderId`.
//!
//! Everything is owned, so a parsed response outlives the buffer it came from.

use quick_xml::events::{BytesRef, Event};
use quick_xml::reader::Reader;

use crate::error::{Error, Result};

/// One XML element.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Node {
    /// Local name, namespace prefix removed.
    pub name: String,
    /// Namespace prefix, when the element had one (`t`, `m`, `soap`, …).
    pub prefix: Option<String>,
    /// Attributes as `(local name, value)` pairs, prefix removed.
    pub attributes: Vec<(String, String)>,
    /// Text directly inside this element, entity references resolved.
    pub text: String,
    /// Child elements in document order.
    pub children: Vec<Node>,
}

impl Node {
    /// Parses a document and returns its root element.
    ///
    /// # Errors
    ///
    /// Returns [`Error::MalformedResponse`] when the input is not well formed
    /// XML or contains no element at all.
    pub fn parse(xml: &str) -> Result<Self> {
        let mut reader = Reader::from_str(xml);
        reader.config_mut().trim_text(false);
        let mut stack: Vec<Node> = Vec::new();
        let mut root: Option<Node> = None;

        loop {
            let event = match reader.read_event() {
                Ok(event) => event,
                Err(error) => {
                    return Err(Error::malformed("SOAP", format!("invalid XML: {error}")));
                }
            };
            match event {
                Event::Start(start) => {
                    let node = Node::from_start(&start, xml)?;
                    stack.push(node);
                }
                Event::Empty(start) => {
                    let node = Node::from_start(&start, xml)?;
                    push_child(&mut stack, &mut root, node)?;
                }
                Event::End(_) => {
                    let Some(node) = stack.pop() else {
                        return Err(Error::malformed("SOAP", "unbalanced closing tag"));
                    };
                    push_child(&mut stack, &mut root, node)?;
                }
                Event::Text(text) => {
                    let value = text.xml_content(quick_xml::XmlVersion::Implicit1_0);
                    append_text(&mut stack, &value);
                }
                Event::CData(data) => {
                    // `MimeContent` and the GroupWise message bodies arrive
                    // wrapped in CDATA and must be kept verbatim.
                    let value = data.xml_content(quick_xml::XmlVersion::Implicit1_0);
                    append_text(&mut stack, &value);
                }
                Event::GeneralRef(reference) => {
                    let value = resolve_reference(&reference);
                    append_text(&mut stack, &value);
                }
                Event::Eof => break,
                // Declarations, comments, processing instructions and doctypes
                // carry nothing HylMail needs.
                _ => {}
            }
        }

        if !stack.is_empty() {
            return Err(Error::malformed(
                "SOAP",
                "unclosed element at the end of the document",
            ));
        }
        root.ok_or_else(|| Error::malformed("SOAP", "the document contains no element"))
    }

    fn from_start(start: &quick_xml::events::BytesStart<'_>, _source: &str) -> Result<Self> {
        let name = start.local_name().as_ref().to_string();
        let prefix = start
            .name()
            .as_ref()
            .split_once(':')
            .map(|(prefix, _)| prefix.to_string());
        let mut attributes = Vec::new();
        for attribute in start.attributes() {
            let attribute = match attribute {
                Ok(attribute) => attribute,
                Err(error) => {
                    return Err(Error::malformed(
                        "SOAP",
                        format!("invalid attribute: {error}"),
                    ));
                }
            };
            let key = attribute.key.local_name().as_ref().to_string();
            let value = match attribute.normalized_value(quick_xml::XmlVersion::Implicit1_0) {
                Ok(value) => value.into_owned(),
                Err(error) => {
                    return Err(Error::malformed(
                        "SOAP",
                        format!("invalid attribute value: {error}"),
                    ));
                }
            };
            attributes.push((key, value));
        }
        Ok(Self {
            name,
            prefix,
            attributes,
            text: String::new(),
            children: Vec::new(),
        })
    }

    /// Text of this element with the surrounding whitespace trimmed.
    #[must_use]
    pub fn trimmed(&self) -> &str {
        self.text.trim()
    }

    /// The value of an attribute, by local name.
    #[must_use]
    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attributes
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    /// The first child with the given local name.
    #[must_use]
    pub fn child(&self, name: &str) -> Option<&Self> {
        self.children.iter().find(|child| child.name == name)
    }

    /// Every child with the given local name.
    pub fn children_named<'a>(&'a self, name: &str) -> impl Iterator<Item = &'a Self> {
        let name = name.to_string();
        self.children.iter().filter(move |child| child.name == name)
    }

    /// Every descendant with the given local name, in document order.
    ///
    /// Used for the parts of the EWS schema whose nesting differs between
    /// operations and server versions (`Includes` of `SyncFolderItems`, `Items`
    /// of `FindItem`): searching for the element itself is both shorter and more
    /// robust than modelling every wrapper.
    #[must_use]
    pub fn descendants_named<'a>(&'a self, name: &str) -> Vec<&'a Self> {
        let mut found = Vec::new();
        let mut stack: Vec<&Self> = self.children.iter().rev().collect();
        while let Some(node) = stack.pop() {
            if node.name == name {
                found.push(node);
            }
            stack.extend(node.children.iter().rev());
        }
        found
    }

    /// Follows a path of local names, e.g. `find(&["Fault", "Code", "Value"])`.
    #[must_use]
    pub fn find(&self, path: &[&str]) -> Option<&Self> {
        let Some((first, rest)) = path.split_first() else {
            return Some(self);
        };
        let child = self.child(first)?;
        child.find(rest)
    }

    /// Text at the end of a path, trimmed; `None` when the path is absent.
    #[must_use]
    pub fn find_text(&self, path: &[&str]) -> Option<&str> {
        self.find(path).map(Self::trimmed)
    }

    /// Every node matching a path, e.g. every `Message` of a response.
    #[must_use]
    pub fn find_all<'a>(&'a self, path: &[&str]) -> Vec<&'a Self> {
        let Some((first, rest)) = path.split_first() else {
            return Vec::new();
        };
        let mut found = Vec::new();
        for child in self.children_named(first) {
            if rest.is_empty() {
                found.push(child);
            } else {
                found.extend(child.find_all(rest));
            }
        }
        found
    }

    /// Reads an element that contains `true`/`false` (EWS) or `1`/`0`.
    #[must_use]
    pub fn find_bool(&self, path: &[&str]) -> Option<bool> {
        let text = self.find_text(path)?;
        match text {
            "true" | "True" | "TRUE" | "1" => Some(true),
            "false" | "False" | "FALSE" | "0" => Some(false),
            _ => None,
        }
    }

    /// Reads an element that contains an integer.
    #[must_use]
    pub fn find_u64(&self, path: &[&str]) -> Option<u64> {
        self.find_text(path)?.parse().ok()
    }
}

fn push_child(stack: &mut [Node], root: &mut Option<Node>, node: Node) -> Result<()> {
    match stack.last_mut() {
        Some(parent) => {
            parent.children.push(node);
            Ok(())
        }
        None => {
            if root.is_some() {
                return Err(Error::malformed(
                    "SOAP",
                    "the document has two root elements",
                ));
            }
            *root = Some(node);
            Ok(())
        }
    }
}

fn append_text(stack: &mut [Node], value: &str) {
    if let Some(current) = stack.last_mut() {
        current.text.push_str(value);
    }
}

/// Resolves `&amp;` and friends; `quick-xml` hands the reference over unresolved.
fn resolve_reference(reference: &BytesRef<'_>) -> String {
    if reference.is_char_ref() {
        return match reference.resolve_char_ref() {
            Ok(Some(character)) => character.to_string(),
            _ => String::new(),
        };
    }
    match reference
        .xml_content(quick_xml::XmlVersion::Implicit1_0)
        .as_ref()
    {
        "amp" => "&".to_string(),
        "lt" => "<".to_string(),
        "gt" => ">".to_string(),
        "quot" => "\"".to_string(),
        "apos" => "'".to_string(),
        other => format!("&{other};"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIND_FOLDER: &str = r#"<?xml version="1.0" encoding="utf-8"?>
        <soap:Envelope xmlns:soap="http://schemas.xmlsoap.org/soap/envelope/"
                       xmlns:t="http://schemas.microsoft.com/exchange/services/2006/types"
                       xmlns:m="http://schemas.microsoft.com/exchange/services/2006/messages">
          <soap:Header/>
          <soap:Body>
            <m:FindFolderResponse>
              <m:ResponseMessages>
                <m:FindFolderResponseMessage ResponseClass="Success">
                  <m:ResponseCode>NoError</m:ResponseCode>
                  <m:RootFolder TotalItemsInView="3">
                    <t:Folders>
                      <t:Folder>
                        <t:FolderId Id="AAA=" ChangeKey="CQAA"/>
                        <t:DisplayName>Posteingang</t:DisplayName>
                        <t:TotalCount>42</t:TotalCount>
                        <t:UnreadCount>7</t:UnreadCount>
                      </t:Folder>
                    </t:Folders>
                  </m:RootFolder>
                </m:FindFolderResponseMessage>
              </m:ResponseMessages>
            </m:FindFolderResponse>
          </soap:Body>
        </soap:Envelope>"#;

    #[test]
    fn prefixed_names_are_reduced_to_local_names() {
        let root = Node::parse(FIND_FOLDER).unwrap();
        assert_eq!(root.name, "Envelope");
        assert_eq!(root.prefix.as_deref(), Some("soap"));
        assert_eq!(
            root.find(&["Body", "FindFolderResponse", "ResponseMessages"])
                .map(|node| node.name.as_str()),
            Some("ResponseMessages")
        );
    }

    #[test]
    fn attributes_and_text_are_read_by_path() {
        let root = Node::parse(FIND_FOLDER).unwrap();
        let folder = root
            .find(&[
                "Body",
                "FindFolderResponse",
                "ResponseMessages",
                "FindFolderResponseMessage",
                "RootFolder",
                "Folders",
                "Folder",
            ])
            .unwrap();
        assert_eq!(
            folder.find(&["DisplayName"]).unwrap().trimmed(),
            "Posteingang"
        );
        assert_eq!(folder.find_u64(&["TotalCount"]), Some(42));
        assert_eq!(folder.find_u64(&["UnreadCount"]), Some(7));
        let id = folder.find(&["FolderId"]).unwrap();
        assert_eq!(id.attr("Id"), Some("AAA="));
        assert_eq!(id.attr("ChangeKey"), Some("CQAA"));
        assert_eq!(id.attr("Missing"), None);

        let message = root
            .find(&[
                "Body",
                "FindFolderResponse",
                "ResponseMessages",
                "FindFolderResponseMessage",
            ])
            .unwrap();
        assert_eq!(message.attr("ResponseClass"), Some("Success"));
        assert_eq!(message.find_text(&["ResponseCode"]), Some("NoError"));
    }

    #[test]
    fn entity_references_and_cdata_are_resolved() {
        let xml = r#"<root><subject>Tom &amp; Jerry &lt;3</subject>
            <body><![CDATA[<p>Hallo & Bob</p>]]></body>
            <numeric>&#65;&#x42;</numeric></root>"#;
        let root = Node::parse(xml).unwrap();
        assert_eq!(root.find_text(&["subject"]), Some("Tom & Jerry <3"));
        assert_eq!(root.find_text(&["body"]), Some("<p>Hallo & Bob</p>"));
        assert_eq!(root.find_text(&["numeric"]), Some("AB"));
    }

    #[test]
    fn booleans_and_lists_are_read() {
        let xml = "<r><IsRead>false</IsRead><HasAttachments>true</HasAttachments>\
            <Items><Message><Subject>a</Subject></Message>\
            <Message><Subject>b</Subject></Message></Items></r>";
        let root = Node::parse(xml).unwrap();
        assert_eq!(root.find_bool(&["IsRead"]), Some(false));
        assert_eq!(root.find_bool(&["HasAttachments"]), Some(true));
        let subjects: Vec<&str> = root
            .find_all(&["Items", "Message"])
            .iter()
            .filter_map(|message| message.find_text(&["Subject"]))
            .collect();
        assert_eq!(subjects, vec!["a", "b"]);
    }

    #[test]
    fn self_closing_elements_have_no_children() {
        let root = Node::parse(r#"<r><Session Id="4711"/><After/></r>"#).unwrap();
        assert_eq!(root.child("Session").unwrap().attr("Id"), Some("4711"));
        assert!(root.child("After").unwrap().children.is_empty());
        assert!(root.find(&["Session", "Child"]).is_none());
    }

    #[test]
    fn a_broken_document_is_an_error_not_a_panic() {
        let error = Node::parse("<r><a></b></r>").unwrap_err();
        assert!(matches!(error, Error::MalformedResponse { .. }));
        assert!(Node::parse("no xml here").is_err());
        assert!(Node::parse("").is_err());
        // Unbalanced end tags are rejected as well.
        assert!(Node::parse("<a></a></b>").is_err());
    }

    #[test]
    fn empty_leaf_text_is_the_empty_string() {
        let root = Node::parse("<r><Empty></Empty><Nil/></r>").unwrap();
        assert_eq!(root.find_text(&["Empty"]), Some(""));
        assert_eq!(root.find_text(&["Nil"]), Some(""));
        assert_eq!(root.find_text(&["Missing"]), None);
    }
}
