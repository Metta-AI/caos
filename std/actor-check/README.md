# Actor check — every order an actor's messages can arrive in

**Status:** built. `tests/actor-check` runs it against a lock actor and checks
its answers against an independent enumeration of every execution.

## Problem

An actor (`std/actor/README.md`) is safe under concurrency because the wrapper
serializes writes with a compare-and-swap. What it is NOT safe against is its
own messages: the order clients' messages land in, and std/actor's one fault
that changes state, a crash after the push. That fault loses the reply, so the
caller retries, and the retry applies the message AGAIN, after whatever other
clients' messages landed in between. Rule 4 says messages are idempotent and
calls that "the whole duplicate-delivery story". It is not the whole story, and
a test that sends a message twice in a row cannot show why.

## Model

A step is one message from one client, applied to the actor's state by its
inner. The inner is a pure function `(state tree, message) -> {state, reply}`,
which is exactly a transition relation, and caos already caches it on those two
inputs. A state is the actor's state tree oid plus each client's position:

- each client runs a fixed script, one message per line, sending the next only
  after hearing back. `<message> => <reply>` makes it resend until it hears that
  reply (a claim that must be granted);
- every application can end two ways: the reply arrives and the client moves
  on, or the message was applied and its reply lost, so the client will send it
  again. `--lost` bounds the losses per message (default 1). That is the
  crash-after-push row of std/actor's failure table; the other rows (a lost
  race, a failed inner) apply nothing and so add no state.

The checker explores breadth-first from the empty state, deduplicating states by
that key, and runs `--invariant` (any image; `ok` or a description of what is
wrong) on every state it reaches. A violation is reported with a shortest path
to it. A state where some client can never proceed is reported as `stuck`.

## Why it is cheap

**A transition is not run by the checker. It is std/actor's own request.**
`step.sh` forms exactly what std/actor's start does — `prepare-request
--base:@=<inner> --state:@=<state> --message:@=<message>` — and the checker
fans those out with `map-then`. So:

- a `(state, message)` pair runs ONCE however many interleavings reach it. The
  number of inner runs is the number of distinct pairs, not the number of
  executions: for the lock in `tests/actor-check`, 12 runs cover 1,701 complete
  executions;
- a re-check with a different invariant runs no transition, and a re-check of a
  bigger model runs only the pairs the smaller one never reached;
- a pair a live actor has already applied is a cache hit here, and the other way
  round, because they are one ArgTree.

The invariant is cached the same way, per state tree.

## A counterexample is a list of requests

Each step of a trace names the inner request that ran it. Re-running that hash
reproduces the step's result state — `tests/actor-check` does exactly that — so
a counterexample is something to execute, not a story about one.

## What it found

The lock half of the lease actor that `std/actor/README.md` proposes for daemons
(`examples/actor-check/lock`), with the obvious release — free the lock — is
idempotent message by message and still breaks mutual exclusion in five steps:

1. A claims the lock: granted.
2. A releases it. The release is applied, and its reply is lost.
3. B claims the now-free lock: granted.
4. A retries its release, which frees B's lock.
5. A claims again: granted. A and B are both in the critical section.

A release that frees only its sender's lock passes in every reachable state.
The general condition is stronger than idempotence: a message that may be
retried must leave unchanged every state that can follow its first
application, or carry an identity the actor can recognise.

## Use

```
LOCK=$(caos-cli curry --base:@=examples/actor-check/lock --release=any)
caos-cli run out --base:@=std/actor-check --inner:hash=$LOCK \
  --invariant:@=examples/actor-check/mutex --clients:@=examples/actor-check/clients
cat out/report
```

`--max-states` bounds the search (default 20000; past it the verdict is
`incomplete`).

## Limits

- **One actor.** Clients do not talk to each other, and a protocol across
  several actors (a transfer between two accounts) is not modelled.
- **Clients are scripts, not programs.** What a client sends next depends only
  on whether it heard the reply it waits for.
- **No time.** A lease's expiry, or a client that gives up, is not modelled.
- **Safety and getting stuck, not liveness in general.** An invariant is a
  predicate on one state.
- **The invariant runs once per state**, a container per state on a cold run;
  the inner runs once per distinct pair. State spaces of thousands are fine;
  millions are not what this is for.
