#define main agent_main
#include "vmm-agent.c"
#undef main
#include <assert.h>

/* Run as an executed command too: check the actual post-exec process state. */
static int probe_child(bool terminal) {
    struct sigaction action;
    assert(sigaction(SIGPIPE, NULL, &action) == 0);
    assert(action.sa_handler == SIG_DFL);
    sigset_t blocked;
    assert(sigprocmask(SIG_SETMASK, NULL, &blocked) == 0);
    assert(!sigismember(&blocked, SIGPIPE));
    assert(fcntl(67, F_GETFD) < 0 && errno == EBADF);
    if (terminal) {
        assert(isatty(STDIN_FILENO));
    } else {
        char byte;
        assert(read(STDIN_FILENO, &byte, 1) == 0);
    }
    puts("CHILD_HYGIENE_OK");
    return 0;
}

static void expect_marker(int fd) {
    char output[4096];
    size_t used = 0;
    ssize_t n;
    while ((n = read(fd, output + used, sizeof(output) - used - 1)) > 0) {
        used += (size_t)n;
        assert(used < sizeof(output) - 1);
    }
    assert(n == 0);
    /* Framed exec contains binary NUL bytes; search all output bytes. */
    bool found = false;
    const char *marker = "CHILD_HYGIENE_OK";
    for (size_t i = 0; i + strlen(marker) <= used; i++) {
        if (memcmp(output + i, marker, strlen(marker)) == 0) found = true;
    }
    assert(found);
}

int main(int argc, char **argv) {
    if (argc > 1) return probe_child(strcmp(argv[1], "--pty-probe") == 0);
    alarm(15);
    signal(SIGPIPE, SIG_IGN);
    sigset_t blocked;
    sigemptyset(&blocked);
    sigaddset(&blocked, SIGPIPE);
    assert(sigprocmask(SIG_BLOCK, &blocked, NULL) == 0);
    int control[2];
    assert(socketpair(AF_UNIX, SOCK_STREAM, 0, control) == 0);
    assert(dup2(control[0], 67) == 67); /* Deliberately inheritable control fd. */
    int input[2];
    assert(pipe(input) == 0);
    assert(write(input[1], "not EOF", 7) == 7);
    assert(dup2(input[0], STDIN_FILENO) == STDIN_FILENO);
    char command[4096];
    assert(snprintf(command, sizeof(command), "'%s' --probe", argv[0]) > 0);
    int output[2];
    assert(pipe(output) == 0);
    run_command(output[1], command);
    close(output[1]);
    expect_marker(output[0]);
    close(output[0]);
#ifdef __linux__
    struct sockaddr_vm peer = { .svm_family = AF_VSOCK, .svm_cid = VMADDR_CID_HOST };
    assert(pty_peer_is_host(&peer, sizeof(peer)));
    assert(!pty_peer_is_host(&peer, sizeof(peer) - 1));
    peer.svm_cid = VMADDR_CID_LOCAL;
    assert(!pty_peer_is_host(&peer, sizeof(peer)));
    peer.svm_cid = 3;
    assert(!pty_peer_is_host(&peer, sizeof(peer)));
    int framed[2];
    assert(socketpair(AF_UNIX, SOCK_STREAM, 0, framed) == 0);
    pid_t server = fork();
    assert(server >= 0);
    if (server == 0) {
        close(framed[0]);
        run_command_chunked(framed[1], 42, command);
        close(framed[1]);
        _exit(0);
    }
    close(framed[1]);
    expect_marker(framed[0]);
    close(framed[0]);
    int status;
    assert(waitpid(server, &status, 0) == server && WIFEXITED(status) && WEXITSTATUS(status) == 0);
    int pty_channel[2];
    assert(socketpair(AF_UNIX, SOCK_STREAM, 0, pty_channel) == 0);
    server = fork();
    assert(server >= 0);
    if (server == 0) {
        close(pty_channel[0]);
        handle_pty_client(pty_channel[1]);
        close(pty_channel[1]);
        _exit(0);
    }
    close(pty_channel[1]);
    char start[4096];
    int length = snprintf(start, sizeof(start), "{\"cols\":80,\"rows\":24,\"shell\":\"'%s' --pty-probe\"}", argv[0]);
    assert(length > 0 && (size_t)length < sizeof(start));
    assert(write_pty_frame(pty_channel[0], PTY_FRAME_START, start, (uint32_t)length) == 0);
    expect_marker(pty_channel[0]);
    close(pty_channel[0]);
    assert(waitpid(server, &status, 0) == server && WIFEXITED(status) && WEXITSTATUS(status) == 0);
#endif
    assert(fcntl(67, F_GETFD) >= 0); /* Parent must retain its control channel. */
    puts("command hygiene passed (serial; framed and PTY on Linux)");
    return 0;
}
