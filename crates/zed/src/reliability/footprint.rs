//! quiet-ui: the process's physical footprint, which is what Activity Monitor
//! and a WindowServer stackshot report, next to the live counts of the
//! entities a day of agent threads piles up. `resident` swings by hundreds of
//! megabytes as macOS compresses and pages, and hid a 6.8 GB footprint.

use acp_thread::AcpThread;
use gpui::{App, WeakEntity};
use std::{
    cell::RefCell,
    rc::Rc,
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

const LOG_INTERVAL: Duration = Duration::from_secs(5 * 60);

static MARKDOWNS: AtomicUsize = AtomicUsize::new(0);
static TERMINALS: AtomicUsize = AtomicUsize::new(0);
static BUFFERS: AtomicUsize = AtomicUsize::new(0);
static WORKSPACES: AtomicUsize = AtomicUsize::new(0);

/// Threads that held the most entries, named in the log line so a leak of
/// closed ones says which.
const HEAVIEST_THREADS: usize = 3;

pub(super) fn start(cx: &mut App) {
    count_live::<markdown::Markdown>(&MARKDOWNS, cx);
    count_live::<terminal::Terminal>(&TERMINALS, cx);
    count_live::<language::Buffer>(&BUFFERS, cx);
    // A workspace loaded and never dropped (one opened to archive a thread,
    // say) holds its agent panel and every thread restored into it.
    count_live::<workspace::Workspace>(&WORKSPACES, cx);
    let threads: Rc<RefCell<Vec<WeakEntity<AcpThread>>>> = Rc::default();
    cx.observe_new({
        let threads = threads.clone();
        move |_: &mut AcpThread, _, cx| threads.borrow_mut().push(cx.weak_entity())
    })
    .detach();

    cx.spawn(async move |cx| {
        loop {
            cx.background_executor().timer(LOG_INTERVAL).await;
            const MIB: u64 = 1024 * 1024;
            let footprint = physical_footprint().map_or_else(
                || "unknown".to_string(),
                |bytes| format!("{} MiB", bytes / MIB),
            );
            let threads = cx.update(|cx| live_threads(&threads, cx));
            log::info!(
                "quiet-ui memory: footprint {footprint}; live Markdown {}, Terminal {}, Buffer {}, Workspace {}; {threads}",
                MARKDOWNS.load(Ordering::Relaxed),
                TERMINALS.load(Ordering::Relaxed),
                BUFFERS.load(Ordering::Relaxed),
                WORKSPACES.load(Ordering::Relaxed),
            );
        }
    })
    .detach();
}

/// "N threads live, heaviest: title (entries), …", dropping released ones.
fn live_threads(threads: &RefCell<Vec<WeakEntity<AcpThread>>>, cx: &App) -> String {
    let mut threads = threads.borrow_mut();
    threads.retain(|thread| thread.upgrade().is_some());
    let mut sizes: Vec<(usize, String)> = threads
        .iter()
        .filter_map(|thread| thread.upgrade())
        .map(|thread| {
            let thread = thread.read(cx);
            let title = thread
                .title()
                .map_or_else(|| "untitled".to_string(), |title| title.to_string());
            (thread.entries().len(), title)
        })
        .collect();
    sizes.sort_by_key(|(entries, _)| std::cmp::Reverse(*entries));
    let heaviest = sizes
        .iter()
        .take(HEAVIEST_THREADS)
        .map(|(entries, title)| format!("{title:?} ({entries} entries)"))
        .collect::<Vec<_>>()
        .join(", ");
    format!("{} threads live, heaviest: {heaviest}", sizes.len())
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
