//! SOAP 1.1 plumbing shared by the EWS and GroupWise clients.
//!
//! Both protocols are "XML in, XML out" over HTTP, with the same transport
//! rules and the same fault shape.  Putting the shared half here keeps the two
//! protocol clients readable and means the escaping, the HTTP error mapping and
//! the fault parsing exist exactly once.
//!
//! | Module | Contents |
//! |---|---|
//! | [`dom`] | read-only DOM with path lookup, CDATA and entity handling |
//! | [`envelope`] | request builder, escaping helpers, fault extraction |
//! | [`http`] | the `reqwest` client, built from a [`crate::net::TlsPolicy`] |

pub mod dom;
pub mod envelope;
pub mod http;

pub use dom::Node;
pub use envelope::{
    escape_attribute, escape_text, fault_message, Envelope, EWS_ACTION_PREFIX, EWS_NAMESPACE,
    EWS_TYPES_NAMESPACE, SOAP_ENVELOPE_NAMESPACE,
};
pub use http::{build_client, HttpClient};
