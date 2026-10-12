//! Strongly typed identifiers. All are UUIDs; new ones are v7 (time-ordered), which keeps
//! database indexes compact. They serialise as plain UUID strings.

use std::fmt;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

macro_rules! id_type {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(pub Uuid);

        impl $name {
            /// A new, time-ordered identifier.
            pub fn new() -> Self {
                Self(Uuid::now_v7())
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }
    };
}

id_type!(
    /// A verified account.
    UserId
);
id_type!(
    /// A registered device of an account.
    DeviceId
);
id_type!(
    /// A direct or group conversation.
    ConversationId
);
id_type!(
    /// A log entry, assigned by the server.
    MessageId
);
id_type!(
    /// A log entry's idempotency key, chosen by the client before the first attempt.
    ClientMsgId
);
