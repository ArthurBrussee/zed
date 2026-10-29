//! quiet-ui: the process's physical footprint, which is what Activity Monitor
//! and a WindowServer stackshot report, next to the live counts of the
//! entities a day of agent threads piles up. `resident` swings by hundreds of
//! megabytes as macOS compresses and pages, and hid a 6.8 GB footprint.

use gpui::{App, AppContext as _};
use std::{
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

const LOG_INTERVAL: Duration = Duration::from_secs(5 * 60);

static MARKDOWNS: AtomicUsize = AtomicUsize::new(0);
static TERMINALS: AtomicUsize = AtomicUsize::new(0);
static BUFFERS: AtomicUsize = AtomicUsize::new(0);

pub(super) fn start(cx: &mut App) {
    count_live::<markdown::Markdown>(&MARKDOWNS, cx);
    count_live::<terminal::Terminal>(&TERMINALS, cx);
    count_live::<language::Buffer>(&BUFFERS, cx);

    let executor = cx.background_executor().clone();
    cx.background_spawn(async move {
        loop {
            executor.timer(LOG_INTERVAL).await;
            const MIB: u64 = 1024 * 1024;
            let footprint = physical_footprint().map_or_else(
                || "unknown".to_string(),
                |bytes| format!("{} MiB", bytes / MIB),
            );
            log::info!(
                "quiet-ui memory: footprint {footprint}; live Markdown {}, Terminal {}, Buffer {}",
                MARKDOWNS.load(Ordering::Relaxed),
                TERMINALS.load(Ordering::Relaxed),
                BUFFERS.load(Ordering::Relaxed),
            );
        }
    })
    .detach();
}

fn count_live<T: 'static>(counter: &'static AtomicUsize, cx: &App) {
    cx.observe_new(move |_: &mut T, _, cx| {
        counter.fetch_add(1, Ordering::Relaxed);
        cx.on_release(move |_, _| {
            counter.fetch_sub(1, Ordering::Relaxed);
        })
        .detach();
    })
    .detach();
}

#[cfg(target_os = "macos")]
fn physical_footprint() -> Option<u64> {
    let mut info = std::mem::MaybeUninit::<libc::rusage_info_v2>::zeroed();
    // SAFETY: `info` is a `rusage_info_v2`, the struct the V2 flavor fills.
    let result = unsafe {
        libc::proc_pid_rusage(
            std::process::id() as libc::c_int,
            libc::RUSAGE_INFO_V2,
            info.as_mut_ptr().cast::<libc::rusage_info_t>(),
        )
    };
    // SAFETY: zeroed, and filled in when the call succeeds.
    (result == 0).then(|| unsafe { info.assume_init() }.ri_phys_footprint)
}

#[cfg(not(target_os = "macos"))]
fn physical_footprint() -> Option<u64> {
    None
}
