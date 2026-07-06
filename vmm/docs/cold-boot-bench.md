# Cold-boot benchmark — 100 iterations

Boots a 128 MiB VM with the in-tree minimal bzImage to first HLT (kernel
init runs, returns to the controller). c8i nested-virt; no userspace
echo because virtio-blk DRIVER_OK doesn't fire on L1 (documented in
remaining_work.md). Bare-metal numbers would skip the L0 trap penalty
and run 2-5× faster.

| metric | value |
|---|---|
| iterations | 100 |
| total wall | 5.74s |
| rate | 17.4 boots/sec |
| min | 8.862 ms |
| p50 | 59.813 ms |
| p95 | 63.809 ms |
| p99 | 70.715 ms |
| p99.9 | 70.715 ms |
| max | 70.715 ms |
| mean | 57.171 ms |
