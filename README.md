# qbuild

`qbuild` builds and runs Quilt images as a standalone OCI container tool.

It is bi-directionally compatible with Docker and the broader OCI ecosystem:

- build images locally from Docker build contexts
- pull standard OCI and Docker images from registries
- push built images back to standard registries
- run compatible images locally from the `qbuild` store

That means `qbuild` works for Quilt image workflows and standard Docker-compatible image workflows without requiring a backend service.

## What it does

- Builds OCI images from local Docker build contexts
- Pulls images from standard OCI and Docker registries
- Pushes images to standard OCI and Docker registries
- Runs locally stored images
- Creates and manages persistent local containers
- Stores image content, metadata, and runtime state locally
- Injects trusted local GPU devices through first-class runtime flags
- Inspects and lists local images and containers

## What it is not

- It is not a Docker daemon replacement
- It is not a hosted build service
- It does not require a separate backend to build or run images

## Compatibility

`qbuild` is designed around standard OCI image formats and registry workflows.

In practice that means:

- standard Docker and OCI base images can be pulled into `qbuild`
- images built with `qbuild` can be pushed to standard registries
- Docker-style build contexts and Dockerfiles are supported
- Quilt image workflows and Docker-compatible image workflows can share the same image distribution model

## Local data layout

By default `qbuild` stores data under:

```text
~/.qbuild/images
~/.qbuild/builds
~/.qbuild/containers
```

You can override paths with flags such as `--store-dir`, `--work-dir`, and `--data-root`.

## Install

```bash
cargo build --release
./target/release/qbuild --help
```

On Linux, the resulting binary can execute builds and containers directly.

On macOS, the binary runs in host-control mode. It starts or reconnects to a
persistent Linux guest managed by a local supervisor, then forwards the normal
`qbuild` commands to the guest daemon instead of trying to reproduce the Linux
runtime on Darwin.

## Usage

Build an image from a local context:

```bash
qbuild build . --image local.test/my-app:latest
```

Run the Linux guest daemon directly on Linux:

```bash
qbuild guestd --listen 127.0.0.1:42141
```

Run the Linux guest daemon on a Unix socket inside a guest:

```bash
qbuild guestd --listen-unix /run/qbuild/guestd.sock
```

Pull a standard base image:

```bash
qbuild pull docker.io/library/alpine:3.20
```

Push a built image to a registry:

```bash
qbuild push ghcr.io/acme/my-app:dev
```

Inspect a local image:

```bash
qbuild inspect local.test/my-app:latest
```

List local images:

```bash
qbuild list
```

Run a local image:

```bash
sudo qbuild run local.test/my-app:latest
```

Create and manage a persistent local container:

```bash
CID=$(qbuild create local.test/my-app:latest --name my-app)
sudo qbuild start "$CID"
qbuild ps
qbuild logs "$CID"
sudo qbuild stop "$CID"
qbuild rm "$CID"
```

Bind host state into a container:

```bash
qbuild create local.test/my-app:latest --name my-app -v /host/workspace:/workspace:rw
qbuild create local.test/my-app:latest --name my-app -v /host/cache:/cache:ro
```

Request local GPU passthrough:

```bash
sudo qbuild run local.test/gpu-app:latest --gpu-count 1
qbuild create local.test/gpu-app:latest --name gpu-app --gpu-count 1
qbuild create local.test/gpu-app:latest --name gpu-app --gpu-count 1 --gpu-id amd0
qbuild create local.test/gpu-app:latest --name gpu-app --gpu-count 1 --gpu-id nvidia0
```

GPU access is intentionally not modeled as user-supplied raw `/dev/*` mounts.
`qbuild` validates the `--gpu-count`/`--gpu-id` request, discovers the local
NVIDIA or AMD/ROCm device surface, and injects only the trusted device mounts
and runtime visibility environment needed by the selected vendor. NVIDIA hosts
receive CUDA/NVIDIA visibility env and `/dev/nvidia*` control/device mounts;
AMD hosts receive ROCm visibility env and either `/dev/kfd` + `/dev/dri` or the
WSL `/dev/dxg` runtime surface.

Build with explicit paths:

```bash
qbuild build ./app \
  --dockerfile Dockerfile \
  --image ghcr.io/acme/my-app:dev \
  --store-dir /tmp/qbuild-store \
  --work-dir /tmp/qbuild-work
```

## Privilege model

`COPY` and `ADD` only builds can run unprivileged.

Builds that execute `RUN` steps currently rely on low-level Linux container primitives. `qbuild` performs a preflight check and fails fast when the current environment cannot support that execution model.

Local image `run`, and persistent container `start` and `stop`, currently require root privileges in the current runtime model.

On macOS, those privileged operations still execute inside the Linux guest. The
macOS host process only forwards the request and renders progress/output.

## Platform Modes

`qbuild` now has three explicit execution modes:

- Linux local mode: the CLI executes the existing build/runtime engine in
  process.
- Linux guest daemon mode: `qbuild guestd` exposes a typed RPC surface over a
  long-lived TCP or Unix control channel.
- macOS host mode: the CLI ensures a local supervisor-managed Linux guest,
  waits for the relayed Unix socket to become healthy, and forwards the regular
  command set.

The guest RPC surface covers:

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

## Verification

Verified locally:

- `cargo check`
- `cargo clippy --all-targets --all-features -- -D warnings`
- `cargo test`
- end-to-end image build
- end-to-end registry push and pull
- end-to-end local image run
- end-to-end persistent container lifecycle

## License

MIT OR Apache-2.0
