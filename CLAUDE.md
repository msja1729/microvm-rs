# Project: minimal KVM-based VMM (Firecracker-like), Rust

## Specs
- Read SPEC.md (project overview) and specs/phase-1/SPEC.md (active phase) before any task.
- Only Phase 1 is active. Do not plan or build anything from Phase 2 or 3.
- Never modify a SPEC.md without asking me first.

## How to work with me
- Implement one milestone at a time (M1, M2, M3). Never start the next milestone without my approval.
- Keep diffs small and scoped to the current milestone. No unrelated refactors.
- Add clear comments on every unsafe block and ioctl: what it does and why.
- If a test fails because the spec is unclear, stop and propose a spec change instead of patching around it.
- When a milestone is done, give me 3 interview-style questions about it before we move on.

## Environment
- Host is WSL2 (Ubuntu 24.04) with nested virtualization; /dev/kvm is available and I am in the kvm group.
- You cannot run sudo. If a system package is needed, tell me the exact apt command and wait.

## Commands
- Build: cargo build
- Test: cargo test   (M3 tests: cargo test -- --ignored)
- Lint: cargo fmt --check && cargo clippy --all-targets -- -D warnings

## Decisions
- When a choice is within the current milestone and the spec, pick your recommended option yourself
  and continue. Don't ask me.
- Log every such choice in specs/phase-1/DECISIONS.md: the question, options, what you chose, and why.
- Still stop and ask me before: changing any SPEC.md, starting the next milestone, adding a crate
  not listed in the spec, or anything needing sudo.