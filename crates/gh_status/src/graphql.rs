//! The batched `gh api graphql` poll: one alias per subject, grouped under a `repository` alias
//! each. The answer is lowered to the [`GhPr`] the `gh pr list` path produces, so what a check
//! amounts to is decided in one place.

use std::fmt::Write as _;

use anyhow::{Context as _, Result, bail};
use collections::HashMap;
use serde::Deserialize;

use crate::{CheckCounts, GhCheck, GhPr};

/// PRs sharing one head branch; `gh pr list` defaults to thirty, which is a node count worth not
/// spending across a batch of fifty.
const PRS_PER_BRANCH: usize = 10;

const CHECKS_PER_PR: usize = 100;

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct RepoId {
    pub owner: String,
    pub name: String,
}

impl RepoId {
    /// Reads `owner/name`, as `gh repo view` reports it and a PR mention carries it.
    pub(crate) fn parse(name_with_owner: &str) -> Result<Self> {
        let trimmed = name_with_owner.trim();
        let Some((owner, name)) = trimmed.split_once('/') else {
            bail!("expected owner/name, got {trimmed:?}");
        };
        if owner.is_empty() || name.is_empty() || name.contains('/') {
            bail!("expected owner/name, got {trimmed:?}");
        }
        Ok(Self {
            owner: owner.to_string(),
            name: name.to_string(),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Ask {
    Branch(String),
    Number(u64),
}

impl Ask {
    /// The index keeps the alias unique when two watches land on the same repository and number.
    pub(crate) fn alias(&self, index: usize) -> String {
        match self {
            Ask::Branch(_) => format!("b{index}"),
            Ask::Number(_) => format!("p{index}"),
        }
    }
}

/// Repository aliases are positional (`r0`, `r1`, ...), so building the query and reading the
/// answer back both number repositories in this order.
fn sorted_repos(asks: &[(RepoId, String, Ask)]) -> Vec<&RepoId> {
    let mut repos = asks.iter().map(|(repo, _, _)| repo).collect::<Vec<_>>();
    repos.sort();
    repos.dedup();
    repos
}

pub(crate) fn build_query(asks: &[(RepoId, String, Ask)]) -> String {
    let mut query = String::from("query {\n");
    for (index, repo) in sorted_repos(asks).into_iter().enumerate() {
        writeln!(
            query,
            "  r{index}: repository(owner: \"{}\", name: \"{}\") {{",
            escape(&repo.owner),
            escape(&repo.name)
        )
        .ok();
        for (_, alias, ask) in asks.iter().filter(|(other, _, _)| other == repo) {
            match ask {
                Ask::Branch(branch) => {
                    writeln!(
                        query,
                        "    {alias}: pullRequests(headRefName: \"{}\", first: {PRS_PER_BRANCH}, \
                         orderBy: {{field: CREATED_AT, direction: DESC}}) {{ nodes {{ ...pr }} }}",
                        escape(branch)
                    )
                    .ok();
                }
                Ask::Number(number) => {
                    writeln!(
                        query,
                        "    {alias}: pullRequest(number: {number}) {{ ...pr }}"
                    )
                    .ok();
                }
            }
        }
        query.push_str("  }\n");
    }
    query.push_str("  rateLimit { cost remaining resetAt }\n}\n");
    // Spelled as a GraphQL variable so that a lost substitution is refused by name rather than
    // quietly asking for a different number of checks.
    query.push_str(&PR_FRAGMENT.replace("$checks", &CHECKS_PER_PR.to_string()));
    query
}

/// A fragment rather than a copy per alias: the query goes out on the command line.
const PR_FRAGMENT: &str = "\
fragment pr on PullRequest {
  number
  url
  title
  state
  isDraft
  reviewDecision
  mergeable
  mergeStateStatus
  commits(last: 1) {
    nodes {
      commit {
        statusCheckRollup {
          state
          contexts(first: $checks) {
            checkRunCountsByState { state count }
            statusContextCountsByState { state count }
            nodes {
              __typename
              ... on CheckRun {
                name
                conclusion
                checkSuite { workflowRun { workflow { name } } }
              }
              ... on StatusContext {
                context
                state
              }
            }
          }
        }
      }
    }
  }
}
";

fn escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            _ => escaped.push(ch),
        }
    }
    escaped
}

#[derive(Debug)]
pub(crate) enum Answer {
    /// An empty list is an answer: the branch has no pull request.
    Prs(Vec<GhPr>),
    /// GitHub answered, and there is no such pull request.
    Missing,
    /// Nothing usable came back, so the caller asks again per subject rather than blanking a chip.
    Unanswered(String),
}

#[derive(Debug, Default)]
pub(crate) struct Batch {
    pub answers: HashMap<String, Answer>,
    pub rate_limit: Option<RateLimit>,
    /// Query-level errors GitHub reported alongside partial data.
    pub errors: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub(crate) struct RateLimit {
    #[serde(default)]
    pub cost: u32,
    #[serde(default)]
    pub remaining: u32,
    #[serde(rename = "resetAt", default)]
    pub reset_at: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Response {
    #[serde(default)]
    data: Option<ResponseData>,
    #[serde(default)]
    errors: Vec<ResponseError>,
}

#[derive(Debug, Deserialize)]
struct ResponseError {
    #[serde(default)]
    message: String,
}

#[derive(Debug, Deserialize)]
struct ResponseData {
    #[serde(rename = "rateLimit", default)]
    rate_limit: Option<RateLimit>,
    /// A repository `gh` cannot see comes back as null.
    #[serde(flatten)]
    repositories: HashMap<String, Option<HashMap<String, serde_json::Value>>>,
}

/// Driven by the asks rather than by the response, so an alias GitHub left out is reported as
/// unanswered instead of silently vanishing.
pub(crate) fn parse_batch(json: &str, asks: &[(RepoId, String, Ask)]) -> Result<Batch> {
    let response: Response =
        serde_json::from_str(json).context("failed to parse gh api graphql output")?;
    let errors = response
        .errors
        .into_iter()
        .map(|error| error.message)
        .filter(|message| !message.is_empty())
        .collect::<Vec<_>>();
    let Some(data) = response.data else {
        if let Some(first) = errors.first() {
            bail!("gh api graphql reported: {first}");
        }
        bail!("gh api graphql answered without data");
    };

    let repos = sorted_repos(asks);
    let mut answers = HashMap::default();
    for (repo, alias, ask) in asks {
        let subjects = repos
            .iter()
            .position(|known| *known == repo)
            .and_then(|index| data.repositories.get(&format!("r{index}")))
            .and_then(|repo| repo.as_ref());
        let answer = match subjects.map(|subjects| subjects.get(alias)) {
            None => Answer::Unanswered(format!(
                "{}/{} was not in the answer",
                repo.owner, repo.name
            )),
            Some(None) => Answer::Unanswered(format!("alias {alias} was not in the answer")),
            Some(Some(value)) => read_answer(value, repo, ask),
        };
        answers.insert(alias.clone(), answer);
    }

    Ok(Batch {
        answers,
        rate_limit: data.rate_limit,
        errors,
    })
}

fn read_answer(value: &serde_json::Value, repo: &RepoId, ask: &Ask) -> Answer {
    match ask {
        Ask::Branch(_) => match serde_json::from_value::<Connection<PrNode>>(value.clone()) {
            Ok(connection) => Answer::Prs(
                connection
                    .nodes
                    .into_iter()
                    .map(|node| node.into_gh_pr(repo))
                    .collect(),
            ),
            Err(error) => Answer::Unanswered(format!("{error}")),
        },
        Ask::Number(_) => match serde_json::from_value::<Option<PrNode>>(value.clone()) {
            Ok(Some(node)) => Answer::Prs(vec![node.into_gh_pr(repo)]),
            Ok(None) => Answer::Missing,
            Err(error) => Answer::Unanswered(format!("{error}")),
        },
    }
}

#[derive(Debug, Deserialize)]
struct Connection<T> {
    #[serde(default = "Vec::new")]
    nodes: Vec<T>,
}

#[derive(Debug, Deserialize)]
struct PrNode {
    #[serde(flatten)]
    pr: GhPr,
    #[serde(default)]
    commits: Option<Connection<CommitNode>>,
}

#[derive(Debug, Deserialize)]
struct CommitNode {
    #[serde(default)]
    commit: Option<Commit>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Commit {
    #[serde(default)]
    status_check_rollup: Option<Rollup>,
}

#[derive(Debug, Deserialize)]
struct Rollup {
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    contexts: Option<Contexts>,
}

/// The rollup's contexts, plus the counts GitHub tallies over all of them rather than
/// over the page `nodes` returns.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Contexts {
    #[serde(default = "Vec::new")]
    nodes: Vec<ContextNode>,
    #[serde(default = "Vec::new")]
    check_run_counts_by_state: Vec<StateCount>,
    #[serde(default = "Vec::new")]
    status_context_counts_by_state: Vec<StateCount>,
}

#[derive(Debug, Deserialize)]
struct StateCount {
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    count: usize,
}

/// A check run or a commit status. A shape GitHub adds later reads as a check with nothing set,
/// which is pending, so it cannot make a PR look greener than it is.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ContextNode {
    #[serde(flatten)]
    check: GhCheck,
    #[serde(default)]
    check_suite: Option<serde_json::Value>,
}

impl PrNode {
    fn into_gh_pr(self, repo: &RepoId) -> GhPr {
        let mut pr = self.pr;
        if pr.url.is_empty() {
            pr.url = format!(
                "https://github.com/{}/{}/pull/{}",
                repo.owner, repo.name, pr.number
            );
        }
        let rollup = self
            .commits
            .and_then(|commits| commits.nodes.into_iter().next())
            .and_then(|node| node.commit)
            .and_then(|commit| commit.status_check_rollup);
        if let Some(rollup) = rollup.as_ref()
            && let Some(contexts) = rollup.contexts.as_ref()
        {
            pr.check_counts = Some(CheckCounts::from_states(
                contexts
                    .check_run_counts_by_state
                    .iter()
                    .chain(contexts.status_context_counts_by_state.iter())
                    .filter_map(|count| Some((count.state.as_deref()?, count.count))),
            ));
        }
        pr.status_check_rollup = rollup.map(|rollup| match rollup.contexts {
            Some(contexts) => contexts
                .nodes
                .into_iter()
                .map(|node| GhCheck {
                    workflow_name: node
                        .check_suite
                        .as_ref()
                        .and_then(|suite| suite.pointer("/workflowRun/workflow/name"))
                        .and_then(|name| name.as_str())
                        .map(str::to_string),
                    ..node.check
                })
                .collect(),
            // A rollup that reports only its own state is one unnamed check, not a PR without CI.
            None => rollup
                .state
                .filter(|state| !state.is_empty())
                .map(|state| GhCheck {
                    state: Some(state),
                    ..GhCheck::default()
                })
                .into_iter()
                .collect(),
        });
        pr
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ChecksState, MergeState, PrState, PrStatus, ReviewState};

    /// A real `gh api graphql` answer, re-keyed to the aliases `build_query` generates.
    const SAMPLE: &str = r#"{
 "data": {
  "r0": {
   "p0": {
    "number": 65077,
    "url": "https://github.com/zed-industries/zed/pull/65077",
    "title": "Remove descriptive comments in default settings",
    "state": "OPEN",
    "isDraft": false,
    "reviewDecision": null,
    "mergeable": "MERGEABLE",
    "mergeStateStatus": "BLOCKED",
    "commits": {
     "nodes": [
      {
       "commit": {
        "statusCheckRollup": {
         "state": "PENDING",
         "contexts": {
          "nodes": [
           {
            "__typename": "CheckRun",
            "name": "route-pr",
            "status": "QUEUED",
            "conclusion": null,
            "checkSuite": {
             "workflowRun": {"workflow": {"name": "Community PR Board"}}
            }
           },
           {
            "__typename": "CheckRun",
            "name": "danger",
            "status": "QUEUED",
            "conclusion": null,
            "checkSuite": {"workflowRun": {"workflow": {"name": "danger"}}}
           },
           {
            "__typename": "CheckRun",
            "name": "check-authorship-and-label",
            "status": "IN_PROGRESS",
            "conclusion": null,
            "checkSuite": {
             "workflowRun": {"workflow": {"name": "PR Issue Labeler"}}
            }
           },
           {
            "__typename": "CheckRun",
            "name": "orchestrate",
            "status": "IN_PROGRESS",
            "conclusion": null,
            "checkSuite": {"workflowRun": {"workflow": {"name": "run_tests"}}}
           },
           {
            "__typename": "CheckRun",
            "name": "build_nix_linux_x86_64",
            "status": "COMPLETED",
            "conclusion": "SKIPPED",
            "checkSuite": {"workflowRun": {"workflow": {"name": "nix_build"}}}
           },
           {
            "__typename": "CheckRun",
            "name": "bundle_linux_aarch64",
            "status": "COMPLETED",
            "conclusion": "SKIPPED",
            "checkSuite": {
             "workflowRun": {"workflow": {"name": "run_bundling"}}
            }
           },
           {
            "__typename": "CheckRun",
            "name": "check_style",
            "status": "QUEUED",
            "conclusion": null,
            "checkSuite": {"workflowRun": {"workflow": {"name": "run_tests"}}}
           },
           {
            "__typename": "CheckRun",
            "name": "bundle_mac_aarch64",
            "status": "COMPLETED",
            "conclusion": "SKIPPED",
            "checkSuite": {
             "workflowRun": {"workflow": {"name": "run_bundling"}}
            }
           },
           {
            "__typename": "CheckRun",
            "name": "bundle_mac_x86_64",
            "status": "COMPLETED",
            "conclusion": "SKIPPED",
            "checkSuite": {
             "workflowRun": {"workflow": {"name": "run_bundling"}}
            }
           },
           {
            "__typename": "StatusContext",
            "context": "verification/cla-signed",
            "state": "SUCCESS"
           }
          ]
         }
        }
       }
      }
     ]
    }
   },
   "p1": {
    "number": 65043,
    "state": "MERGED",
    "isDraft": false,
    "mergeable": "UNKNOWN",
    "mergeStateStatus": "UNKNOWN",
    "commits": {
     "nodes": [
      {"commit": {"statusCheckRollup": {"state": "SUCCESS"}}}
     ]
    }
   }
  },
  "rateLimit": {
   "cost": 1,
   "remaining": 4074,
   "resetAt": "2026-10-02T09:23:33Z"
  }
 }
}"#;

    fn zed() -> RepoId {
        RepoId {
            owner: "zed-industries".into(),
            name: "zed".into(),
        }
    }

    fn only_pr(answer: Answer) -> PrStatus {
        match answer {
            Answer::Prs(prs) => {
                assert_eq!(prs.len(), 1, "a number alias answers with one pull request");
                PrStatus::from_gh(prs.into_iter().next().unwrap())
            }
            other => panic!("expected pull requests, got {other:?}"),
        }
    }

    #[test]
    fn the_entrys_own_answer_reads_as_the_two_pull_requests_it_describes() {
        let asks = vec![
            (zed(), "p0".to_string(), Ask::Number(65077)),
            (zed(), "p1".to_string(), Ask::Number(65043)),
        ];
        let mut batch = parse_batch(SAMPLE, &asks).expect("the entry's own answer parses");
        assert!(batch.errors.is_empty());

        assert_eq!(
            batch.rate_limit,
            Some(RateLimit {
                cost: 1,
                remaining: 4074,
                reset_at: Some("2026-10-02T09:23:33Z".into()),
            })
        );

        let open = only_pr(batch.answers.remove("p0").expect("the open pull request"));
        assert_eq!(open.number, 65077);
        assert_eq!(
            open.url.as_ref(),
            "https://github.com/zed-industries/zed/pull/65077"
        );
        assert_eq!(
            open.title.as_ref(),
            "Remove descriptive comments in default settings"
        );
        assert_eq!(open.state, PrState::Open);
        assert_eq!(open.checks, ChecksState::Pending);
        assert_eq!(open.review, ReviewState::None);
        assert_eq!(open.merge, MergeState::Blocked);
        assert!(open.failing_checks.is_empty());

        let merged = only_pr(batch.answers.remove("p1").expect("the merged pull request"));
        assert_eq!(merged.number, 65043);
        assert_eq!(merged.state, PrState::Merged);
        assert_eq!(merged.checks, ChecksState::Passing);
        assert_eq!(merged.merge, MergeState::Unknown);
        assert_eq!(
            merged.url.as_ref(),
            "https://github.com/zed-industries/zed/pull/65043"
        );
        assert_eq!(merged.title.as_ref(), "");
    }

    /// GitHub counts the whole rollup, so the counts must come from its tallies
    /// rather than from the `contexts` page, which `CHECKS_PER_PR` truncates.
    #[test]
    fn the_rollups_own_tallies_say_how_much_ci_is_left() {
        const COUNTED: &str = r#"{
 "data": {
  "r0": {
   "p0": {
    "number": 65285,
    "url": "https://github.com/zed-industries/zed/pull/65285",
    "title": "Counted",
    "state": "OPEN",
    "isDraft": false,
    "reviewDecision": null,
    "mergeable": "MERGEABLE",
    "mergeStateStatus": "BLOCKED",
    "commits": {
     "nodes": [
      {
       "commit": {
        "statusCheckRollup": {
         "state": "PENDING",
         "contexts": {
          "checkRunCountsByState": [
           {"state": "SUCCESS", "count": 23},
           {"state": "SKIPPED", "count": 25},
           {"state": "FAILURE", "count": 1},
           {"state": "IN_PROGRESS", "count": 7},
           {"state": "QUEUED", "count": 3}
          ],
          "statusContextCountsByState": [
           {"state": "SUCCESS", "count": 2},
           {"state": "ERROR", "count": 1}
          ],
          "nodes": [
           {
            "__typename": "CheckRun",
            "name": "tests",
            "conclusion": "FAILURE",
            "checkSuite": {"workflowRun": {"workflow": {"name": "CI"}}}
           }
          ]
         }
        }
       }
      }
     ]
    }
   }
  },
  "rateLimit": {"cost": 1, "remaining": 4074, "resetAt": "2026-10-02T09:23:33Z"}
 }
}"#;

        let asks = vec![(zed(), "p0".to_string(), Ask::Number(65285))];
        let mut batch = parse_batch(COUNTED, &asks).expect("the counted answer parses");
        assert!(batch.errors.is_empty());
        let pr = only_pr(batch.answers.remove("p0").expect("the pull request"));

        assert_eq!(
            pr.check_counts,
            crate::CheckCounts {
                failed: 2,
                running: 10,
                passed: 25,
                skipped: 25,
            },
            "both kinds of count are added up, and a status ERROR counts as failed"
        );
        assert_eq!(
            pr.failing_checks.len(),
            1,
            "the names still come from the contexts the query listed"
        );
    }

    /// Without the tallies there is only the flattened list `gh pr list` gives.
    #[test]
    fn a_rollup_with_no_tallies_is_counted_from_its_own_list() {
        let asks = vec![(zed(), "p0".to_string(), Ask::Number(65077))];
        let mut batch = parse_batch(SAMPLE, &asks).expect("the sample parses");
        let pr = only_pr(batch.answers.remove("p0").expect("the pull request"));
        assert_eq!(
            pr.check_counts.total(),
            10,
            "every check the sample lists is counted once"
        );
        assert_eq!(pr.checks, ChecksState::Pending);
        assert!(pr.check_counts.running > 0, "the sample's checks are queued");
        assert_eq!(pr.check_counts.failed, 0);
    }

    #[test]
    fn a_check_run_is_named_by_its_workflow_and_its_job() {
        let asks = vec![(zed(), "p0".to_string(), Ask::Number(65077))];
        let mut batch = parse_batch(SAMPLE, &asks).unwrap();
        let Answer::Prs(prs) = batch.answers.remove("p0").unwrap() else {
            panic!("expected pull requests");
        };
        let checks = prs[0].status_check_rollup.as_ref().expect("the rollup");
        assert_eq!(checks.len(), 10);
        assert_eq!(
            checks[0].label().as_deref(),
            Some("Community PR Board / route-pr")
        );
        assert_eq!(checks[1].label().as_deref(), Some("danger"));
        assert_eq!(
            checks[9].label().as_deref(),
            Some("verification/cla-signed")
        );
    }

    #[test]
    fn a_branch_alias_answers_with_every_pull_request_on_it() {
        let asks = vec![
            (zed(), "b0".to_string(), Ask::Branch("quiet-ui".into())),
            (zed(), "b1".to_string(), Ask::Branch("no-pr".into())),
        ];
        let json = r#"{"data": {"r0": {
            "b0": {"nodes": [
                {"number": 7, "url": "u7", "title": "t7", "state": "OPEN"},
                {"number": 8, "url": "u8", "title": "t8", "state": "CLOSED"}
            ]},
            "b1": {"nodes": []}
        }}}"#;
        let mut batch = parse_batch(json, &asks).unwrap();
        let Answer::Prs(prs) = batch.answers.remove("b0").unwrap() else {
            panic!("expected pull requests");
        };
        assert_eq!(
            prs.iter().map(|pr| pr.number).collect::<Vec<_>>(),
            vec![7, 8]
        );
        assert_eq!(
            PrStatus::from_gh(prs.into_iter().next().unwrap()).checks,
            ChecksState::Unknown
        );
        let Answer::Prs(none) = batch.answers.remove("b1").unwrap() else {
            panic!("expected an empty answer, not a missing one");
        };
        assert!(none.is_empty());
    }

    #[test]
    fn an_answer_that_says_nothing_is_told_from_one_that_says_no() {
        let asks = vec![
            (zed(), "p0".to_string(), Ask::Number(1)),
            (zed(), "p1".to_string(), Ask::Number(2)),
            (zed(), "p2".to_string(), Ask::Number(3)),
            (
                RepoId {
                    owner: "zed-industries".into(),
                    name: "zed-private".into(),
                },
                "p3".to_string(),
                Ask::Number(4),
            ),
        ];
        let json = r#"{"data": {
            "r0": {"p0": {"number": 1, "url": "u", "title": "t", "state": "OPEN"}, "p1": null},
            "r1": null
        }, "errors": [{"message": "Could not resolve to a Repository"}]}"#;
        let mut batch = parse_batch(json, &asks).unwrap();
        assert_eq!(
            batch.errors,
            vec!["Could not resolve to a Repository".to_string()]
        );
        assert!(matches!(batch.answers.remove("p0"), Some(Answer::Prs(_))));
        assert!(matches!(batch.answers.remove("p1"), Some(Answer::Missing)));
        assert!(matches!(
            batch.answers.remove("p2"),
            Some(Answer::Unanswered(_))
        ));
        assert!(matches!(
            batch.answers.remove("p3"),
            Some(Answer::Unanswered(_))
        ));
    }

    #[test]
    fn a_pull_request_with_no_state_is_unanswered_rather_than_assumed_open() {
        let asks = vec![(zed(), "p0".to_string(), Ask::Number(1))];
        let json = r#"{"data": {"r0": {"p0": {"number": 1, "url": "u", "title": "t"}}}}"#;
        let mut batch = parse_batch(json, &asks).unwrap();
        assert!(matches!(
            batch.answers.remove("p0"),
            Some(Answer::Unanswered(_))
        ));
    }

    #[test]
    fn a_check_of_an_unknown_shape_still_counts_as_unfinished() {
        let asks = vec![(zed(), "p0".to_string(), Ask::Number(1))];
        let json = r#"{"data": {"r0": {"p0": {
            "number": 1, "url": "u", "title": "t", "state": "OPEN",
            "commits": {"nodes": [{"commit": {"statusCheckRollup": {"state": "PENDING",
                "contexts": {"nodes": [
                    {"__typename": "CheckRun", "name": "ok", "conclusion": "SUCCESS"},
                    {"__typename": "SomethingNew", "whatever": 1}
                ]}
            }}}]}
        }}}}"#;
        let mut batch = parse_batch(json, &asks).unwrap();
        assert_eq!(
            only_pr(batch.answers.remove("p0").unwrap()).checks,
            ChecksState::Pending
        );
    }

    #[test]
    fn an_answer_with_no_data_is_the_whole_querys_failure() {
        let asks = vec![(zed(), "p0".to_string(), Ask::Number(1))];
        assert!(parse_batch("not json", &asks).is_err());
        assert!(
            parse_batch(r#"{"errors": [{"message": "Bad credentials"}]}"#, &asks)
                .is_err_and(|error| format!("{error:#}").contains("Bad credentials"))
        );
    }

    #[test]
    fn one_query_groups_its_subjects_under_a_repository_each() {
        let other = RepoId {
            owner: "arthurbrussee".into(),
            name: "zed".into(),
        };
        let asks = vec![
            (zed(), "b0".to_string(), Ask::Branch("main".into())),
            (other, "p1".to_string(), Ask::Number(42)),
            (zed(), "p2".to_string(), Ask::Number(65077)),
        ];
        let query = build_query(&asks);

        assert_eq!(query.matches("repository(owner:").count(), 2);
        assert!(query.contains("r0: repository(owner: \"arthurbrussee\", name: \"zed\")"));
        assert!(query.contains("r1: repository(owner: \"zed-industries\", name: \"zed\")"));
        let zed_block = query
            .split("r1: repository")
            .nth(1)
            .expect("the second repository's block");
        assert!(zed_block.contains("b0: pullRequests(headRefName: \"main\""));
        assert!(zed_block.contains("p2: pullRequest(number: 65077)"));
        assert!(query.contains("p1: pullRequest(number: 42)"));

        assert!(query.contains("rateLimit { cost remaining resetAt }"));
        assert_eq!(query.matches("fragment pr on PullRequest").count(), 1);
        assert!(query.contains("contexts(first: 100)"));
        assert!(!query.contains("$checks"));
    }

    #[test]
    fn a_branch_name_cannot_end_the_string_it_sits_in() {
        let asks = vec![(zed(), "b0".to_string(), Ask::Branch("od\"d\\name".into()))];
        let query = build_query(&asks);
        assert!(query.contains(r#"headRefName: "od\"d\\name""#));
    }

    #[test]
    fn a_repository_is_named_owner_then_name() {
        assert_eq!(RepoId::parse("zed-industries/zed\n").unwrap(), zed());
        assert!(RepoId::parse("zed").is_err());
        assert!(RepoId::parse("/zed").is_err());
        assert!(RepoId::parse("zed-industries/").is_err());
        assert!(RepoId::parse("a/b/c").is_err());
    }
}
