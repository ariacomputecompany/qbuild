# qbuild on macOS

This document describes the recommended macOS architecture for `qbuild` when performance and Quilt-runtime fidelity are the priority.

The goal is not "make the current Linux runtime work natively on Darwin". The goal is to keep the real Quilt build and run logic on Linux, and use the smallest possible macOS-specific layer to host and control it.

## Decision

Use a persistent Linux guest on macOS and run the real Quilt builder/runtime inside that guest.

Do not use Apple `container build` as the main build engine.

Do not use Apple per-container VM runtime as the main Quilt runtime.

Do not attempt a native Darwin port of the current Linux runtime primitives.

Use Apple `containerization` only as the substrate for:

- booting a persistent Linux VM
- mounting host directories into the guest
- providing a low-overhead control channel
- optionally enabling Rosetta when `linux/amd64` execution is explicitly needed

Everything performance-critical stays in Linux.

## Current Implementation

The codebase now enforces an explicit platform split:

- `src/platform/linux_local.rs`
- `src/platform/macos_host.rs`
- `src/guestd.rs`
- `src/services.rs`
- `src/protocol.rs`

That split creates three real modes:

1. Linux local CLI mode
2. Linux guest daemon mode via `qbuild guestd`
3. macOS host-control mode that forwards commands to the guest daemon

The host/guest request surface is typed and high-level. On macOS, the host now
targets a supervisor-managed Unix socket relay backed by the persistent guest.

## Why

The current `qbuild` implementation is Linux-native:

- `chroot`
- `/proc` mount handling
- `mknod`
- Linux namespaces
- overlayfs assumptions
- cgroups

Those are not "portability gaps". They are core execution model choices. Replacing them with Darwin-native equivalents would create a second runtime with different semantics and higher long-term risk.

The lowest-overhead path is therefore:

1. Keep the authoritative build/runtime engine Linux-native.
2. Keep the guest warm and persistent.
3. Minimize host/guest crossings.
4. Use the macOS side only as a shim.

## Non-Goals

This design does not try to:

- make Quilt containers run directly on the Darwin kernel
- replace the Linux runtime with Apple container semantics
- adopt BuildKit semantics as the source of truth for Quilt builds
- keep parity with Docker Desktop or Apple `container` feature-for-feature

## Recommended Architecture

There are two layers.

### macOS host layer

This should stay minimal.

Responsibilities:

- create and manage one persistent Linux VM
- mount the host repository into the guest
- provide a control RPC path to the guest
- forward credentials or registry auth when needed
- expose the normal `qbuild` CLI UX to the user

This layer should not:

- execute Dockerfile steps
- own the OCI layer store
- own the build cache
- own container lifecycle state
- interpret Quilt runtime semantics

### Linux guest layer

This is the real `qbuild` execution environment.

Responsibilities:

- Dockerfile execution
- layer extraction and merging
- overlayfs handling
- rootfs preparation
- `RUN` execution
- namespace setup
- cgroup application
- container lifecycle
- local OCI content store
- local build cache
- local image cache

This layer should be as close as possible to the Linux implementation already used by `qbuild`.

## Process Model

Recommended process model:

1. `qbuild` on macOS starts or connects to a persistent Linux VM.
2. A Quilt guest daemon runs inside that VM.
3. The macOS `qbuild` CLI sends high-level requests to that daemon.
4. The guest daemon performs all build and run work locally inside Linux.
5. Results are returned as structured status, logs, and OCI references.

The guest daemon should be the execution authority.

The host CLI should be a control client.

## Implemented RPC Surface

The daemon currently accepts high-level command requests for:

- `build`
- `pull`
- `push`
- `inspect`
- `list`
- `run`
- `create`
- `start`
- `stop`
- `rm`
- `ps`
- `logs`

Responses are explicit and typed, and progress/status messages are streamed back
as daemon events during long-running operations like `build`, `pull`, and
`push`.

## Current Host Contract

The macOS host now targets a fixed local supervisor state directory under
`~/.qbuild/macos`.

For each CLI invocation, the macOS host:

1. Resolves the bundled macOS supervisor binary and guest assets
2. Ensures the supervisor daemon is running
3. Waits for the relayed host Unix socket for guestd to become healthy
4. Forwards the requested high-level command
5. Prints streamed progress and the final command result

The supervisor is responsible for maintaining the persistent Linux guest and the
host-facing Unix socket relay.

## Why Not Apple `container build`

Apple's current build stack is useful, but it is not the right performance or semantics target for Quilt.

Apple's build path uses:

- a builder VM
- BuildKit
- `container-builder-shim`
- host-to-builder file sync

That means:

- extra protocol translation
- extra file transfer layers
- BuildKit-driven semantics instead of Quilt-driven semantics

That is the wrong direction if the priority is preserving Quilt's runtime behavior and hot-path performance.

## Why Not Apple Per-Container Runtime

Apple's `container` runtime model is built around lightweight VMs per container.

That is attractive for security and product integration, but it is not the lowest-overhead way to host Quilt.

For Quilt, the better shape is:

- one persistent Linux guest
- many guest-local build and runtime operations

This avoids repeated VM startup and reduces per-container host orchestration overhead.

## Lowest-Overhead macOS Strategy

The core rule is:

Keep host/guest crossings coarse-grained.

Good:

- "build this context"
- "run this image"
- "stream logs"
- "list containers"

Bad:

- host-mediated file operations for every build step
- host-mediated layer assembly
- host-mediated runtime namespace operations
- host-mediated container lifecycle transitions

The more often the host crosses into the guest for fine-grained operations, the more performance is lost.

## Storage Layout

Recommended split:

### Guest-local state

Store these inside the Linux guest on guest-local disk:

- OCI blobs
- manifests
- configs
- build cache
- prepared layers
- container state
- runtime metadata

This should live on guest-local ext4 storage, not on a host-shared mount.

Reason:

- heavy metadata traffic is faster guest-local
- layer extraction is faster guest-local
- overlayfs expectations match Linux directly
- avoids turning the host-shared filesystem into the bottleneck

### Host-shared state

Share only the user working tree from macOS into the guest.

Examples:

- source repo
- developer config files that must be visible to builds

Avoid placing the main OCI store or runtime root on the shared mount.

## Filesystem Strategy

There are two practical modes for source access.

### Mode A: Build directly from shared mount

Pros:

- simplest
- immediate edit/build loop
- no explicit sync step

Cons:

- shared filesystem performance may become the bottleneck
- metadata-heavy builds may slow down

Use this by default for:

- normal application builds
- small to medium contexts
- fast iteration

### Mode B: Stage source into guest-local workspace before build

Pros:

- best performance for large builds
- best behavior for metadata-heavy toolchains
- build runs fully on guest-local storage

Cons:

- requires a staging step
- slightly more complexity

Use this for:

- large monorepos
- heavy C/C++/Rust/Go builds
- builds with many small files
- performance-sensitive CI-like local runs

Recommended policy:

- support both modes
- default to direct shared-mount builds
- allow an explicit or automatic switch to guest-local staging for large/heavy builds

## Control Channel

Preferred options:

1. vsock
2. unix socket bridge
3. loopback TCP inside a tightly controlled local-only path

Preferred choice: vsock.

Reason:

- designed for host/guest communication
- avoids unnecessary host networking exposure
- low coordination overhead

The protocol should be thin and high-level.

Examples:

- `BuildRequest`
- `RunRequest`
- `CreateContainerRequest`
- `StartContainerRequest`
- `StopContainerRequest`
- `LogsRequest`

Avoid streaming raw build protocol translations unless necessary.

## Guest Daemon

The guest should run a small long-lived Quilt daemon responsible for:

- serving RPC requests from the macOS host
- owning the OCI store
- owning container metadata
- executing builds
- executing runtime actions
- maintaining warm caches

This daemon is the right place to evolve the standalone `qbuild` lifecycle model.

It should not depend on the Quilt backend, sync engine, or tenant model.

It should stay single-node and local-first.

## Build Path

Recommended build flow on macOS:

1. Host CLI resolves the local project path.
2. Host CLI ensures the Linux guest is running.
3. Host CLI sends a high-level build request to the guest daemon.
4. Guest daemon reads source from the shared path or from a staged guest-local workspace.
5. Guest daemon executes the existing Quilt builder logic.
6. Guest daemon stores the image in the guest-local OCI store.
7. Guest daemon returns the resulting image reference, manifest digest, config digest, and logs.

This keeps build execution entirely Linux-local after the initial request.

## Run Path

Recommended run flow on macOS:

1. Host CLI sends a run or create/start request to the guest daemon.
2. Guest daemon resolves the image from the guest-local OCI store.
3. Guest daemon prepares the rootfs and applies Linux runtime logic.
4. Guest daemon starts the container using the Quilt runtime path.
5. Guest daemon returns structured state and optional log stream handles.

Again, the host should not manage the runtime details.

## Caching

To preserve performance, caches must stay in the guest.

Important caches:

- pulled base layers
- built layers
- extracted layer cache
- prepared rootfs cache if introduced later
- registry auth/session state if safe to persist locally

Host-side caching should be minimal and non-authoritative.

The guest cache is the source of truth for local macOS builds.

## Performance Priorities

In order of importance:

1. Avoid cold-starting a VM for each build or run.
2. Keep the OCI store and build cache guest-local.
3. Minimize host/guest round trips during build execution.
4. Avoid replacing Quilt build semantics with BuildKit semantics.
5. Use shared mounts only for source input, not for heavy runtime state.

If those rules are followed, most performance-critical work remains Linux-native.

## Architecture-Specific Notes

### Preferred local architecture

Prefer `linux/arm64` local builds on Apple silicon.

Reason:

- native execution in the guest
- avoids translation overhead
- best local performance

### `linux/amd64`

`linux/amd64` local execution on Apple silicon will require translation or emulation.

That overhead is unavoidable.

Recommended policy:

- use `linux/arm64` by default for local development
- support `linux/amd64` for explicit compatibility testing
- keep heavy multi-arch production builds in CI or dedicated Linux infrastructure when possible

## Why `containerization` Is Still Useful

Even though we do not want to use Apple's higher-level build/runtime stack as the Quilt engine, `containerization` is still valuable because it gives us:

- a supported Apple-silicon Linux VM substrate
- shared-directory integration
- host/guest lifecycle management
- optional Rosetta integration for non-native Linux architecture execution

This is exactly the right level of reuse: enough Mac integration to make the platform work, but not enough to replace the Quilt runtime.

## Recommended Implementation Sequence

1. Introduce a platform abstraction in `qbuild`.
2. Keep the existing Linux engine as the source of truth.
3. Add a macOS host client mode.
4. Add a Linux guest daemon mode.
5. Implement persistent guest boot and health checking.
6. Implement high-level RPC over vsock.
7. Mount the working tree into the guest.
8. Keep guest-local OCI/cache/runtime state on guest disk.
9. Add optional guest-local source staging for heavy builds.

Do not start by integrating BuildKit.

Do not start by trying to rewrite Linux runtime operations into native macOS behavior.

## Source Notes

The recommendations above are based on:

- the current Linux-native `qbuild` implementation
- Apple `container` documentation showing:
  - per-container lightweight VMs
  - a separate builder VM
  - persistent `container machine`
- Apple `container-builder-shim` documentation showing a BuildKit translation layer
- Apple `containerization` sources showing VM, OCI, ext4, and virtiofs-oriented primitives

Relevant upstream repos:

- `https://github.com/apple/container`
- `https://github.com/apple/containerization`
- `https://github.com/apple/container-builder-shim`
