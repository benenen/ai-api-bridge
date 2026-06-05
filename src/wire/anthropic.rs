//! Anthropic Messages wire format — not implemented in v1.

use crate::error::BridgeError;

pub fn not_implemented() -> BridgeError {
    BridgeError::NotImplemented("anthropic-messages wire format".into())
}
