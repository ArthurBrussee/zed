//! Points in a thread worth coming back to.
//!
//! A long thread has moments that matter — the message that set the
//! direction, the command that proved something — and an entry's index is no
//! way to name one: entries are appended while the agent works, and a
//! compaction can take a run of them away. So a mark is pinned to something
//! the entry carries itself and resolved to an index at the moment it is
//! jumped to.
//!
//! Movement runs over bookmarks and user messages together. A user message is
//! where a turn starts, which is the waypoint nobody has to set, and an
//! explicit mark is everything else worth stopping at; stepping through them
//! as one sequence is what makes either useful.

use std::sync::Arc;

use acp_thread::AgentThreadEntry;

/// What a bookmark points at.
///
/// A tool call carries an id the agent gave it, which survives anything that
/// happens to the entries around it. A message carries nothing of the kind,
/// so it is named by its place in the thread's run of messages — messages are
/// appended and never reordered, so that ordinal outlives the entry indices
/// on either side of it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum BookmarkAnchor {
    ToolCall(Arc<str>),
    Message(usize),
}

/// An entry, reduced to what deciding its anchor needs. Keeping the rule in
/// terms of this rather than [`AgentThreadEntry`] is what lets it be tested
/// against a thread's shape without building one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind<'a> {
    UserMessage,
    AssistantMessage,
    ToolCall(&'a str),
    /// A plan, a compaction, an elicitation: entries with no identity of
    /// their own and nothing anyone would come back to.
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

/// The anchor for each entry, by index. `None` where an entry cannot be
/// marked.
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

/// The marks set in one thread, in the order they were set.
///
/// Stored with the thread's metadata, so they survive a restart the way the
/// PR snapshot does.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ThreadBookmarks {
    marks: Vec<BookmarkAnchor>,
}

impl ThreadBookmarks {
    pub fn is_empty(&self) -> bool {
        self.marks.is_empty()
    }

    pub fn contains(&self, anchor: &BookmarkAnchor) -> bool {
        self.marks.contains(anchor)
    }

    /// Sets the mark if it is not there, clears it if it is. Returns whether
    /// the entry is bookmarked afterwards.
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

    /// The entry indices these marks currently point at, in thread order.
    ///
    /// A mark whose entry is not in `anchors` resolves to nothing rather than
    /// to the wrong row: a thread that compacted away the entry under a mark
    /// should lose the mark's destination, not acquire a misleading one.
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

/// Every point movement stops at, in thread order: the marks the user set and
/// the user messages, which are where the turns begin.
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

/// The next waypoint strictly after `from`, or the last one strictly before
/// it. Movement does not wrap: running off either end of a thread reads as
/// being lost, where stopping at the end reads as having arrived.
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
        // The thread the mark was set in.
        let before = [EntryKind::UserMessage, tool("call-1")];
        let anchors_before = anchors(before);
        let mut bookmarks = ThreadBookmarks::default();
        assert!(bookmarks.toggle(anchors_before[1].clone().unwrap()));
        assert_eq!(bookmarks.resolve(&anchors_before), vec![1]);

        // The same thread once the agent has kept working: the tool call it
        // marked is two rows further down, and the mark follows it.
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
        assert!(bookmarks.is_empty());
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
        // One mark on a tool call, and one on a user message that is already
        // a waypoint: the user message must not be listed twice.
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
        // A position between two waypoints moves to the one on each side.
        assert_eq!(next_waypoint(&waypoints, 3), Some(5));
        assert_eq!(previous_waypoint(&waypoints, 3), Some(2));
    }
}
