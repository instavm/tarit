#define _GNU_SOURCE
#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>

#define SOCKET_PATH "/tmp/tarit-memory-growth.sock"
#define INITIAL (256ULL << 20)
#define GROWN (3ULL << 30)

/* Real process/RAM witness: the nonce exists only in this process's anonymous
 * mapping. Each page has its own marker; growing adds and verifies 3GiB total. */
static unsigned char marker(size_t page) { return (unsigned char)((page * 17 + 93) % 251 + 1); }
static void fail(const char *what) { perror(what); exit(1); }
int main(int argc, char **argv) {
    if (argc != 2) return 2;
    int fd = socket(AF_UNIX, SOCK_STREAM, 0);
    if (fd < 0) fail("socket");
    struct sockaddr_un addr = { .sun_family = AF_UNIX };
    strcpy(addr.sun_path, SOCKET_PATH);
    if (strcmp(argv[1], "serve") != 0) {
        if (connect(fd, (struct sockaddr *)&addr, sizeof(addr))) fail("connect");
        char op = strcmp(argv[1], "grow") == 0 ? 'G' : 'S';
        if (write(fd, &op, 1) != 1) fail("write");
        char buf[256]; ssize_t n;
        while ((n = read(fd, buf, sizeof(buf))) > 0) {
            if (fwrite(buf, 1, (size_t)n, stdout) != (size_t)n) fail("stdout");
        }
        close(fd);
        return n < 0 ? 1 : 0;
    }
    unsigned char *ram = mmap(NULL, GROWN, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (ram == MAP_FAILED) fail("mmap");
    size_t resident = INITIAL;
    for (size_t p = 0; p < resident / 4096; p++) ram[p * 4096] = marker(p);
    uint64_t nonce;
    FILE *random = fopen("/dev/urandom", "rb");
    if (!random || fread(&nonce, sizeof(nonce), 1, random) != 1) fail("random");
    fclose(random);
    unlink(SOCKET_PATH);
    if (bind(fd, (struct sockaddr *)&addr, sizeof(addr)) || listen(fd, 4)) fail("listen");
    for (;;) {
        int client = accept(fd, NULL, NULL);
        if (client < 0) { if (errno == EINTR) continue; fail("accept"); }
        char op;
        if (read(client, &op, 1) != 1) { close(client); continue; }
        size_t bad = 0;
        for (size_t p = 0; p < resident / 4096; p++) if (ram[p * 4096] != marker(p)) bad++;
        if (op == 'G' && !bad) {
            for (size_t p = resident / 4096; p < GROWN / 4096; p++) ram[p * 4096] = marker(p);
            resident = GROWN;
        }
        dprintf(client, "{\"pid\":%ld,\"nonce\":\"%016llx\",\"resident\":%zu,\"bad_pages\":%zu}\n",
                (long)getpid(), (unsigned long long)nonce, resident, bad);
        close(client);
    }
}
