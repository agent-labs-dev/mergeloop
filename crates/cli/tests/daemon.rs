//! `mergeloop daemon` against a fake server, a fake agent CLI, and a bare repo on disk as the
//! GitHub remote. The fake agent is a shell script, run as the config names it.

mod fake;

use std::path::{Path, PathBuf};
use std::time::Duration;

use mergeloop_cli::client::{Client, Server};
use mergeloop_cli::daemon::Daemon;
use mergeloop_cli::daemon::config::Config;
use mergeloop_cli::session::{self, Secret};
use serde_json::{Value, json};

const BOT: (&str, &str) = ("mp[bot]", "1+mp[bot]@users.noreply.github.com");
const DEV: (&str, &str) = ("Ana", "ana@example.com");

/// A test's world: the fake server, a bare remote with a `main`, and a temp folder.
struct World {
    fake: fake::Server,
    dir: tempfile::TempDir,
}

impl World {
    async fn new() -> Self {
        let fake = fake::Server::start().await;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git(root, &["init", "-q", "--bare", "-b", "main", "remote.git"]);
        git(root, &["init", "-q", "-b", "main", "seed"]);
        std::fs::write(root.join("seed/README.md"), "# r\n").unwrap();
        git(&root.join("seed"), &["add", "."]);
        git(&root.join("seed"), &["commit", "-q", "-m", "first"]);
        git(&root.join("seed"), &["push", "-q", "../remote.git", "main"]);
        World { fake, dir }
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    fn remote(&self) -> PathBuf {
        self.root().join("remote.git")
    }

    fn workspace(&self) -> PathBuf {
        self.root().join("ws")
    }

    /// A task for issue `issue`, as `claim_build` gives it.
    fn task(&self, build: u64, issue: u64, verify: &[&str]) -> Value {
        json!({
            "build": build, "claim": 1, "repo": "o/r",
            "clone_url": self.remote().to_str().unwrap(),
            "issue": issue, "title": format!("Issue {issue}"), "body": "Build it.",
            "default_branch": "main", "branch": format!("mergeloop/build-{issue}"),
            "verify": verify,
            "author": { "name": BOT.0, "email": BOT.1 },
            "co_author": { "name": DEV.0, "email": DEV.1 },
            "pr": null,
        })
    }

    /// A fake Claude CLI at `bin/<name>`: it notes each run in `runs`, runs `body` in the
    /// worktree, and answers `answer` as its last message.
    fn agent(&self, name: &str, body: &str, answer: &str) -> PathBuf {
        let bin = self.root().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let result = json!({ "type": "result", "result": answer, "is_error": false });
        let out = bin.join(format!("{name}.out"));
        std::fs::write(&out, format!("{{\"type\":\"assistant\"}}\n{result}\n")).unwrap();
        let runs = self.root().join("runs");
        let script = bin.join(name);
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nprompt=\"$2\"\necho \"$PWD\" >> '{}'\n{body}\ncat '{}'\n",
                runs.display(),
                out.display()
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    /// How many times the agent ran.
    fn runs(&self) -> usize {
        std::fs::read_to_string(self.root().join("runs"))
            .map(|r| r.lines().count())
            .unwrap_or(0)
    }

    fn config(&self, slots: usize, command: &Path) -> Config {
        let text = format!(
            "slots = {slots}\nworkspace = \"{}\"\naccess = \"full\"\n\n[builder]\ncommand = \"{}\"\n",
            self.workspace().display(),
            command.display()
        );
        Config::parse(&text, self.root()).unwrap()
    }

    /// `config` in `workspace` access, denying reads of `deny_read`.
    fn confined(&self, command: &Path, deny_read: &[PathBuf]) -> Config {
        let paths: Vec<String> = deny_read
            .iter()
            .map(|p| format!("\"{}\"", p.display()))
            .collect();
        let text = format!(
            "workspace = \"{}\"\naccess = \"workspace\"\ndeny_read = [{}]\n\n[builder]\ncommand = \"{}\"\n",
            self.workspace().display(),
            paths.join(", "),
            command.display()
        );
        Config::parse(&text, self.root()).unwrap()
    }

    fn client(&self) -> Client {
        self.fake.add_session("sess");
        let path = self.root().join("home/.mergeloop/session/session.json");
        let server = Server::parse(&self.fake.url).unwrap();
        session::keep(&path, &server, &Secret::new("sess".into())).unwrap();
        Client::signed_in(&server, &path).unwrap()
    }

    /// Runs the daemon until `done` says so, quickly polling and heartbeating.
    async fn run(&self, config: Config, done: impl Fn(&World) -> bool) {
        let mut daemon = Daemon::new(config, self.client());
        daemon.poll = Duration::from_millis(50);
        daemon.heartbeat_every = Duration::from_millis(200);
        let stop = until(|| done(self));
        tokio::time::timeout(Duration::from_secs(60), daemon.run(stop))
            .await
            .expect("the daemon stops in time")
            .unwrap();
    }

    /// The remote's commit on `branch`, if it has one: subject, author, committer, body.
    fn pushed(&self, branch: &str) -> Option<[String; 4]> {
        let out = std::process::Command::new("git")
            .args([
                "log",
                "-1",
                "--format=%s%x00%an <%ae>%x00%cn <%ce>%x00%b",
                branch,
            ])
            .current_dir(self.remote())
            .output()
            .unwrap();
        if !out.status.success() {
            return None;
        }
        let text = String::from_utf8(out.stdout).unwrap();
        let parts: Vec<String> = text.trim().split('\0').map(str::to_owned).collect();
        Some(parts.try_into().unwrap())
    }

    /// What is left of builds in the workspace folder: worktrees, and local build branches.
    fn left(&self) -> (Vec<String>, Vec<String>) {
        let builds = std::fs::read_dir(self.workspace().join("builds"))
            .map(|d| {
                d.map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        let clone = self.workspace().join("repos/o/r.git");
        let branches = if clone.exists() {
            git(
                &clone,
                &[
                    "for-each-ref",
                    "--format=%(refname)",
                    "refs/heads/mergeloop/",
                ],
            )
            .lines()
            .map(str::to_owned)
            .collect()
        } else {
            vec![]
        };
        (builds, branches)
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
    String::from_utf8(out.stdout).unwrap()
}

/// Resolves once `done` is true, checking often.
async fn until(done: impl Fn() -> bool) {
    while !done() {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

const BUILT: &str = "Adds the feature.\nMERGELOOP_COMMIT: Add the feature";

/// Whether `inner` appears in `outer` in order, not necessarily side by side.
fn in_order(outer: &[String], inner: &[&str]) -> bool {
    let mut rest = outer.iter();
    inner.iter().all(|want| rest.any(|t| t == want))
}

#[tokio::test(flavor = "multi_thread")]
async fn a_task_is_built_pushed_as_the_bot_with_the_developer_and_its_pr_opened() {
    let w = World::new().await;
    w.fake.add_task(w.task(1, 7, &["test -f feature.txt"]));
    let agent = w.agent("my-agent", "echo built > feature.txt", BUILT);

    w.run(w.config(1, &agent), |w| w.fake.lane(|l| l.ended() == 1))
        .await;

    let [subject, author, committer, body] = w.pushed("mergeloop/build-7").unwrap();
    assert_eq!(subject, "Add the feature");
    assert_eq!(author, format!("{} <{}>", BOT.0, BOT.1));
    assert_eq!(committer, author);
    assert!(
        body.contains(&format!("Co-authored-by: {} <{}>", DEV.0, DEV.1)),
        "{body}"
    );
    assert_eq!(
        git(&w.remote(), &["show", "mergeloop/build-7:feature.txt"]),
        "built\n"
    );
    assert_eq!(
        git(&w.remote(), &["rev-parse", "mergeloop/build-7~1"]),
        git(&w.remote(), &["rev-parse", "main"]),
        "one commit on the default branch"
    );
    let tools = w.fake.lane_tools();
    assert!(
        in_order(
            &tools,
            &[
                "claim_build",
                "heartbeat_build",
                "mint_push_token",
                "mint_push_token",
                "open_build_pr"
            ]
        ),
        "{tools:?}"
    );
    assert_eq!(
        w.fake.lane(|l| l.opened.clone()),
        [(
            1,
            "Add the feature".to_owned(),
            "Adds the feature.".to_owned()
        )]
    );
    assert!(w.fake.lane(|l| l.released.is_empty()));
    let heartbeat = &w.fake.args_of("heartbeat_build")[0];
    assert_eq!(
        (
            &heartbeat["workspace"],
            &heartbeat["build"],
            &heartbeat["claim"],
            &heartbeat["stage"]
        ),
        (&json!(1), &json!(1), &json!(1), &json!("building"))
    );
    assert_eq!(w.runs(), 1);
    assert_eq!(w.left(), (vec![], vec![]), "no worktree, no branch");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_agent_that_gives_up_releases_the_build_as_stuck_and_pushes_nothing() {
    let w = World::new().await;
    w.fake.add_task(w.task(2, 8, &[]));
    let agent = w.agent(
        "giver",
        "echo half > half.txt",
        "MERGEPATROL_GIVE_UP: the issue does not say which flag",
    );

    w.run(w.config(1, &agent), |w| w.fake.lane(|l| l.ended() == 1))
        .await;

    let released = w.fake.lane(|l| l.released.clone());
    assert_eq!(released.len(), 1);
    let (build, why) = &released[0];
    assert_eq!(*build, 2);
    let why = why.as_deref().unwrap();
    assert!(why.contains("the issue does not say which flag"), "{why}");
    assert!(w.pushed("mergeloop/build-8").is_none(), "no push");
    assert!(!w.fake.tools().contains(&"open_build_pr".to_owned()));
    assert_eq!(w.left(), (vec![], vec![]));
}

#[tokio::test(flavor = "multi_thread")]
async fn verify_that_fails_gets_one_fix_then_the_build_is_stuck() {
    let w = World::new().await;
    // Fixed on the second run: the PR opens.
    w.fake.add_task(w.task(3, 9, &["test -f fixed"]));
    let fixes = w.agent(
        "fixer",
        "case \"$prompt\" in *'failed on it'*) touch fixed;; *) touch broken;; esac",
        BUILT,
    );
    w.run(w.config(1, &fixes), |w| w.fake.lane(|l| l.ended() == 1))
        .await;
    assert_eq!(w.runs(), 2);
    assert_eq!(w.fake.lane(|l| l.opened.len()), 1);

    // Never fixed: stuck, with the command and its output.
    let w = World::new().await;
    w.fake.add_task(w.task(4, 10, &["echo nope; exit 1"]));
    let never = w.agent("never", "touch tried", BUILT);
    w.run(w.config(1, &never), |w| w.fake.lane(|l| l.ended() == 1))
        .await;
    assert_eq!(w.runs(), 2);
    let why = w.fake.lane(|l| l.released[0].1.clone()).unwrap();
    assert!(why.contains("still fails") && why.contains("nope"), "{why}");
    assert!(w.pushed("mergeloop/build-10").is_none());
    assert_eq!(w.left(), (vec![], vec![]));
}

#[tokio::test(flavor = "multi_thread")]
async fn two_slots_never_run_more_than_two_builds_at_once() {
    let w = World::new().await;
    for (build, issue) in [(1, 11), (2, 12), (3, 13)] {
        w.fake.add_task(w.task(build, issue, &[]));
    }
    let gauge = w.root().join("gauge");
    std::fs::create_dir_all(&gauge).unwrap();
    // Each run marks itself running, notes how many are, and stays a while.
    let body = format!(
        "g='{g}'; touch \"$g/run.$$\"; ls \"$g\" | grep -c '^run' >> \"$g/counts\"; sleep 3; rm \"$g/run.$$\"; echo x > x.txt",
        g = gauge.display()
    );
    let agent = w.agent("slow", &body, BUILT);

    w.run(w.config(2, &agent), |w| w.fake.lane(|l| l.ended() == 3))
        .await;

    let counts: Vec<u32> = std::fs::read_to_string(gauge.join("counts"))
        .unwrap()
        .lines()
        .map(|l| l.trim().parse().unwrap())
        .collect();
    assert_eq!(counts.len(), 3);
    assert_eq!(counts.iter().max(), Some(&2), "{counts:?}");
    assert_eq!(w.fake.lane(|l| l.opened.len()), 3);
    assert_eq!(w.left(), (vec![], vec![]));
}

/// A verify command a build's agent wrote, trying what it may not, each attempt noted in the
/// tree as `<what> wrote` or `<what> denied`.
fn hostile(ws: &Path, fired: &Path, session: &Path) -> String {
    let (ws, fired, session) = (ws.display(), fired.display(), session.display());
    let clone = format!("{ws}/repos/o/r.git");
    let trip = format!("#!/bin/sh\\ntouch {fired}/$0.ran\\ncat\\n");
    [
        format!("printf '{trip}' > {clone}/hooks/post-commit && chmod +x {clone}/hooks/post-commit && echo hook wrote || echo hook denied"),
        format!("printf '{trip}' > {clone}/hooks/reference-transaction && echo ref-hook wrote || echo ref-hook denied"),
        format!("printf '[filter \"evil\"]\\n\\tclean = {fired}/clean\\n' >> {clone}/config && echo filter wrote || echo filter denied"),
        format!("rm -f {ws}/daemon/lock && echo lock wrote || echo lock denied"),
        format!("mkdir -p {ws}/builds/2/tree && echo other-build wrote || echo other-build denied"),
        format!("cat {session} && echo session wrote || echo session denied"),
        // What the build may write: its own tree, pointed at a repo of its own.
        "echo '* filter=evil' > .gitattributes".to_owned(),
        format!("git init -q --bare evil && printf '{trip}' > evil/hooks/pre-commit && chmod +x evil/hooks/pre-commit && printf '[core]\\n\\tfsmonitor = {fired}/monitor\\n[filter \"evil\"]\\n\\tclean = {fired}/clean\\n' > evil/config"),
        "echo \"gitdir: $PWD/evil\" > .git".to_owned(),
    ]
    .iter()
    .map(|line| format!("({line}) >> attempts 2>/dev/null"))
    .collect::<Vec<_>>()
    .join("; ")
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[tokio::test(flavor = "multi_thread")]
async fn a_hostile_verify_command_writes_only_its_own_tree_and_reads_no_session() {
    let w = World::new().await;
    let fired = w.root().join("fired");
    std::fs::create_dir_all(&fired).unwrap();
    // The session the daemon signed in with is in its own folder, denied to every build.
    let session_dir = w.root().join("home/.mergeloop/session");
    let ws = w.root().canonicalize().unwrap().join("ws");
    let verify = hostile(&ws, &fired, &session_dir.join("session.json"));
    w.fake.add_task(w.task(8, 18, &[&verify]));
    let agent = w.agent("builder", "echo x > x.txt", BUILT);
    let config = w.confined(&agent, &[session_dir]);

    w.run(config, |w| w.fake.lane(|l| l.ended() == 1)).await;

    assert_eq!(
        w.fake.lane(|l| l.opened.len()),
        1,
        "{:?}",
        w.fake.lane(|l| l.released.clone())
    );
    let attempts = git(&w.remote(), &["show", "mergeloop/build-18:attempts"]);
    for what in [
        "hook",
        "ref-hook",
        "filter",
        "lock",
        "other-build",
        "session",
    ] {
        assert!(
            attempts.contains(&format!("{what} denied")),
            "{what}: {attempts}"
        );
    }
    assert!(!attempts.contains("wrote"), "{attempts}");
    let session =
        std::fs::read_to_string(w.root().join("home/.mergeloop/session/session.json")).unwrap();
    assert!(
        !attempts.contains(session.trim()),
        "read the session: {attempts}"
    );
    let clone = ws.join("repos/o/r.git");
    assert!(!clone.join("hooks/post-commit").exists());
    assert!(
        !std::fs::read_to_string(clone.join("config"))
            .unwrap()
            .contains("evil")
    );
    assert!(ws.join("daemon/lock").exists());
    // The daemon's git ran none of what the tree pointed it at.
    let ran: Vec<_> = std::fs::read_dir(&fired)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert!(ran.is_empty(), "ran {ran:?}");
    assert_eq!(w.left(), (vec![], vec![]));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_branch_that_moved_under_the_build_is_not_overwritten_and_the_build_is_stuck() {
    let w = World::new().await;
    w.fake.add_task(w.task(9, 19, &[]));
    let main = git(&w.remote(), &["rev-parse", "main"]).trim().to_owned();
    // Someone pushes to the build's branch while the agent works.
    let body = format!(
        "git --git-dir='{}' update-ref refs/heads/mergeloop/build-19 {main}; echo x > x.txt",
        w.remote().display()
    );
    let agent = w.agent("raced", &body, BUILT);

    w.run(w.config(1, &agent), |w| w.fake.lane(|l| l.ended() == 1))
        .await;

    let released = w.fake.lane(|l| l.released.clone());
    let why = released[0].1.as_deref().unwrap_or_default();
    assert!(why.contains("mergeloop/build-19 changed"), "{released:?}");
    assert_eq!(
        git(&w.remote(), &["rev-parse", "mergeloop/build-19"]).trim(),
        main
    );
    assert!(w.fake.lane(|l| l.opened.is_empty()));
    assert_eq!(w.left(), (vec![], vec![]));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_branch_left_from_before_the_build_is_replaced() {
    let w = World::new().await;
    git(&w.remote(), &["branch", "mergeloop/build-20", "main"]);
    w.fake.add_task(w.task(10, 20, &[]));
    let agent = w.agent("builder", "echo x > x.txt", BUILT);

    w.run(w.config(1, &agent), |w| w.fake.lane(|l| l.ended() == 1))
        .await;

    assert_eq!(w.fake.lane(|l| l.opened.len()), 1);
    let pushed = w.pushed("mergeloop/build-20").unwrap();
    assert_eq!(pushed[0], "Add the feature");
}

#[tokio::test(flavor = "multi_thread")]
async fn on_start_a_worktree_and_branch_of_a_build_it_does_not_hold_are_removed() {
    let w = World::new().await;
    // What a crash left: the clone, a worktree on its branch, and a branch alone.
    let clone = w.workspace().join("repos/o/r.git");
    std::fs::create_dir_all(clone.parent().unwrap()).unwrap();
    git(
        w.root(),
        &[
            "clone",
            "-q",
            "--bare",
            "remote.git",
            clone.to_str().unwrap(),
        ],
    );
    let wt = w.workspace().join("builds/99");
    git(
        &clone,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "mergeloop/build-5",
            wt.to_str().unwrap(),
            "main",
        ],
    );
    git(&clone, &["branch", "mergeloop/build-6", "main"]);
    std::fs::create_dir_all(w.workspace().join("builds/half-made")).unwrap();
    // And a clone a crash left before `git init`.
    std::fs::create_dir_all(w.workspace().join("repos/o/half.git")).unwrap();
    assert_eq!(w.left().1.len(), 2);
    let agent = w.agent("idle", "", BUILT);

    w.run(w.config(1, &agent), |w| {
        w.fake
            .tools()
            .iter()
            .filter(|t| *t == "claim_build")
            .count()
            >= 1
    })
    .await;

    assert_eq!(w.left(), (vec![], vec![]));
    assert!(!git(&clone, &["worktree", "list"]).contains("builds/99"));
    assert!(!w.workspace().join("repos/o/half.git").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn on_start_it_gives_back_only_the_builds_without_a_pr_that_this_daemon_holds() {
    let w = World::new().await;
    std::fs::create_dir_all(w.workspace().join("daemon")).unwrap();
    std::fs::write(w.workspace().join("daemon/id"), "this-one\n").unwrap();
    w.fake.seen.lock().unwrap().lane.held = vec![
        json!({ "id": 40, "claim": 3, "pr": null, "daemon": "this-one" }),
        json!({ "id": 41, "claim": 2, "pr": 141, "daemon": "this-one" }),
        // The same login's other daemon, alive on another machine.
        json!({ "id": 42, "claim": 1, "pr": null, "daemon": "another-one" }),
    ];
    let agent = w.agent("idle", "", BUILT);

    w.run(w.config(1, &agent), |w| {
        w.fake.tools().contains(&"claim_build".to_owned())
    })
    .await;

    assert_eq!(w.fake.lane(|l| l.released.clone()), [(40, None)]);
    let released = &w.fake.args_of("release_build")[0];
    assert_eq!(
        (&released["claim"], &released["daemon"]),
        (&json!(3), &json!("this-one"))
    );
    assert_eq!(w.fake.args_of("list_builds")[0]["mine"], true);
    assert_eq!(w.fake.args_of("claim_build")[0]["daemon"], "this-one");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_daemon_names_itself_once_and_keeps_its_name() {
    let w = World::new().await;
    let agent = w.agent("idle", "", BUILT);
    let claimed = |w: &World| {
        w.fake
            .tools()
            .iter()
            .filter(|t| *t == "claim_build")
            .count()
    };

    w.run(w.config(1, &agent), |w| claimed(w) >= 1).await;
    let (before, listed) = (claimed(&w), w.fake.args_of("list_builds").len());
    w.run(w.config(1, &agent), |w| claimed(w) > before).await;

    let names: Vec<Value> = w
        .fake
        .args_of("claim_build")
        .iter()
        .map(|a| a["daemon"].clone())
        .collect();
    let first = names[0].as_str().expect("a claim names its daemon");
    assert!(first.len() >= 16, "{first}");
    assert!(names.iter().all(|n| n == first), "{names:?}");
    assert_eq!(w.fake.args_of("list_builds")[listed]["daemon"], first);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_build_it_no_longer_holds_is_dropped_without_a_word_and_cleaned_up() {
    let w = World::new().await;
    w.fake.add_task(w.task(5, 14, &[]));
    w.fake.refuse(
        "mint_push_token",
        "lost",
        409,
        "You no longer hold build 5.",
    );
    let agent = w.agent("never-runs", "", BUILT);

    // It claims again once the slot is free.
    w.run(w.config(1, &agent), |w| {
        w.fake
            .tools()
            .iter()
            .filter(|t| *t == "claim_build")
            .count()
            >= 2
    })
    .await;

    assert!(
        w.fake
            .lane(|l| l.released.is_empty() && l.opened.is_empty())
    );
    assert_eq!(w.runs(), 0);
    assert_eq!(w.left(), (vec![], vec![]));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cancelled_build_is_given_back_not_stuck() {
    let w = World::new().await;
    w.fake.add_task(w.task(6, 15, &[]));
    w.fake.refuse(
        "heartbeat_build",
        "cancelled",
        409,
        "Build 6's issue was closed.",
    );
    let agent = w.agent("never-runs", "", BUILT);

    w.run(w.config(1, &agent), |w| w.fake.lane(|l| l.ended() == 1))
        .await;

    assert_eq!(w.fake.lane(|l| l.released.clone()), [(6, None)]);
    assert_eq!(w.runs(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn stopped_mid_build_it_gives_the_build_back_and_cleans_up() {
    let w = World::new().await;
    w.fake.add_task(w.task(7, 16, &[]));
    let agent = w.agent("sleeper", "touch started; sleep 30", BUILT);

    let started = w.root().join("runs");
    w.run(w.config(1, &agent), move |_| started.exists()).await;

    assert_eq!(w.fake.lane(|l| l.released.clone()), [(7, None)]);
    assert!(w.pushed("mergeloop/build-16").is_none());
    assert_eq!(w.left(), (vec![], vec![]));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_sign_in_the_server_no_longer_takes_stops_the_daemon() {
    let w = World::new().await;
    let agent = w.agent("idle", "", BUILT);
    let config = w.config(1, &agent);
    let client = w.client();
    w.fake.seen.lock().unwrap().sessions.clear();

    let err = Daemon::new(config, client)
        .run(std::future::pending())
        .await
        .unwrap_err();

    assert!(format!("{err:#}").contains("mergeloop login"), "{err:#}");
}

/// Runs the `mergeloop` binary with `home` as `$HOME`, the fake as its server, and the test's
/// `bin` folder first on `PATH`.
fn mergeloop(w: &World, home: &Path, args: &[&str]) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_mergeloop"));
    cmd.args(args)
        .env("HOME", home)
        .env(
            "PATH",
            format!("{}:/usr/bin:/bin", w.root().join("bin").display()),
        )
        .env("MERGELOOP_URL", &w.fake.url)
        .env_remove("MERGELOOP_TOKEN")
        .kill_on_drop(true);
    cmd
}

#[tokio::test(flavor = "multi_thread")]
async fn the_command_runs_the_agent_the_config_names_and_stops_on_ctrl_c() {
    let w = World::new().await;
    w.fake.add_task(w.task(1, 7, &[]));
    // Found on PATH by the name the config gives it: no `claude` anywhere.
    w.agent("my-agent", "echo built > feature.txt", BUILT);
    let home = w.root().join("home");
    let _ = w.client();
    std::fs::write(
        home.join(".mergeloop/daemon.toml"),
        format!(
            "access = \"full\"\nworkspace = \"{}\"\n[builder]\ncommand = \"my-agent\"\n",
            w.workspace().display()
        ),
    )
    .unwrap();

    let child = mergeloop(&w, &home, &["daemon"])
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    tokio::time::timeout(
        Duration::from_secs(60),
        until(|| w.fake.lane(|l| l.ended() == 1)),
    )
    .await
    .expect("the PR opens");
    let pid = child.id().unwrap().to_string();
    assert!(
        std::process::Command::new("kill")
            .args(["-INT", &pid])
            .status()
            .unwrap()
            .success()
    );
    let out = tokio::time::timeout(Duration::from_secs(30), child.wait_with_output())
        .await
        .expect("it stops")
        .unwrap();

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{stderr}");
    assert!(stderr.contains("builder `my-agent`"), "{stderr}");
    assert!(stderr.contains("opened PR #101"), "{stderr}");
    assert_eq!(w.runs(), 1);
    assert!(w.pushed("mergeloop/build-7").is_some());
    assert_eq!(w.left(), (vec![], vec![]));
}

#[tokio::test(flavor = "multi_thread")]
async fn daemon_help_shows_every_setting_with_its_default() {
    let w = World::new().await;
    let out = mergeloop(&w, w.root(), &["daemon", "--help"])
        .output()
        .await
        .unwrap();
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{out:?}");
    for said in [
        "~/.mergeloop/daemon.toml",
        "slots = 1",
        "workspace = \"~/.mergeloop/daemon\"",
        "access = \"workspace\"",
        "agent = \"claude\"",
        "command = \"<agent>\"",
        "mclaude",
        "model = ",
        "effort = ",
        "args = []",
        "timeout_minutes = 60",
    ] {
        assert!(help.contains(said), "{said}: {help}");
    }
    assert!(w.fake.tools().is_empty(), "help calls nobody");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bad_config_stops_the_command_and_names_the_line() {
    let w = World::new().await;
    let config = w.root().join("bad.toml");
    std::fs::write(&config, "slots = 1\nacess = \"full\"\n").unwrap();
    let out = mergeloop(
        &w,
        w.root(),
        &["daemon", "--config", config.to_str().unwrap()],
    )
    .output()
    .await
    .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "{stderr}");
    assert!(
        stderr.contains("bad.toml") && stderr.contains("line 2"),
        "{stderr}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_workspace_whose_lane_is_off_is_asked_again_later() {
    let w = World::new().await;
    w.fake.add_task(w.task(8, 17, &[]));
    let off = "The build lane is off for this workspace.";
    w.fake.refuse("list_builds", "lane_off", 409, off);
    w.fake.refuse("claim_build", "lane_off", 409, off);
    let agent = w.agent("builder", "echo x > x.txt", BUILT);

    w.run(w.config(1, &agent), |w| w.fake.lane(|l| l.ended() == 1))
        .await;

    let claims = w
        .fake
        .tools()
        .iter()
        .filter(|t| *t == "claim_build")
        .count();
    assert_eq!(claims, 2, "refused, then the task");
    assert_eq!(w.fake.lane(|l| l.opened.len()), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_claim_is_tried_again_later() {
    let w = World::new().await;
    w.fake.add_task(w.task(9, 18, &[]));
    w.fake.refuse("claim_build", "", 500, "Internal error.");
    let agent = w.agent("builder", "echo x > x.txt", BUILT);

    w.run(w.config(1, &agent), |w| w.fake.lane(|l| l.ended() == 1))
        .await;

    assert_eq!(w.fake.lane(|l| l.opened.len()), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_sign_in_refused_mid_run_stops_every_build_and_the_daemon() {
    for refused in ["claim_build", "heartbeat_build"] {
        let w = World::new().await;
        w.fake.add_task(w.task(10, 19, &[]));
        w.fake.refuse(refused, "", 401, "Sign in again first.");
        let agent = w.agent("builder", "echo x > x.txt", BUILT);
        let mut daemon = Daemon::new(w.config(1, &agent), w.client());
        daemon.poll = Duration::from_millis(50);

        let err = tokio::time::timeout(Duration::from_secs(30), daemon.run(std::future::pending()))
            .await
            .expect("it stops by itself")
            .unwrap_err();

        assert!(
            format!("{err:#}").contains("Sign in again"),
            "{refused}: {err:#}"
        );
        assert!(w.fake.lane(|l| l.opened.is_empty()), "{refused}");
        assert_eq!(w.left(), (vec![], vec![]), "{refused}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pr_refused_for_an_unheard_claim_is_opened_after_a_heartbeat() {
    let w = World::new().await;
    w.fake.add_task(w.task(8, 17, &[]));
    w.fake.refuse(
        "open_build_pr",
        "unheard",
        409,
        "Build 8 was not heard from for 30 minutes.",
    );
    let agent = w.agent("my-agent", "echo built > feature.txt", BUILT);

    w.run(w.config(1, &agent), |w| w.fake.lane(|l| l.ended() == 1))
        .await;

    let tools = w.fake.lane_tools();
    assert!(
        in_order(
            &tools,
            &["open_build_pr", "heartbeat_build", "open_build_pr"]
        ),
        "{tools:?}"
    );
    assert_eq!(w.fake.lane(|l| l.opened.len()), 1);
    assert!(w.fake.lane(|l| l.released.is_empty()));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_build_whose_pr_merged_is_reported_merged_not_stuck() {
    let w = World::new().await;
    w.fake.add_task(w.task(9, 18, &[]));
    w.fake
        .refuse("open_build_pr", "merged", 409, "PR #40 merged.");
    let agent = w.agent("my-agent", "echo built > feature.txt", BUILT);

    w.run(w.config(1, &agent), |w| {
        w.fake.tools().contains(&"report_build".to_owned())
    })
    .await;

    assert_eq!(w.fake.args_of("report_build")[0]["standing"], "merged");
    assert!(w.fake.lane(|l| l.released.is_empty()));
    assert_eq!(w.left(), (vec![], vec![]));
}
