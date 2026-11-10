//! quiet-ui: a PR chip's click goes to the browser tab already showing that PR
//! rather than piling up another one. macOS only, for Chrome and Safari; every
//! other case opens the URL as any link does.

use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use futures::FutureExt as _;
use gpui::App;

/// macOS asks once for permission to control the browser, and the script waits
/// on that answer; past this the PR opens in a new tab as it always did.
const TAB_SEARCH_TIMEOUT: Duration = Duration::from_secs(1);

pub(crate) fn install(cx: &mut App) {
    cx.set_global(ui::PrUrlOpener(open_pr_url));
}

fn open_pr_url(url: String, cx: &mut App) {
    if !cfg!(target_os = "macos") {
        cx.open_url(&url);
        return;
    }
    let executor = cx.background_executor().clone();
    cx.spawn(async move |cx| {
        let focused = futures::select_biased! {
            focused = focus_existing_tab(&url).fuse() => focused,
            _ = executor.timer(TAB_SEARCH_TIMEOUT).fuse() => {
                Err(anyhow::anyhow!("the browser did not answer in time"))
            }
        };
        match focused {
            Ok(true) => {}
            Ok(false) => cx.update(|cx| cx.open_url(&url)),
            Err(error) => {
                log::debug!("quiet-ui: no existing tab for {url}: {error:#}");
                cx.update(|cx| cx.open_url(&url));
            }
        }
    })
    .detach();
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Browser {
    Chrome,
    Safari,
}

impl Browser {
    fn from_bundle_id(bundle_id: &str) -> Option<Self> {
        match bundle_id.to_ascii_lowercase().as_str() {
            "com.google.chrome" => Some(Self::Chrome),
            "com.apple.safari" => Some(Self::Safari),
            _ => None,
        }
    }

    fn bundle_id(self) -> &'static str {
        match self {
            Self::Chrome => "com.google.Chrome",
            Self::Safari => "com.apple.Safari",
        }
    }
}

/// Whether the tab is on the PR's page or one of its own (`/files`, `/checks`).
fn tab_shows_pr(tab_url: &str, pr_url: &str) -> bool {
    let pr_url = pr_url.trim_end_matches('/');
    tab_url
        .strip_prefix(pr_url)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with(['/', '?', '#']))
}

/// The https handler `defaults read` lists, if one was ever chosen; macOS
/// falls back to Safari when none was.
fn default_browser_bundle_id(ls_handlers: &str) -> Option<String> {
    let mut depth = 0usize;
    let mut scheme = None;
    let mut handler = None;
    for line in ls_handlers.lines() {
        let line = line.trim();
        if line.ends_with('{') {
            depth += 1;
            if depth == 1 {
                scheme = None;
                handler = None;
            }
            continue;
        }
        if line.starts_with('}') {
            if depth == 1 && scheme.as_deref() == Some("https") {
                return handler;
            }
            depth = depth.saturating_sub(1);
            continue;
        }
        if depth != 1 {
            continue;
        }
        let Some((key, value)) = line.trim_end_matches(';').split_once(" = ") else {
            continue;
        };
        let value = value.trim().trim_matches('"').to_string();
        match key.trim() {
            "LSHandlerURLScheme" => scheme = Some(value.to_ascii_lowercase()),
            "LSHandlerRoleAll" => handler = Some(value),
            _ => {}
        }
    }
    None
}

/// One line per tab, `window id<TAB>tab index<TAB>url`, or nothing when the
/// browser is not running, so asking never launches it.
fn list_tabs_script(browser: Browser) -> String {
    let bundle_id = browser.bundle_id();
    format!(
        r#"set fieldSeparator to character id 9
if application id "{bundle_id}" is not running then return ""
set listing to ""
tell application id "{bundle_id}"
	repeat with browserWindow in windows
		try
			set windowId to id of browserWindow
			set tabUrls to URL of tabs of browserWindow
			repeat with tabIndex from 1 to count of tabUrls
				set listing to listing & windowId & fieldSeparator & tabIndex & fieldSeparator & (item tabIndex of tabUrls) & linefeed
			end repeat
		end try
	end repeat
end tell
return listing"#
    )
}

fn select_tab_script(browser: Browser, window_id: u64, tab_index: u64) -> String {
    let bundle_id = browser.bundle_id();
    let select = match browser {
        Browser::Chrome => format!("set active tab index of browserWindow to {tab_index}"),
        Browser::Safari => {
            format!("set current tab of browserWindow to tab {tab_index} of browserWindow")
        }
    };
    format!(
        r#"tell application id "{bundle_id}"
	set browserWindow to window id {window_id}
	{select}
	set index of browserWindow to 1
	activate
end tell"#
    )
}

/// The first listed tab showing the PR, as its window id and tab index.
fn find_pr_tab(listing: &str, pr_url: &str) -> Option<(u64, u64)> {
    listing.lines().find_map(|line| {
        let mut fields = line.splitn(3, '\t');
        let window_id = fields.next()?.trim().parse().ok()?;
        let tab_index = fields.next()?.trim().parse().ok()?;
        let url = fields.next()?.trim();
        tab_shows_pr(url, pr_url).then_some((window_id, tab_index))
    })
}

async fn focus_existing_tab(pr_url: &str) -> Result<bool> {
    let ls_handlers = run(
        "defaults",
        &[
            "read",
            "com.apple.LaunchServices/com.apple.launchservices.secure",
            "LSHandlers",
        ],
    )
    .await
    .unwrap_or_default();
    let bundle_id =
        default_browser_bundle_id(&ls_handlers).unwrap_or_else(|| "com.apple.Safari".into());
    let Some(browser) = Browser::from_bundle_id(&bundle_id) else {
        return Ok(false);
    };
    let listing = run("osascript", &["-e", &list_tabs_script(browser)]).await?;
    let Some((window_id, tab_index)) = find_pr_tab(&listing, pr_url) else {
        return Ok(false);
    };
    run(
        "osascript",
        &["-e", &select_tab_script(browser, window_id, tab_index)],
    )
    .await?;
    Ok(true)
}

/// Killed if dropped, which is how the caller's timeout ends a script stuck on
/// the permission prompt.
async fn run(program: &str, args: &[&str]) -> Result<String> {
    use util::command::Stdio;

    let child = util::command::new_command(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("failed to run {program}"))?;
    let output = child.output().await?;
    if !output.status.success() {
        bail!(
            "{program} exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    const PR: &str = "https://github.com/ArthurBrussee/zed/pull/12";

    #[test]
    fn test_a_tab_on_the_pr_or_its_pages_matches() {
        assert!(tab_shows_pr(PR, PR));
        assert!(tab_shows_pr(&format!("{PR}/"), PR));
        assert!(tab_shows_pr(&format!("{PR}/files"), PR));
        assert!(tab_shows_pr(&format!("{PR}/checks"), PR));
        assert!(tab_shows_pr(&format!("{PR}/files?w=1"), PR));
        assert!(tab_shows_pr(&format!("{PR}#issuecomment-1"), PR));
        assert!(tab_shows_pr(PR, &format!("{PR}/")));

        // Another PR whose number starts the same way.
        assert!(!tab_shows_pr(&format!("{PR}3"), PR));
        assert!(!tab_shows_pr(
            "https://github.com/ArthurBrussee/zed/pull/1",
            PR
        ));
        assert!(!tab_shows_pr(
            "https://github.com/ArthurBrussee/zed/pulls",
            PR
        ));
    }

    #[test]
    fn test_the_listing_finds_the_first_tab_showing_the_pr() {
        let listing = format!(
            "101\t1\thttps://github.com/ArthurBrussee/zed\n\
             101\t2\t{PR}3\n\
             205\t1\tmissing value\n\
             205\t4\t{PR}/checks\n\
             205\t5\t{PR}\n"
        );
        assert_eq!(find_pr_tab(&listing, PR), Some((205, 4)));
        assert_eq!(find_pr_tab("", PR), None);
        assert_eq!(
            find_pr_tab("101\t1\thttps://github.com/ArthurBrussee/zed\n", PR),
            None
        );
    }

    #[test]
    fn test_the_default_browser_is_read_from_the_https_handler() {
        // `defaults read com.apple.LaunchServices/com.apple.launchservices.secure LSHandlers`
        let ls_handlers = r#"(
        {
        LSHandlerContentType = "public.html";
        LSHandlerPreferredVersions =         {
            LSHandlerRoleAll = "-";
        };
        LSHandlerRoleAll = "com.apple.safari";
    },
        {
        LSHandlerPreferredVersions =         {
            LSHandlerRoleAll = "-";
        };
        LSHandlerRoleAll = "com.google.chrome";
        LSHandlerURLScheme = https;
    },
        {
        LSHandlerRoleAll = "com.microsoft.edgemac";
        LSHandlerURLScheme = mailto;
    }
)"#;
        assert_eq!(
            default_browser_bundle_id(ls_handlers).as_deref(),
            Some("com.google.chrome")
        );
        assert_eq!(
            Browser::from_bundle_id("com.google.chrome"),
            Some(Browser::Chrome)
        );
        assert_eq!(
            Browser::from_bundle_id("com.apple.Safari"),
            Some(Browser::Safari)
        );
        assert_eq!(Browser::from_bundle_id("org.mozilla.firefox"), None);
        assert_eq!(default_browser_bundle_id(""), None);
    }

    #[test]
    fn test_the_scripts_never_launch_the_browser_and_select_by_window_id() {
        let list = list_tabs_script(Browser::Chrome);
        assert!(
            list.contains(r#"if application id "com.google.Chrome" is not running then return """#)
        );
        assert!(list.contains(r#"tell application id "com.google.Chrome""#));
        assert!(list.contains("set tabUrls to URL of tabs of browserWindow"));

        assert_eq!(
            select_tab_script(Browser::Chrome, 205, 4),
            "tell application id \"com.google.Chrome\"\n\
             \tset browserWindow to window id 205\n\
             \tset active tab index of browserWindow to 4\n\
             \tset index of browserWindow to 1\n\
             \tactivate\n\
             end tell"
        );
        assert_eq!(
            select_tab_script(Browser::Safari, 7, 2),
            "tell application id \"com.apple.Safari\"\n\
             \tset browserWindow to window id 7\n\
             \tset current tab of browserWindow to tab 2 of browserWindow\n\
             \tset index of browserWindow to 1\n\
             \tactivate\n\
             end tell"
        );
    }
}
