//! A worktree made before anyone asks for one.
//!
//! Pressing `+` costs a git checkout and a workspace open. The checkout is git
//! writing files, which does not get faster for being asked politely; what it
//! can be is already done. One spare per repository set is created in the
//! background, at the base a `+` would use, and handed over whole the moment
//! `+` is pressed, with the next one started immediately after.
//!
//! A spare is an ordinary Zed-made worktree with nothing in it: detached, as
//! every worktree Zed creates is, with no thread and no draft beside it. That
//! last part is why it is recorded as a spare. The sweep that reclaims the
//! worktrees an abandoned `+` leaves behind finds them by the empty draft
//! beside them, so without a mark of its own a spare left by a crash — or by a
//! quit between making one and claiming it — would be a worktree nothing ever
//! collects.

use std::path::PathBuf;

use collections::HashMap;
use gpui::{App, Global};

/// A worktree that has been created and not yet claimed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadySpare {
    /// One directory per underlying repository of the set, at the paths the
    /// claiming `+` would itself have created.
    pub paths: Vec<PathBuf>,
    /// The base it was checked out at, spelled as
    /// `resolve_worktree_branch_target` spells it. A `+` that asks for another
    /// base cannot use this one.
    pub base_ref: Option<String>,
    /// Whether the set had two Zed worktrees backed by one repository, so the
    /// creation consolidated them. The claiming caller reports it the same way
    /// it would report its own creation's.
    pub consolidated_worktrees: bool,
}

/// What a repository set's spare is doing.
enum Spare {
    /// One is on its way. A second must not be started, and a `+` arriving now
    /// makes its own worktree rather than waiting on this one: a half-built
    /// spare is not a faster checkout, it is the same checkout with a queue in
    /// front of it.
    Building,
    Ready(ReadySpare),
}

/// Which repositories a spare belongs to: the paths a new worktree of theirs
/// would be created at, which resolve through the main checkout and so are the
/// same for a repository and for its linked worktrees.
pub type SpareKey = Vec<PathBuf>;

/// The spare worktrees of every repository set this process has opened.
#[derive(Default)]
pub struct SpareWorktrees {
    spares: HashMap<SpareKey, Spare>,
}

impl Global for SpareWorktrees {}

impl SpareWorktrees {
    fn global(cx: &mut App) -> &mut Self {
        cx.default_global::<Self>()
    }

    /// Takes the spare for this repository set, if there is one and it was
    /// checked out at the base being asked for. A spare is handed out once:
    /// it is removed here, and the caller owns the directories from then on.
    pub fn claim(key: &SpareKey, base_ref: Option<&str>, cx: &mut App) -> Option<ReadySpare> {
        let spares = &mut Self::global(cx).spares;
        let Some(Spare::Ready(ready)) = spares.get(key) else {
            return None;
        };
        if ready.base_ref.as_deref() != base_ref {
            return None;
        }
        match spares.remove(key) {
            Some(Spare::Ready(ready)) => Some(ready),
            _ => None,
        }
    }

    /// Whether this set has neither a spare nor one on its way.
    pub fn needs_one(key: &SpareKey, cx: &mut App) -> bool {
        !Self::global(cx).spares.contains_key(key)
    }

    /// Claims the right to build this set's spare. `false` means one is
    /// already ready or already being built, and nothing should be started.
    pub fn start_building(key: SpareKey, cx: &mut App) -> bool {
        let spares = &mut Self::global(cx).spares;
        if spares.contains_key(&key) {
            return false;
        }
        spares.insert(key, Spare::Building);
        true
    }

    /// Hands a finished spare over to whoever asks for one next.
    pub fn finish_building(key: SpareKey, spare: ReadySpare, cx: &mut App) {
        Self::global(cx).spares.insert(key, Spare::Ready(spare));
    }

    /// Gives up on a spare that could not be made, so a later attempt may try
    /// again rather than finding a build that never finished.
    pub fn abandon_building(key: &SpareKey, cx: &mut App) {
        Self::global(cx).spares.remove(key);
    }

    /// Every worktree standing ready but unclaimed, for surfaces that have to
    /// know a directory is spoken for.
    pub fn ready_paths(cx: &App) -> Vec<PathBuf> {
        let Some(this) = cx.try_global::<Self>() else {
            return Vec::new();
        };
        this.spares
            .values()
            .filter_map(|spare| match spare {
                Spare::Ready(ready) => Some(ready.paths.clone()),
                Spare::Building => None,
            })
            .flatten()
            .collect()
    }
}
