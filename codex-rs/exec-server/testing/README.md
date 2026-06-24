# Exec-server test support

## Linux bwrap exec-server

`bwrap_exec_server.rs` starts the production Linux exec-server inside the
outer bubblewrap environment used by `bwrap-exec` integration tests. Only
bwrap-related test actions request a fresh Firecracker VM from RBE; the rest of
the build and test graph keeps its normal execution environment.

The outer wrapper creates user, mount, IPC, and UTS namespaces, but
deliberately keeps the disposable VM's PID and network namespaces and exposes
its existing `/proc` read-write. The inherited `/proc` is required so the
production sandbox can write its child UID/GID maps; giving the outer wrapper
a fresh PID namespace or a read-only `/proc` prevents the inner sandbox from
mounting the fresh `/proc` that its PID namespace needs. The outer process runs
as a non-root user with no capabilities. Its root is read-only except for
`/proc`, `/tmp`, and the Bazel test workspace; the workspace carveout lets a
nested bwrap create otherwise-missing mount targets. The Firecracker boundary
supplies host isolation for this outer layer.
The test action requests BuildBuddy's `external` network mode because its
`off` mode leaves loopback unavailable on the current pool; the test runner
needs loopback to reach the exec-server. The production-sandbox smoke case
separately proves that restricted commands cannot reach that loopback listener.

The two smoke targets pin down both layers of the sandbox contract:

```
bazel test --config=buildbuddy-openai-rbe //bazel/rules/testing/bwrap:bwrap-test-support-smoke-test --test_output=errors
bazel test --config=buildbuddy-openai-rbe //codex-rs/exec-server/testing:bwrap-exec-server-smoke-test --test_output=errors
```

The generic smoke test covers the deliberate outer namespace topology,
filesystem, procfs, loopback, credentials, and cleanup behavior. The
exec-server smoke test sends commands through the real exec-server and verifies
that the production Codex sandbox creates its nested user, mount, PID, and
network namespaces with a fresh `/proc`, seccomp, and filesystem/network
restrictions intact.

## Windows exec-server fixture

This directory contains the small Windows exec-server binary used by
foreign-OS tests. It links only `codex-exec-server` because the full Codex
Windows graph does not yet cross-build with Bazel.
