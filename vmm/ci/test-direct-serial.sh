#!/usr/bin/env bash
# Linux/KVM. Build a disposable PID 1 that writes through the guest tty driver.
set -euo pipefail
: "${VMM_BIN:?set VMM_BIN to the boot-enabled vmm executable}"
: "${VMM_TEST_KERNEL:?set VMM_TEST_KERNEL to the guest kernel}"
tmp=$(mktemp -d)
trap 'rm -rf -- "$tmp"' EXIT

cc -static -O2 -x c -o "$tmp/init" - <<'C'
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <sys/reboot.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <unistd.h>

int main(void) {
    if (getpid() != 1) return 1;
    if (mkdir("/dev", 0755) && errno != EEXIST) return 2;
    if (mknod("/dev/console", S_IFCHR | 0600, makedev(5, 1)) && errno != EEXIST)
        return 3;
    int fd = open("/dev/console", O_RDWR);
    if (fd < 0 || dprintf(fd, "console write from pid %d\n", getpid()) < 0)
        return 4;
    close(fd);
    reboot(RB_POWER_OFF);
    return 5;
}
C
(cd "$tmp" && printf 'init\n' | cpio --quiet -o -H newc) > "$tmp/initramfs.cpio"
VMM_BOOT_TIMEOUT=10 timeout -k 2 25 "$VMM_BIN" run --full-boot --mem 256 --vcpus 1 \
    --kernel "$VMM_TEST_KERNEL" --initramfs "$tmp/initramfs.cpio" \
    --cmdline 'console=ttyS0 rdinit=/init' > "$tmp/boot.log" 2>&1
# The kernel banner and a zero VMM exit status can both precede a PID 1 panic.
if ! tr -d '\r' < "$tmp/boot.log" | grep -Fx 'console write from pid 1'; then
    tail -25 "$tmp/boot.log" >&2
    exit 1
fi
