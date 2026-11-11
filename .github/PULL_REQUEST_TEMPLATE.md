## What and why

<!-- One or two sentences. Link the issue if there is one. -->

## Checks

- [ ] `make gate GATE=A` passes
- [ ] Conventional Commit title (`feat:`, `fix:`, `deps:` ...)
- [ ] If this touches a delete path or a path guard: adversarial tests extended, `make safety` passes
- [ ] If this changes output: golden files updated and the change is described here
