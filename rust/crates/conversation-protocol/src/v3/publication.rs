//! Agent publication outcomes and reconciliation.
use super::{Evidence, Oid, PublicationRecord, PublicationStatus};

pub struct Outcome {
    pub status: PublicationStatus,
    pub evidence: Evidence,
    pub observed: Option<Oid>,
}

impl Outcome {
    pub fn new(
        status: PublicationStatus,
        kind: &str,
        diagnostic: Option<String>,
        observed: Option<Oid>,
    ) -> Self {
        Self {
            status,
            evidence: Evidence {
                kind: kind.to_string(),
                diagnostic,
            },
            observed,
        }
    }

    pub fn from_observation(
        pending: &PublicationRecord,
        observed: Option<Oid>,
        diagnostic: String,
        lease_rejected: bool,
    ) -> Self {
        let (status, kind) = if observed.as_ref() == Some(&pending.planned_head) {
            (PublicationStatus::Complete, "ref-converged")
        } else if observed == pending.expected_old {
            (PublicationStatus::Uncertain, "ambiguous")
        } else {
            (
                PublicationStatus::Conflict,
                if lease_rejected {
                    "lease-rejected"
                } else {
                    "ref-drift"
                },
            )
        };
        Self::new(status, kind, Some(diagnostic), observed)
    }
}
