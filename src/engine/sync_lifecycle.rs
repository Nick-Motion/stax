//! Pure decisions for the destructive part of `stax sync`.
//!
//! This module deliberately contains no Git, filesystem, forge, BB, or clock
//! access. Callers must collect and revalidate the observations around it.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // Closed states are matched exhaustively and exercised by policy tests.
pub enum MergeEvidence<'a> {
    /// A fresh forge response tied to the exact source revision it merged.
    MergedPullRequest {
        head_oid: &'a str,
        /// True when newer local commits descend from the exact merged PR head.
        branch_contains_head: bool,
    },
    /// Git proved this branch tip is already an ancestor of an eligible base.
    Ancestry {
        branch_oid: &'a str,
    },
    OpenPullRequest,
    ClosedUnmergedPullRequest,
    Unknown,
    NoPullRequest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckoutKind {
    Primary,
    Linked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Owner<'a> {
    Stax,
    Unmanaged,
    BbEnvironment { id: &'a str, path: &'a str },
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationState {
    Idle,
    Locked,
    RebaseOrMerge,
    Conflicts,
    ActiveAgentWork,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetentionReason {
    PrimaryCheckout,
    NoMergeProof,
    HeadChanged,
    Locked,
    OperationInProgress,
    ActiveAgentWork,
    UnknownOperation,
    UnknownOwner,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleDecision {
    Keep(RetentionReason),
    RecoverThenRetire,
}

/// Immutable facts gathered for one exact linked checkout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Observation<'a> {
    pub checkout: CheckoutKind,
    pub owner: Owner<'a>,
    pub evidence: MergeEvidence<'a>,
    pub branch_oid: &'a str,
    pub branch_contains_merge_head: bool,
    pub operation: OperationState,
}

/// Decide whether a target may enter recovery. Recovery and a fresh equality
/// check are still required before callers may construct an effect capability.
pub fn decide(observation: Observation<'_>) -> LifecycleDecision {
    if observation.checkout == CheckoutKind::Primary {
        return LifecycleDecision::Keep(RetentionReason::PrimaryCheckout);
    }

    match observation.operation {
        OperationState::Idle => {}
        OperationState::Locked => return LifecycleDecision::Keep(RetentionReason::Locked),
        OperationState::RebaseOrMerge | OperationState::Conflicts => {
            return LifecycleDecision::Keep(RetentionReason::OperationInProgress);
        }
        OperationState::ActiveAgentWork => {
            return LifecycleDecision::Keep(RetentionReason::ActiveAgentWork);
        }
        OperationState::Unknown => {
            return LifecycleDecision::Keep(RetentionReason::UnknownOperation);
        }
    }

    if observation.owner == Owner::Unknown {
        return LifecycleDecision::Keep(RetentionReason::UnknownOwner);
    }

    let merge_proven = match observation.evidence {
        MergeEvidence::MergedPullRequest { head_oid, .. } => {
            head_oid == observation.branch_oid || observation.branch_contains_merge_head
        }
        MergeEvidence::Ancestry { branch_oid } => branch_oid == observation.branch_oid,
        MergeEvidence::OpenPullRequest
        | MergeEvidence::ClosedUnmergedPullRequest
        | MergeEvidence::Unknown
        | MergeEvidence::NoPullRequest => false,
    };
    if !merge_proven {
        let head_changed = matches!(
            observation.evidence,
            MergeEvidence::MergedPullRequest { head_oid, .. }
                | MergeEvidence::Ancestry { branch_oid: head_oid }
                if head_oid != observation.branch_oid
        );
        return LifecycleDecision::Keep(if head_changed {
            RetentionReason::HeadChanged
        } else {
            RetentionReason::NoMergeProof
        });
    }

    LifecycleDecision::RecoverThenRetire
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observation(evidence: MergeEvidence<'_>) -> Observation<'_> {
        Observation {
            checkout: CheckoutKind::Linked,
            owner: Owner::Stax,
            evidence,
            branch_oid: "head-1",
            branch_contains_merge_head: false,
            operation: OperationState::Idle,
        }
    }

    #[test]
    fn only_matching_merge_evidence_allows_recovery() {
        assert_eq!(
            decide(observation(MergeEvidence::MergedPullRequest {
                head_oid: "head-1",
                branch_contains_head: false,
            })),
            LifecycleDecision::RecoverThenRetire
        );
        assert_eq!(
            decide(observation(MergeEvidence::Ancestry {
                branch_oid: "head-1"
            })),
            LifecycleDecision::RecoverThenRetire
        );
    }

    #[test]
    fn missing_or_nonmerged_pr_evidence_never_allows_recovery() {
        for evidence in [
            MergeEvidence::OpenPullRequest,
            MergeEvidence::ClosedUnmergedPullRequest,
            MergeEvidence::Unknown,
            MergeEvidence::NoPullRequest,
        ] {
            assert!(matches!(
                decide(observation(evidence)),
                LifecycleDecision::Keep(RetentionReason::NoMergeProof)
            ));
        }
    }

    #[test]
    fn changed_head_and_unsafe_checkout_states_are_retained() {
        assert_eq!(
            decide(observation(MergeEvidence::MergedPullRequest {
                head_oid: "head-old",
                branch_contains_head: false,
            })),
            LifecycleDecision::Keep(RetentionReason::HeadChanged)
        );
        let mut target = observation(MergeEvidence::Ancestry {
            branch_oid: "head-1",
        });
        target.checkout = CheckoutKind::Primary;
        assert_eq!(
            decide(target),
            LifecycleDecision::Keep(RetentionReason::PrimaryCheckout)
        );
        target.checkout = CheckoutKind::Linked;
        target.operation = OperationState::Locked;
        assert_eq!(
            decide(target),
            LifecycleDecision::Keep(RetentionReason::Locked)
        );
        target.operation = OperationState::Idle;
        target.owner = Owner::Unknown;
        assert_eq!(
            decide(target),
            LifecycleDecision::Keep(RetentionReason::UnknownOwner)
        );
    }
}
