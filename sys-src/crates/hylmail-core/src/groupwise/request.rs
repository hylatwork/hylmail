//! Request bodies of the GroupWise SOAP operations HylMail uses.
//!
//! The shapes follow the GroupWise SOAP API that Novell and Micro Focus ship
//! with the GroupWise Internet Agent and the SOAP proxy: a `loginRequest` that
//! hands out a session id, and every other operation wrapped in the
//! `methods` namespace with the session in the SOAP header.
//!
//! As with the EWS builders, every function returns the element that goes into
//! `<soap:Body>`; the envelope and the session header are added by
//! [`super::GroupWiseClient`].  Keeping the bodies separate means a field name
//! that differs on a specific installation is a one-line change with a unit test
//! next to it.

use crate::soap::envelope::escape_text;

/// Namespace of the login/logout operations.
pub const LOGIN_NAMESPACE: &str = "http://schemas.novell.com/2005/01/GroupWise/log";
/// Namespace of the method definitions.
pub const METHODS_NAMESPACE: &str = "http://schemas.novell.com/2005/01/GroupWise/methods";
/// Namespace of the type definitions.
pub const TYPES_NAMESPACE: &str = "http://schemas.novell.com/2005/01/GroupWise/types";

/// `loginRequest`: exchanges user name and password for a session id.
#[must_use]
pub fn login(user_name: &str, password: &str) -> String {
    format!(
        "<log:loginRequest xmlns:log=\"{LOGIN_NAMESPACE}\">\
         <log:userName>{}</log:userName><log:password>{}</log:password>\
         </log:loginRequest>",
        escape_text(user_name),
        escape_text(password)
    )
}

/// `logoutRequest`: ends the session on the server.
#[must_use]
pub fn logout() -> String {
    format!("<log:logoutRequest xmlns:log=\"{LOGIN_NAMESPACE}\"/>")
}

/// `getFolderListRequest`: every folder the user can see.
///
/// `folder_type` selects which part of the account is listed; `0` asks for the
/// whole tree.
#[must_use]
pub fn get_folder_list(folder_type: i32) -> String {
    format!(
        "<gw:getFolderListRequest xmlns:gw=\"{METHODS_NAMESPACE}\">\
         <gw:folderType>{folder_type}</gw:folderType>\
         </gw:getFolderListRequest>"
    )
}

/// `getItemsRequest`: one page of the items of a folder.
///
/// GroupWise has no delta protocol for messages, so a synchronisation compares
/// the ids of consecutive pages with the ids that are stored locally.
#[must_use]
pub fn get_items(folder_id: &str, count: u32, offset: u32) -> String {
    format!(
        "<gw:getItemsRequest xmlns:gw=\"{METHODS_NAMESPACE}\">\
         <gw:container>{container}</gw:container>\
         <gw:view>mail</gw:view>\
         <gw:count>{count}</gw:count><gw:offset>{offset}</gw:offset>\
         </gw:getItemsRequest>",
        container = escape_text(folder_id),
    )
}

/// `getItemRequest` with the raw MIME content of one message.
#[must_use]
pub fn get_item(item_id: &str) -> String {
    format!(
        "<gw:getItemRequest xmlns:gw=\"{METHODS_NAMESPACE}\">\
         <gw:id>{}</gw:id><gw:view>mail</gw:view>\
         </gw:getItemRequest>",
        escape_text(item_id)
    )
}

/// One of the `markItems…Request` operations.
#[must_use]
pub fn mark_items(item_ids: &[String], read: bool) -> String {
    let method = if read {
        "markItemsReadRequest"
    } else {
        "markItemsUnreadRequest"
    };
    format!(
        "<gw:{method} xmlns:gw=\"{METHODS_NAMESPACE}\">{}</gw:{method}>",
        item_ids_element(item_ids)
    )
}

/// `moveItemRequest`: moves items into another folder.
#[must_use]
pub fn move_items(item_ids: &[String], destination: &str) -> String {
    format!(
        "<gw:moveItemRequest xmlns:gw=\"{METHODS_NAMESPACE}\">\
         {}<gw:destination>{}</gw:destination></gw:moveItemRequest>",
        item_ids_element(item_ids),
        escape_text(destination)
    )
}

/// `deleteItemRequest`.
#[must_use]
pub fn delete_items(item_ids: &[String]) -> String {
    format!(
        "<gw:deleteItemRequest xmlns:gw=\"{METHODS_NAMESPACE}\">{}</gw:deleteItemRequest>",
        item_ids_element(item_ids)
    )
}

/// The `<gw:items>` container with one `<gw:item><gw:id>` per id.
fn item_ids_element(item_ids: &[String]) -> String {
    let items: String = item_ids
        .iter()
        .map(|id| format!("<gw:item><gw:id>{}</gw:id></gw:item>", escape_text(id)))
        .collect();
    format!("<gw:items>{items}</gw:items>")
}

/// The SOAP header fragment that carries the session id.
///
/// Every request after the login needs it; without it GroupWise answers with
/// `status code 0x4400` (not logged in) instead of data.
#[must_use]
pub fn session_header(session: &str) -> String {
    format!(
        "<log:session xmlns:log=\"{LOGIN_NAMESPACE}\">{}</log:session>",
        escape_text(session)
    )
}

/// The SOAP action of a request, as the `SOAPAction` header expects it.
///
/// GroupWise uses the bare method name (`getItemsRequest`), not a URI.
#[must_use]
pub fn action_of(body: &str) -> String {
    // The body starts with `<prefix:name ` or `<prefix:name>`, and the caller
    // only uses this for logging and the header, so a small scan is enough.
    let name_start = body.find(':').map(|index| index + 1);
    let Some(name_start) = name_start else {
        return String::new();
    };
    let rest = &body[name_start..];
    let end = rest
        .find(|c: char| c == '>' || c.is_whitespace())
        .unwrap_or(rest.len());
    rest[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::soap::Node;

    fn parse(xml: &str) -> Node {
        match Node::parse(xml) {
            Ok(node) => node,
            Err(error) => panic!("the request is not valid XML: {error}\n{xml}"),
        }
    }

    #[test]
    fn a_login_carries_the_credentials_in_the_log_namespace() {
        let request = parse(&login("ada@example.org", "geheim & mehr"));
        assert_eq!(request.name, "loginRequest");
        assert_eq!(request.find_text(&["userName"]), Some("ada@example.org"));
        assert_eq!(request.find_text(&["password"]), Some("geheim & mehr"));
    }

    #[test]
    fn paging_requests_carry_a_container_and_a_window() {
        let request = parse(&get_items("F123", 100, 200));
        assert_eq!(request.find_text(&["container"]), Some("F123"));
        assert_eq!(request.find_text(&["view"]), Some("mail"));
        assert_eq!(request.find_text(&["count"]), Some("100"));
        assert_eq!(request.find_text(&["offset"]), Some("200"));
        let list = parse(&get_folder_list(0));
        assert_eq!(list.find_text(&["folderType"]), Some("0"));
    }

    #[test]
    fn item_lists_are_built_for_every_operation() {
        let ids = vec!["A1".to_string(), "B2".to_string()];
        let read = parse(&mark_items(&ids, true));
        assert_eq!(read.name, "markItemsReadRequest");
        assert_eq!(read.descendants_named("id").len(), 2);
        assert_eq!(read.descendants_named("id")[1].text.trim(), "B2");
        let unread = parse(&mark_items(&ids, false));
        assert_eq!(unread.name, "markItemsUnreadRequest");

        let moved = parse(&move_items(&ids, "F9"));
        assert_eq!(moved.find_text(&["destination"]), Some("F9"));
        assert_eq!(moved.descendants_named("id").len(), 2);

        let deleted = parse(&delete_items(&ids));
        assert_eq!(deleted.name, "deleteItemRequest");
    }

    #[test]
    fn the_session_header_is_escaped_and_an_empty_list_renders() {
        let header = parse(&session_header("A&B"));
        assert_eq!(header.text.trim(), "A&B");
        let empty = parse(&delete_items(&[]));
        assert!(empty.child("items").is_some());
        assert!(empty.descendants_named("id").is_empty());
    }

    #[test]
    fn the_action_is_read_from_the_body() {
        assert_eq!(action_of(&get_items("F1", 1, 0)), "getItemsRequest");
        assert_eq!(action_of(&logout()), "logoutRequest");
        assert_eq!(action_of("kein Element"), "");
    }

    #[test]
    fn a_crafted_id_cannot_end_the_element() {
        let request = parse(&get_item("x</gw:id><evil/>"));
        assert_eq!(request.find_text(&["id"]), Some("x</gw:id><evil/>"));
        assert!(request.descendants_named("evil").is_empty());
    }
}
