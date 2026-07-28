//! Which WORLD a caos binary belongs to, baked in at compile time.
//!
//! The host build is `host`; the test stack's own binaries are built with
//! `CAOS_WORLD=test` (design/test-stack-image.md). Every client stamps this on
//! its requests and the server rejects a mismatch, so a client of one world
//! cannot drive the other's stack.
//!
//! Why this exists: that crossing is SILENT in the direction that matters. A
//! host client driving the test stack passes every test — until the tree under
//! test changes the client, at which point the suite is quietly exercising host
//! code in the one place the tested code was the entire point. (The other
//! direction, a tested client aimed at the outer server, already fails loudly.)
//!
//! Why it is compiled in and NOT read from the environment: the test stack's
//! interpreter exports its environment into `worker1` — that is how
//! `CAOS_SERVER_URL` gets flipped — so an env-carried tag would travel along
//! with it and declare the wrong binary correct. The tag has to be a property
//! of the artifact.
//!
//! Why it is COARSE (a world, not a build identity): a source hash would also
//! catch stale binaries, but it would break the dev loop, where a freshly built
//! client is deliberately run against an already-running stack. A string rather
//! than a bool leaves room to say more later without touching the protocol.

/// This binary's world. `CAOS_WORLD` at compile time, else `host`.
pub const WORLD: &str = match option_env!("CAOS_WORLD") {
    Some(world) => world,
    None => "host",
};

/// The header clients stamp their world on. Requests WITHOUT it are allowed
/// through: git smart-HTTP goes out through `git` (which the server hands to a
/// CGI delegate before this check) and health probes are plain curl.
pub const WORLD_HEADER: &str = "X-Caos-World";

/// The rejection message, so client and server describe a mismatch the same way.
pub fn mismatch(server: &str, client: &str) -> String {
    format!(
        "caos world mismatch: this server was built as `{server}`, the client as \
         `{client}`. A client must talk to its own stack — see \
         design/test-stack-image.md."
    )
}
