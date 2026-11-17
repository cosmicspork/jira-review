//! Jira from the command line and from agents, where no write is applied
//! without review: reads answer directly; writes are proposed as a change
//! holding the issue's current fields, the proposed ones and its `updated`
//! stamp, shown as a diff, and applied only if the issue has not moved.

pub mod args;
pub mod auth;
pub mod change;
pub mod contract;
pub mod jira;
pub mod mcp;
pub mod review;
#[cfg(unix)]
pub mod serve;
pub mod stdio;
pub mod store;
pub mod views;
pub mod wiki;
