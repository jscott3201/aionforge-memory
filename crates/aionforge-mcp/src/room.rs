//! Reader-scoped room-resource parsing and rendering.

use aionforge_domain::contracts::Embedder;
use aionforge_domain::ids::Id;
use aionforge_engine::{Memory, Principal};
use rmcp::model::{Annotated, RawResourceTemplate, ResourceTemplate};

use crate::message::{self, MessagePageRequest};
use crate::principal::{AuthEnabled, resolve_reader};
use crate::validated::ValidatedPrincipal;

/// The template URI advertised to clients; concrete room URIs are never enumerated.
pub(crate) const ROOM_URI_TEMPLATE: &str = "aionforge://room/{room_id}";
pub(crate) const ROOM_URI_PREFIX: &str = "aionforge://room/";

/// A validated concrete room URI plus its auth-disabled identity assertion, if supplied.
#[derive(Debug)]
pub(crate) struct RoomUri {
    /// The room grouping id to filter through the caller's visible inboxes.
    pub(crate) room_id: Id,
    viewer: Option<String>,
    teams: Vec<String>,
}

/// Return the one non-enumerable room resource template.
pub(crate) fn resource_template() -> ResourceTemplate {
    Annotated::new(
        RawResourceTemplate::new(ROOM_URI_TEMPLATE, "room-messages")
            .with_title("Reader-visible room messages")
            .with_description(
                "Untrusted messages visible to the authenticated reader for one room.",
            )
            .with_mime_type("text/plain"),
        None,
    )
}

/// Parse an exact room URI, including the auth-disabled reader assertion query suffix.
pub(crate) fn parse_room_uri(uri: &str) -> Result<RoomUri, ()> {
    let rest = uri.strip_prefix(ROOM_URI_PREFIX).ok_or(())?;
    if rest.contains('#') {
        return Err(());
    }
    let (path, query) = match rest.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (rest, None),
    };
    if path.is_empty()
        || path.contains('/')
        || path.contains("..")
        || path.contains('%')
        || path.chars().any(char::is_whitespace)
    {
        return Err(());
    }
    let room_id = Id::parse(path).map_err(|_| ())?;
    let mut viewer = None;
    let mut teams = Vec::new();
    if let Some(query) = query {
        if query.is_empty() {
            return Err(());
        }
        let mut saw_teams = false;
        for pair in query.split('&') {
            let (key, value) = pair.split_once('=').ok_or(())?;
            match percent_decode(key)?.as_str() {
                "viewer" if viewer.is_none() => viewer = Some(percent_decode(value)?),
                "teams" if !saw_teams => {
                    saw_teams = true;
                    for encoded_team in value.split(',') {
                        let team = percent_decode(encoded_team)?;
                        if team.is_empty() {
                            return Err(());
                        }
                        teams.push(team);
                    }
                }
                _ => return Err(()),
            }
        }
    }
    Ok(RoomUri {
        room_id,
        viewer,
        teams,
    })
}

/// Resolve a room reader from cryptographic context or the auth-off URI assertion.
pub(crate) fn resolve_room_reader(
    room: &RoomUri,
    extension: Option<ValidatedPrincipal>,
    auth_enabled: AuthEnabled,
) -> Result<Principal, String> {
    if auth_enabled.is_enabled() {
        resolve_reader(None, Vec::new(), None, extension, auth_enabled)
    } else {
        resolve_reader(
            room.viewer.as_deref(),
            room.teams.clone(),
            None,
            None,
            auth_enabled,
        )
    }
}

/// Read one room through the exact message-poll visibility and wrapper renderer path.
pub(crate) fn read_room_resource<E: Embedder>(
    memory: &Memory<E>,
    room: &RoomUri,
    extension: Option<ValidatedPrincipal>,
    auth_enabled: AuthEnabled,
) -> Result<Option<String>, String> {
    let principal = resolve_room_reader(room, extension, auth_enabled)?;
    let request = MessagePageRequest {
        recipients: message::visible_recipients(&principal),
        room_id: Some(room.room_id),
        after: None,
        limit: message::DEFAULT_POLL_LIMIT,
        unread_only: false,
    };
    let page = message::read_page(memory, &request, "ERR_ROOM_RESOURCE")?;
    if page.messages.is_empty() {
        return Ok(None);
    }
    let text = message::render_page_text(&page);
    crate::telemetry::record_recall_served("room_resource", &text);
    Ok(Some(text))
}

fn percent_decode(raw: &str) -> Result<String, ()> {
    let bytes = raw.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'%' {
            decoded.push(bytes[index]);
            index += 1;
            continue;
        }
        let high = *bytes.get(index + 1).ok_or(())?;
        let low = *bytes.get(index + 2).ok_or(())?;
        let high = hex(high)?;
        let low = hex(low)?;
        decoded.push((high << 4) | low);
        index += 3;
    }
    String::from_utf8(decoded).map_err(|_| ())
}

fn hex(byte: u8) -> Result<u8, ()> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(()),
    }
}

#[cfg(test)]
mod tests {
    use super::parse_room_uri;
    use aionforge_domain::ids::Id;

    #[test]
    fn uri_parser_preserves_percent_encoded_team_separators() {
        let room = Id::generate();
        let parsed = parse_room_uri(&format!(
            "aionforge://room/{room}?viewer=agent:{room}&teams=alpha%2Cbeta,delta%26echo"
        ))
        .expect("valid room uri");
        assert_eq!(parsed.teams, ["alpha,beta", "delta&echo"]);
    }
}
