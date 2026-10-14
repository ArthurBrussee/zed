//! Which pull requests a thread has named, read from whole GitHub PR URLs in
//! the agent's prose and in the output of `gh pr create`. A bare `#123` does
//! not count: a wrong PR in the set is worse than a missing one.

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PrMention {
    /// `owner/name`.
    pub repo: String,
    pub number: u64,
}

/// Text naming this many distinct pull requests is a list (a query's answer),
/// not a thread saying which PRs it is about, so none of them count.
const LIST_THRESHOLD: usize = 3;

/// The pull requests named in a text, in order and without repeats.
///
/// The text must have finished arriving: every prefix of `/pull/15700` is a
/// valid PR number, and two thirds of a list is not a list.
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

/// Whether a command line ran `gh pr create`, whose output names a PR this
/// thread is about. Parsed, so `grep 'gh pr create'` does not count.
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

fn is_url_end(c: char) -> bool {
    const DELIMITERS: &str = "\"'<>()[]{}`,;|";
    c.is_whitespace() || DELIMITERS.contains(c)
}

/// Rejects `notgithub.com/…`.
fn host_is_its_own_word(before: &str) -> bool {
    match before.chars().next_back() {
        None => true,
        Some('/') | Some('.') => true,
        Some(c) => !c.is_alphanumeric() && c != '-' && c != '_',
    }
}

fn pr_mention_after_host(after: &str) -> Option<PrMention> {
    let span_end = after.find(is_url_end).unwrap_or(after.len());
    let mut parts = after[..span_end].split('/');

    let owner = parts.next()?;
    let name = parts.next()?;
    // `pulls` is the API's spelling.
    match parts.next()? {
        "pull" | "pulls" => {}
        _ => return None,
    }
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

    /// Why the caller may only read an entry once it has finished arriving:
    /// this is the hazard, not a guard against it.
    #[test]
    fn test_half_a_url_reads_as_a_different_pull_request() {
        let whole = "Opened https://github.com/zed-industries/zed/pull/15700";
        let mined: Vec<u64> = (0..=whole.len())
            .flat_map(|split| pr_mentions(&whole[..split]))
            .map(|mention| mention.number)
            .collect();
        assert_eq!(mined, vec![1, 15, 157, 1570, 15700]);
        assert_eq!(
            pr_mentions(whole),
            vec![mention("zed-industries/zed", 15700)]
        );

        let list = "Looked at https://github.com/o/n/pull/1, \
                    https://github.com/o/n/pull/2 and https://github.com/o/n/pull/3.";
        assert_eq!(pr_mentions(list), Vec::new(), "three is a list");
        let first_two = &list[..list.find("and").unwrap()];
        assert_eq!(
            pr_mentions(first_two),
            vec![mention("o/n", 1), mention("o/n", 2)],
        );
    }

    #[test]
    fn test_urls_are_found_through_their_surroundings() {
        for (text, expected) in [
            (
                "Creating pull request for quiet-ui into main in ArthurBrussee/zed\n\n\
                 https://github.com/ArthurBrussee/zed/pull/412\n",
                vec![mention("ArthurBrussee/zed", 412)],
            ),
            (
                "I opened (https://github.com/zed-industries/zed/pull/64463), \
                 see also <https://github.com/zed-industries/zed/pull/64464>.",
                vec![
                    mention("zed-industries/zed", 64463),
                    mention("zed-industries/zed", 64464),
                ],
            ),
            (
                "https://github.com/owner/name/pull/7/files",
                vec![mention("owner/name", 7)],
            ),
            (
                "https://github.com/owner/name/pull/7#discussion_r1",
                vec![mention("owner/name", 7)],
            ),
            (
                "[#412](https://github.com/ArthurBrussee/zed/pull/412)",
                vec![mention("ArthurBrussee/zed", 412)],
            ),
            // Repeats count once, and so do not make a list.
            (
                "https://github.com/owner/name/pull/9 ".repeat(4).as_str(),
                vec![mention("owner/name", 9)],
            ),
        ] {
            assert_eq!(pr_mentions(text), expected, "{text}");
        }
    }

    #[test]
    fn test_what_is_not_a_pull_request() {
        for text in [
            "https://github.com/owner/name/issues/3",
            "fixes #3 in the parser",
            "https://gitlab.com/owner/name/pull/3",
            "https://notgithub.com/owner/name/pull/3",
            "https://github.com/owner/name",
            "https://github.com/owner/name/pull/none",
            "https://github.com/owner/name/pull/0",
        ] {
            assert!(pr_mentions(text).is_empty(), "{text}");
        }
    }

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

    #[test]
    fn test_only_creating_a_pr_counts_as_making_one() {
        assert!(creates_pull_request("gh pr create --fill"));
        assert!(creates_pull_request(
            "git push -u origin quiet-ui && gh pr create --fill --base main"
        ));
        assert!(creates_pull_request(
            "nix develop .#vision-dev --command gh pr create --fill"
        ));

        assert!(!creates_pull_request("gh pr view 412"));
        assert!(!creates_pull_request("gh pr list --state open"));
        assert!(!creates_pull_request("gh pr checkout 412"));
        assert!(!creates_pull_request("gh pr diff 412"));
        assert!(!creates_pull_request("gh issue create --title x"));
        assert!(!creates_pull_request("rg 'gh pr create' script/"));
    }
}
