# qbuild Benchmarks

## Docker vs qbuild Local Redeploy Loop

Date: 2026-07-23

Host: GMK Strix Halo WSL Ubuntu 24.04

Workload: 30 edit-build-create-start-health-stop cycles per engine using the same BusyBox HTTP image. Each iteration changed one payload file, rebuilt the image, deployed a fresh service container, waited for `GET /payload.txt`, then tore the container down. qbuild cycles also exercised a bind mount by writing a startup marker into mounted host state.

Artifacts:

- Raw results: `/tmp/qbuild-docker-bench/results.jsonl`
- Summary: `/tmp/qbuild-docker-bench/summary.txt`

### Results

| Metric | Docker mean | qbuild mean | qbuild delta |
| --- | ---: | ---: | ---: |
| Full edit-build-deploy cycle | 1847.8 ms | 2074.6 ms | increased by 12.3% |
| Build | 944.2 ms | 1776.0 ms | increased by 88.1% |
| Create | 126.1 ms | 34.6 ms | decreased by 72.6% |
| Start | 320.2 ms | 38.2 ms | decreased by 88.1% |
| Health readiness | 20.4 ms | 81.6 ms | increased by 300.0% |
| Stop/remove | 451.0 ms | 122.2 ms | decreased by 72.9% |

Success rate:

- Docker: 30/30
- qbuild: 30/30

### Interpretation

Docker currently wins the full edit-build-deploy loop because its incremental build path is materially faster on the changed `COPY` layer. qbuild's container lifecycle is substantially faster after an image exists: create, start, and stop/remove are all far lower overhead than Docker in this workload.

For AFW-GPU specifically, Docker is the safer default today for repeated source edit redeploys. qbuild is promising for hot service lifecycle and persistent local runtime use, but the next meaningful optimization target is qbuild's incremental build/cache path.

### Follow-Up

- Add first-class incremental build cache reuse for unchanged Dockerfile stages and unchanged context digests.
- Keep root-owned qbuild store/data-root consistent while privileged runtime operations are still required.
- Re-run this benchmark after build-cache optimization; qbuild only needs to erase roughly 227 ms from mean full-cycle time to beat Docker on this workload.
