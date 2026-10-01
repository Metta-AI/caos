//! The model credential check `caos tui` makes before it takes the terminal:
//! conversations need a SecretReaderKey whose store grants the model key
//! (SPEC.md, "Secrets"). Whether the key is granted to this run's step is only
//! known once the server evaluates it, so that is reported per turn.

use caos::GitTransport;
use caos_cli::{ensure_conversation_secret, TurnOptions};

pub(crate) fn ensure_model_secret(
    _transport: &GitTransport,
    _turn: &TurnOptions,
) -> Result<(), String> {
    ensure_conversation_secret()
}
