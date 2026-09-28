# SPEC.md — Minimal KVM-based VMM (project overview)

> This is the **project-level** spec. It stays short and changes rarely.
> Each phase has its own detailed spec in `specs/phase-N/SPEC.md`.
> Only the phase marked **ACTIVE** may be planned or implemented.

## Goal
Build a minimal, Firecracker-like virtual machine monitor (VMM) in Rust on top of Linux KVM that can boot and run a Linux guest securely.

## Phases

| Phase | Name | Outcome | Status |
|---|---|---|---|
| 1 | Boot a guest | VMM boots an unmodified Linux kernel to a BusyBox shell over an emulated serial console | **ACTIVE** — see `specs/phase-1/SPEC.md` |
| 2 | Real VMM | virtio-mmio block + net devices, multiple vCPUs, seccomp/jailer-style hardening, fuzzed/verified device parsing | Not yet specified |
| 3 | Confidential computing | Guest memory encryption (AMD SEV-SNP or Intel TDX) and attestation | Not yet specified; requires special hardware |

Phase 2 and 3 specs are written **only after** the previous phase passes its verification script. Their details depend on what earlier phases reveal.

## Global constraints (apply to every phase)
- Language: Rust, stable toolchain, edition 2021.
- Host: x86_64 Linux with `/dev/kvm` (development host: WSL2 with nested virtualization, Ubuntu 24.04).
- The VMM must run as a normal user in the `kvm` group — never require root.
- Built on the `rust-vmm` crates. Reading Firecracker, Cloud Hypervisor, crosvm, or kvmtool source for reference is allowed; copying their code or depending on them as crates is not.
- Every `unsafe` block has a `// SAFETY:` comment explaining why it is sound.
- A guest is untrusted: nothing a guest does (any port write, any MMIO access, any byte value) may panic the VMM.
- `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings` pass at every commit.

## Workflow rules
1. Spec → Plan → Implement → Verify → Iterate, one milestone at a time.
2. **Fix the spec, not the code.** If a test fails because the spec was ambiguous or wrong, update the relevant `SPEC.md` first, then re-implement.
3. No scope creep: a milestone's diff touches only what that milestone's spec requires.
4. Each phase ends with `scripts/verify-phase-N.sh` passing and a learning note in `notes/phase-N.md`.

## Repository layout
```
SPEC.md                  # this file
CLAUDE.md                # agent working rules
specs/phase-N/SPEC.md    # detailed phase spec (human-owned)
specs/phase-N/PLAN.md    # implementation plan (agent-drafted, human-approved)
notes/phase-N.md         # learning notes + interview questions per phase
src/  tests/  guests/  scripts/
```