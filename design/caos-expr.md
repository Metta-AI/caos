# `.caos-expr`: evaluable trees and DEEP-DEPS resolution (simplified)

Caos provides a standard way for trees to express the computation required build tools or otherwise work with the tree

Examples:
- Tools can be defined in any language and can describe how they should be built
- The whole tree can be restructured so that dependencies are inside the packages that depend on them, supporting argument-addressable compilation

## Evaluating trees

- `caos eval-path [--tree=oid] <path>` interprets `.caos-expr` files that are embedded in the tree from the root to the provided path and returns the result
- Each expression is evaluated in the tree returned by the parent expression. Most expressions will evaluate to a tree with a similar shape to the original. But this is not required. A valid path is one where each segment after an expression is valid in the result of that expression. This can't be determined statically
- A `.caos-expr` contains one line: a run or curry command, potentially with subcommands of the same form. For example:
  ```
    run   <image> -- [--name=value | --name:@=path | --name:commit=rev]
    curry <image> -- [--name=value | --name:@=path] 
    curry <runner-ref> -- --worker1:@=$( run <cargo-ref> -- --src:@=src )
  ```
- Arguments are parsed as with a normal curry/run-then command, except that paths are relative to the directory containing the `.caos-expr` file. `/std/...` is interpreted as normal for now (until we remove it later)
- There is no lazy evaluation here
- `eval-path` converts the expression into an arg tree and then requests that the arg tree is run, providing normal caching

Coas use `eval-path` in several places:
- To find tools to register with an agent: `eval-path caos-tools`
- When an agent requests a tool: `eval-path caos-tools/<tool>` to generate the tool
- When running an image: `eval-path <image>`

This replaces many other mechanisms:
- A simple script can curry itself with the bash worker to become a worker
- A rust program can be compiled by rustc, without any help from build-builtins
- A flake can be passed to flake-builder, without any special support for flakes in caos

## Deep deps

Most repos will have a top-level `.caos-expr` that invokes `std/deep-deps` on the tree, allowing any directory to declare deps outside its subtree

## Initial workers

Problem:
- The flake builder can't be built until there's a flake builder. Other workers like rustc could be build by the flake builder but can be built faster if we reuse build results from building the caos core
- Many workers depend on other core workers but still need to be built before the rest of std can be built normally. deep-deps is one such exampe

Solution:
- Instead, we have a seed runner that registers with the caos server for work with an arg tree that exactly matches the arg tree for each image that we seed from the core
- These seeded images can still be described in /std/*, with a `.caos-expr`. They list their image as `std/flake-builder` or whatever else would be appropriate, if everything else existed. And they list their DEPS like normal too
- However, `build-builtins.sh` builds these core images in advance, including transforming them to give them deep deps. The script will have a hard-coded sequence that it moves through to build each image after any needed deps are built
- A core-seeder-runner registers with the server to handle an arg tree that exactly matches the arg tree for each of these core images. Because it is a very specific match for each image, it runs in preference to the regular runner. (Also, much of this happens when there is no regular runner.) core-seed-runner compute the arg trees to seed by using caos to form the arg tree for each of the core images in std. This ensures that it seeds the keys that later code uses

## Removal of `std`

Later we will devise a way to expose `std` in these `DEPS` files so that a subtree may depend on any item in std that it likes. This is better than other mechanisms that we have considered:
- Passing all of std to every worker means that any change to std invalides all cache entries
- Having workers pass std members when they call other workers means that when a worker needs a new std member, it all of its transitive callers must be updated
