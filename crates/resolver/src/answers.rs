//! What an agent that worked a PR's review threads for the build lane's daemon wants said on
//! them. The agent changes files only: the daemon commits and pushes its change, then replies
//! and resolves as the App's bot, so that each reply can name the pushed commit and no thread is
//! resolved before its fix is on GitHub.

/// A line `MERGELOOP_REPLY: <thread id> <reply>`.
pub const REPLY: &str = "MERGELOOP_REPLY:";
/// A line `MERGELOOP_RESOLVE: <thread id>`.
pub const RESOLVE: &str = "MERGELOOP_RESOLVE:";

/// What the agent's last message asks the daemon to say on the PR's threads.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ThreadAnswers {
    /// Each reply, in order: the thread's id, and what to say.
    pub replies: Vec<(String, String)>,
    /// The threads to resolve, in order, once replied to.
    pub resolve: Vec<String>,
}

/// The answers the agent's last message gives. A line without a thread id, or a reply without
/// text, is not one.
pub fn thread_answers(text: &str) -> ThreadAnswers {
    let mut answers = ThreadAnswers::default();
    for line in text.lines().map(str::trim) {
        if let Some(rest) = line.strip_prefix(REPLY)
            && let Some((thread, reply)) = rest.trim().split_once(char::is_whitespace)
            && !reply.trim().is_empty()
        {
            answers
                .replies
                .push((thread.to_owned(), reply.trim().to_owned()));
        } else if let Some(thread) = line.strip_prefix(RESOLVE).map(str::trim)
            && !thread.is_empty()
            && !thread.contains(char::is_whitespace)
            && !answers.resolve.iter().any(|t| t == thread)
        {
            answers.resolve.push(thread.to_owned());
        }
    }
    answers
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_agents_last_message_says_what_to_reply_and_what_to_resolve() {
        let text = "Fixed the null check.\n\
            MERGELOOP_REPLY: PRRT_1 Checks for null before reading it.\n\
            MERGELOOP_REPLY: PRRT_2   The caller already checks it: src/a.rs:12.\n\
            MERGELOOP_REPLY: PRRT_3\n\
            MERGELOOP_RESOLVE: PRRT_1\n\
            MERGELOOP_RESOLVE: PRRT_1\n\
            MERGELOOP_RESOLVE: PRRT_2 and more\n\
            MERGELOOP_RESOLVE:\n\
            MERGELOOP_COMMIT: Check for null";
        assert_eq!(
            thread_answers(text),
            ThreadAnswers {
                replies: vec![
                    ("PRRT_1".into(), "Checks for null before reading it.".into()),
                    (
                        "PRRT_2".into(),
                        "The caller already checks it: src/a.rs:12.".into()
                    ),
                ],
                resolve: vec!["PRRT_1".into()],
            }
        );
        assert_eq!(thread_answers("Nothing to say."), ThreadAnswers::default());
    }
}
