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

## Usage

Build an image from a local context:

```bash
qbuild build . --image local.test/my-app:latest
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
CID=$(qbuild create local.test/my-app:latest)
sudo qbuild start "$CID"
qbuild ps
qbuild logs "$CID"
sudo qbuild stop "$CID"
qbuild rm "$CID"
```

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
