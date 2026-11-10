/* Exercise production helpers without starting init, serial/vsock servers,
 * mounting filesystems, repairing entropy, or requiring KVM/root. */
#define main agent_main_not_run
#include "vmm-agent.c"
#undef main
#include <assert.h>

static void test_line_overflow_resynchronizes(void) {
    int fds[2];
    assert(pipe(fds) == 0);
    const char input[] = "too-long\nOK\r\n";
    assert(write_all(fds[1], input, sizeof(input) - 1) == 0);
    close(fds[1]);
    char line[4];
    assert(read_line(fds[0], line, sizeof(line), true) == 1);
    assert(strcmp(line, "too") == 0);
    assert(read_line(fds[0], line, sizeof(line), true) == 0);
    assert(strcmp(line, "OK") == 0);
    assert(read_line(fds[0], line, sizeof(line), true) == -1);
    close(fds[0]);
}

#ifdef __linux__
static void test_clone_repair_protocol_bounds(void) {
    unsigned char nonce[CLONE_REPAIR_NONCE_BYTES];
    struct timespec timestamp = {0};
    int version = 0;
    char request[256];
    char encoded_nonce[CLONE_REPAIR_NONCE_BYTES * 2 + 1];
    memset(encoded_nonce, 'a', sizeof(encoded_nonce) - 1);
    encoded_nonce[sizeof(encoded_nonce) - 1] = '\0';
    snprintf(request, sizeof(request), "%s%s", CLONE_REPAIR_PREFIX_V2, encoded_nonce);
    assert(decode_clone_repair_request(request, nonce, &timestamp, &version) == 0);
    assert(version == 2 && nonce[0] == 0xaa);
    snprintf(request, sizeof(request), "%s%s00000000000000110000007b",
             CLONE_REPAIR_PREFIX_V3, encoded_nonce);
    assert(decode_clone_repair_request(request, nonce, &timestamp, &version) == 0);
    assert(version == 3 && timestamp.tv_sec == 17 && timestamp.tv_nsec == 123);
    snprintf(request, sizeof(request), "%s%s00000000000000113b9aca00",
             CLONE_REPAIR_PREFIX_V3, encoded_nonce);
    assert(decode_clone_repair_request(request, nonce, &timestamp, &version) == -1);
    assert(errno == ERANGE);
    snprintf(request, sizeof(request), "%s%s", CLONE_REPAIR_PREFIX_V2, encoded_nonce);
    request[CLONE_REPAIR_PREFIX_V2_LEN] = 'z';
    assert(decode_clone_repair_request(request, nonce, &timestamp, &version) == -1);
    request[CLONE_REPAIR_PREFIX_V2_LEN] = '\0';
    assert(decode_clone_repair_request(request, nonce, &timestamp, &version) == -1);
}
#endif

int main(void) {
    test_line_overflow_resynchronizes();
#ifdef __linux__
    test_clone_repair_protocol_bounds();
#endif
    puts("agent protocol tests passed");
    return 0;
}
