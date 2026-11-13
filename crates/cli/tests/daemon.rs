//! `mergeloop daemon` against a fake server, a fake agent CLI, and a bare repo on disk as the
//! GitHub remote. The fake agent is a shell script, run as the config names it.

mod fake;

use std::path::{Path, PathBuf};
use std::time::Duration;

use mergeloop_cli::client::{Client, Server};
use mergeloop_cli::daemon::Daemon;
use mergeloop_cli::daemon::config::Config;
use mergeloop_cli::daemon::lane::Task;
use mergeloop_cli::session::{self, Secret};
use serde_json::{Value, json};

/// A test's daemon reviews nothing unless it says so: the real `codex` may be on `PATH`.
const NO_REVIEW: &str = "\n[reviewer]\nenabled = false\n";

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
        let out = self.output(name, answer);
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

    /// A fake Claude CLI's output, answering `answer` as its last message: what a fake agent
    /// prints with `cat`.
    fn output(&self, name: &str, answer: &str) -> PathBuf {
        let bin = self.root().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let result = json!({ "type": "result", "result": answer, "is_error": false });
        let out = bin.join(format!("{name}.out"));
        std::fs::write(&out, format!("{{\"type\":\"assistant\"}}\n{result}\n")).unwrap();
        out
    }

    /// How many times the agent ran.
    fn runs(&self) -> usize {
        std::fs::read_to_string(self.root().join("runs"))
            .map(|r| r.lines().count())
            .unwrap_or(0)
    }

    fn config(&self, slots: usize, command: &Path) -> Config {
        let text = format!(
            "slots = {slots}\nworkspace = \"{}\"\naccess = \"full\"\n\n[builder]\ncommand = \"{}\"\n{NO_REVIEW}",
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
            "workspace = \"{}\"\naccess = \"workspace\"\ndeny_read = [{}]\n\n[builder]\ncommand = \"{}\"\n{NO_REVIEW}",
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
        daemon.github = self.fake.url.clone();
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

    // Once the build is over: its slot claims again.
    let claims = |w: &World| {
        w.fake
            .tools()
            .iter()
            .filter(|t| *t == "claim_build")
            .count()
    };
    w.run(w.config(1, &agent), |w| {
        w.fake.lane(|l| l.ended() == 1) && claims(w) >= 2
    })
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
                "mint_build_token",
                "mint_build_token",
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
            "Adds the feature.\n\n### Local review\n\nThe local review was turned off on the \
             machine that built this PR (`[reviewer] enabled = false`), so no reviewer read the \
             change before it opened."
                .to_owned()
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
        format!("cd {clone}/worktrees/tree && (printf '[core]\\n\\tfsmonitor = {fired}/monitor\\n' >> config.worktree && echo worktree-config wrote || echo worktree-config denied)"),
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

/// Whether `workspace` access can be confined here. A Linux machine may have no working
/// bubblewrap, and the test is skipped, unless `MERGELOOP_REQUIRE_SANDBOX` is set, as on CI;
/// macOS always has sandbox-exec.
async fn sandboxed() -> bool {
    let probe = mergepatrol_resolver::Access::workspace(&[]);
    let Err(e) = mergeloop_cli::daemon::verify::check(&probe).await else {
        return true;
    };
    if !cfg!(target_os = "linux") || std::env::var_os("MERGELOOP_REQUIRE_SANDBOX").is_some() {
        panic!("{e:#}");
    }
    eprintln!("skipped: {e:#}");
    false
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[tokio::test(flavor = "multi_thread")]
async fn a_hostile_verify_command_writes_only_its_own_tree_and_reads_no_session() {
    if !sandboxed().await {
        return;
    }
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
        "worktree-config",
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

/// Whether the repo at `repo` holds a blob of `content`.
fn holds_blob(repo: &Path, content: &str) -> bool {
    let hash = std::process::Command::new("git")
        .args(["hash-object", "--stdin"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            child.stdin.take().unwrap().write_all(content.as_bytes())?;
            child.wait_with_output()
        })
        .unwrap();
    let hash = String::from_utf8(hash.stdout).unwrap();
    std::process::Command::new("git")
        .args(["cat-file", "-e", hash.trim()])
        .current_dir(repo)
        .status()
        .unwrap()
        .success()
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[tokio::test(flavor = "multi_thread")]
async fn a_build_that_swaps_its_tree_for_a_link_to_a_denied_folder_is_stuck_and_pushes_none_of_it()
{
    if !sandboxed().await {
        return;
    }
    let w = World::new().await;
    let decoy = w.root().canonicalize().unwrap().join("decoy");
    std::fs::create_dir_all(&decoy).unwrap();
    std::fs::write(decoy.join("token"), "DECOY CREDENTIAL\n").unwrap();
    // The last verify command passes, having put a link to the decoy where the tree was.
    let swap = format!("cd .. && mv tree saved && ln -s {} tree", decoy.display());
    w.fake.add_task(w.task(8, 18, &[&swap]));
    let agent = w.agent("builder", "echo x > x.txt", BUILT);
    let config = w.confined(&agent, std::slice::from_ref(&decoy));

    w.run(config, |w| w.fake.lane(|l| l.ended() == 1)).await;

    let released = w.fake.lane(|l| l.released.clone());
    assert_eq!(released.len(), 1, "{released:?}");
    let why = released[0].1.as_deref().expect("released as stuck");
    assert!(why.contains("replaced"), "{why}");
    assert!(w.fake.lane(|l| l.opened.is_empty()));
    assert!(w.pushed("mergeloop/build-18").is_none());
    let clone = w.workspace().join("repos/o/r.git");
    assert!(!holds_blob(&clone, "DECOY CREDENTIAL\n"), "the clone");
    assert!(!holds_blob(&w.remote(), "DECOY CREDENTIAL\n"), "the remote");
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
async fn a_build_that_panics_is_released_as_stuck_and_cleaned_up_and_the_daemon_goes_on() {
    let w = World::new().await;
    w.fake.add_task(w.task(12, 22, &[]));
    w.fake.add_task(w.task(13, 23, &[]));
    let agent = w.agent("builder", "echo x > x.txt", BUILT);
    let mut daemon = Daemon::new(w.config(1, &agent), w.client());
    daemon.poll = Duration::from_millis(50);
    daemon.in_room = Some(std::sync::Arc::new(|task: &Task| {
        assert_ne!(task.build, 12, "a bug in the daemon");
    }));
    let stop = until(|| w.fake.lane(|l| l.ended() == 2));
    tokio::time::timeout(Duration::from_secs(60), daemon.run(stop))
        .await
        .expect("the daemon stops in time")
        .unwrap();

    let released = w.fake.lane(|l| l.released.clone());
    assert_eq!(released.len(), 1, "{released:?}");
    assert_eq!(released[0].0, 12);
    assert!(
        released[0]
            .1
            .as_deref()
            .unwrap_or_default()
            .contains("a bug in the daemon"),
        "{released:?}"
    );
    assert_eq!(w.fake.args_of("release_build")[0]["claim"], 1);
    assert_eq!(w.fake.lane(|l| l.opened.len()), 1, "the next build goes on");
    assert_eq!(w.left(), (vec![], vec![]));
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
async fn on_start_nothing_a_crash_left_half_made_stops_the_daemon_and_the_next_build_runs() {
    let w = World::new().await;
    // A clone a crash left after `git init`, before it had its origin.
    let clone = w.workspace().join("repos/o/r.git");
    std::fs::create_dir_all(&clone).unwrap();
    git(&clone, &["init", "-q", "--bare"]);
    assert!(clone.join("HEAD").exists());
    // And a process-group record that is no file the daemon can read or remove as one.
    let groups = w.workspace().join("daemon/groups");
    std::fs::create_dir_all(groups.join("77.999999/x")).unwrap();
    // And a lock and a name that are folders.
    std::fs::create_dir_all(w.workspace().join("daemon/lock/x")).unwrap();
    std::fs::create_dir_all(w.workspace().join("daemon/id/x")).unwrap();
    w.fake.add_task(w.task(1, 7, &[]));
    let agent = w.agent("my-agent", "echo built > feature.txt", BUILT);

    w.run(w.config(1, &agent), |w| w.fake.lane(|l| l.ended() == 1))
        .await;

    assert_eq!(w.fake.lane(|l| l.opened.len()), 1, "{:?}", w.fake.tools());
    assert!(w.pushed("mergeloop/build-7").is_some());
    assert_eq!(std::fs::read_dir(&groups).unwrap().count(), 0);
    assert!(w.workspace().join("daemon/id").is_file());
    assert_eq!(w.left(), (vec![], vec![]));
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
        "mint_build_token",
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

/// Whether process `pid` still runs.
fn alive(pid: &str) -> bool {
    std::process::Command::new("kill")
        .args(["-0", pid.trim()])
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap()
        .success()
}

/// The pids noted in `file`, one a line, once there are `n`.
async fn pids(file: &Path, n: usize) -> Vec<String> {
    until(|| std::fs::read_to_string(file).is_ok_and(|p| p.lines().count() >= n)).await;
    let pids = std::fs::read_to_string(file).unwrap();
    pids.lines().map(str::to_owned).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn stopped_mid_build_it_gives_the_build_back_and_cleans_up() {
    let w = World::new().await;
    w.fake.add_task(w.task(7, 16, &[]));
    let noted = w.root().join("pids");
    let body = format!("sleep 60 & echo $! >> '{}'; sleep 60", noted.display());
    let agent = w.agent("sleeper", &body, BUILT);

    let started = noted.clone();
    w.run(w.config(1, &agent), move |_| started.exists()).await;

    assert_eq!(w.fake.lane(|l| l.released.clone()), [(7, None)]);
    assert!(w.pushed("mergeloop/build-16").is_none());
    assert_eq!(w.left(), (vec![], vec![]));
    let left = pids(&noted, 1).await;
    assert!(!alive(&left[0]), "the agent's child outlived the build");
}

#[tokio::test(flavor = "multi_thread")]
async fn what_the_agent_and_verify_leave_running_ends_with_their_run() {
    let w = World::new().await;
    let noted = w.root().join("pids");
    let bg = format!("sleep 60 & echo $! >> '{}'", noted.display());
    w.fake.add_task(w.task(11, 21, &[&bg]));
    let agent = w.agent("leaver", &format!("{bg}; echo x > x.txt"), BUILT);

    w.run(w.config(1, &agent), |w| w.fake.lane(|l| l.ended() == 1))
        .await;

    assert_eq!(w.fake.lane(|l| l.opened.len()), 1);
    for pid in pids(&noted, 2).await {
        assert!(!alive(&pid), "{pid} outlived its run");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn on_start_it_stops_the_processes_a_crashed_daemon_left() {
    let w = World::new().await;
    // A build's process group, as a daemon killed mid-build left it, noted in its folder.
    let mut left = std::process::Command::new("sh");
    left.args(["-c", "sleep 60 & sleep 60"]);
    std::os::unix::process::CommandExt::process_group(&mut left, 0);
    let mut left = left.spawn().unwrap();
    let pid = left.id().to_string();
    // Reaped once it dies, as an orphan of a dead daemon would be.
    let reaped = std::thread::spawn(move || left.wait());
    let started = std::process::Command::new("ps")
        .args(["-o", "lstart=", "-p", &pid])
        .output()
        .unwrap();
    let groups = w.workspace().join("daemon/groups");
    std::fs::create_dir_all(&groups).unwrap();
    std::fs::write(groups.join(format!("77.{pid}")), &started.stdout).unwrap();
    let agent = w.agent("idle", "", BUILT);

    w.run(w.config(1, &agent), |w| {
        w.fake.tools().contains(&"claim_build".to_owned())
    })
    .await;

    let _ = reaped.join();
    let gone = std::process::Command::new("kill")
        .args(["-0", "--", &format!("-{pid}")])
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(!gone.success(), "the group outlived the crash");
    assert_eq!(std::fs::read_dir(&groups).unwrap().count(), 0);
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
    // Found on PATH by the name the config gives it: no `claude` anywhere. With no `codex` on
    // PATH either, it reviews too, in a fresh run.
    let none = w.output("none", NOTHING);
    let body = format!(
        "case \"$prompt\" in 'You are reviewing'*) cat '{}'; exit 0;; esac\necho built > feature.txt",
        none.display()
    );
    w.agent("my-agent", &body, BUILT);
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
    assert!(stderr.contains("reviewer `my-agent` (claude)"), "{stderr}");
    assert!(stderr.contains("opened PR #101"), "{stderr}");
    assert_eq!(w.runs(), 2, "it built, then reviewed");
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
        "[reviewer]",
        "enabled = true",
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

    // The one slot holds the task from its claim: claims after it, once the PR is open, are
    // the next ones.
    let claims = w
        .fake
        .lane_tools()
        .iter()
        .take_while(|t| *t != "heartbeat_build")
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

const THREADS: &str = "Work the threads.";
const WORKED: &str = "Checked it.\nMERGELOOP_REPLY: T1 Checks it now.\nMERGELOOP_REPLY: T9 Not this PR's.\nMERGELOOP_RESOLVE: T1\nMERGELOOP_COMMIT: Check it";

impl World {
    /// A fake agent that builds, and works a need's brief as `WORKED` says. Each run notes the
    /// token `gh` would act with in `tokens`.
    fn lander(&self) -> PathBuf {
        let worked = self.output("worked", WORKED);
        let tokens = self.root().join("tokens");
        let body = format!(
            "echo \"${{GH_TOKEN:-none}}\" >> '{}'\n\
             case \"$prompt\" in '{THREADS}') echo checked >> feature.txt; cat '{}'; exit 0;; esac\n\
             echo built > feature.txt",
            tokens.display(),
            worked.display()
        );
        self.agent("lander", &body, BUILT)
    }

    fn tokens(&self) -> Vec<String> {
        let tokens = std::fs::read_to_string(self.root().join("tokens")).unwrap_or_default();
        tokens.lines().map(str::to_owned).collect()
    }

    /// A build this daemon holds from before, with PR `pr` from `branch`, pushed to the remote.
    fn held(&self, build: u64, issue: u64, pr: u64) {
        let branch = format!("mergeloop/build-{issue}");
        git(
            &self.root().join("seed"),
            &[
                "push",
                "-q",
                "../remote.git",
                &format!("main:refs/heads/{branch}"),
            ],
        );
        std::fs::create_dir_all(self.workspace().join("daemon")).unwrap();
        std::fs::write(self.workspace().join("daemon/id"), "this-one\n").unwrap();
        let mut task = self.task(build, issue, &[]);
        task["pr"] = pr.into();
        let mut seen = self.fake.seen.lock().unwrap();
        seen.lane
            .held
            .push(json!({ "id": build, "claim": 1, "pr": pr, "daemon": "this-one" }));
        seen.lane.held_tasks.insert(build, task);
    }

    fn transcripts(&self) -> Vec<String> {
        let dir = self.workspace().join("transcripts");
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .map(|d| {
                d.map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn its_prs_threads_are_worked_by_the_agent_as_the_bot_and_the_build_ends_once_merged() {
    let w = World::new().await;
    w.fake.add_task(w.task(1, 7, &[]));
    w.fake.script(
        101,
        vec![
            fake::need("threads", "t1", THREADS),
            fake::need("merged", "m1", "#101 merged."),
        ],
    );
    w.fake.seen.lock().unwrap().lane.threads = vec![("T1".into(), false)];
    let agent = w.lander();

    w.run(w.config(1, &agent), |w| w.fake.lane(|l| l.merged(1)))
        .await;

    assert_eq!(w.runs(), 2, "the build, then the need");
    assert_eq!(
        w.tokens(),
        ["none", fake::READ_TOKEN],
        "the builder acts as nobody; the need's agent as the bot, with a token that only reads"
    );
    let [subject, author, _, body] = w.pushed("mergeloop/build-7").unwrap();
    assert_eq!(subject, "Check it");
    assert_eq!(author, format!("{} <{}>", BOT.0, BOT.1));
    assert!(body.contains(&format!("Co-authored-by: {} <{}>", DEV.0, DEV.1)));
    assert_eq!(
        git(&w.remote(), &["show", "mergeloop/build-7:feature.txt"]),
        "built\nchecked\n"
    );
    let head = git(
        &w.remote(),
        &["rev-parse", "--short=7", "mergeloop/build-7"],
    );
    let graphql = w.fake.lane(|l| l.graphql.clone());
    assert!(
        graphql
            .iter()
            .all(|(bearer, ..)| bearer == fake::PUSH_TOKEN),
        "{graphql:?}"
    );
    let replies: Vec<&Value> = graphql
        .iter()
        .filter(|(_, q, _)| q.contains("addPullRequestReviewThreadReply"))
        .map(|(.., vars)| vars)
        .collect();
    assert_eq!(
        replies,
        [
            &json!({ "thread": "T1", "body": format!("Checks it now.\n\nPushed in {}.", head.trim()) })
        ],
        "a thread not on the PR gets no reply"
    );
    assert_eq!(w.fake.lane(|l| l.threads.clone()), [("T1".into(), true)]);
    let need = &w.fake.args_of("next_need")[0];
    assert_eq!(
        (
            &need["workspace"],
            &need["repo"],
            &need["number"],
            &need["for_daemon"]
        ),
        (&json!(1), &json!("o/r"), &json!(101), &json!(true))
    );
    assert!(w.fake.lane(|l| l.released.is_empty()));
    assert_eq!(w.left(), (vec![], vec![]), "no worktree, no branch");
    assert_eq!(w.transcripts(), ["1"], "only its transcripts");
}

/// A shell line that notes in `notes`, as `<who> <tokens>`, which of the fake's two tokens are
/// anywhere in its environment: `none`, `none+read`, `none+push`, `none+push+read`.
fn which_tokens(notes: &Path, who: &str) -> String {
    let (push, read) = (fake::PUSH_TOKEN, fake::READ_TOKEN);
    format!(
        "held=none; env | grep -q '{push}' && held=\"$held+push\"; env | grep -q '{read}' && held=\"$held+read\"; echo \"{who} $held\" >> '{}'",
        notes.display()
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn no_agent_run_nor_verify_command_ever_holds_the_push_token() {
    let w = World::new().await;
    let notes = w.root().join("notes");
    let verify = which_tokens(&notes, "verify");
    w.fake.add_task(w.task(1, 7, &[verify.as_str()]));
    w.fake.script(
        101,
        vec![
            fake::need("threads", "t1", THREADS),
            fake::need("merged", "m1", "#101 merged."),
        ],
    );
    w.fake.seen.lock().unwrap().lane.threads = vec![("T1".into(), false)];
    let worked = w.output("worked", WORKED);
    let body = format!(
        "{}\ncase \"$prompt\" in '{THREADS}') echo checked >> feature.txt; cat '{}'; exit 0;; esac\n\
         echo built > feature.txt",
        which_tokens(&notes, "agent"),
        worked.display()
    );
    let agent = w.agent("noter", &body, BUILT);

    w.run(w.config(1, &agent), |w| w.fake.lane(|l| l.merged(1)))
        .await;

    let notes = std::fs::read_to_string(&notes).unwrap();
    assert_eq!(
        notes.lines().collect::<Vec<_>>(),
        [
            "agent none",
            "verify none",
            "agent none+read",
            "verify none"
        ],
        "the build, its verify, the need, its verify"
    );
    let scopes: Vec<Value> = w
        .fake
        .args_of("mint_build_token")
        .into_iter()
        .map(|a| a["scope"].clone())
        .collect();
    assert!(
        scopes.contains(&json!("push")) && scopes.contains(&json!("read")),
        "{scopes:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_restarted_daemon_lands_the_pr_it_holds_and_a_human_need_runs_no_agent() {
    let w = World::new().await;
    w.held(5, 12, 105);
    w.fake.script(
        105,
        vec![
            fake::need("human", "h1", "It waits on a human: one approval."),
            fake::need("merged", "m1", "#105 merged."),
        ],
    );
    let agent = w.lander();

    w.run(w.config(1, &agent), |w| w.fake.lane(|l| l.merged(5)))
        .await;

    assert_eq!(w.runs(), 0);
    assert_eq!(
        w.fake.lane(|l| l.reports.clone()),
        [(5, "waiting".into()), (5, "merged".into())]
    );
    let task = &w.fake.args_of("get_build_task")[0];
    assert_eq!(
        (&task["build"], &task["claim"], &task["daemon"]),
        (&json!(5), &json!(1), &json!("this-one"))
    );
    assert!(w.fake.lane(|l| l.released.is_empty()), "not given back");
    assert_eq!(w.left(), (vec![], vec![]));
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[tokio::test(flavor = "multi_thread")]
async fn a_need_whose_tree_is_swapped_for_a_link_to_a_denied_folder_is_stuck_and_pushes_none_of_it()
{
    if !sandboxed().await {
        return;
    }
    let w = World::new().await;
    let decoy = w.root().canonicalize().unwrap().join("decoy");
    std::fs::create_dir_all(&decoy).unwrap();
    std::fs::write(decoy.join("token"), "DECOY CREDENTIAL\n").unwrap();
    w.held(5, 12, 105);
    let swap = format!("cd .. && mv tree saved && ln -s {} tree", decoy.display());
    w.fake
        .seen
        .lock()
        .unwrap()
        .lane
        .held_tasks
        .get_mut(&5)
        .unwrap()["verify"] = json!([swap]);
    w.fake
        .script(105, vec![fake::need("threads", "t1", THREADS)]);
    let agent = w.lander();
    let config = w.confined(&agent, std::slice::from_ref(&decoy));

    w.run(config, |w| w.fake.lane(|l| !l.released.is_empty()))
        .await;

    let released = w.fake.lane(|l| l.released.clone());
    let why = released[0].1.as_deref().expect("released as stuck");
    assert!(why.contains("replaced"), "{why}");
    assert_eq!(w.runs(), 1);
    let clone = w.workspace().join("repos/o/r.git");
    assert!(!holds_blob(&clone, "DECOY CREDENTIAL\n"), "the clone");
    assert!(!holds_blob(&w.remote(), "DECOY CREDENTIAL\n"), "the remote");
    assert_eq!(w.left(), (vec![], vec![]));
}

#[tokio::test(flavor = "multi_thread")]
async fn ejected_once_it_is_queued_again_and_ejected_twice_it_is_stuck() {
    let w = World::new().await;
    w.held(5, 12, 105);
    w.fake.script(
        105,
        vec![
            fake::need("ejected", "e1", "MergeLoop ejected #105: checks failed."),
            fake::need("queue", "e1-q", "#105 is not queued."),
            json!({ "needs": "ejected", "id": "e2", "brief": "b", "status": { "ejected": { "reason": "checks failed again" } } }),
        ],
    );
    let agent = w.lander();

    w.run(w.config(1, &agent), |w| {
        w.fake.lane(|l| !l.released.is_empty())
    })
    .await;

    let released = w.fake.lane(|l| l.released.clone());
    assert_eq!(released.len(), 1);
    let why = released[0].1.as_deref().unwrap();
    assert!(why.contains("a second time: checks failed again"), "{why}");
    assert_eq!(w.fake.args_of("queue_pull").len(), 1);
    assert_eq!(w.runs(), 0);
    assert_eq!(w.left(), (vec![], vec![]));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pr_that_waits_holds_no_slot() {
    let w = World::new().await;
    // Waits on the queue for good.
    w.held(5, 12, 105);
    w.fake.add_task(w.task(1, 7, &[]));
    let agent = w.lander();

    // Build 1's PR opened.
    w.run(w.config(1, &agent), |w| {
        w.fake.lane(|l| l.opened.iter().any(|o| o.0 == 1))
    })
    .await;

    assert_eq!(w.runs(), 1, "the one slot built the task");
    assert!(
        w.fake
            .args_of("next_need")
            .iter()
            .any(|a| a["number"] == 105)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_task_claimed_with_its_pr_is_landed_not_given_back() {
    let w = World::new().await;
    w.held(5, 12, 105);
    w.fake.seen.lock().unwrap().lane.held.clear();
    let mut task = w.task(5, 12, &[]);
    task["pr"] = 105.into();
    w.fake.add_task(task);
    w.fake
        .script(105, vec![fake::need("merged", "m1", "#105 merged.")]);
    let agent = w.lander();

    w.run(w.config(1, &agent), |w| w.fake.lane(|l| l.merged(5)))
        .await;

    assert!(w.fake.lane(|l| l.released.is_empty()));
    assert!(
        w.fake.lane(|l| l.assigned.contains(&105)),
        "its setup finished"
    );
    assert_eq!(w.runs(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_build_taken_up_with_its_pr_has_the_pr_labeled_and_assigned_before_it_lands() {
    let w = World::new().await;
    // The server recorded PR #105, then failed before it assigned it.
    w.held(5, 12, 105);
    w.fake
        .script(105, vec![fake::need("merged", "m1", "#105 merged.")]);
    let agent = w.lander();

    w.run(w.config(1, &agent), |w| w.fake.lane(|l| l.merged(5)))
        .await;

    assert!(w.fake.lane(|l| l.assigned.contains(&105)));
    let tools = w.fake.lane_tools();
    assert!(
        in_order(&tools, &["get_build_task", "open_build_pr", "next_need"]),
        "{tools:?}"
    );
    assert!(w.fake.lane(|l| l.released.is_empty()));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_build_taken_up_whose_pr_was_closed_is_stuck_without_landing() {
    let w = World::new().await;
    w.held(5, 12, 105);
    w.fake.refuse("open_build_pr", "closed", 409, CLOSED);
    let agent = w.lander();

    w.run(w.config(1, &agent), |w| w.fake.lane(|l| l.ended() == 1))
        .await;

    let released = w.fake.lane(|l| l.released.clone());
    assert_eq!(released.len(), 1, "{released:?}");
    assert!(released[0].1.as_deref().unwrap().contains("closed PR #105"));
    assert!(w.fake.args_of("next_need").is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_repo_renamed_while_its_pr_lands_is_landed_under_its_new_name() {
    let w = World::new().await;
    w.held(5, 12, 105);
    // The same remote by another path: the renamed repo's URL.
    let moved = w.root().join("moved.git");
    std::os::unix::fs::symlink(w.remote(), &moved).unwrap();
    w.fake.seen.lock().unwrap().lane.threads = vec![("T1".into(), false)];
    w.fake.script(
        105,
        vec![
            fake::rename("o/new", moved.to_str().unwrap()),
            fake::need("threads", "t1", THREADS),
            fake::need("merged", "m1", "#105 merged."),
        ],
    );
    let agent = w.lander();

    w.run(w.config(1, &agent), |w| {
        w.fake.lane(|l| l.merged(5) || !l.released.is_empty())
    })
    .await;

    assert_eq!(w.fake.lane(|l| l.released.clone()), [], "not stuck");
    let asked: Vec<Value> = w.fake.args_of("next_need");
    assert_eq!(asked.first().unwrap()["repo"], "o/r");
    assert_eq!(asked.last().unwrap()["repo"], "o/new");
    assert_eq!(
        git(&w.remote(), &["show", "mergeloop/build-12:feature.txt"]),
        "checked\n"
    );
    let clone = w.workspace().join("repos/o/new.git");
    assert_eq!(
        git(&clone, &["remote", "get-url", "origin"]).trim(),
        moved.to_str().unwrap()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pr_github_would_not_label_yet_is_opened_on_a_later_call() {
    let w = World::new().await;
    w.fake.add_task(w.task(1, 7, &[]));
    w.fake.refuse(
        "open_build_pr",
        "",
        502,
        "GitHub would not label PR #101. Call `open_build_pr` again.",
    );
    let agent = w.lander();

    w.run(w.config(1, &agent), |w| {
        w.fake.lane(|l| l.opened.len() == 1)
    })
    .await;

    let calls = w.fake.args_of("open_build_pr").len();
    assert_eq!(calls, 2);
    assert!(w.fake.lane(|l| l.released.is_empty()));
}

#[tokio::test(flavor = "multi_thread")]
async fn transcripts_older_than_seven_days_are_removed() {
    let w = World::new().await;
    for (build, stamp) in [("1", "202001010000"), ("2", "")] {
        let dir = w.workspace().join("transcripts").join(build);
        std::fs::create_dir_all(&dir).unwrap();
        if !stamp.is_empty() {
            let touched = std::process::Command::new("touch")
                .args(["-t", stamp])
                .arg(&dir)
                .status()
                .unwrap();
            assert!(touched.success());
        }
    }
    let agent = w.lander();

    w.run(w.config(1, &agent), |w| {
        w.fake.tools().contains(&"claim_build".to_owned())
    })
    .await;

    assert_eq!(w.transcripts(), ["2"]);
}

/// The threads the daemon replied in, one entry per reply posted, in order.
fn replies(w: &World) -> Vec<String> {
    let graphql = w.fake.lane(|l| l.graphql.clone());
    graphql
        .iter()
        .filter(|(_, q, _)| q.contains("addPullRequestReviewThreadReply"))
        .map(|(.., vars)| vars["thread"].as_str().unwrap().to_owned())
        .collect()
}

/// How many times the daemon asked GitHub to resolve a thread.
fn resolves(w: &World) -> usize {
    let graphql = w.fake.lane(|l| l.graphql.clone());
    graphql
        .iter()
        .filter(|(_, q, _)| q.contains("resolveReviewThread"))
        .count()
}

#[tokio::test(flavor = "multi_thread")]
async fn an_answer_github_refuses_is_tried_again_without_the_agent_and_posted_once() {
    let w = World::new().await;
    w.held(5, 12, 105);
    w.fake.script(
        105,
        vec![
            fake::need("threads", "t1", THREADS),
            fake::need("merged", "m1", "#105 merged."),
        ],
    );
    {
        let mut seen = w.fake.seen.lock().unwrap();
        seen.lane.threads = vec![("T1".into(), false)];
        seen.lane.graphql_refusals = vec![("resolveReviewThread".into(), 1)];
    }
    let agent = w.lander();

    w.run(w.config(1, &agent), |w| w.fake.lane(|l| l.merged(5)))
        .await;

    assert_eq!(w.runs(), 1, "the agent ran once");
    assert_eq!(replies(&w), ["T1"], "its reply went through once");
    assert_eq!(resolves(&w), 2, "the refused resolve was asked again");
    assert_eq!(w.fake.lane(|l| l.threads.clone()), [("T1".into(), true)]);
    assert!(w.fake.lane(|l| l.released.is_empty()));
}

#[tokio::test(flavor = "multi_thread")]
async fn an_answer_github_keeps_refusing_releases_the_build_as_stuck_saying_why() {
    let w = World::new().await;
    w.held(5, 12, 105);
    w.fake
        .script(105, vec![fake::need("threads", "t1", THREADS)]);
    {
        let mut seen = w.fake.seen.lock().unwrap();
        seen.lane.threads = vec![("T1".into(), false)];
        seen.lane.graphql_refusals = vec![("addPullRequestReviewThreadReply".into(), u32::MAX)];
    }
    let agent = w.lander();

    w.run(w.config(1, &agent), |w| {
        w.fake.lane(|l| !l.released.is_empty())
    })
    .await;

    let released = w.fake.lane(|l| l.released.clone());
    let why = released[0].1.as_deref().expect("released as stuck");
    assert!(
        why.contains("Resource not accessible by integration"),
        "{why}"
    );
    assert_eq!(w.runs(), 1, "the agent ran once");
    assert_eq!(replies(&w).len(), 3, "asked three times");
    assert_eq!(w.left(), (vec![], vec![]));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_run_that_leaves_its_need_as_it_was_is_counted_and_told_and_ends_stuck() {
    let w = World::new().await;
    w.held(5, 12, 105);
    w.fake.script(
        105,
        (0..11)
            .map(|_| fake::need("threads", "t1", THREADS))
            .collect(),
    );
    w.fake.seen.lock().unwrap().lane.threads = vec![("T1".into(), false)];
    let told = w.root().join("told");
    let body = format!(
        "case \"$prompt\" in *'as it was'*) echo told >> '{}';; esac",
        told.display()
    );
    let agent = w.agent("idle", &body, "Nothing to change here.");

    w.run(w.config(1, &agent), |w| {
        w.fake.lane(|l| !l.released.is_empty())
    })
    .await;

    assert_eq!(w.runs(), 10, "the cap on runs");
    let told = std::fs::read_to_string(told).unwrap_or_default();
    assert_eq!(told.lines().count(), 9, "each run after the first is told");
    let released = w.fake.lane(|l| l.released.clone());
    let why = released[0].1.as_deref().expect("released as stuck");
    assert!(why.contains("as it was"), "{why}");
    assert!(replies(&w).is_empty());
    assert!(w.pushed("mergeloop/build-12").unwrap()[0] == "first");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_thread_past_the_first_page_is_answered() {
    let w = World::new().await;
    w.held(5, 12, 105);
    w.fake.script(
        105,
        vec![
            fake::need("threads", "t1", THREADS),
            fake::need("merged", "m1", "#105 merged."),
        ],
    );
    w.fake.seen.lock().unwrap().lane.threads = (0..150).map(|n| (format!("T{n}"), false)).collect();
    let answer = w.output(
        "late",
        "MERGELOOP_REPLY: T140 Done.\nMERGELOOP_RESOLVE: T140\nMERGELOOP_COMMIT: Do it",
    );
    let body = format!(
        "echo done >> feature.txt; cat '{}'; exit 0",
        answer.display()
    );
    let agent = w.agent("late-agent", &body, "unused");

    w.run(w.config(1, &agent), |w| w.fake.lane(|l| l.merged(5)))
        .await;

    assert_eq!(replies(&w), ["T140"]);
    let threads = w.fake.lane(|l| l.threads.clone());
    assert!(threads.iter().any(|t| *t == ("T140".into(), true)));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_server_that_cannot_answer_for_now_never_ends_a_build() {
    let w = World::new().await;
    w.fake.add_task(w.task(1, 7, &[]));
    for tool in ["heartbeat_build", "mint_build_token"] {
        w.fake
            .refuse(tool, "", 502, "GitHub did not answer in time");
    }
    let agent = w.agent("builder", "echo built > feature.txt", BUILT);

    w.run(w.config(1, &agent), |w| w.fake.lane(|l| l.ended() == 1))
        .await;

    assert!(
        w.fake
            .lane(|l| l.released.is_empty() && l.opened.len() == 1),
        "built and opened, not released"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_need_the_server_still_shows_after_a_push_is_not_worked_again_before_it_hears_the_push() {
    let w = World::new().await;
    w.fake.add_task(w.task(1, 7, &[]));
    // Asked again after the push, the server has not heard of it yet: the need is as it was.
    w.fake.script(
        101,
        vec![
            fake::need("threads", "t1", THREADS),
            fake::need("threads", "t1", THREADS),
            fake::need("threads", "t1", THREADS),
            fake::need("merged", "m1", "#101 merged."),
        ],
    );
    w.fake.seen.lock().unwrap().lane.threads = vec![("T1".into(), false)];
    let agent = w.lander();

    w.run(w.config(1, &agent), |w| w.fake.lane(|l| l.merged(1)))
        .await;

    assert_eq!(w.runs(), 2, "the build, then the need once");
    assert_eq!(
        git(&w.remote(), &["show", "mergeloop/build-7:feature.txt"]),
        "built\nchecked\n"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reply_that_leaves_a_thread_open_does_not_settle_the_need_nor_is_posted_again() {
    let w = World::new().await;
    w.held(5, 12, 105);
    w.fake
        .script(105, vec![fake::need("threads", "t1", THREADS)]);
    w.fake.seen.lock().unwrap().lane.threads = vec![("T1".into(), false), ("T2".into(), false)];
    let prompts = w.root().join("prompts");
    let body = format!(
        "printf '%s\\n=====\\n' \"$prompt\" >> '{}'",
        prompts.display()
    );
    let answer =
        "MERGELOOP_REPLY: T1 Fixed it.\nMERGELOOP_RESOLVE: T1\nMERGELOOP_REPLY: T2 Not this PR's.";
    let agent = w.agent("replier", &body, answer);

    w.run(w.config(1, &agent), |w| {
        w.fake.lane(|l| !l.released.is_empty())
    })
    .await;

    assert_eq!(w.runs(), 10, "each run that left the need counts");
    assert_eq!(
        replies(&w),
        ["T1", "T2"],
        "each reply posted once over all runs"
    );
    let prompts = std::fs::read_to_string(prompts).unwrap();
    let prompts: Vec<&str> = prompts.split("=====\n").collect();
    assert!(!prompts[0].contains("still open"), "{}", prompts[0]);
    for later in &prompts[1..10] {
        assert!(
            later.contains("still open: T2.") && !later.contains("T1"),
            "{later}"
        );
        assert!(later.contains("A reply alone does not close"), "{later}");
    }
    let released = w.fake.lane(|l| l.released.clone());
    let why = released[0].1.as_deref().expect("released as stuck");
    assert!(why.contains("as it was"), "{why}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_server_that_cannot_answer_for_now_never_ends_a_landing_build() {
    let w = World::new().await;
    w.held(5, 12, 105);
    w.fake.script(
        105,
        vec![
            fake::need("threads", "t1", THREADS),
            fake::need("merged", "m1", "#105 merged."),
        ],
    );
    w.fake.seen.lock().unwrap().lane.threads = vec![("T1".into(), false)];
    for tool in [
        "get_build_task",
        "next_need",
        "report_build",
        "mint_build_token",
    ] {
        w.fake
            .refuse(tool, "", 502, "GitHub did not answer in time");
    }
    let agent = w.lander();

    w.run(w.config(1, &agent), |w| w.fake.lane(|l| l.merged(5)))
        .await;

    assert_eq!(w.runs(), 1);
    assert_eq!(replies(&w), ["T1"]);
    assert!(w.fake.lane(|l| l.released.is_empty()));
    let reports = w.fake.args_of("report_build");
    assert_eq!(reports[0]["standing"], "landing");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reply_whose_answer_was_lost_is_not_posted_a_second_time() {
    let w = World::new().await;
    w.held(5, 12, 105);
    w.fake.script(
        105,
        vec![
            fake::need("threads", "t1", THREADS),
            fake::need("merged", "m1", "#105 merged."),
        ],
    );
    {
        let mut seen = w.fake.seen.lock().unwrap();
        seen.lane.threads = vec![("T1".into(), false)];
        seen.lane.graphql_lost = vec![("addPullRequestReviewThreadReply".into(), 1)];
    }
    let agent = w.lander();

    w.run(w.config(1, &agent), |w| w.fake.lane(|l| l.merged(5)))
        .await;

    assert_eq!(replies(&w), ["T1"], "asked once: the retry saw it there");
    let comments = w.fake.lane(|l| l.comments["T1"].clone());
    assert_eq!(comments.len(), 1, "{comments:?}");
    assert_eq!(w.fake.lane(|l| l.threads.clone()), [("T1".into(), true)]);
}

const CLOSED: &str = "PR #105 was closed without merging. Release the build as stuck, saying so.";

#[tokio::test(flavor = "multi_thread")]
async fn a_pr_taken_out_and_closed_without_merging_ends_its_build_as_stuck() {
    let w = World::new().await;
    w.held(5, 12, 105);
    w.fake.script(
        105,
        vec![fake::need(
            "removed",
            "r1",
            "#105 was taken out of the queue.",
        )],
    );
    w.fake.refuse("report_build", "closed", 409, CLOSED);
    let agent = w.lander();

    w.run(w.config(1, &agent), |w| {
        w.fake.lane(|l| !l.released.is_empty())
    })
    .await;

    let released = w.fake.lane(|l| l.released.clone());
    let why = released[0].1.as_deref().expect("released as stuck");
    assert!(why.contains("A person closed PR #105"), "{why}");
    assert_eq!(w.runs(), 0);
    assert_eq!(w.left(), (vec![], vec![]));
}

#[tokio::test(flavor = "multi_thread")]
async fn an_issue_whose_pr_was_closed_is_built_anew_over_the_old_branch_once_labeled_again() {
    let w = World::new().await;
    w.held(5, 12, 105);
    let seed = w.root().join("seed");
    git(
        &seed,
        &["commit", "-q", "--allow-empty", "-m", "the closed PR's"],
    );
    git(
        &seed,
        &[
            "push",
            "-q",
            "-f",
            "../remote.git",
            "HEAD:refs/heads/mergeloop/build-12",
        ],
    );
    git(&seed, &["reset", "-q", "--hard", "HEAD~1"]);
    w.fake.script(
        105,
        vec![fake::need("removed", "r1", "#105 was taken out.")],
    );
    w.fake.refuse("report_build", "closed", 409, CLOSED);
    let agent = w.lander();
    // Once stuck, the label goes back on: the server gives the issue as a fresh task, no PR.
    let relabeled = std::sync::atomic::AtomicBool::new(false);

    w.run(w.config(1, &agent), |w| {
        if w.fake.lane(|l| !l.released.is_empty())
            && !relabeled.swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            let mut task = w.task(5, 12, &[]);
            task["claim"] = 2.into();
            w.fake.add_task(task);
        }
        // Opened once more after its take-up's call: built anew.
        w.fake.lane(|l| l.opened.len() == 2)
    })
    .await;

    let released = w.fake.lane(|l| l.released.clone());
    assert!(released[0].1.as_deref().unwrap().contains("closed PR #105"));
    let pushed = w.pushed("mergeloop/build-12").unwrap();
    assert_eq!(pushed[0], "Add the feature", "the old branch is replaced");
    assert_eq!(
        git(&w.remote(), &["rev-parse", "mergeloop/build-12~1"]),
        git(&w.remote(), &["rev-parse", "main"]),
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pr_merged_outside_the_queue_ends_its_build_as_merged() {
    let w = World::new().await;
    w.held(5, 12, 105);
    w.fake.script(
        105,
        vec![fake::need(
            "removed",
            "r1",
            "#105 was taken out of the queue.",
        )],
    );
    w.fake.refuse(
        "report_build",
        "merged",
        409,
        "PR #105 merged. Report it with `report_build` `merged`.",
    );
    let agent = w.lander();

    w.run(w.config(1, &agent), |w| w.fake.lane(|l| l.merged(5)))
        .await;

    assert!(w.fake.lane(|l| l.released.is_empty()));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pr_closed_while_it_waits_on_a_human_ends_its_build_as_stuck() {
    let w = World::new().await;
    w.held(5, 12, 105);
    w.fake.script(
        105,
        vec![fake::need(
            "human",
            "h1",
            "It waits on a human: one approval.",
        )],
    );
    let agent = w.lander();
    let closed = std::sync::atomic::AtomicBool::new(false);

    w.run(w.config(1, &agent), |w| {
        // Closed once the daemon reported it waiting: only asking again finds out.
        let waiting = w.fake.lane(|l| l.reports.len() == 1);
        if waiting && !closed.swap(true, std::sync::atomic::Ordering::SeqCst) {
            w.fake.refuse("report_build", "closed", 409, CLOSED);
        }
        w.fake.lane(|l| !l.released.is_empty())
    })
    .await;

    let released = w.fake.lane(|l| l.released.clone());
    let why = released[0].1.as_deref().expect("released as stuck");
    assert!(why.contains("A person closed PR #105"), "{why}");
    assert_eq!(w.fake.lane(|l| l.reports.clone()), [(5, "waiting".into())]);
}

/// A reviewer's answer when the change needs nothing.
const NOTHING: &str = "{\"findings\": []}";

fn finding(file: &str, line: u32, what: &str) -> String {
    json!({ "findings": [{ "file": file, "line": line, "finding": what }] }).to_string()
}

impl World {
    /// A fake reviewer at `bin/<name>`: a Claude CLI whose `n`th run answers `answers[n]`, or
    /// the last. Each run notes its cwd in `reviews`, its prompt in `review-prompts`, and the
    /// tokens it holds in `notes`, then runs `body`.
    fn reviewer(&self, name: &str, answers: &[&str], body: &str) -> PathBuf {
        let outs: Vec<PathBuf> = answers
            .iter()
            .enumerate()
            .map(|(i, a)| self.output(&format!("{name}-{i}"), a))
            .collect();
        let pick: String = outs
            .iter()
            .enumerate()
            .map(|(i, out)| format!("{i}) out='{}';;\n", out.display()))
            .collect();
        let last = outs.last().unwrap().display();
        let (reviews, prompts) = (
            self.root().join("reviews"),
            self.root().join("review-prompts"),
        );
        let script = self.root().join("bin").join(name);
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nprompt=\"$2\"\nn=$(wc -l < '{reviews}' 2>/dev/null || echo 0)\nn=$((n))\n\
                 echo \"$PWD\" >> '{reviews}'\nprintf '%s\\n---\\n' \"$prompt\" >> '{prompts}'\n{tokens}\n{body}\n\
                 case $n in\n{pick}*) out='{last}';;\nesac\ncat \"$out\"\n",
                reviews = reviews.display(),
                prompts = prompts.display(),
                tokens = which_tokens(&self.root().join("notes"), "reviewer"),
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    /// The folders the reviewer ran in, one a run.
    fn reviews(&self) -> Vec<String> {
        let reviews = std::fs::read_to_string(self.root().join("reviews")).unwrap_or_default();
        reviews.lines().map(str::to_owned).collect()
    }

    /// `config`, with `reviewer` as the reviewer's command, in `access`.
    fn reviewed(&self, builder: &Path, reviewer: &Path, access: &str) -> Config {
        let text = format!(
            "workspace = \"{}\"\naccess = \"{access}\"\ndeny_read = []\n\n[builder]\ncommand = \"{}\"\n\n[reviewer]\ncommand = \"{}\"\n",
            self.workspace().display(),
            builder.display(),
            reviewer.display()
        );
        Config::parse(&text, self.root()).unwrap()
    }

    /// A fake builder that notes each prompt it is given in `prompts`, and builds.
    fn noting_builder(&self) -> PathBuf {
        let prompts = self.root().join("prompts");
        let body = format!(
            "printf '%s\\n---\\n' \"$prompt\" >> '{}'\necho \"built $(($(wc -l < '{}')))\" > feature.txt",
            prompts.display(),
            self.root().join("runs").display()
        );
        self.agent("builder", &body, BUILT)
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_finding_goes_back_to_the_builder_and_once_the_reviewer_asks_nothing_the_pr_opens() {
    let w = World::new().await;
    w.fake.add_task(w.task(1, 7, &["test -f feature.txt"]));
    let builder = w.noting_builder();
    let one = finding("feature.txt", 1, "Say what was built.");
    let reviewer = w.reviewer("reviewer", &[&one, NOTHING], "");

    w.run(w.reviewed(&builder, &reviewer, "full"), |w| {
        w.fake.lane(|l| l.ended() == 1)
    })
    .await;

    assert_eq!(
        (w.runs(), w.reviews().len()),
        (2, 2),
        "built twice, reviewed twice"
    );
    let prompts = std::fs::read_to_string(w.root().join("prompts")).unwrap();
    let second = prompts.split("\n---\n").nth(1).unwrap();
    assert!(
        second.contains("- `feature.txt:1`: Say what was built."),
        "{second}"
    );
    let tree = w.workspace().canonicalize().unwrap().join("builds/1/tree");
    let room = w.workspace().canonicalize().unwrap().join("reviews/1");
    assert!(
        w.reviews().iter().all(|r| *r == room.display().to_string()),
        "{:?}",
        w.reviews()
    );
    let asked = std::fs::read_to_string(w.root().join("review-prompts")).unwrap();
    assert!(
        asked.contains(&format!("git -C {}", tree.display())),
        "{asked}"
    );
    let notes = std::fs::read_to_string(w.root().join("notes")).unwrap();
    assert_eq!(
        notes.lines().collect::<Vec<_>>(),
        ["reviewer none", "reviewer none"]
    );

    // What it pushed is the second build, as one commit, and the PR says how the review went.
    assert_eq!(
        git(&w.remote(), &["show", "mergeloop/build-7:feature.txt"]),
        "built 2\n"
    );
    assert_eq!(
        git(&w.remote(), &["rev-parse", "mergeloop/build-7~1"]),
        git(&w.remote(), &["rev-parse", "main"]),
    );
    let (_, title, body) = w.fake.lane(|l| l.opened[0].clone());
    assert_eq!(title, "Add the feature");
    assert!(body.starts_with("Adds the feature."), "{body}");
    assert!(body.contains("### Local review"), "{body}");
    assert!(
        body.contains("It took 2 rounds: the builder fixed 1 finding(s)"),
        "{body}"
    );
    let stages: Vec<Value> = w
        .fake
        .args_of("heartbeat_build")
        .into_iter()
        .map(|a| a["stage"].clone())
        .collect();
    assert!(stages.contains(&json!("reviewing")), "{stages:?}");
    assert_eq!(w.left(), (vec![], vec![]));
    assert!(!w.workspace().join("reviews/1").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn findings_left_after_three_rounds_release_the_build_as_stuck_with_them_and_push_nothing() {
    let w = World::new().await;
    w.fake.add_task(w.task(1, 7, &[]));
    let builder = w.noting_builder();
    let always = finding("feature.txt", 1, "Still wrong.");
    let reviewer = w.reviewer("reviewer", &[&always], "");

    w.run(w.reviewed(&builder, &reviewer, "full"), |w| {
        w.fake.lane(|l| l.ended() == 1)
    })
    .await;

    assert_eq!((w.runs(), w.reviews().len()), (3, 3));
    let released = w.fake.lane(|l| l.released.clone());
    let why = released[0].1.as_deref().expect("released as stuck");
    assert!(why.contains("after 3 rounds"), "{why}");
    assert!(why.contains("- `feature.txt:1`: Still wrong."), "{why}");
    assert!(w.pushed("mergeloop/build-7").is_none());
    assert!(w.fake.lane(|l| l.opened.is_empty()));
    assert_eq!(w.left(), (vec![], vec![]));
    assert!(!w.workspace().join("reviews/1").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reviewer_that_does_not_answer_with_its_findings_makes_the_build_stuck() {
    let w = World::new().await;
    w.fake.add_task(w.task(1, 7, &[]));
    let builder = w.noting_builder();
    let reviewer = w.reviewer("reviewer", &["Looks good to me."], "");

    w.run(w.reviewed(&builder, &reviewer, "full"), |w| {
        w.fake.lane(|l| l.ended() == 1)
    })
    .await;

    let released = w.fake.lane(|l| l.released.clone());
    let why = released[0].1.as_deref().expect("released as stuck");
    assert!(
        why.contains("The reviewer did not review the change"),
        "{why}"
    );
    assert!(w.pushed("mergeloop/build-7").is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reviewer_that_changes_the_worktree_makes_the_build_stuck_and_none_of_it_is_pushed() {
    let w = World::new().await;
    w.fake.add_task(w.task(1, 7, &[]));
    let builder = w.noting_builder();
    // In full access nothing confines it: the daemon's check of the tree is what holds.
    let tree = w.workspace().join("builds/1/tree");
    let writes = format!("echo reviewer > '{}/feature.txt'", tree.display());
    let reviewer = w.reviewer("reviewer", &[NOTHING], &writes);

    w.run(w.reviewed(&builder, &reviewer, "full"), |w| {
        w.fake.lane(|l| l.ended() == 1)
    })
    .await;

    let released = w.fake.lane(|l| l.released.clone());
    let why = released[0].1.as_deref().expect("released as stuck");
    assert!(
        why.contains("The reviewer changed the build's worktree"),
        "{why}"
    );
    assert!(w.pushed("mergeloop/build-7").is_none());
    assert_eq!(w.left(), (vec![], vec![]));
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[tokio::test(flavor = "multi_thread")]
async fn in_workspace_access_the_reviewer_cannot_write_the_worktree_and_the_pr_opens() {
    if !sandboxed().await {
        return;
    }
    let w = World::new().await;
    w.fake.add_task(w.task(1, 7, &[]));
    let builder = w.noting_builder();
    // The fake holds itself to what a CLI's sandbox gives a run in workspace access: writes in
    // its working folder and the room `--add-dir` names, and nowhere else.
    let tree = w.root().canonicalize().unwrap().join("ws/builds/1/tree");
    // It notes what it tried in `tried`, the one file outside them it may write.
    let tried = w.root().canonicalize().unwrap().join("tried");
    std::fs::write(&tried, "").unwrap();
    let attempt = format!(
        "echo reviewer >> '{tree}/feature.txt' && echo wrote >> '{tried}' || echo refused >> '{tried}'; \
         echo own > \"$PWD/scratch\" && echo own >> '{tried}'",
        tree = tree.display(),
        tried = tried.display()
    );
    let confine = if cfg!(target_os = "macos") {
        format!(
            "room=$(cat_add_dir \"$@\")\nsandbox-exec -p \"(version 1)(allow default)(deny file-write*)(allow file-write* (subpath \\\"$PWD\\\") (subpath \\\"$room\\\") (literal \\\"/dev/null\\\") (literal \\\"{tried}\\\"))\" sh -c \"{}\"",
            attempt.replace('"', "\\\"").replace('$', "\\$"),
            tried = tried.display(),
        )
    } else {
        format!(
            "room=$(cat_add_dir \"$@\")\nbwrap --ro-bind / / --dev /dev --bind \"$PWD\" \"$PWD\" --bind \"$room\" \"$room\" --bind '{tried}' '{tried}' sh -c \"{}\"",
            attempt.replace('"', "\\\"").replace('$', "\\$"),
            tried = tried.display(),
        )
    };
    let body = format!(
        "cat_add_dir() {{ while [ $# -gt 0 ]; do [ \"$1\" = --add-dir ] && {{ echo \"$2\"; return; }}; shift; done; echo \"$PWD\"; }}\n{confine}"
    );
    let reviewer = w.reviewer("reviewer", &[NOTHING], &body);

    w.run(w.reviewed(&builder, &reviewer, "workspace"), |w| {
        w.fake.lane(|l| l.ended() == 1)
    })
    .await;

    let tried = std::fs::read_to_string(&tried).unwrap();
    assert_eq!(
        tried.lines().collect::<Vec<_>>(),
        ["refused", "own"],
        "{tried}"
    );
    assert_eq!(w.fake.lane(|l| l.opened.len()), 1, "the tree check passed");
    assert_eq!(
        git(&w.remote(), &["show", "mergeloop/build-7:feature.txt"]),
        "built 1\n"
    );
    assert_eq!(w.left(), (vec![], vec![]));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_review_turned_off_in_the_config_never_runs_and_the_pr_says_so() {
    let w = World::new().await;
    w.fake.add_task(w.task(1, 7, &[]));
    let builder = w.noting_builder();
    let reviewer = w.reviewer("reviewer", &[NOTHING], "");
    let text = format!(
        "workspace = \"{}\"\naccess = \"full\"\n\n[builder]\ncommand = \"{}\"\n\n[reviewer]\nenabled = false\ncommand = \"{}\"\n",
        w.workspace().display(),
        builder.display(),
        reviewer.display()
    );

    w.run(Config::parse(&text, w.root()).unwrap(), |w| {
        w.fake.lane(|l| l.ended() == 1)
    })
    .await;

    assert_eq!((w.runs(), w.reviews().len()), (1, 0));
    let (_, _, body) = w.fake.lane(|l| l.opened[0].clone());
    assert!(
        body.contains("### Local review\n\nThe local review was turned off"),
        "{body}"
    );
    assert!(
        !body.contains("read the change against the issue"),
        "{body}"
    );
}
