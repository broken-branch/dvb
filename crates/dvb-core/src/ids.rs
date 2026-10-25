//! Identifiers. Ids are ULIDs as strings, made by the engine;
//! `EventId` is the store's rowid.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

macro_rules! string_id {
    ($($(#[$meta:meta])* $name:ident;)*) => {$(
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, ToSchema)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            pub fn new(id: impl Into<String>) -> Self {
                Self(id.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }
    )*};
}

string_id! {
    /// A project.
    ProjectId;
    /// A goal the owner stated.
    GoalId;
    /// A task in a goal's plan.
    TaskId;
    /// One run of an agent CLI in a unit.
    RunId;
    /// A question put to the owner.
    QuestionId;
    /// A role record. Unique in the install; the default roles' stable names are
    /// in `RoleRecord::slug`.
    RoleId;
    /// A hire: an agent (model and effort) in a role.
    HireId;
    /// A reviewer's verdict on one head.
    VerdictId;
}

impl RunId {
    /// Whether the id has only the characters of the ULIDs the engine makes
    /// (Crockford base 32, upper case, 26 long), so it can name a run's
    /// directory and no path beyond it.
    pub fn is_ulid(&self) -> bool {
        self.0.len() == 26
            && self.0.bytes().all(|b| {
                b.is_ascii_digit()
                    || (b.is_ascii_uppercase() && !matches!(b, b'I' | b'L' | b'O' | b'U'))
            })
    }
}

/// The store's rowid of an event; also the SSE event id.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, ToSchema,
)]
#[serde(transparent)]
pub struct EventId(pub u64);

impl std::fmt::Display for EventId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::RunId;

    #[test]
    fn only_an_engine_made_ulid_is_a_run_id() {
        assert!(RunId::new("01M59VRF3PQDEY1AQYY29F3TS6").is_ulid());
        for id in [
            "",
            "../../state",
            "01M59VRF3PQDEY1AQYY29F3TS",
            "01M59VRF3PQDEY1AQYY29F3TS67",
            "01m59vrf3pqdey1aqyy29f3ts6",
            "01M59VRF3PQDEY1AQYY29F3TSU",
            "01M59VRF3PQDEY1AQYY/../TS6",
        ] {
            assert!(!RunId::new(id).is_ulid(), "{id}");
        }
    }
}
