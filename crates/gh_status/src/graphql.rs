//! The one batched question, in place of a request per watched thing.
//!
//! Every watched branch and pull request used to be its own `gh` invocation
//! and so its own GitHub request: a session with a hundred of them spent a
//! hundred of an hourly five thousand on every poll, which is how the budget
//! went. GraphQL answers about as many as are asked in one request, so one
//! query carries an alias per subject, grouped under a `repository` alias
//! each, and `rateLimit` beside them says what the poll actually cost rather
//! than leaving the count to be inferred from invocations.
//!
//! Everything here is pure: building the query text and reading the answer
//! back. What runs `gh` and what decides who to ask about live in
//! `gh_status.rs`, and the answer is lowered to the same [`GhPr`] the
//! `gh pr list` path produces, so the chip semantics have one home.

use std::fmt::Write as _;

use anyhow::{Context as _, Result, bail};
use collections::HashMap;
use serde::Deserialize;

use crate::{GhCheck, GhPr};

/// How many pull requests one branch alias asks for.
///
/// This is the number of pull requests that share a head branch, which is one
/// in almost every case and a handful in the worst; `gh pr list` defaults to
/// thirty, and asking for thirty per branch across a batch of fifty is a node
/// count worth not spending. Surfaces deduplicate by URL anyway.
const PRS_PER_BRANCH: usize = 10;

/// How many of a commit's checks one alias asks for. Zed's own pull requests
/// carry a few dozen; the hover card lists far fewer than that and counts the
/// rest.
const CHECKS_PER_PR: usize = 100;

/// The repository a question is about. GraphQL has no notion of "the
/// repository this directory is in", so the batched query has to name it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct RepoId {
    pub owner: String,
    pub name: String,
}

impl RepoId {
    /// Reads the `owner/name` spelling `gh repo view` reports and a PR mention
    /// in a thread carries.
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

    /// Where a pull request of this repository lives, for an answer that
    /// reported a number without a URL.
    fn pr_url(&self, number: u64) -> String {
        format!(
            "https://github.com/{}/{}/pull/{number}",
            self.owner, self.name
        )
    }
}

/// What one alias asks about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Ask {
    /// Every pull request whose head is this branch. This is how a number is
    /// discovered in the first place.
    Branch(String),
    /// One pull request, by number.
    Number(u64),
}

impl Ask {
    /// The alias this ask's answer comes back on. The prefix says which shape
    /// to expect — a connection for a branch, a single pull request for a
    /// number — and the index keeps it unique even when two watches land on
    /// the same repository and number.
    pub(crate) fn alias(&self, index: usize) -> String {
        match self {
            Ask::Branch(_) => format!("b{index}"),
            Ask::Number(_) => format!("p{index}"),
        }
    }
}

/// Builds the one query asking about every `(repository, alias, ask)` given.
///
/// Repositories are grouped and emitted in sorted order so the same batch
/// always produces the same query text, which is what makes this testable.
pub(crate) fn build_query(asks: &[(RepoId, String, Ask)]) -> String {
    let mut by_repo: Vec<(&RepoId, Vec<(&String, &Ask)>)> = Vec::new();
    for (repo, alias, ask) in asks {
        match by_repo.iter_mut().find(|(known, _)| *known == repo) {
            Some((_, subjects)) => subjects.push((alias, ask)),
            None => by_repo.push((repo, vec![(alias, ask)])),
        }
    }
    by_repo.sort_by_key(|(repo, _)| *repo);

    let mut query = String::from("query {\n");
    for (index, (repo, subjects)) in by_repo.iter().enumerate() {
        writeln!(
            query,
            "  r{index}: repository(owner: \"{}\", name: \"{}\") {{",
            escape(&repo.owner),
            escape(&repo.name)
        )
        .ok();
        for (alias, ask) in subjects {
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
                    writeln!(query, "    {alias}: pullRequest(number: {number}) {{ ...pr }}").ok();
                }
            }
        }
        query.push_str("  }\n");
    }
    // Free to ask for, and it is the only thing that says what the poll spent.
    query.push_str("  rateLimit { cost remaining resetAt }\n}\n");
    // `$checks` is spelled as a GraphQL variable on purpose: if this
    // substitution is ever lost, GitHub refuses the query by name rather than
    // quietly asking for a different number of checks.
    query.push_str(&PR_FRAGMENT.replace("$checks", &CHECKS_PER_PR.to_string()));
    query
}

/// The fields every alias wants, named once. A fragment rather than a hundred
/// copies: the query goes out on the command line, and the copies are what
/// would make its size the thing that broke.
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

/// A GraphQL string literal's escaping. Branch names cannot hold a quote and
/// a repository name cannot hold anything interesting, but a query that is
/// merely malformed costs a whole poll, so neither is taken on trust.
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

/// What the batched answer said about one alias.
#[derive(Debug)]
pub(crate) enum Answer {
    /// The pull requests this alias asked about. An empty list is an answer:
    /// the branch has no pull request.
    Prs(Vec<GhPr>),
    /// GitHub answered, and there is no such pull request.
    Missing,
    /// Nothing usable came back for this alias — its repository was null, the
    /// alias was absent, or its own fields would not parse. The caller asks
    /// again the old way rather than blanking a chip on it.
    Unanswered(String),
}

/// Everything one batched answer carries.
#[derive(Debug, Default)]
pub(crate) struct Batch {
    /// One entry per alias that was asked about, whatever came back.
    pub answers: HashMap<String, Answer>,
    pub rate_limit: Option<RateLimit>,
    /// The query-level errors GitHub reported, if any. Partial data is still
    /// used: one unreadable repository should not cost every other alias its
    /// answer.
    pub errors: Vec<String>,
}

/// What the poll cost and what is left, straight from GitHub rather than
/// inferred from how many invocations this app made.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub(crate) struct RateLimit {
    #[serde(default)]
    pub cost: u32,
    #[serde(default)]
    pub remaining: u32,
    /// When the budget refills, as GitHub's own RFC 3339 timestamp.
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
    /// Everything else in `data` is a repository alias holding that
    /// repository's own subject aliases. A repository `gh` cannot see comes
    /// back as a null.
    #[serde(flatten)]
    repositories: HashMap<String, Option<HashMap<String, serde_json::Value>>>,
}

/// Reads the answer back, against the aliases that were asked for.
///
/// Driven by the asks rather than by what the response happens to hold, so an
/// alias GitHub left out is reported as unanswered instead of silently
/// vanishing — a missing answer has to become a question asked the old way,
/// and nothing else here would notice it was missing.
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

    // The repository aliases are positional: `build_query` sorts the
    // repositories and numbers them, so the same ordering finds each ask's
    // own repository again.
    let mut repos: Vec<&RepoId> = Vec::new();
    for (repo, _, _) in asks {
        if !repos.contains(&repo) {
            repos.push(repo);
        }
    }
    repos.sort();

    let mut answers = HashMap::default();
    for (repo, alias, ask) in asks {
        let repo_index = repos.iter().position(|known| known == &repo);
        let subjects = repo_index
            .and_then(|index| data.repositories.get(&format!("r{index}")))
            .and_then(|repo| repo.as_ref());
        let Some(subjects) = subjects else {
            answers.insert(
                alias.clone(),
                Answer::Unanswered(format!(
                    "{}/{} was not in the answer",
                    repo.owner, repo.name
                )),
            );
            continue;
        };
        let Some(value) = subjects.get(alias) else {
            answers.insert(
                alias.clone(),
                Answer::Unanswered(format!("alias {alias} was not in the answer")),
            );
            continue;
        };
        answers.insert(alias.clone(), read_answer(value, repo, ask));
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
#[serde(rename_all = "camelCase")]
struct PrNode {
    number: u64,
    /// Asked for on every alias, so absent only from an answer that left it
    /// out; the number and the repository are enough to say where it lives.
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    title: Option<String>,
    /// Load-bearing: `OPEN`, `CLOSED`, `MERGED`. An answer without it is one
    /// to ask again rather than guess at.
    state: String,
    #[serde(default)]
    is_draft: bool,
    #[serde(default)]
    review_decision: Option<String>,
    #[serde(default)]
    mergeable: Option<String>,
    #[serde(default)]
    merge_state_status: Option<String>,
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
    contexts: Option<Connection<ContextNode>>,
}

/// One entry of the check rollup. GitHub reports two shapes under one list and
/// says which by `__typename`: a check run carries a job name, a workflow and
/// a conclusion, a commit status carries a context and a state.
#[derive(Debug, Deserialize)]
#[serde(tag = "__typename")]
enum ContextNode {
    CheckRun {
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        conclusion: Option<String>,
        #[serde(default, rename = "checkSuite")]
        check_suite: Option<CheckSuite>,
    },
    StatusContext {
        #[serde(default)]
        context: Option<String>,
        #[serde(default)]
        state: Option<String>,
    },
    /// A shape GitHub has added since. Kept rather than dropped: an entry
    /// nothing can read is still an entry that has not passed, and dropping it
    /// would report a pull request greener than it is.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
struct CheckSuite {
    #[serde(default, rename = "workflowRun")]
    workflow_run: Option<WorkflowRun>,
}

#[derive(Debug, Deserialize)]
struct WorkflowRun {
    #[serde(default)]
    workflow: Option<Workflow>,
}

#[derive(Debug, Deserialize)]
struct Workflow {
    #[serde(default)]
    name: Option<String>,
}

impl ContextNode {
    /// Lowered to the shape the `gh pr list` path produces, which is where
    /// every judgement about what a check amounts to already lives.
    fn into_gh_check(self) -> GhCheck {
        match self {
            ContextNode::CheckRun {
                name,
                conclusion,
                check_suite,
            } => GhCheck {
                state: None,
                conclusion,
                name,
                workflow_name: check_suite
                    .and_then(|suite| suite.workflow_run)
                    .and_then(|run| run.workflow)
                    .and_then(|workflow| workflow.name),
                context: None,
            },
            ContextNode::StatusContext { context, state } => GhCheck {
                state,
                conclusion: None,
                name: None,
                workflow_name: None,
                context,
            },
            // Nothing known about it, which `GhCheck::outcome` reads as
            // pending — the same bias the rest of this file takes.
            ContextNode::Unknown => GhCheck {
                state: None,
                conclusion: None,
                name: None,
                workflow_name: None,
                context: None,
            },
        }
    }
}

impl PrNode {
    fn into_gh_pr(self, repo: &RepoId) -> GhPr {
        let rollup = self
            .commits
            .and_then(|commits| commits.nodes.into_iter().next())
            .and_then(|node| node.commit)
            .and_then(|commit| commit.status_check_rollup);
        // A rollup that is absent is not an empty one: the distinction is what
        // tells "this pull request has no CI" from "nobody has said yet", and
        // the `gh pr list` path draws it the same way.
        let status_check_rollup = rollup.map(|rollup| match rollup.contexts {
            Some(contexts) => contexts
                .nodes
                .into_iter()
                .map(ContextNode::into_gh_check)
                .collect(),
            // A rollup that reported its own state and no contexts still says
            // whether CI passed. Read it as the one unnamed check it amounts
            // to rather than as a pull request without CI.
            None => rollup
                .state
                .filter(|state| !state.is_empty())
                .map(|state| GhCheck {
                    state: Some(state),
                    conclusion: None,
                    name: None,
                    workflow_name: None,
                    context: None,
                })
                .into_iter()
                .collect(),
        });
        GhPr {
            number: self.number,
            url: self
                .url
                .unwrap_or_else(|| repo.pr_url(self.number)),
            title: self.title.unwrap_or_default(),
            state: self.state,
            is_draft: self.is_draft,
            review_decision: self.review_decision,
            status_check_rollup,
            mergeable: self.mergeable,
            merge_state_status: self.merge_state_status,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ChecksState, MergeState, PrState, PrStatus, ReviewState};

    /// The response the Work queue entry carries, from Arthur's own `gh`,
    /// under the alias names [`build_query`] generates: `r0` for the
    /// repository it asked about, `p0` and `p1` for the two pull requests it
    /// asked about by number. Everything inside those two pull requests is the
    /// entry's own, down to the bytes — `a`, an open pull request with CI
    /// running, and `b`, a merged one whose rollup reports a state and no
    /// contexts at all.
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

        // One query, two pull requests, and GitHub charged one for it: the
        // whole reason the entry asks for this.
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
        // Two checks queued and two in progress against five skipped and one
        // successful commit status: running, which is what the rollup's own
        // `PENDING` says too.
        assert_eq!(open.checks, ChecksState::Pending);
        assert_eq!(open.review, ReviewState::None);
        // Mergeable and blocked at once, which is the pair of fields this
        // chip exists to read together.
        assert_eq!(open.merge, MergeState::Blocked);
        assert!(open.failing_checks.is_empty());

        let merged = only_pr(batch.answers.remove("p1").expect("the merged pull request"));
        assert_eq!(merged.number, 65043);
        assert_eq!(merged.state, PrState::Merged);
        // A rollup that reported its own state and no contexts still says CI
        // passed; reading it as a pull request without CI would lose that.
        assert_eq!(merged.checks, ChecksState::Passing);
        assert_eq!(merged.merge, MergeState::Unknown);
        // Nothing in the answer said where it lives, so the repository and
        // the number say it instead.
        assert_eq!(
            merged.url.as_ref(),
            "https://github.com/zed-industries/zed/pull/65043"
        );
        assert_eq!(merged.title.as_ref(), "");
    }

    /// The workflow name is why the query reaches through `checkSuite` at all:
    /// without it a hover card shows two identical `clippy` lines.
    #[test]
    fn a_check_run_is_named_by_its_workflow_and_its_job() {
        let asks = vec![(zed(), "p0".to_string(), Ask::Number(65077))];
        let mut batch = parse_batch(SAMPLE, &asks).unwrap();
        let Answer::Prs(prs) = batch.answers.remove("p0").unwrap() else {
            panic!("expected pull requests");
        };
        let checks = prs[0].status_check_rollup.as_ref().expect("the rollup");
        assert_eq!(checks.len(), 10);
        assert_eq!(checks[0].label().as_deref(), Some("Community PR Board / route-pr"));
        // A job whose workflow carries the same name says it once.
        assert_eq!(checks[1].label().as_deref(), Some("danger"));
        // A commit status has a context in place of both.
        assert_eq!(
            checks[9].label().as_deref(),
            Some("verification/cla-signed")
        );
    }

    /// A branch alias answers with a connection rather than a pull request,
    /// and an empty one is an answer: the branch has no pull request.
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
        // No rollup at all is not an empty rollup: nobody has said yet.
        assert_eq!(
            PrStatus::from_gh(prs.into_iter().next().unwrap()).checks,
            ChecksState::Unknown
        );
        let Answer::Prs(none) = batch.answers.remove("b1").unwrap() else {
            panic!("expected an empty answer, not a missing one");
        };
        assert!(none.is_empty());
    }

    /// What has to become a question asked the old way, and what must not.
    #[test]
    fn an_answer_that_says_nothing_is_told_from_one_that_says_no() {
        let asks = vec![
            (zed(), "p0".to_string(), Ask::Number(1)),
            (zed(), "p1".to_string(), Ask::Number(2)),
            (zed(), "p2".to_string(), Ask::Number(3)),
            (
                // Sorts after `zed`, so it is the second repository block and
                // the one that comes back null below.
                RepoId {
                    owner: "zed-industries".into(),
                    name: "zed-private".into(),
                },
                "p3".to_string(),
                Ask::Number(4),
            ),
        ];
        // `p0` answered, `p1` is explicitly no such pull request, `p2` was
        // left out of the answer, and `p3`'s whole repository came back null.
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
        // GitHub answered. Asking again one at a time would only collect the
        // same nothing.
        assert!(matches!(batch.answers.remove("p1"), Some(Answer::Missing)));
        // These two have to be asked again, and only these two.
        assert!(matches!(
            batch.answers.remove("p2"),
            Some(Answer::Unanswered(_))
        ));
        assert!(matches!(
            batch.answers.remove("p3"),
            Some(Answer::Unanswered(_))
        ));
    }

    /// A pull request without the one field that cannot be guessed at is a
    /// question to ask again, not a chip to draw from a default.
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

    /// A shape GitHub adds later must not make a pull request read greener
    /// than it is.
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
        assert_eq!(only_pr(batch.answers.remove("p0").unwrap()).checks, ChecksState::Pending);
    }

    /// Neither a refusal nor an answer: nothing to read at all, which is the
    /// whole batch's failure and sends every subject back the old way.
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

        // One repository block each, sorted, so the same batch always
        // produces the same query — and so reading the answer back can find
        // each subject's repository by the same ordering.
        assert_eq!(query.matches("repository(owner:").count(), 2);
        assert!(query.contains("r0: repository(owner: \"arthurbrussee\", name: \"zed\")"));
        assert!(query.contains("r1: repository(owner: \"zed-industries\", name: \"zed\")"));
        // The two subjects of one repository share its block.
        let zed_block = query
            .split("r1: repository")
            .nth(1)
            .expect("the second repository's block");
        assert!(zed_block.contains("b0: pullRequests(headRefName: \"main\""));
        assert!(zed_block.contains("p2: pullRequest(number: 65077)"));
        assert!(query.contains("p1: pullRequest(number: 42)"));

        // What the poll cost, which is the other half of the entry.
        assert!(query.contains("rateLimit { cost remaining resetAt }"));
        // The fields are named once however many aliases ask for them, and
        // the check limit is substituted rather than left as a variable
        // nothing declares.
        assert_eq!(query.matches("fragment pr on PullRequest").count(), 1);
        assert!(query.contains("contexts(first: 100)"));
        assert!(!query.contains("$checks"));
    }

    #[test]
    fn a_branch_name_cannot_end_the_string_it_sits_in() {
        let asks = vec![(
            zed(),
            "b0".to_string(),
            Ask::Branch("od\"d\\name".into()),
        )];
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
