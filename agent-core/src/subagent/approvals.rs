//! Approval requests a subagent puts to the agent that launched it.
//!
//! A subagent's `unsandboxed` escalation does not go straight to the user:
//! it parks here until the parent agent answers it with `subagent approve`
//! or `subagent deny`. The answer the child reads comes only from its own
//! parent — `decide` refuses any other agent's answer — and a denial
//! carries the parent's reason back to the child.
//!
//! [`ChildApprovals`] is shared three ways: the session's permission
//! handler parks the child's request on [`open`](Self::open), the `subagent`
//! call that launched the child watches [`wait_for`](Self::wait_for) so the
//! parent's turn is released when the child needs a decision, and the
//! `subagent approve`/`deny` commands resolve through
//! [`decide`](Self::decide).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use aither_core::llm::{AgentIdentity, Approval, ApprovalRequest, CallIdentity};
use askama::Template;

use crate::agent_link::{AttributeText, UntrustedText};

/// A subagent's request awaiting its parent's decision, as the parent's
/// `subagent` call and the `subagent approve`/`deny` commands see it.
#[derive(Debug)]
pub struct ChildApproval {
    /// The id the parent answers, a `word-word-word` sequence.
    id: String,
    /// The call that launched the child asking.
    parent: CallIdentity,
    /// The child asking.
    child: AgentIdentity,
    /// What it asks to do.
    request: ApprovalRequest,
}

impl ChildApproval {
    /// The id `subagent approve`/`deny` name.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The name of the subagent asking.
    #[must_use]
    pub fn child_name(&self) -> &str {
        match &self.child {
            AgentIdentity::Subagent(subagent) => subagent.name(),
            AgentIdentity::Main => "main",
        }
    }

    /// The request, verbatim as the child raised it.
    #[must_use]
    pub const fn request(&self) -> &ApprovalRequest {
        &self.request
    }

    /// The call that launched the child asking, for attribution.
    #[must_use]
    pub const fn parent(&self) -> &CallIdentity {
        &self.parent
    }
}

#[derive(Debug)]
struct PendingEntry {
    parent: CallIdentity,
    child: AgentIdentity,
    request: ApprovalRequest,
    decision: async_channel::Sender<Approval>,
}

#[derive(Debug)]
struct Shared {
    pending: Mutex<HashMap<String, PendingEntry>>,
    /// The parent calls whose `subagent` call is watching for a request to
    /// them: a request nobody watches goes to the parent's session as a
    /// prompt instead.
    watchers: Mutex<Vec<CallIdentity>>,
    notify: event_listener::Event,
    /// Where a request nobody watches is delivered: the parent's session,
    /// as hidden context that wakes it.
    prompts: Mutex<Option<(crate::task_wake::SessionPrompts, String)>>,
}

/// The registry of a session's subagent approval requests.
///
/// Cloning shares the registry: the permission handler and the `subagent`
/// command see the same requests.
#[derive(Debug, Clone)]
pub struct ChildApprovals {
    shared: Arc<Shared>,
}

impl Default for ChildApprovals {
    fn default() -> Self {
        Self {
            shared: Arc::new(Shared {
                pending: Mutex::new(HashMap::new()),
                watchers: Mutex::new(Vec::new()),
                notify: event_listener::Event::new(),
                prompts: Mutex::new(None),
            }),
        }
    }
}

/// A request's own call did not identify the subagent asking or the parent
/// call that launched it.
#[derive(Debug, thiserror::Error)]
#[error("an approval request to a parent must carry the subagent call that raised it")]
pub struct OpenError;

/// The registration a [`ChildApprovals::watch`] returns; dropping it ends
/// the watch.
struct Watcher<'a> {
    shared: &'a Shared,
    parent: CallIdentity,
}

impl Drop for Watcher<'_> {
    fn drop(&mut self) {
        let mut watchers = self
            .shared
            .watchers
            .lock()
            .expect("child approvals watchers lock poisoned");
        if let Some(position) = watchers.iter().position(|call| *call == self.parent) {
            watchers.swap_remove(position);
        }
    }
}

/// The reminder a request nobody watches is delivered as, to the parent
/// session that launched the asking subagent.
#[derive(Template)]
#[template(path = "subagent/approval_request.txt", escape = "none")]
struct ApprovalReminder<'a> {
    subagent: AttributeText<'a>,
    id: AttributeText<'a>,
    subject: UntrustedText<'a>,
    reason: Option<UntrustedText<'a>>,
}

impl ChildApprovals {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Delivers requests nobody watches to `session_id` through `prompts`:
    /// hidden context that wakes an idle session and queues behind a busy
    /// one.
    ///
    /// # Panics
    ///
    /// Panics if the registry lock is poisoned.
    #[must_use]
    pub fn with_session(
        self,
        prompts: crate::task_wake::SessionPrompts,
        session_id: impl Into<String>,
    ) -> Self {
        *self
            .shared
            .prompts
            .lock()
            .expect("child approvals prompts lock poisoned") = Some((prompts, session_id.into()));
        self
    }

    /// The session a request nobody watches is delivered to, if bound.
    ///
    /// # Panics
    ///
    /// Panics if the registry lock is poisoned.
    #[must_use]
    pub fn session(&self) -> Option<(crate::task_wake::SessionPrompts, String)> {
        self.shared
            .prompts
            .lock()
            .expect("child approvals prompts lock poisoned")
            .clone()
    }

    /// Parks `request` as a request to the parent the call that raised it
    /// names: [`ApprovalRequest::call`] must carry an
    /// [`AgentIdentity::Subagent`] whose `started_by` is set. The returned
    /// receiver resolves once [`decide`](Self::decide) answers it.
    ///
    /// # Errors
    /// [`OpenError`] when the request names no subagent or no parent call.
    ///
    /// # Panics
    ///
    /// Panics if the registry lock is poisoned.
    pub fn open(
        &self,
        request: ApprovalRequest,
    ) -> Result<(ChildApproval, async_channel::Receiver<Approval>), OpenError> {
        let call = request.call().ok_or(OpenError)?;
        let AgentIdentity::Subagent(subagent) = call.agent() else {
            return Err(OpenError);
        };
        let parent = subagent.started_by().cloned().ok_or(OpenError)?;
        let (decision, rx) = async_channel::bounded(1);
        let id = crate::id::new();
        self.shared
            .pending
            .lock()
            .expect("child approvals lock poisoned")
            .insert(
                id.clone(),
                PendingEntry {
                    parent: parent.clone(),
                    child: call.agent().clone(),
                    request: request.clone(),
                    decision,
                },
            );
        self.shared.notify.notify(usize::MAX);
        Ok((
            ChildApproval {
                id,
                parent,
                child: call.agent().clone(),
                request,
            },
            rx,
        ))
    }

    /// Delivers `approval`'s request to its parent's session when no
    /// `subagent` call watches for it: the one-shot alert `wait_for`
    /// otherwise delivers itself.
    ///
    /// # Panics
    ///
    /// Panics if the registry lock is poisoned.
    pub async fn alert_unwatched(&self, approval: &ChildApproval) {
        let Some((prompts, session_id)) = self.session() else {
            return;
        };
        let text = ApprovalReminder {
            subagent: AttributeText(approval.child_name()),
            id: AttributeText(approval.id()),
            subject: UntrustedText(approval.request().subject()),
            reason: approval.request().reason().map(UntrustedText),
        }
        .render()
        .expect("subagent approval reminder template must render");
        if let Err(error) = prompts.deliver_hidden(&session_id, text).await {
            tracing::warn!(%error, "failed to deliver a subagent's approval request to its parent");
        }
    }

    /// The request a parent answers: `id` must still be pending and
    /// `decider` must be the parent the request was put to — a sibling
    /// subagent never answers for its parent. Returns `None` when the
    /// decision went through, `Some(approval)` back when `decider` is not
    /// the parent, and the entry stays pending.
    ///
    /// # Panics
    ///
    /// Panics if the registry lock is poisoned.
    #[must_use]
    pub fn decide(
        &self,
        id: &str,
        decider: &AgentIdentity,
        approval: Approval,
    ) -> Option<Approval> {
        let mut pending = self
            .shared
            .pending
            .lock()
            .expect("child approvals lock poisoned");
        let Some(entry) = pending.get(id) else {
            return Some(approval);
        };
        if entry.parent.agent() != decider {
            return Some(approval);
        }
        let entry = pending.remove(id).expect("the entry was just read");
        drop(pending);
        let _ = entry.decision.try_send(approval);
        self.shared.notify.notify(usize::MAX);
        None
    }

    /// The next request waiting on `parent`'s decision, if any.
    ///
    /// # Panics
    ///
    /// Panics if the registry lock is poisoned.
    #[must_use]
    pub fn pending_for(&self, parent: &CallIdentity) -> Option<ChildApproval> {
        let pending = self
            .shared
            .pending
            .lock()
            .expect("child approvals lock poisoned");
        pending.iter().find_map(|(id, entry)| {
            (entry.parent == *parent).then(|| ChildApproval {
                id: id.clone(),
                parent: entry.parent.clone(),
                child: entry.child.clone(),
                request: entry.request.clone(),
            })
        })
    }

    /// Resolves when a request for `parent` parks: the `subagent` call that
    /// launched the child awaits this to release the parent's turn.
    ///
    /// Awaiting a request also marks `parent` watched for its duration, so a
    /// request it answers is not additionally prompted to the session; see
    /// [`watched`](Self::watched).
    ///
    /// # Panics
    ///
    /// Panics if the registry lock is poisoned.
    pub async fn wait_for(&self, parent: &CallIdentity) -> ChildApproval {
        let _watch = self.watch(parent);
        loop {
            let listener = self.shared.notify.listen();
            if let Some(approval) = self.pending_for(parent) {
                return approval;
            }
            listener.await;
        }
    }

    /// Whether a `subagent` call is awaiting a request to `parent` right
    /// now: one that parks unwatched is prompted to the parent's session.
    ///
    /// # Panics
    ///
    /// Panics if the registry lock is poisoned.
    #[must_use]
    pub fn watched(&self, parent: &CallIdentity) -> bool {
        self.shared
            .watchers
            .lock()
            .expect("child approvals watchers lock poisoned")
            .contains(parent)
    }

    /// Marks `parent` watched until the returned guard drops.
    fn watch(&self, parent: &CallIdentity) -> Watcher<'_> {
        self.shared
            .watchers
            .lock()
            .expect("child approvals watchers lock poisoned")
            .push(parent.clone());
        Watcher {
            shared: &self.shared,
            parent: parent.clone(),
        }
    }

    /// Every request awaiting a decision, for listing.
    ///
    /// # Panics
    ///
    /// Panics if the registry lock is poisoned.
    #[must_use]
    pub fn pending(&self) -> Vec<ChildApproval> {
        self.shared
            .pending
            .lock()
            .expect("child approvals lock poisoned")
            .iter()
            .map(|(id, entry)| ChildApproval {
                id: id.clone(),
                parent: entry.parent.clone(),
                child: entry.child.clone(),
                request: entry.request.clone(),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aither_core::llm::{ApprovalKind, SubagentIdentity, ToolCallId};

    fn parent_call() -> CallIdentity {
        CallIdentity::new(ToolCallId::new("call-1"), AgentIdentity::Main)
    }

    fn child_call(parent: &CallIdentity) -> CallIdentity {
        CallIdentity::new(
            ToolCallId::new("call-2"),
            AgentIdentity::Subagent(SubagentIdentity::new("explorer", Some(parent.clone()))),
        )
    }

    fn request(call: &CallIdentity) -> ApprovalRequest {
        ApprovalRequest::new(ApprovalKind::new("unsandboxed"), "make install")
            .with_call(call.clone())
    }

    #[tokio::test]
    async fn a_request_parks_until_its_parent_decides() {
        let approvals = ChildApprovals::new();
        let parent = parent_call();
        let (approval, rx) = approvals.open(request(&child_call(&parent))).expect("open");
        let id = approval.id().to_string();

        assert_eq!(approvals.pending_for(&parent).expect("pending").id(), id);
        assert!(
            approvals
                .decide(&id, &AgentIdentity::Main, Approval::Approved)
                .is_none()
        );
        assert!(matches!(
            rx.recv().await.expect("decided"),
            Approval::Approved
        ));
        assert!(approvals.pending_for(&parent).is_none());
    }

    #[tokio::test]
    async fn a_denial_carries_the_parents_reason() {
        let approvals = ChildApprovals::new();
        let parent = parent_call();
        let (approval, rx) = approvals.open(request(&child_call(&parent))).expect("open");

        assert!(
            approvals
                .decide(
                    approval.id(),
                    &AgentIdentity::Main,
                    Approval::Denied {
                        reason: "not this one".to_string(),
                    },
                )
                .is_none()
        );
        let Approval::Denied { reason } = rx.recv().await.expect("decided") else {
            panic!("expected a denial");
        };
        assert_eq!(reason, "not this one");
    }

    #[test]
    fn a_sibling_cannot_answer_for_its_parent() {
        let approvals = ChildApprovals::new();
        let parent = parent_call();
        let (approval, _rx) = approvals.open(request(&child_call(&parent))).expect("open");
        let sibling =
            AgentIdentity::Subagent(SubagentIdentity::new("sibling", Some(parent_call())));

        let returned = approvals.decide(approval.id(), &sibling, Approval::Approved);
        assert!(matches!(returned, Some(Approval::Approved)));
        assert!(approvals.pending_for(&parent).is_some());
    }

    #[test]
    fn a_request_without_a_subagent_call_is_refused() {
        let approvals = ChildApprovals::new();
        let main = CallIdentity::new(ToolCallId::new("call"), AgentIdentity::Main);
        assert!(approvals.open(request(&main)).is_err());
    }

    #[tokio::test]
    async fn wait_for_resolves_when_a_request_parks() {
        let approvals = ChildApprovals::new();
        let parent = parent_call();
        let waiting = tokio::spawn({
            let approvals = approvals.clone();
            let parent = parent.clone();
            async move { approvals.wait_for(&parent).await.id().to_string() }
        });
        tokio::task::yield_now().await;
        assert!(approvals.watched(&parent));
        let (approval, _rx) = approvals.open(request(&child_call(&parent))).expect("open");
        assert_eq!(waiting.await.expect("joined"), approval.id());
    }
}
