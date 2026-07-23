# Clean code

- Write simple, clean code
- Do not write defensive code just because you aren't sure about how things work
- Die when there's an unexpected error
- Don't truncate/tail logs. Leave the full thing. We can shorten later, closer to usage if we like

# Before committing

- Build and test with `caos-cli run-tool test`
- If this doesn't catch everything, we need to add it to the above step
