//! Every route the server answers, and the MCP tool that calls it. The dashboard does nothing
//! but call these routes, so a tool for each is what lets an agent do all the dashboard does. A
//! route no agent should call says why.
//!
//! A tool calls its route in this process, as the agent: the route checks who may, and its
//! answer is the tool's. The route's path parameters are the tool's arguments, as `PATH_ARGS`
//! names them, and so are the `args`: the query of a GET, the JSON body of anything else.

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::handler::Handler;
use axum::http::Method;
use axum::routing::{MethodFilter, MethodRouter, on};
use serde_json::{Map, Value, json};

use crate::{
    AppState, api, auth, billing, builds, changelog, mcp, need, oauth, review, review_stats, v1,
    webhook,
};

pub(crate) struct Route {
    pub method: Method,
    pub path: &'static str,
    handler: fn() -> MethodRouter<AppState>,
    pub agent: Agent,
}

pub(crate) enum Agent {
    Tool(Tool),
    /// Why no agent calls it.
    Exempt(
        #[cfg_attr(not(test), expect(dead_code, reason = "for whoever reads the table"))]
        &'static str,
    ),
}

pub(crate) struct Tool {
    pub name: &'static str,
    pub description: &'static str,
    pub args: &'static [Arg],
    pub effect: Effect,
}

pub(crate) enum Effect {
    Reads,
    Writes,
    /// Undoes something the user set up, as removing a key does.
    Destroys,
}

pub(crate) struct Arg {
    pub name: &'static str,
    pub ty: Ty,
    pub description: &'static str,
    pub required: bool,
}

pub(crate) enum Ty {
    Int,
    Str,
    Bool,
    OneOf(&'static [&'static str]),
    /// An object that is the whole JSON body.
    Body,
}

/// A path parameter, the argument it is, and the argument's schema. `{owner}/{name}` is one
/// argument: `repo`.
pub(crate) static PATH_ARGS: [(&str, &str, Ty, &str); 6] = [
    (
        "id",
        "workspace",
        Ty::Int,
        "The workspace: the installation ID that `get_session` lists. A tool with a `repo` finds it from the repo without one.",
    ),
    ("owner", "repo", Ty::Str, "The repo, as `owner/name`."),
    ("number", "number", Ty::Int, "The PR's number."),
    (
        "provider",
        "provider",
        Ty::Str,
        "The model provider, as `get_settings` lists them.",
    ),
    (
        "login",
        "login",
        Ty::Str,
        "The GitHub login the seat is for.",
    ),
    (
        "build",
        "build",
        Ty::Int,
        "The build, as `claim_build` or `list_builds` gives it.",
    ),
];

const fn arg(name: &'static str, ty: Ty, description: &'static str) -> Arg {
    Arg {
        name,
        ty,
        description,
        required: true,
    }
}

const fn opt(name: &'static str, ty: Ty, description: &'static str) -> Arg {
    Arg {
        name,
        ty,
        description,
        required: false,
    }
}

const SIGN_IN: &str =
    "Signing in is GitHub's OAuth, in a browser. An MCP client signs in through `/oauth`.";
const SETUP: &str = "Creating the GitHub App is GitHub's manifest flow, in a browser.";
const OAUTH: &str = "MCP's OAuth sign-in.";

macro_rules! route {
    ($method:ident $path:literal, $handler:expr, $agent:expr) => {
        Route {
            method: Method::$method,
            path: $path,
            handler: || on(MethodFilter::$method, $handler),
            agent: $agent,
        }
    };
}

macro_rules! tool {
    ($name:literal, $effect:ident, $description:literal $(, $arg:expr)* $(,)?) => {
        Agent::Tool(Tool {
            name: $name,
            description: $description,
            args: &[$($arg),*],
            effect: Effect::$effect,
        })
    };
}

pub(crate) static ROUTES: &[Route] = &[
    route!(GET "/api/health", || async { "ok" }, Agent::Exempt("A liveness probe.")),
    route!(GET "/api/changelog", changelog::latest, tool!("get_changelog", Reads,
        "MergeLoop's newest changelog entries, newest first: what changed for its users, each as a slug, a title, and new, improved or fixed. Each entry's page is /changelog/<slug>.")),
    route!(GET "/api/session", auth::session, tool!("get_session", Reads,
        "Who you are signed in to MergeLoop as, and your workspaces: the installations of its GitHub App you can reach. A GitHub token is signed in as no one, and has no workspaces: it reaches single repos.")),
    route!(POST "/api/setup/unlock", auth::unlock, Agent::Exempt(SETUP)),
    route!(POST "/api/setup/manifest", auth::start_manifest, Agent::Exempt(SETUP)),
    route!(GET "/setup/callback", auth::manifest_callback, Agent::Exempt(SETUP)),
    route!(GET "/setup/installed", auth::installed, Agent::Exempt(SETUP)),
    route!(GET "/auth/login", auth::login, Agent::Exempt(SIGN_IN)),
    route!(GET "/auth/callback", auth::login_callback, Agent::Exempt(SIGN_IN)),
    route!(POST "/auth/logout", auth::logout, Agent::Exempt(SIGN_IN)),
    route!(POST "/webhooks/github",
        webhook::receive.layer(DefaultBodyLimit::max(webhook::MAX_BODY_BYTES)),
        Agent::Exempt("GitHub calls it.")),
    route!(POST "/webhooks/stripe", billing::webhook, Agent::Exempt("Stripe calls it.")),
    route!(POST "/mcp", mcp::post, Agent::Exempt("The MCP server itself.")),
    route!(GET "/mcp", mcp::get, Agent::Exempt("The MCP server itself.")),
    route!(GET "/.well-known/oauth-protected-resource", oauth::resource, Agent::Exempt(OAUTH)),
    route!(GET "/.well-known/oauth-protected-resource/mcp", oauth::resource, Agent::Exempt(OAUTH)),
    route!(GET "/.well-known/oauth-authorization-server", oauth::server, Agent::Exempt(OAUTH)),
    route!(POST "/oauth/register", oauth::register, Agent::Exempt(OAUTH)),
    route!(GET "/oauth/authorize", oauth::authorize, Agent::Exempt(OAUTH)),
    route!(POST "/oauth/token", oauth::token, Agent::Exempt(OAUTH)),
    route!(GET "/api/v1/repos/{owner}/{name}/pulls/{number}", v1::pull,
        Agent::Exempt("For other apps, by repo and with an ETag. An agent calls `pull_status`, which says the same where MergeLoop is installed.")),
    route!(GET "/api/v1/repos/{owner}/{name}/pulls", v1::pulls,
        Agent::Exempt("For other apps, by repo and with an ETag. An agent calls `pull_status`, which says the same where MergeLoop is installed.")),
    route!(GET "/api/w/{id}/lanes", api::lanes, tool!("list_queue", Reads,
        "The queues, one per repo and branch PRs merge into, each in the order it takes them, with each PR's `stage` (`rebase`, `checks`, `review` or `lane`), what the queue is doing with its active PR as `step` (`rebasing`, `checks`, `fixing` or `merging`), and the `fixes` it has used of its `fix_budget`. A stack lists its PRs, bottom to top, in `stack`, each with its title, author, diff and checks; `hold.layer` is the one that holds it. Without a `repo`, those of the workspace's repos you can read: that needs a MergeLoop sign-in.",
        opt("repo", Ty::Str, "Only this repo's queues, as `owner/name`."))),
    route!(GET "/api/w/{id}/events", api::events, tool!("list_events", Reads,
        "A repo's queue events, newest first, up to 200: what the queue did, and what its agent spent.",
        arg("repo", Ty::Str, "The repo, as `owner/name`."),
        opt("pr", Ty::Int, "Only this PR's events."))),
    route!(GET "/api/w/{id}/history", api::history, tool!("list_history", Reads,
        "The PRs that left the queue in the repos you can read, newest first, 50 a page: merged, ejected, or removed. Needs a MergeLoop sign-in.",
        arg("days", Ty::Int, "How many days back, from 1 to 365."),
        opt("repo", Ty::Str, "Only this repo's, as `owner/name`."),
        opt("author", Ty::Str, "Only this GitHub login's."),
        opt("outcome", Ty::OneOf(&["merged", "ejected", "removed"]), "Only the PRs that left this way."),
        opt("before", Ty::Int, "The `next` of the page before."))),
    route!(GET "/api/w/{id}/history/stats", api::history_stats, tool!("get_history_stats", Reads,
        "The queue's numbers in the repos you can read, over a window and the window before: PRs merged, ejected and removed, time in the queue, time in each step, each developer's, and what the agents spent. Needs a MergeLoop sign-in.",
        arg("days", Ty::Int, "How many days back, from 1 to 365."),
        opt("repo", Ty::Str, "Only this repo's, as `owner/name`."),
        opt("author", Ty::Str, "Only this GitHub login's."))),
    route!(GET "/api/w/{id}/history/review", review_stats::review_stats_view, tool!("get_review_stats", Reads,
        "How the agent's review did on the PRs it first reviewed in the window, in the repos you can read, each with its whole history. `changes` sorts each change it asked for, sent back to the author or asked of a human, by what became of it: `fixed` in a later push, `wrong` or `intended` on a reply that held up, `dismissed` when the author resolved its thread with no push or a human called it fine, else `open`. Each PR counts its reviews up to its first approval, so a carried approval or a later re-review adds nothing. `rounds` counts the reviews each PR took to pass: on the first, within two (one round of fixing), five or more, and the mean. `humans` counts the PRs whose review asked a person, the reviews one answered, and the median wait for that answer. Needs a MergeLoop sign-in.",
        arg("days", Ty::Int, "How many days back, from 1 to 365."),
        opt("repo", Ty::Str, "Only this repo's, as `owner/name`."))),
    route!(GET "/api/w/{id}/repos", api::repos, tool!("list_repos", Reads,
        "The repos the workspace's installation reaches that you can read, and whether each queues: by its config file, or turned on or off. Needs a MergeLoop sign-in.")),
    route!(PUT "/api/w/{id}/repos/{owner}/{name}", api::set_repo, tool!("set_repo", Writes,
        "Turns a repo's queue on or off, over what its config file says, or back to following the file. Needs a MergeLoop sign-in, or a GitHub token with admin access to the repo.",
        opt("enabled", Ty::Bool, "`true` on, `false` off. Without it, the repo follows its config file."))),
    route!(GET "/api/w/{id}/repos/{owner}/{name}/pulls/{number}/status", api::pull_status, tool!("pull_status", Reads,
        "Where a PR is, as `GET /api/v1/repos/{owner}/{name}/pulls/{number}` says it: whether the repo's queue runs (`mergeloop`, with `hold` when any PR queued there now goes on hold, and `missing_key` when the workspace has no model key for the agent's tasks), the PR's place in the queue (`queue`, null when it is not queued, with its `stage` and a typed `hold` while held), how it last left the queue (`merged` or `ejected`), its dashboard link (`url`), and its latest queue events, newest first.")),
    route!(GET "/api/w/{id}/repos/{owner}/{name}/pulls/{number}/next", need::next_need, tool!("next_need", Reads,
        "What a PR needs next from the agent that queued it, one need at a time: `needs` is `queue`, `wait`, `threads` (work its unresolved review threads), `changes` (a required reviewer asked for changes), `draft`, `human` (it waits on one), `ejected`, `removed`, `merged` or `off` (its repo's queue does not run). `brief` says what to do, and `then` the command to run once done. Pass the need's `id` back as `after`, with a `wait`, and the call waits until there is a newer need, or answers `wait`. `exit_code` is the `mergeloop` CLI's for it.",
        opt("after", Ty::Str, "The `id` of the need you saw last. Without it, the need as it is now."),
        opt("wait", Ty::Int, "How many seconds to wait for a need newer than `after`, up to 50. Default 0."))),
    route!(PUT "/api/w/{id}/repos/{owner}/{name}/pulls/{number}/queue", api::queue_pull, tool!("queue_pull", Writes,
        "Queue a PR to merge: MergeLoop rebases it unless it can merge as it is with its checks passed, tests it, and merges it once its checks pass. On a queued PR, changes its priority. Queuing one PR of a stack queues the whole stack. Returns `message`, which says when the PR goes on hold whatever it is (as without a subscription) or the workspace has no model key for the agent, and the repo's status (`mergeloop`) as `pull_status` reads it. Needs triage access to the repo.",
        opt("priority", Ty::Bool, "Put it at the front of the queue. Default false."))),
    route!(DELETE "/api/w/{id}/repos/{owner}/{name}/pulls/{number}/queue", api::dequeue_pull, tool!("dequeue_pull", Writes,
        "Take a PR, and its stack if it is in one, out of the queue. Cancels its run. Needs triage access to the repo.")),
    route!(GET "/api/w/{id}/repos/{owner}/{name}/pulls/{number}/agent-review", review::agent_review, tool!("get_agent_review", Reads,
        "The agent's review of a queued PR, the queue's first gate: its newest, of whatever head, or null before its first. Each finding says what the agent saw, how sure it was, and where it went: back to the PR's author (`sent_back`), to a human (`asked`, for a call the author cannot make from the PR, `needs_human`), withdrawn on the author's reply (`withdrawn`), or cleared. A change it asked for carries its `outcome` once known: `fixed`, `wrong`, `intended` or `dismissed`, as `get_review_stats` counts them. A finding that holds the PR has a review thread on its line (`threaded`): the author fixes it or replies why not, then resolves the thread. A bug that recurs is one finding: `also_at` lists each other place it is, with its code, and a fix settles every one. A finding's `evidence` is the command MergeLoop ran on the PR's head, what it printed, and the whole test the agent wrote for it as a patch, also when GitHub's comment had no room for it: `failed` says MergeLoop saw it fail, the only evidence that sends a change back. `status` is `changes` while it waits for the author, `asks` while it waits for a human's answers, and `approved` once the author resolved every thread, or nothing held the PR.")),
    route!(GET "/api/w/{id}/repos/{owner}/{name}/config", api::repo_config, tool!("get_repo_config", Reads,
        "A repo's queue settings, as its config file on the default branch sets them over the workspace's defaults, or as the open settings PR would. With those `defaults`, and the models and efforts to pick from. A setting equal to its default follows the workspace, unless `follows` is false: the file spells it out, so it stays put until the file is trimmed.")),
    route!(PUT "/api/w/{id}/repos/{owner}/{name}/config", api::propose_repo_config, tool!("propose_repo_config", Writes,
        "Opens a PR that writes these settings to the repo's config file, or updates the one open, and queues it if the repo queues. The file keeps only what differs from the workspace's defaults, so the rest follows them. Returns the PR, with `queued` and a `note` when it is not. The queue uses the settings once it merges. Needs a MergeLoop sign-in.",
        arg("settings", Ty::Body, "Every setting, as `get_repo_config` returns them in `settings`."))),
    route!(GET "/api/w/{id}/repos/{owner}/{name}/skill", api::repo_skill_view, tool!("get_repo_skill", Reads,
        "Whether the repo's copy of the `mergeloop` agent skill, on its default branch, is the one MergeLoop ships now: `current`, `outdated` (older, edited, or missing from one skills dir) or `missing`. With the paths an update writes, and the open PR that updates it.")),
    route!(POST "/api/w/{id}/repos/{owner}/{name}/skill", api::update_repo_skill, tool!("update_repo_skill", Writes,
        "Opens a PR that writes the current `mergeloop` agent skill over the repo's older copies, deletes its copies under the old `mergepatrol` name, or updates the one open, and queues it if the repo queues. Returns the PR, with `queued` and a `note` when it is not. Agents read it once it merges. Refused when the repo has the current skill already. Needs a MergeLoop sign-in.")),
    route!(POST "/api/w/{id}/repos/{owner}/{name}/update", api::update_repo, tool!("update_repo", Writes,
        "Brings a queuing repo up to the latest settings and `mergeloop` agent skill: opens the PRs it needs, one for the config file in the shape MergeLoop writes now, which keeps only what differs from the workspace's defaults, and one for the skill, and queues them. Returns the PRs, each with `queued` and a `note` when it is not: an open config PR that changes the settings or would lose comments is left alone, and a new one that would drop the file's comments waits for a human. `failed` says why the skill's half failed, with the settings' PR still in `pulls`. `skipped` says why it left the repo alone: off, no config file, or one that does not parse. With neither, the repo was up to date. Safe to call again. Needs a MergeLoop sign-in.")),
    route!(GET "/api/w/{id}/settings/repo-defaults", api::repo_defaults, tool!("get_repo_defaults", Reads,
        "What the workspace's repos follow where their config files say nothing: the trigger label, batch size, default model, effort, per-task models and CI fixes. An empty config file follows all of them. With the models and efforts to pick from. Needs a MergeLoop sign-in.")),
    route!(PUT "/api/w/{id}/settings/repo-defaults", api::set_repo_defaults, tool!("set_repo_defaults", Writes,
        "Sets what the workspace's repos follow where their config files say nothing. It applies at once, with no PR, to every repo that follows it. Needs a MergeLoop sign-in.",
        arg("defaults", Ty::Body, "Every default, as `get_repo_defaults` returns them in `defaults`."))),
    route!(GET "/api/w/{id}/settings", api::settings, tool!("get_settings", Reads,
        "The workspace's model API keys and ChatGPT login: which are set, and when. Never the keys themselves. Needs a MergeLoop sign-in.")),
    route!(PUT "/api/w/{id}/settings/api-keys/{provider}", api::set_api_key, tool!("set_api_key", Writes,
        "Sets the workspace's API key for a model provider, once the provider takes it. Needs a MergeLoop sign-in.",
        arg("value", Ty::Str, "The key."))),
    route!(DELETE "/api/w/{id}/settings/api-keys/{provider}", api::delete_api_key, tool!("delete_api_key", Destroys,
        "Removes the workspace's API key for a model provider. Needs a MergeLoop sign-in.")),
    route!(POST "/api/w/{id}/settings/chatgpt", api::start_chatgpt_login, tool!("start_chatgpt_login", Writes,
        "Starts a sign-in to a ChatGPT plan, for the workspace's Codex runs: returns a code, and the page where the user enters it. Then call `poll_chatgpt_login`. Needs a MergeLoop sign-in.")),
    route!(DELETE "/api/w/{id}/settings/chatgpt", api::delete_chatgpt_login, tool!("delete_chatgpt_login", Destroys,
        "Removes the workspace's ChatGPT login. Needs a MergeLoop sign-in.")),
    route!(POST "/api/w/{id}/settings/chatgpt/poll", api::poll_chatgpt_login, tool!("poll_chatgpt_login", Writes,
        "Asks once whether the user entered the ChatGPT code, and keeps the login once they have. Call it no more often than the sign-in's `interval_secs`. Needs a MergeLoop sign-in.")),
    route!(GET "/api/w/{id}/settings/review", review::review_settings, tool!("get_review_settings", Reads,
        "The workspace's settings for the agent's review, the queue's first gate: whether it is on (by default it is, once the workspace has a model key), whether the gate waits for every review thread on a PR to be resolved (`require_resolved`, on by default), how hard the review looks (`depth`: `quick`, one reviewer that reports only what could be wrong; `standard`, the default, one that also says what it checked and found sound; or `deep`, three fresh reviewers at once whose findings merge, and a panel of three that votes on every change no failing command shows, so it spends several times what `standard` does), and how sure the agent must be to send each kind of change back to the PR's author to fix; `always_ask` asks a human about the kind instead. With the defaults, and each kind's findings of the last 90 days by confidence, with how often a human overruled the agent. Needs a MergeLoop sign-in.")),
    route!(PUT "/api/w/{id}/settings/review", review::set_review_settings, tool!("set_review_settings", Writes,
        "Sets the workspace's settings for the agent's review. A repo's config file can set its own, in `review`. Needs a MergeLoop sign-in.",
        arg("settings", Ty::Body, "`{ enabled, bars, require_resolved, depth }`: whether the repos' queues review queued PRs first; each kind's bar, `[{ kind, bar }]`, with kind `security`, `migrations`, `dependencies`, `public_api` or `other`, and bar a percent from 50 to 100: how sure the agent must be to send a change back to the author, lower sending more; or `\"always_ask\"`, to ask a human about the kind. A kind left out keeps its default; optionally whether the gate waits for every review thread to be resolved; and optionally `depth`, `quick`, `standard` or `deep`, how hard the review looks. Each optional one left out keeps what is saved."))),
    route!(GET "/api/w/{id}/settings/builds", builds::build_settings, tool!("get_build_settings", Reads,
        "Whether the workspace's build lane is on: its issues labeled with a repo's label are tasks that its developers' `mergeloop daemon` claims, builds on their machines, and opens PRs for. Off by default. Needs a MergeLoop sign-in.")),
    route!(PUT "/api/w/{id}/settings/builds", builds::set_build_settings, tool!("set_build_settings", Writes,
        "Turns the workspace's build lane on or off. While it is off, every lane tool but `get_build_settings` and this one refuses. Turning it off forgets the tasks nobody holds, which are read again from the issues when it is turned back on; builds that a daemon holds, or that are stuck, keep their record. Needs a MergeLoop sign-in.",
        arg("enabled", Ty::Bool, "`true` on, `false` off."))),
    route!(GET "/api/w/{id}/builds", builds::list_builds, tool!("list_builds", Reads,
        "The build lane's builds in the repos you can read, oldest first: each issue's repo, number and title, its `status` (`ready`, a task; held by a daemon: `claimed` while it builds, `landing` once its PR is open, `waiting` while that PR waits on a human; `stuck`, its holder gave up, for `reason`; `merged`), who holds it (`holder`) with its `claim`, its `stage` (`building` or `reviewing`), seconds since its holder was heard from (`heard_secs`), and its PR. Refused while the lane is off. Needs a MergeLoop sign-in.",
        opt("mine", Ty::Bool, "Only the builds your login holds: claimed, landing or waiting. Default false."))),
    route!(POST "/api/w/{id}/builds/claim", builds::claim_build, tool!("claim_build", Writes,
        "Claims one task of the workspace for you, or answers null when there is none: an issue labeled with its repo's label, in a repo whose queue runs and that GitHub says you may push to, that nobody holds. Issues assigned to you come first, then unassigned ones; one assigned to someone else is never yours, unless its holder went unheard for 30 minutes. Assigns you on the issue. Returns the `build` and its `claim`, which its later calls pass, the repo, its clone URL, default branch and `verify` commands, the issue's number, title and body, the `branch` to push, the commits' `author` (the App's bot) and `co_author` (you), and the PR when the claim taken over had opened one. Heartbeat it with `heartbeat_build` at least every 30 minutes, or anyone may claim it again. Refused while the lane is off. Needs a MergeLoop sign-in.")),
    route!(POST "/api/w/{id}/builds/{build}/heartbeat", builds::heartbeat_build, tool!("heartbeat_build", Writes,
        "Says you are still working on a build you claimed, and what on. Refused once you no longer hold it: given back or over, its issue closed or unlabeled before it had a PR, or claimed again after 30 minutes unheard. Then stop working on it. Needs a MergeLoop sign-in.",
        arg("claim", Ty::Int, "The `claim` that `claim_build` gave."),
        opt("stage", Ty::OneOf(&["building", "reviewing"]), "What you are doing now."))),
    route!(POST "/api/w/{id}/builds/{build}/release", builds::release_build, tool!("release_build", Writes,
        "Gives back a build you claimed, so it is a task again and its issue no longer assigned to you by the claim, or marks it stuck with a reason, which goes on the issue as a comment. A build with a PR given back stays as it is, for anyone to claim at once with its PR. A stuck build is no task until a human takes the label off the issue and puts it back. Refused once you no longer hold it. Needs a MergeLoop sign-in.",
        arg("claim", Ty::Int, "The `claim` that `claim_build` gave."),
        opt("stuck", Ty::Str, "Why it cannot be built. Without it, the build is given back."))),
    route!(POST "/api/w/{id}/builds/{build}/token", builds::mint_push_token, tool!("mint_push_token", Writes,
        "Gives you a GitHub token for the repo of a build you hold, to push its branch and act on its PR as the App's bot: for that one repo, with `contents: write` and `pull_requests: write` only. GitHub ends it after an hour (`expires_at`); ask again for a new one. Refused once you no longer hold the build, when it went 30 minutes unheard (heartbeat it first), or when GitHub says you may not push to the repo. Needs a MergeLoop sign-in.",
        arg("claim", Ty::Int, "The `claim` that `claim_build` gave."))),
    route!(POST "/api/w/{id}/builds/{build}/pr", builds::open_build_pr, tool!("open_build_pr", Writes,
        "Has the App open the PR of a build you hold, once you pushed its `branch`: into the default branch, ready for review, labeled with the repo's label so it queues, assigned to you, its body ending in `Closes #<issue>`. The build is then `landing`. Calling it again returns the same PR. Refused once you no longer hold the build. Needs a MergeLoop sign-in.",
        arg("claim", Ty::Int, "The `claim` that `claim_build` gave."),
        arg("title", Ty::Str, "The PR's title."),
        arg("body", Ty::Str, "What the PR does."))),
    route!(POST "/api/w/{id}/builds/{build}/report", builds::report_build, tool!("report_build", Writes,
        "Says how the PR of a build you hold stands: `landing` while you act on what the queue needs of its author, `waiting` while it waits on a human, `merged` once it merged, which ends the build. Keep heartbeating a landing or waiting build, or anyone may claim it again with its PR. Refused before the build has a PR, and once you no longer hold it. Needs a MergeLoop sign-in.",
        arg("claim", Ty::Int, "The `claim` that `claim_build` gave."),
        arg("standing", Ty::OneOf(&["landing", "waiting", "merged"]), "How the PR stands."))),
    route!(GET "/api/w/{id}/billing", billing::billing, tool!("get_billing", Reads,
        "The workspace's plan: trial or subscription, its seats, and the price. Null when self-hosted. Needs a MergeLoop sign-in.")),
    route!(POST "/api/w/{id}/billing/checkout", billing::checkout, tool!("start_checkout", Writes,
        "A Stripe checkout page to subscribe the workspace, for the user to open. Needs a MergeLoop sign-in.")),
    route!(POST "/api/w/{id}/billing/portal", billing::portal, tool!("open_billing_portal", Writes,
        "A Stripe page where the user manages the workspace's subscription. Needs a MergeLoop sign-in.")),
    route!(DELETE "/api/w/{id}/billing/seats/{login}", billing::free_seat, tool!("free_seat", Destroys,
        "Frees a seat. The subscription drops it at its next period. Needs a MergeLoop sign-in.")),
];

pub(crate) fn router() -> Router<AppState> {
    ROUTES.iter().fold(Router::new(), |r, route| {
        r.route(route.path, (route.handler)())
    })
}

pub(crate) fn tools() -> impl Iterator<Item = (&'static Route, &'static Tool)> {
    ROUTES.iter().filter_map(|route| match &route.agent {
        Agent::Tool(tool) => Some((route, tool)),
        Agent::Exempt(_) => None,
    })
}

impl Route {
    /// The path parameters, as `PATH_ARGS` has them, in order. `{name}` goes with `{owner}`.
    pub fn path_args(
        &self,
    ) -> impl Iterator<Item = &'static (&'static str, &'static str, Ty, &'static str)> {
        self.path
            .split('/')
            .filter_map(|s| s.strip_prefix('{')?.strip_suffix('}'))
            .filter(|&p| p != "name")
            .map(|p| {
                PATH_ARGS
                    .iter()
                    .find(|(param, ..)| *param == p)
                    .unwrap_or_else(|| panic!("no argument for {{{p}}}"))
            })
    }

    /// The repo that names the workspace: one in the path, or a `repo` argument.
    pub fn takes_repo(&self, tool: &Tool) -> bool {
        self.path.contains("{owner}") || tool.args.iter().any(|a| a.name == "repo")
    }
}

impl Tool {
    /// For `tools/list`.
    pub fn listing(&self, route: &Route) -> Value {
        let mut properties = Map::new();
        let mut required = Vec::new();
        for (param, name, ty, description) in route.path_args() {
            properties.insert((*name).into(), ty.schema(description));
            // The repo names the workspace.
            if !(*param == "id" && route.takes_repo(self)) {
                required.push(*name);
            }
        }
        for arg in self.args {
            properties.insert(arg.name.into(), arg.ty.schema(arg.description));
            if arg.required {
                required.push(arg.name);
            }
        }
        let annotations = match self.effect {
            Effect::Reads => json!({ "readOnlyHint": true }),
            Effect::Writes => json!({ "readOnlyHint": false, "destructiveHint": false }),
            Effect::Destroys => json!({ "readOnlyHint": false, "destructiveHint": true }),
        };
        json!({
            "name": self.name,
            "description": self.description,
            "inputSchema": { "type": "object", "properties": properties, "required": required },
            "annotations": annotations,
        })
    }
}

impl Ty {
    fn schema(&self, description: &str) -> Value {
        let mut schema = match self {
            Ty::Int => json!({ "type": "integer" }),
            Ty::Str => json!({ "type": "string" }),
            Ty::Bool => json!({ "type": "boolean" }),
            Ty::OneOf(values) => json!({ "type": "string", "enum": values }),
            Ty::Body => json!({ "type": "object" }),
        };
        schema["description"] = description.into();
        schema
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    /// A route added outside the table would be one no agent can call, and no one chose that.
    #[test]
    fn the_server_routes_only_the_table() {
        let lib = include_str!("lib.rs");
        assert_eq!(lib.matches(".route(").count(), 0, "add routes to `ROUTES`");
    }

    #[test]
    fn each_tool_has_its_own_name_and_known_arguments() {
        let mut names = HashSet::new();
        for (route, tool) in tools() {
            assert!(names.insert(tool.name), "two tools named {}", tool.name);
            let mut args: Vec<&str> = route.path_args().map(|(_, name, ..)| *name).collect();
            args.extend(tool.args.iter().map(|a| a.name));
            let unique: HashSet<_> = args.iter().collect();
            assert_eq!(unique.len(), args.len(), "{}: an argument twice", tool.name);
            let bodies = tool
                .args
                .iter()
                .filter(|a| matches!(a.ty, Ty::Body))
                .count();
            assert!(
                bodies == 0 || (bodies == 1 && tool.args.len() == 1 && route.method != Method::GET),
                "{}: a body argument stands alone, on a route with a body",
                tool.name
            );
        }
    }

    #[test]
    fn every_api_route_is_a_tool_or_says_why_not() {
        for route in ROUTES {
            if let Agent::Exempt(why) = route.agent {
                assert!(!why.is_empty(), "{} {}: say why", route.method, route.path);
                assert!(
                    !route.path.starts_with("/api/w/"),
                    "{} {}: the dashboard's workspace calls are an agent's too",
                    route.method,
                    route.path
                );
            }
        }
    }

    /// The README's table of tools is the table here.
    #[test]
    fn the_readme_lists_every_tool() {
        let readme = include_str!("../../../README.md");
        let start = readme
            .find("<!-- tools:")
            .expect("the README marks its table of tools");
        let table = &readme[start..readme.find("<!-- /tools -->").unwrap()];
        let listed: Vec<&str> = table
            .lines()
            .filter_map(|l| l.strip_prefix("| `")?.split_once('`'))
            .map(|(name, _)| name)
            .collect();
        let tools: Vec<&str> = tools().map(|(_, t)| t.name).collect();
        assert_eq!(
            listed, tools,
            "README.md lists the tools in the order of `ROUTES`"
        );
    }
}
