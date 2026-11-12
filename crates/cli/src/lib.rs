//! `mergeloop`, the command an agent lands a PR with, as a library its subcommands share.
//!
//! - `client`: the server and its MCP endpoint, called with a session or a GitHub token.
//! - `session`: the sessions `mergeloop login` keeps, one per server, private to the user.
//! - `login`: `mergeloop login` and `logout`, through the server's OAuth for MCP clients.
//! - `land`: `mergeloop [PR]`, which waits for what a PR needs.
//! - `gh`: the GitHub CLI, for the current PR and a token.

pub mod client;
pub mod gh;
pub mod land;
pub mod login;
pub mod session;
