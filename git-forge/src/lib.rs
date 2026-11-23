#![cfg_attr(test, allow(clippy::disallowed_methods))]
//! May's git forge layer: the GitHub and GitLab clients and check views
//! behind the `pr` and `ci` commands.
//!
//! [`ci`] records what each job, step and test of a session PR's checks
//! took and reports the abnormally slow ones; [`pr`] holds the forge-side
//! machinery of the `pr` command — check views, the GitLab merge-request
//! backend, the merge watch and the checkout pull sync.

pub mod ci;
pub mod pr;
pub mod publish;
