// The worker prelude module. A worker script is a LONE FILE, and a lone `go
// run` file may import nothing but the standard library — so /worker copies
// this skeleton beside the script and runs it from there, which is what makes
// `caos/w` and `github.com/bitfield/script` importable from a curried script.
module caos

go 1.25.0

require github.com/bitfield/script v0.25.0

require (
	github.com/itchyny/gojq v0.12.13 // indirect
	github.com/itchyny/timefmt-go v0.1.5 // indirect
	mvdan.cc/sh/v3 v3.7.0 // indirect
)
