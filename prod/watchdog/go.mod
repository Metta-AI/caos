// The caos watchdog: the program the watchdog-prod host runs. Its own module,
// like prod/caosd/deploy: it runs on a HOST, not in a worker, and borrows
// caos/w so its error policy is the one the rest of this tree uses.
module caos-watchdog

go 1.25.0

require caos v0.0.0

require (
	github.com/bitfield/script v0.25.0 // indirect
	github.com/itchyny/gojq v0.12.13 // indirect
	github.com/itchyny/timefmt-go v0.1.5 // indirect
	mvdan.cc/sh/v3 v3.7.0 // indirect
)

replace caos => ../../std/go/prelude
