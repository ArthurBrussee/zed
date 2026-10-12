//! Bookmarks on points in a thread. Entry indices shift as entries are added
//! or compacted away, so a mark is pinned to what the entry carries and
//! resolved to an index when jumped to. Movement steps through bookmarks and
//! user messages as one sequence.

use std::sync::Arc;

use acp_thread::AgentThreadEntry;

/// A tool call by its id; a message, which has no id, by its ordinal among
/// messages, since messages are never reordered.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum BookmarkAnchor {
    ToolCall(Arc<str>),
    Message(usize),
}

/// Lets the anchoring rules be tested without building a thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind<'a> {
    UserMessage,
    AssistantMessage,
    ToolCall(&'a str),
    Unanchorable,
}

impl EntryKind<'_> {
    pub fn of(entry: &AgentThreadEntry) -> EntryKind<'_> {
        match entry {
            AgentThreadEntry::UserMessage(_) => EntryKind::UserMessage,
            AgentThreadEntry::AssistantMessage(_) => EntryKind::AssistantMessage,
            AgentThreadEntry::ToolCall(tool_call) => EntryKind::ToolCall(tool_call.id.0.as_ref()),
            AgentThreadEntry::Elicitation(_) | AgentThreadEntry::ContextCompaction(_) => {
                EntryKind::Unanchorable
            }
        }
    }
}

pub fn anchors<'a>(kinds: impl IntoIterator<Item = EntryKind<'a>>) -> Vec<Option<BookmarkAnchor>> {
    let mut message_ordinal = 0;
    kinds
        .into_iter()
        .map(|kind| match kind {
            EntryKind::ToolCall(id) => Some(BookmarkAnchor::ToolCall(id.into())),
            EntryKind::UserMessage | EntryKind::AssistantMessage => {
                let anchor = BookmarkAnchor::Message(message_ordinal);
                message_ordinal += 1;
                Some(anchor)
            }
            EntryKind::Unanchorable => None,
        })
        .collect()
}

#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ThreadBookmarks {
    marks: Vec<BookmarkAnchor>,
}

impl ThreadBookmarks {
    pub fn contains(&self, anchor: &BookmarkAnchor) -> bool {
        self.marks.contains(anchor)
    }

    /// Returns whether the entry is bookmarked afterwards.
    pub fn toggle(&mut self, anchor: BookmarkAnchor) -> bool {
        match self.marks.iter().position(|mark| mark == &anchor) {
            Some(ix) => {
                self.marks.remove(ix);
                false
            }
            None => {
                self.marks.push(anchor);
                true
            }
        }
    }

    /// The entry indices in thread order; a mark whose entry is gone resolves
    /// to nothing rather than to the wrong row.
    pub fn resolve(&self, anchors: &[Option<BookmarkAnchor>]) -> Vec<usize> {
        let mut indices: Vec<usize> = self
            .marks
            .iter()
            .filter_map(|mark| {
                anchors
                    .iter()
                    .position(|anchor| anchor.as_ref() == Some(mark))
            })
            .collect();
        indices.sort_unstable();
        indices.dedup();
        indices
    }
}

pub fn waypoints<'a>(
    bookmarks: &ThreadBookmarks,
    kinds: &[EntryKind<'a>],
    anchors: &[Option<BookmarkAnchor>],
) -> Vec<usize> {
    let mut indices = bookmarks.resolve(anchors);
    indices.extend(
        kinds
            .iter()
            .enumerate()
            .filter(|(_, kind)| matches!(kind, EntryKind::UserMessage))
            .map(|(ix, _)| ix),
    );
    indices.sort_unstable();
    indices.dedup();
    indices
}

/// Movement does not wrap at either end of the thread.
pub fn next_waypoint(waypoints: &[usize], from: usize) -> Option<usize> {
    waypoints.iter().copied().find(|&ix| ix > from)
}

pub fn previous_waypoint(waypoints: &[usize], from: usize) -> Option<usize> {
    waypoints.iter().copied().rev().find(|&ix| ix < from)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(id: &str) -> EntryKind<'_> {
        EntryKind::ToolCall(id)
    }

    #[test]
    fn test_anchors_number_messages_and_name_tool_calls() {
        let kinds = [
            EntryKind::UserMessage,
            EntryKind::AssistantMessage,
            tool("call-1"),
            EntryKind::Unanchorable,
            EntryKind::UserMessage,
        ];
        assert_eq!(
            anchors(kinds),
            vec![
                Some(BookmarkAnchor::Message(0)),
                Some(BookmarkAnchor::Message(1)),
                Some(BookmarkAnchor::ToolCall("call-1".into())),
                None,
                Some(BookmarkAnchor::Message(2)),
            ]
        );
    }

    #[test]
    fn test_a_mark_survives_entries_appearing_before_it() {
        let before = [EntryKind::UserMessage, tool("call-1")];
        let anchors_before = anchors(before);
        let mut bookmarks = ThreadBookmarks::default();
        assert!(bookmarks.toggle(anchors_before[1].clone().unwrap()));
        assert_eq!(bookmarks.resolve(&anchors_before), vec![1]);

        let after = [
            EntryKind::UserMessage,
            EntryKind::AssistantMessage,
            EntryKind::Unanchorable,
            tool("call-1"),
            tool("call-2"),
        ];
        assert_eq!(bookmarks.resolve(&anchors(after)), vec![3]);
    }

    #[test]
    fn test_a_mark_whose_entry_is_gone_resolves_to_nothing() {
        let mut bookmarks = ThreadBookmarks::default();
        bookmarks.toggle(BookmarkAnchor::ToolCall("compacted-away".into()));
        let anchors = anchors([EntryKind::UserMessage, tool("call-1")]);
        assert_eq!(bookmarks.resolve(&anchors), Vec::<usize>::new());
    }

    #[test]
    fn test_toggling_the_same_entry_twice_clears_it() {
        let mut bookmarks = ThreadBookmarks::default();
        let anchor = BookmarkAnchor::Message(3);
        assert!(bookmarks.toggle(anchor.clone()));
        assert!(bookmarks.contains(&anchor));
        assert!(!bookmarks.toggle(anchor.clone()));
        assert!(!bookmarks.contains(&anchor));
    }

    #[test]
    fn test_waypoints_are_marks_and_user_messages_without_duplicates() {
        let kinds = vec![
            EntryKind::UserMessage,
            EntryKind::AssistantMessage,
            tool("call-1"),
            EntryKind::UserMessage,
        ];
        let anchors = anchors(kinds.iter().copied());
        let mut bookmarks = ThreadBookmarks::default();
        // The marked user message is already a waypoint; it must not repeat.
        bookmarks.toggle(anchors[2].clone().unwrap());
        bookmarks.toggle(anchors[0].clone().unwrap());

        assert_eq!(waypoints(&bookmarks, &kinds, &anchors), vec![0, 2, 3]);
    }

    #[test]
    fn test_movement_stops_at_the_ends() {
        let waypoints = [0usize, 2, 5];
        assert_eq!(next_waypoint(&waypoints, 0), Some(2));
        assert_eq!(next_waypoint(&waypoints, 2), Some(5));
        assert_eq!(next_waypoint(&waypoints, 5), None);
        assert_eq!(previous_waypoint(&waypoints, 5), Some(2));
        assert_eq!(previous_waypoint(&waypoints, 0), None);
        assert_eq!(next_waypoint(&waypoints, 3), Some(5));
        assert_eq!(previous_waypoint(&waypoints, 3), Some(2));
    }
}
