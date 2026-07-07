# macOS guest assets

This directory is the production asset boundary for the persistent Linux guest
used by `qbuild` on macOS.

Expected files:

- `vmlinux`
- `init.block`
- `guest-rootfs.ext4`
- `manifest.json`

Build them with:

```bash
./packaging/macos-guest/build.sh
```

By default the script now does the full upstream prerequisite flow too:

- clones `apple/containerization` at the pinned tag
- patches the local checkout for the host SDK compatibility issue
- builds `cctl`
- cross-builds `vminitd` and `vmexec`
- creates `init.block`
- fetches a bootstrap kernel
- cross-builds `qbuild` for `aarch64-unknown-linux-gnu`
- pulls `docker.io/library/alpine:3.20`
- injects `/usr/local/bin/qbuild`
- emits a versioned asset bundle under `packaging/macos-guest/out`

By default, the guest bundle uses the fetched upstream arm64 kernel artifact.
Source-kernel rebuilds remain available, but they are opt-in because upstream's
containerized kernel build currently depends on a registry image that may
require authenticated access.

You can still override explicit artifact paths with:

```bash
KERNEL_PATH=/path/to/vmlinux \
VMINITD_PATH=/path/to/vminitd \
VMEXEC_PATH=/path/to/vmexec \
./packaging/macos-guest/build.sh
```

Useful knobs:

```bash
UPSTREAM_REF=0.1.1
WORK_DIR=/custom/cache/dir
KERNEL_MODE=fetch
BOOTSTRAP_KERNEL_MODE=fetch
KERNEL_SOURCE_URL=https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-6.14.9.tar.xz
```

For an explicit source-kernel rebuild:

```bash
KERNEL_MODE=source ./packaging/macos-guest/build.sh
```

`guest-rootfs.ext4` must contain `/usr/local/bin/qbuild` for `qbuild guestd`
execution inside the guest.

The macOS host supervisor mounts the current user's home directory into the
guest at the same absolute path and relays `/run/qbuild/guestd.sock` onto the
host as `~/.qbuild/macos/guestd.sock`.
