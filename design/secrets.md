# Secrets: identity-is-capability — design note

**Status:** proposed. Design discussion only; not built. Builds on
`.caos-expr` (eval-path, deep-deps) and map-then (server-mediated worker
starts).

## Problem

Some tools need secrets: the github-push tool needs an auth token, and there
will be many like it. But:
- we don't want secrets in content-addressed stores, where they might leak
- we don't want secrets in keys, because we don't want to invalidate (most) keys if a secret is rotated
- we don't want secrets in one worker/arg tree to be able to be read from it by another worker

## Solution

`.caos-secrets`:
- Secrets live in a git-ignored .caos-secrets directory
- Each secret file contains the secret's value and a list of partial arg trees. This is formatted as a repeated-key file. For example:
```
# Inline secret
value=<secret key>
# External key
value:@=<file containing key>
# Reader with no args
reader=std/github-push
# Reader with args. `=`, `@=`, etc are valid here. Paths are evaluated with `caos eval-path`
reader=tools/deploy -- --repo=github.com/me/proj
```
- A reader is a partial arg set. A secret is visible to a worker if the worker's arg tree is a superset of one of the arg trees of the secret 

Worker experience:
- If a secret is visible to a worker, it is injected into a worker in `/secret/<name>`
- We attempt to scrub secret values from the logs of workers
- We attempt to check files that are added to git with `caos put` for secret values. Any new file (hash not in git) that contains the value of a secret that is visible to this worker is rejected

Correctness requirements: Neither the secret values nor the names of the secrets are part of the work's cache key. Thus:
- A worker must fail if the secret is missing or invalid
- If the secret is present and valid, the worker's result must not depend on the secret's value
- For example, a worker can fail with an invalid secret, but it should not return a list that is filtered to what is visible to this secret's account

Server behavior:
- When dispatching a work request, at the same time that the server selects a runner based on the closest match of the arg tree, the server also finds all secrets that list arg trees that are subsets of the worker's arg tree, and provides them to the runner
