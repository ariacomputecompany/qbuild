# qbuild

`qbuild` is a standalone OCI image builder for Quilt-compatible artifacts.

It does not call the Quilt backend. It builds and pulls images directly into a local OCI content store, using code extracted from Quilt's existing OCI/image/builder stack and then cut free from the sync engine and HTTP API.

## What it does

- Builds OCI images from a local Docker build context
- Pulls OCI images from standard registries
- Pushes locally stored OCI images to standard registries
- Runs locally stored OCI images standalone from the qbuild store
- Stores blobs, manifests, configs, and reference metadata locally
- Inspects and lists locally stored image references

## What it does not do

- It is not a Docker daemon replacement
- It does not require or use the Quilt backend

## Architecture

`qbuild` was carved out of `quilt-prod` by copying the existing implementation for:

- `src/build`
- `src/image`
- `src/registry`

Then replacing backend coupling with a local filesystem-backed contract:

- no HTTP API
- no tenant context
- no SQLite sync engine
- no server-side operation manager

That keeps the builder lineage direct and gives future work one place to extend rather than splitting the logic into another parallel implementation.

## Local store layout

By default `qbuild` stores data under:

```text
~/.qbuild/images
~/.qbuild/builds
```

You can override both with `--store-dir` and `--work-dir`.

## Install

```bash
cargo build --release
./target/release/qbuild --help
```

## Usage

Build from a local context:

```bash
qbuild build . --image local.test/my-app:latest
```

Run a locally stored image:

```bash
sudo qbuild run local.test/my-app:latest
```

Build with explicit paths:

```bash
qbuild build ./app \
  --dockerfile Dockerfile \
  --image ghcr.io/acme/my-app:dev \
  --store-dir /tmp/qbuild-store \
  --work-dir /tmp/qbuild-work
```

Pull a base image into the local store:

```bash
qbuild pull docker.io/library/alpine:3.20
```

Push a locally built image to a registry:

```bash
qbuild push ghcr.io/acme/my-app:dev
```

Inspect a locally stored reference:

```bash
qbuild inspect docker.io/library/alpine:3.20
```

List local references:

```bash
qbuild list
```

## Privilege model

`COPY`/`ADD`-only builds can run unprivileged.

`RUN` steps use the same chroot-and-mount execution model as Quilt's current builder. In practice that means:

- `/proc` mount setup may require elevated privileges
- device node setup for `/dev/null`, `/dev/zero`, and related paths may require elevated privileges

`qbuild` now performs an explicit preflight for Dockerfiles that contain `RUN` and fails fast if the current worker cannot satisfy that execution model.

If you want rootless `RUN` support, that should be added here in `qbuild` rather than reimplemented elsewhere.

Standalone container `run` currently also requires root privileges. It uses the same low-level rootfs and namespace model rather than shelling out to Docker or depending on the Quilt backend.

## Current status

Verified locally:

- `cargo check`
- `cargo clippy --all-targets --all-features -- -D warnings`
- `cargo test`
- end-to-end standalone build of a `FROM scratch` image with `COPY`
- local inspect of the resulting OCI reference
- end-to-end local registry loop: build, push, pull, inspect
- end-to-end standalone build and run of an OCI image from the qbuild store

## License

MIT OR Apache-2.0
