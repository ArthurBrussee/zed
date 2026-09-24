//! Which pull requests a thread has named.
//!
//! A thread's pull requests are not only the ones its current branch has: it
//! opens one and prints the URL, or it works on one in another repository.
//! Both arrive as a URL in the transcript, so a URL is what this reads.
//!
//! Only whole GitHub pull-request URLs count. A bare `#123` is too common in
//! prose and commit subjects to be a statement that a thread watches that PR,
//! and a wrong PR in the set is worse than a missing one: the set is what the
//! chips show.
//!
//! Where the text came from matters as much as what is in it. A command
//! prints whatever it read — a `gh pr list`, a changelog, a search that hit a
//! release note — and mining that filled the set with pull requests nobody
//! had seen. Two things say what a thread is about: the agent's own prose,
//! and the output of the command that made a PR ([`creates_pull_request`]).

/// A pull request some text named: which repository, and which number.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PrMention {
    /// `owner/name`, exactly as the URL spelled it.
    pub repo: String,
    pub number: u64,
}

/// A piece of text naming this many distinct pull requests is a list, and a
/// list is not a thread saying which PRs it is about: nothing in it joins the
/// set. One or two is someone talking about their pull requests; ten is a
/// query's answer that happened to be pasted where the miner could read it.
const LIST_THRESHOLD: usize = 3;

/// The pull requests named in a message or a command's output, in the order
/// they appear and without repeats.
///
/// The text must be finished. For text that is still arriving, see
/// [`pr_mentions_so_far`].
pub fn pr_mentions(text: &str) -> Vec<PrMention> {
    let mut found: Vec<PrMention> = Vec::new();
    let mut searched = 0;
    while let Some(offset) = text[searched..].find(HOST) {
        let host_at = searched + offset;
        let after = &text[host_at + HOST.len()..];
        searched = host_at + HOST.len();
        if !host_is_its_own_word(&text[..host_at]) {
            continue;
        }
        let Some(mention) = pr_mention_after_host(after) else {
            continue;
        };
        if !found.contains(&mention) {
            found.push(mention);
        }
        if found.len() == LIST_THRESHOLD {
            return Vec::new();
        }
    }
    found
}

/// Whether a command line is the one that made a pull request, and so prints
/// the URL of one this thread is about.
///
/// Every other `gh` invocation is the agent looking at a PR rather than
/// working on one — `gh pr view`, `gh pr checkout`, `gh pr list` — and what
/// they print is what GitHub had to say, not what the thread is for. The line
/// goes through the command parser rather than a substring test, so
/// `gh pr create` inside a chain, a pipeline or a devshell still counts and a
/// `grep 'gh pr create'` does not.
pub fn creates_pull_request(command: &str) -> bool {
    crate::parse_command(command)
        .segments
        .iter()
        .any(|segment| {
            matches!(
                &segment.kind,
                crate::SegmentKind::GitHub { operation, .. } if operation == "pr create"
            )
        })
}

/// The pull requests named so far in text that is still being written.
///
/// A URL that runs to the very end of what has arrived is not a mention yet:
/// the next chunk may extend it, and every number a URL passes through on its
/// way is itself a valid pull request. A chunk boundary inside
/// `.../pull/15700` mines `157`, the pass after it mines `15700`, and both
/// land in the set — which is how `#157` appeared beside `#15700`.
pub fn pr_mentions_so_far(text: &str) -> Vec<PrMention> {
    pr_mentions(&text[..unfinished_url_start(text).unwrap_or(text.len())])
}

/// Where the URL the text ends inside begins, if it ends inside one. A span
/// that nothing has closed — no whitespace, no delimiter — is one the writer
/// has not finished.
fn unfinished_url_start(text: &str) -> Option<usize> {
    let mut searched = 0;
    let mut last_host = None;
    while let Some(offset) = text[searched..].find(HOST) {
        let host_at = searched + offset;
        searched = host_at + HOST.len();
        if host_is_its_own_word(&text[..host_at]) {
            last_host = Some(host_at);
        }
    }
    let host_at = last_host?;
    let closed = text[host_at + HOST.len()..].contains(is_url_end);
    (!closed).then_some(host_at)
}

const HOST: &str = "github.com/";

/// What ends a URL: the first character that cannot be in one.
fn is_url_end(c: char) -> bool {
    const DELIMITERS: &str = "\"'<>()[]{}`,;|";
    c.is_whitespace() || DELIMITERS.contains(c)
}

/// Whether the text before `github.com/` leaves it a host rather than the
/// tail of some other word: a scheme's `//`, `www.`, or nothing at all.
/// Without this, `notgithub.com/owner/name/pull/1` would read as GitHub.
fn host_is_its_own_word(before: &str) -> bool {
    match before.chars().next_back() {
        None => true,
        Some('/') | Some('.') => true,
        Some(c) => !c.is_alphanumeric() && c != '-' && c != '_',
    }
}

/// The pull request named by the path after the host. The URL is taken to run
/// to the first character that cannot be in one, which is what finds it inside
/// a markdown link or before a sentence's full stop.
fn pr_mention_after_host(after: &str) -> Option<PrMention> {
    let span_end = after.find(is_url_end).unwrap_or(after.len());
    let mut parts = after[..span_end].split('/');

    let owner = parts.next()?;
    let name = parts.next()?;
    // `pull` is what a web URL says; `pulls` is the API's plural, and tools
    // print both.
    match parts.next()? {
        "pull" | "pulls" => {}
        _ => return None,
    }
    // `.../pull/123/files`, `.../pull/123#discussion` and a `.../pull/123.`
    // that ends a sentence all name 123.
    let number = parts
        .next()?
        .split(['#', '?', '.'])
        .next()?
        .parse::<u64>()
        .ok()
        .filter(|number| *number > 0)?;

    if owner.is_empty() || name.is_empty() {
        return None;
    }
    Some(PrMention {
        repo: format!("{owner}/{name}"),
        number,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mention(repo: &str, number: u64) -> PrMention {
        PrMention {
            repo: repo.into(),
            number,
        }
    }

    /// The case from the report: adding #15700 also added #157.
    ///
    /// A message arriving in chunks is read on every pass, and the numbers a
    /// URL passes through on its way are all valid pull requests, so a chunk
    /// boundary inside one mines a shorter PR that nobody ever named.
    #[test]
    fn test_a_url_still_arriving_is_not_a_mention_yet() {
        let whole = "Opened https://github.com/zed-industries/zed/pull/15700";
        // Every prefix of the message, as the chunks would deliver it.
        for split in 0..whole.len() {
            let so_far = &whole[..split];
            assert_eq!(
                pr_mentions_so_far(so_far),
                Vec::new(),
                "{so_far:?} names no finished pull request"
            );
        }
        // Including the whole message: nothing has closed the URL, so as far
        // as this can tell the next chunk may still extend it. It is the
        // entry finishing that settles it, and a finished entry is read with
        // `pr_mentions`, which names the one it actually says.
        assert_eq!(pr_mentions_so_far(whole), Vec::new());
        assert_eq!(pr_mentions(whole), vec![mention("zed-industries/zed", 15700)]);

        // A URL something has closed is finished even with more to come: the
        // chips should not wait for the end of a paragraph.
        assert_eq!(
            pr_mentions_so_far("Opened https://github.com/zed-industries/zed/pull/15700 and now"),
            vec![mention("zed-industries/zed", 15700)]
        );
        assert_eq!(
            pr_mentions_so_far("See (https://github.com/zed-industries/zed/pull/15700)"),
            vec![mention("zed-industries/zed", 15700)],
            "a closing bracket ends the span the same way whitespace does"
        );

        // Only the last URL can be the unfinished one; the ones before it
        // were closed by whatever came after them.
        assert_eq!(
            pr_mentions_so_far(
                "First https://github.com/o/n/pull/12, then https://github.com/o/n/pull/34"
            ),
            vec![mention("o/n", 12)]
        );

        // Text that ends in something that is not a URL at all is finished.
        assert_eq!(pr_mentions_so_far("nothing here"), Vec::new());
        assert_eq!(
            pr_mentions_so_far("done: https://github.com/o/n/pull/7\n"),
            vec![mention("o/n", 7)]
        );
    }

    /// What `gh pr create` prints when it has made one: the URL on its own
    /// line, which is the whole reason this reads output at all.
    #[test]
    fn test_gh_pr_create_output_names_its_pr() {
        let output = "Creating pull request for quiet-ui into main in ArthurBrussee/zed\n\
                      \n\
                      https://github.com/ArthurBrussee/zed/pull/412\n";
        assert_eq!(pr_mentions(output), vec![mention("ArthurBrussee/zed", 412)]);
    }

    #[test]
    fn test_a_url_in_prose_is_found_through_its_punctuation() {
        let text = "I opened (https://github.com/zed-industries/zed/pull/64463), \
                    see also <https://github.com/zed-industries/zed/pull/64464>.";
        assert_eq!(
            pr_mentions(text),
            vec![
                mention("zed-industries/zed", 64463),
                mention("zed-industries/zed", 64464),
            ]
        );
    }

    #[test]
    fn test_a_deep_link_still_names_its_pr() {
        assert_eq!(
            pr_mentions("https://github.com/owner/name/pull/7/files"),
            vec![mention("owner/name", 7)]
        );
        assert_eq!(
            pr_mentions("https://github.com/owner/name/pull/7#discussion_r1"),
            vec![mention("owner/name", 7)]
        );
    }

    #[test]
    fn test_a_markdown_link_names_its_pr() {
        assert_eq!(
            pr_mentions("[#412](https://github.com/ArthurBrussee/zed/pull/412)"),
            vec![mention("ArthurBrussee/zed", 412)]
        );
    }

    #[test]
    fn test_the_same_pr_named_twice_joins_once() {
        let text = "https://github.com/owner/name/pull/3 and again \
                    https://github.com/owner/name/pull/3";
        assert_eq!(pr_mentions(text), vec![mention("owner/name", 3)]);
    }

    #[test]
    fn test_what_is_not_a_pull_request() {
        // An issue is not a pull request, a bare number is not a statement,
        // and another host's URL is not GitHub's.
        assert!(pr_mentions("https://github.com/owner/name/issues/3").is_empty());
        assert!(pr_mentions("fixes #3 in the parser").is_empty());
        assert!(pr_mentions("https://gitlab.com/owner/name/pull/3").is_empty());
        assert!(pr_mentions("https://github.com/owner/name").is_empty());
        assert!(pr_mentions("https://github.com/owner/name/pull/none").is_empty());
        assert!(pr_mentions("https://github.com/owner/name/pull/0").is_empty());
    }

    #[test]
    fn test_a_page_of_urls_is_not_a_thread_s_pull_requests() {
        let text = (1..=20)
            .map(|n| format!("https://github.com/owner/name/pull/{n}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(pr_mentions(&text).is_empty());
    }

    /// The complaint this rule came from: a `gh pr list` names ten, and the
    /// old cap took the first eight of them.
    #[test]
    fn test_a_pr_list_joins_nothing() {
        let output = "\
Showing 4 of 4 open pull requests in ArthurBrussee/zed

#412  Narrow PR mining        quiet-ui    https://github.com/ArthurBrussee/zed/pull/412
#411  Mark points in a thread quiet-ui    https://github.com/ArthurBrussee/zed/pull/411
#410  Kill a command          quiet-ui    https://github.com/ArthurBrussee/zed/pull/410
#409  Read the switch         quiet-ui    https://github.com/ArthurBrussee/zed/pull/409
";
        assert!(pr_mentions(output).is_empty());
    }

    /// One or two is a thread talking about its own pull requests, which is
    /// the case the rule has to leave alone.
    #[test]
    fn test_one_or_two_is_not_a_list() {
        assert_eq!(
            pr_mentions("Opened https://github.com/owner/name/pull/1."),
            vec![mention("owner/name", 1)]
        );
        assert_eq!(
            pr_mentions(
                "Opened https://github.com/owner/name/pull/1 and \
                 https://github.com/owner/name/pull/2."
            ),
            vec![mention("owner/name", 1), mention("owner/name", 2)]
        );
    }

    /// The same PR named four times is one pull request, not a list: the
    /// threshold counts distinct ones.
    #[test]
    fn test_repeats_do_not_make_a_list() {
        let text = "https://github.com/owner/name/pull/9 ".repeat(4);
        assert_eq!(pr_mentions(&text), vec![mention("owner/name", 9)]);
    }

    #[test]
    fn test_only_creating_a_pr_counts_as_making_one() {
        assert!(creates_pull_request("gh pr create --fill"));
        assert!(creates_pull_request(
            "git push -u origin quiet-ui && gh pr create --fill --base main"
        ));
        assert!(creates_pull_request(
            "nix develop .#vision-dev --command gh pr create --fill"
        ));

        // Looking at a pull request is not opening one.
        assert!(!creates_pull_request("gh pr view 412"));
        assert!(!creates_pull_request("gh pr list --state open"));
        assert!(!creates_pull_request("gh pr checkout 412"));
        assert!(!creates_pull_request("gh pr diff 412"));
        assert!(!creates_pull_request("gh issue create --title x"));
        // Talking about the command is not running it.
        assert!(!creates_pull_request("rg 'gh pr create' script/"));
    }
}
