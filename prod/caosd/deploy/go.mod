// The deploy driver for a caosd-prod host. Its own module, not a worker
// script: it runs on the HOST, before any caos stack exists. It borrows
// caos/w from the worker prelude for one reason — the error policy should be
// the same one the rest of this tree already uses.
module caos-deploy

go 1.25.0

require (
	caos v0.0.0
	github.com/bitfield/script v0.25.0
)

require (
	github.com/itchyny/gojq v0.12.13 // indirect
	github.com/itchyny/timefmt-go v0.1.5 // indirect
	mvdan.cc/sh/v3 v3.7.0 // indirect
)

replace caos => ../../../std/go/prelude
