//! Strongly typed identifiers. All are UUIDs; new ones are v7 (time-ordered), which keeps
//! database indexes compact. They serialise as plain UUID strings.

use std::fmt;
use std::str::FromStr;

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

        impl From<Uuid> for $name {
            fn from(uuid: Uuid) -> Self {
                Self(uuid)
            }
        }

        impl FromStr for $name {
            type Err = uuid::Error;

            /// Reads the hyphenated form that `Display` writes (other UUID forms too).
            fn from_str(text: &str) -> Result<Self, Self::Err> {
                Uuid::parse_str(text).map(Self)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_id_parses_what_it_displays_and_comes_from_a_uuid() {
        let user = UserId::new();
        assert_eq!(user.to_string().parse::<UserId>(), Ok(user));
        assert_eq!(UserId::from(user.0), user);
        assert!("not-a-uuid".parse::<ConversationId>().is_err());
    }
}
