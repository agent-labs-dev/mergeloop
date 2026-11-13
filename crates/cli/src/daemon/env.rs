//! What a build's agent and verify commands may act as. They never act as the developer on
//! GitHub or MergeLoop: a reply or a resolved thread from the developer's own login would count
//! as a human's answer to MergeLoop's review. So every run loses the developer's tokens, `gh`
//! finds no login of theirs, and git asks no credential helper and no SSH agent of theirs. An
//! agent that works a PR's need reads as the App's bot, with a token minted to read only: it
//! reads what anyone who can comment wrote, so it holds nothing that writes. The daemon's push
//! token reaches only its own git and GraphQL calls.

use mergepatrol_resolver::{Env, Room};

/// Names that carry the developer's credentials, or point a tool at them.
const DEVELOPERS: &[&str] = &[
    "GH_TOKEN",
    "GITHUB_TOKEN",
    "GH_ENTERPRISE_TOKEN",
    "GITHUB_ENTERPRISE_TOKEN",
    "GH_HOST",
    "MERGELOOP_TOKEN",
    "MERGELOOP_URL",
    "SSH_AUTH_SOCK",
    "GIT_ASKPASS",
    "SSH_ASKPASS",
    "GIT_CONFIG_PARAMETERS",
];

/// A run that acts as nobody: it builds, and verifies.
pub fn nobody(room: &Room) -> Env {
    let gh = room.tmp().join("gh");
    Env {
        remove: DEVELOPERS.iter().map(|n| (*n).to_owned()).collect(),
        set: [
            // `gh` keeps its login there: in the room, there is none.
            ("GH_CONFIG_DIR", gh.display().to_string()),
            ("GIT_TERMINAL_PROMPT", "0".into()),
            // An empty helper clears those the developer's git config names, such as the
            // macOS keychain's.
            ("GIT_CONFIG_COUNT", "1".into()),
            ("GIT_CONFIG_KEY_0", "credential.helper".into()),
            ("GIT_CONFIG_VALUE_0", String::new()),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v))
        .collect(),
    }
}

/// A run that reads as the App's bot, with `read`, a token that only reads: `gh`, and
/// `mergeloop call` on `server`.
pub fn bot(room: &Room, read: &str, server: &str) -> Env {
    let mut env = nobody(room);
    for name in ["GH_TOKEN", "GITHUB_TOKEN", "MERGELOOP_TOKEN"] {
        env.set.push((name.into(), read.into()));
    }
    env.set.push(("MERGELOOP_URL".into(), server.into()));
    env
}

#[cfg(test)]
mod tests {
    use super::*;

    fn room() -> Room {
        Room {
            root: "/ws/builds/1".into(),
            tree: "/ws/builds/1/tree".into(),
        }
    }

    fn set<'a>(env: &'a Env, name: &str) -> Option<&'a str> {
        env.set
            .iter()
            .rev()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    #[test]
    fn a_build_holds_none_of_the_developers_credentials() {
        let env = nobody(&room());
        for name in [
            "GH_TOKEN",
            "GITHUB_TOKEN",
            "MERGELOOP_TOKEN",
            "SSH_AUTH_SOCK",
        ] {
            assert!(env.remove.iter().any(|n| n == name), "{name}");
            assert_eq!(set(&env, name), None, "{name}");
        }
        assert_eq!(set(&env, "GH_CONFIG_DIR"), Some("/ws/builds/1/tmp/gh"));
        assert_eq!(set(&env, "GIT_CONFIG_KEY_0"), Some("credential.helper"));
        assert_eq!(set(&env, "GIT_CONFIG_VALUE_0"), Some(""));
    }

    #[test]
    fn an_agent_on_a_need_acts_as_the_bot() {
        let env = bot(&room(), "test-bot", "https://ml.example");
        for name in ["GH_TOKEN", "GITHUB_TOKEN", "MERGELOOP_TOKEN"] {
            assert_eq!(set(&env, name), Some("test-bot"), "{name}");
        }
        assert_eq!(set(&env, "MERGELOOP_URL"), Some("https://ml.example"));
        assert_eq!(set(&env, "GH_CONFIG_DIR"), Some("/ws/builds/1/tmp/gh"));
    }
}
