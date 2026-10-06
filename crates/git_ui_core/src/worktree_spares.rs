//! One pre-made worktree per repository set, so `+` doesn't wait on a git checkout.
//!
//! Spares are marked as such in `created_worktrees` so one orphaned by a crash or quit can be
//! reclaimed: it has no thread to find it by.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use collections::HashMap;
use gpui::{App, Global};

static CREATIONS_IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);

/// Held while a `+` opens its window, since a spare's checkout racing that open made it take
/// 12-14s. A static rather than a [`Global`] so the guard can release without an `App`.
pub struct CreationInFlight;

impl CreationInFlight {
    pub fn begin() -> Self {
        CREATIONS_IN_FLIGHT.fetch_add(1, Ordering::SeqCst);
        Self
    }

    pub fn any() -> bool {
        CREATIONS_IN_FLIGHT.load(Ordering::SeqCst) > 0
    }
}

impl Drop for CreationInFlight {
    fn drop(&mut self) {
        CREATIONS_IN_FLIGHT.fetch_sub(1, Ordering::SeqCst);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadySpare {
    pub paths: Vec<PathBuf>,
    pub base_ref: Option<String>,
    pub consolidated_worktrees: bool,
}

enum Spare {
    /// A `+` arriving now does its own checkout rather than queue behind this one.
    Building,
    Ready(ReadySpare),
}

pub type SpareKey = Vec<PathBuf>;

#[derive(Default)]
pub struct SpareWorktrees {
    spares: HashMap<SpareKey, Spare>,
}

impl Global for SpareWorktrees {}

impl SpareWorktrees {
    fn global(cx: &mut App) -> &mut Self {
        cx.default_global::<Self>()
    }

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

    pub fn needs_one(key: &SpareKey, cx: &mut App) -> bool {
        !Self::global(cx).spares.contains_key(key)
    }

    /// `false` when a spare is ready, already building, or a `+` is opening its window.
    pub fn start_building(key: SpareKey, cx: &mut App) -> bool {
        if CreationInFlight::any() {
            return false;
        }
        let spares = &mut Self::global(cx).spares;
        if spares.contains_key(&key) {
            return false;
        }
        spares.insert(key, Spare::Building);
        true
    }

    pub fn finish_building(key: SpareKey, spare: ReadySpare, cx: &mut App) {
        Self::global(cx).spares.insert(key, Spare::Ready(spare));
    }

    pub fn abandon_building(key: &SpareKey, cx: &mut App) {
        Self::global(cx).spares.remove(key);
    }

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
