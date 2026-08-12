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

/// The reserved ArgTree entry carrying a run's secret-cache-isolation tag
/// (design/secrets.md). Present only when the run is granted ≥1 secret.
pub const SECRET_HASH_ARG: &str = "secret-hash";

/// Canonical bytes to hash for the [`SECRET_HASH_ARG`] entry: the granted
/// secrets' `(worker-visible name, entropy)` pairs, sorted and de-duplicated,
/// each serialized `name\0entropy\n`. Whoever assembles an ArgTree hashes THIS
/// (as a git blob) and stores the resulting digest as the entry — so the key
/// depends on the entropy's DIGEST, never the entropy itself (which is a bearer
/// capability for the cache). The canonical form lives here, in the crate the
/// client and server share, because both compute it and a disagreement would
/// silently split or merge their caches.
pub fn secret_hash_material(pairs: &[(&str, &str)]) -> Vec<u8> {
    let mut pairs: Vec<(&str, &str)> = pairs.to_vec();
    pairs.sort_unstable();
    pairs.dedup();
    let mut out = Vec::new();
    for (name, entropy) in pairs {
        out.extend_from_slice(name.as_bytes());
        out.push(0);
        out.extend_from_slice(entropy.as_bytes());
        out.push(b'\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_hash_material_is_order_independent_and_deduped() {
        let a = secret_hash_material(&[("gh", "E1"), ("npm", "E2")]);
        let b = secret_hash_material(&[("npm", "E2"), ("gh", "E1"), ("gh", "E1")]);
        assert_eq!(a, b, "sorted + de-duplicated, so caller order can't matter");
        // Rotating an entropy changes the material (hence the cache namespace).
        assert_ne!(a, secret_hash_material(&[("gh", "E9"), ("npm", "E2")]));
        // The name is part of it (same secret at a different mount is distinct).
        assert_ne!(a, secret_hash_material(&[("GH", "E1"), ("npm", "E2")]));
    }
}
