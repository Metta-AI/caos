# Actor check — every order an actor's messages can arrive in

**Status:** built. `tests/actor-check` runs it against a lock actor, checks its
answers against an independent enumeration of every execution, and delivers the
counterexample it finds to a real `std/actor` branch.

## Problem

An actor (`std/actor/README.md`) serializes writes to its branch with a
compare-and-swap. What that does not protect it from is its own messages: the
order clients' messages land in, and std/actor's one fault that changes state, a
crash after the push. That fault loses the reply, so the caller retries, and the
retry applies the message AGAIN, after whatever other clients' messages landed
in between. Rule 4 says messages are idempotent and calls that "the whole
duplicate-delivery story". It is not the whole story, and a test that sends a
message twice in a row cannot show why.

## Model

A step is one message from one client, applied to the actor's state by its
inner. The inner is a pure function `(state tree, message) -> {state, reply}`,
which is exactly a transition relation, and caos already caches it on those two
inputs. A state of the search is the actor's state tree oid plus, for each
client, its position, the replies it has heard, and how many times the message
it is sending has been applied without a reply reaching it:

- each client runs a fixed script, one message per line, sending the next only
  after hearing back. `<message> => <reply>` makes it resend until it hears that
  reply (a claim that must be granted);
- an application that changes the state can end two ways: the reply arrives and
  the client moves on, or its reply is lost and the client will send the same
  message again. That is the crash-after-push row of std/actor's failure table.
  An application that changes nothing loses no reply, because std/actor pushes
  nothing for it and its lost reply is the same as the request not having run;
  nor do the other rows (a lost race, a failed inner), which apply nothing.
  `--lost` bounds the losses per message (default 1).

The checker explores breadth-first from the empty state, deduplicating states by
that key, and runs `--invariant` (any image; `ok` or a description of what is
wrong) on every state it reaches. A violation is reported with a shortest path
to it. Once every state has been judged it reports, as `stuck`, the shallowest
state from which the clients can no longer all finish: nobody can move, or they
can only go round a cycle.

**The loss bound is reported, not assumed away.** If the search never cut a
lost reply off at the bound, a larger bound adds no transition, and the report
says the verdict holds for any number of lost replies; otherwise it says which
bound it holds for.

## Why it is cheap

**A transition is not run by the checker. It is std/actor's own request.**
`step.sh` forms exactly what std/actor's start does — `prepare-request
--base:@=<inner> --state:@=<state> --message:@=<message>` — and the checker
fans those out with `map-then`. So:

- a `(state, message)` pair runs ONCE however many executions reach it: for the
  lock in `tests/actor-check`, 12 distinct pairs cover 792 complete executions;
- a re-check with a different invariant runs no transition, and a re-check of a
  bigger model runs only the pairs the smaller one never reached;
- a pair a live actor has already applied is a cache hit here, and the other way
  round, because they are one ArgTree. That needs the same inner object, the
  same salt, and the same message bytes: the checker sends each line with a
  trailing newline, as `tests/actor` does, which a literal `--message=` on the
  command line does not.

The invariant is cached the same way, once per search state.

## A counterexample is a list of requests

Each step of a trace names the inner request that ran it. Re-running that hash
reproduces the step's result state — `tests/actor-check` does exactly that, then
sends the whole trace through the real wrapper — so a counterexample is
something to execute, not a story about one.

## What it found

The claim/release half of the kind of lease actor `std/actor/README.md`
proposes for daemons (`examples/actor-check/lock`), written the obvious way —
release frees the lock — is idempotent message by message and still breaks
mutual exclusion in five steps:

1. A claims the lock: granted.
2. A releases it. The release is applied, and its reply is lost.
3. B claims the now-free lock: granted.
4. A retries its release, which frees B's lock.
5. A claims again: granted. A and B are both in the critical section.

A release that frees only its sender's lock passes in all 84 reachable states,
for any number of lost replies. `std/actor/README.md` ("Idempotent is necessary
and not sufficient") states the condition a retried message actually has to
meet.

## Use

```
caos-cli run out --base:@=examples/actor-check/release-any && cat out/report
```

`examples/actor-check/release-any` and `release-holder` are the checker curried
with a lock, the `mutex` invariant and the clients; swap a part with
`caos-cli curry --unbind=<arg>`. `--max-states` bounds the search (default
20000; past it the verdict is `incomplete`).

## Limits

- **One actor.** Clients do not talk to each other, and a protocol across
  several actors (a transfer between two accounts) is not modelled.
- **Clients are scripts.** What a client sends next depends only on whether it
  heard the reply it waits for, and a retry re-sends only the message it is
  waiting on. A caller that re-runs a whole sequence after a failure, with fresh
  nonces, re-sends messages already acknowledged; that is not modelled.
- **Messages and replies are one line of text.** A tree or multi-line reply
  fails the check, saying so.
- **No time.** A lease's expiry, or a client that gives up, is not modelled.
- **Safety and getting stuck, not liveness in general.** An invariant is a
  predicate on one state.
- **An inner that fails** on a reachable state fails the whole check, with the
  inner's error.
- **The invariant runs once per state**, a container per state on a cold run;
  the inner runs once per distinct pair. State spaces of thousands are fine;
  millions are not what this is for.
