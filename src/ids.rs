use std::fmt;
use uuid::Uuid;

macro_rules! id_type {
    ($name:ident) => {
        #[derive(Clone, Copy, Debug, Hash, Eq, PartialEq, Ord, PartialOrd)]
        pub struct $name(Uuid);

        impl $name {
            pub(crate) fn new() -> Self {
                Self(Uuid::new_v4())
            }

            /// Returns the underlying UUID.
            pub fn as_uuid(self) -> Uuid {
                self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }
    };
}

id_type!(TaskSpecId);
id_type!(ExecutionId);
id_type!(LaneId);
id_type!(ScopeId);

/// Identity of one 1-based attempt within an execution.
#[derive(Clone, Copy, Debug, Hash, Eq, PartialEq)]
pub struct AttemptId {
    pub execution_id: ExecutionId,
    pub number: u32,
}
