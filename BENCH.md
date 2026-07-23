# qbuild Benchmarks

## Docker vs qbuild Local Redeploy Loop

Date: 2026-07-23

Host: GMK Strix Halo WSL Ubuntu 24.04

Workload: 40 edit-build-create-start-health-stop cycles per engine using the same BusyBox HTTP image. Each iteration changed one payload file, rebuilt the image, deployed a fresh service container, waited for `GET /payload.txt`, then tore the container down. qbuild cycles also exercised a writable bind mount.

Artifacts:

- Raw results: `/tmp/qbuild-docker-bench-final/results.jsonl`
- Summary: `/tmp/qbuild-docker-bench-final/summary.txt`

### Current Results

| Metric | Docker mean | qbuild mean | qbuild delta |
| --- | ---: | ---: | ---: |
| Full edit-build-deploy cycle | 1830.9 ms | 855.1 ms | decreased by 53.3% |
| Build | 842.4 ms | 687.1 ms | decreased by 18.4% |
| Create | 144.6 ms | 19.7 ms | decreased by 86.4% |
| Start | 336.6 ms | 18.9 ms | decreased by 94.4% |
| Health readiness | 13.2 ms | 64.9 ms | increased by 391.7% |
| Stop/remove | 494.1 ms | 64.7 ms | decreased by 86.9% |

Median full cycle:

- Docker: 1822.0 ms
- qbuild: 656.5 ms
- qbuild median cycle decreased by 64.0%

Success rate:

- Docker: 40/40
- qbuild: 40/40

Final qbuild footprint after the 40-run loop:

- Store: 8.0M
- Work dir: 4.0K
- Data root: 8.0K
- Leftover build rootfs containers: 0

### Optimization Delta

The original benchmark showed qbuild losing the full cycle because the build path was rebuilding a tiny `COPY` as if it needed a full rootfs snapshot/diff pass:

| Metric | Previous qbuild mean | Optimized qbuild mean | Delta |
| --- | ---: | ---: | ---: |
| Full edit-build-deploy cycle | 2074.6 ms | 855.1 ms | decreased by 58.8% |
| Build | 1776.0 ms | 687.1 ms | decreased by 61.3% |
| Create | 34.6 ms | 19.7 ms | decreased by 43.1% |
| Start | 38.2 ms | 18.9 ms | decreased by 50.5% |
| Stop/remove | 122.2 ms | 64.7 ms | decreased by 47.1% |

The controlled probe before the fix expanded a tiny BusyBox build store to roughly 799M after two builds. The optimized path keeps the final benchmark store at 8.0M after 40 builds.

### Interpretation

qbuild is now faster than Docker for the local edit-build-deploy loop measured here. The main fixes were architectural rather than benchmark-specific:

- `COPY` now emits the layer directly from the paths it mutates instead of snapshotting and hashing the entire rootfs before and after the copy.
- Build rootfs preparation now cleans up transient container rootfs state after stage materialization.
- Rootfs copying now preserves hardlinks, preventing BusyBox-style hardlinked applets from exploding into hundreds of megabytes of duplicated files.
- Persistent container `stop` now waits for the process to exit, so immediate `rm` after `stop` is race-free.

Docker still has faster HTTP readiness in this toy workload because qbuild uses host-network process startup without Docker's port-mapping readiness behavior, but that cost is small relative to qbuild's lower create/start/stop overhead.

### Recommendation

Use qbuild for AFW-GPU local sandbox redeploy loops when Quilt-native execution is desired. Docker remains a good compatibility fallback, but qbuild no longer gives up performance on the measured edit-build-deploy path.
