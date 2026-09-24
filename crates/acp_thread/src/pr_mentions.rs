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

const HOST: &str = "github.com/";

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
    const DELIMITERS: &str = "\"'<>()[]{}`,;|";
    let span_end = after
        .find(|c: char| c.is_whitespace() || DELIMITERS.contains(c))
        .unwrap_or(after.len());
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
