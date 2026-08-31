#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <sched.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/random.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/un.h>
#include <unistd.h>

#define SOCKET_PATH "/run/tarit/clone-workload.sock"
#define PID_PATH "/run/tarit/clone-workload.pid"
#define CLONE_ID_PATH "/run/tarit/clone-id"
#define REPAIRED_PATH "/run/tarit/clone-workload-repaired"
#define SECRET_BYTES 32U
#define PREFIX_BYTES 16U
#define MAX_REQUEST 256U
#define REPAIR_WAIT_ITERATIONS 1000000U

struct workload_state {
    unsigned char prng_state[SECRET_BYTES];
    unsigned char ticket_key[SECRET_BYTES];
    unsigned char nonce_prefix[PREFIX_BYTES];
    uint64_t nonce_counter;
    char cached_session[65];
    char clone_id[33];
};

static volatile sig_atomic_t repair_requested;

static void request_repair(int signal_number) {
    (void)signal_number;
    repair_requested = 1;
}

static int write_all(int fd, const void *buffer, size_t length) {
    const unsigned char *cursor = buffer;
    while (length > 0) {
        ssize_t written = write(fd, cursor, length);
        if (written < 0) {
            if (errno == EINTR) continue;
            return -1;
        }
        cursor += (size_t)written;
        length -= (size_t)written;
    }
    return 0;
}

static int random_bytes(void *buffer, size_t length) {
    unsigned char *cursor = buffer;
    while (length > 0) {
        ssize_t received = getrandom(cursor, length, 0);
        if (received < 0) {
            if (errno == EINTR) continue;
            return -1;
        }
        cursor += (size_t)received;
        length -= (size_t)received;
    }
    return 0;
}

static void hex_encode(const unsigned char *input, size_t input_length, char *output) {
    static const char digits[] = "0123456789abcdef";
    for (size_t index = 0; index < input_length; index++) {
        output[index * 2] = digits[input[index] >> 4];
        output[index * 2 + 1] = digits[input[index] & 0x0fU];
    }
    output[input_length * 2] = '\0';
}

static int read_clone_id(char clone_id[33]) {
    int fd = open(CLONE_ID_PATH, O_RDONLY | O_CLOEXEC | O_NOFOLLOW);
    if (fd < 0) return -1;
    ssize_t length = read(fd, clone_id, 33);
    int saved_errno = errno;
    close(fd);
    errno = saved_errno;
    if (length < 32) {
        errno = EINVAL;
        return -1;
    }
    clone_id[32] = '\0';
    for (size_t index = 0; index < 32; index++) {
        unsigned char byte = (unsigned char)clone_id[index];
        if (!((byte >= '0' && byte <= '9') || (byte >= 'a' && byte <= 'f'))) {
            errno = EINVAL;
            return -1;
        }
    }
    return 0;
}

static int publish_marker(const char *clone_id) {
    char temporary_path[] = REPAIRED_PATH ".tmp.XXXXXX";
    int fd = mkstemp(temporary_path);
    if (fd < 0) return -1;
    if (fchmod(fd, 0600) < 0 || write_all(fd, clone_id, 32) < 0 ||
        write_all(fd, "\n", 1) < 0 || fsync(fd) < 0 || close(fd) < 0 ||
        rename(temporary_path, REPAIRED_PATH) < 0) {
        int saved_errno = errno;
        close(fd);
        unlink(temporary_path);
        errno = saved_errno;
        return -1;
    }
    return 0;
}

static int initialize_state(struct workload_state *state) {
    memset(state, 0, sizeof(*state));
    if (random_bytes(state->prng_state, sizeof(state->prng_state)) < 0 ||
        random_bytes(state->ticket_key, sizeof(state->ticket_key)) < 0 ||
        random_bytes(state->nonce_prefix, sizeof(state->nonce_prefix)) < 0) {
        return -1;
    }
    strcpy(state->clone_id, "cold-boot");
    return 0;
}

static uint64_t splitmix64(uint64_t *state) {
    uint64_t value = (*state += UINT64_C(0x9e3779b97f4a7c15));
    value = (value ^ (value >> 30)) * UINT64_C(0xbf58476d1ce4e5b9);
    value = (value ^ (value >> 27)) * UINT64_C(0x94d049bb133111eb);
    return value ^ (value >> 31);
}

static void rotate_state(unsigned char *state, size_t length, const char *clone_id,
                         uint64_t domain) {
    uint64_t seed = UINT64_C(0xcbf29ce484222325) ^ domain;
    for (size_t index = 0; index < length; index++) {
        seed = (seed ^ state[index]) * UINT64_C(0x100000001b3);
    }
    for (size_t index = 0; index < 32; index++) {
        seed = (seed ^ (unsigned char)clone_id[index]) * UINT64_C(0x100000001b3);
    }
    for (size_t index = 0; index < length; index++) {
        if ((index % sizeof(uint64_t)) == 0) seed = splitmix64(&seed);
        state[index] ^= (unsigned char)(seed >> ((index % sizeof(uint64_t)) * 8U));
    }
}

static int repair_state(struct workload_state *state) {
    char clone_id[33];

    if (read_clone_id(clone_id) < 0) return -1;
    /* The clone ID is generated from fresh host randomness after restore.
     * Fold it into each cached state domain without depending on another
     * potentially blocking guest-randomness call inside the repair hook. */
    rotate_state(state->prng_state, sizeof(state->prng_state), clone_id, 1);
    rotate_state(state->ticket_key, sizeof(state->ticket_key), clone_id, 2);
    rotate_state(state->nonce_prefix, sizeof(state->nonce_prefix), clone_id, 3);
    memset(state->cached_session, 0, sizeof(state->cached_session));
    memcpy(state->clone_id, clone_id, sizeof(state->clone_id));
    state->nonce_counter = 0;
    return publish_marker(clone_id);
}

static uint64_t next_prng_word(struct workload_state *state) {
    uint64_t value = state->nonce_counter + UINT64_C(0x9e3779b97f4a7c15);
    for (size_t index = 0; index < sizeof(state->prng_state); index++) {
        value ^= (uint64_t)state->prng_state[index] << ((index % 8U) * 8U);
        value ^= value >> 12;
        value ^= value << 25;
        value ^= value >> 27;
    }
    return value * UINT64_C(0x2545f4914f6cdd1d);
}

static int respond(int fd, const char *response) {
    return write_all(fd, response, strlen(response));
}

static int handle_request(int fd, struct workload_state *state) {
    char request[MAX_REQUEST + 1];
    ssize_t length = read(fd, request, MAX_REQUEST);
    if (length < 0) return errno == EINTR ? 0 : -1;
    request[length] = '\0';
    request[strcspn(request, "\r\n")] = '\0';

    if (strcmp(request, "STATE") == 0) {
        char prng[SECRET_BYTES * 2 + 1];
        char ticket[SECRET_BYTES * 2 + 1];
        char prefix[PREFIX_BYTES * 2 + 1];
        char response[512];
        hex_encode(state->prng_state, sizeof(state->prng_state), prng);
        hex_encode(state->ticket_key, sizeof(state->ticket_key), ticket);
        hex_encode(state->nonce_prefix, sizeof(state->nonce_prefix), prefix);
        int size = snprintf(response, sizeof(response),
                            "clone=%s prng=%s ticket=%s prefix=%s counter=%llu cache=%s\n",
                            state->clone_id, prng, ticket, prefix,
                            (unsigned long long)state->nonce_counter,
                            state->cached_session[0] ? state->cached_session : "-");
        return size > 0 && (size_t)size < sizeof(response) ? respond(fd, response) : -1;
    }
    if (strcmp(request, "ISSUE") == 0) {
        char prefix[PREFIX_BYTES * 2 + 1];
        char response[160];
        hex_encode(state->nonce_prefix, sizeof(state->nonce_prefix), prefix);
        uint64_t counter = state->nonce_counter++;
        uint64_t word = next_prng_word(state);
        int size = snprintf(response, sizeof(response), "%s-%016llx-%016llx\n", prefix,
                            (unsigned long long)counter, (unsigned long long)word);
        return size > 0 && (size_t)size < sizeof(response) ? respond(fd, response) : -1;
    }
    if (strcmp(request, "TICKET") == 0) {
        char ticket[SECRET_BYTES * 2 + 2];
        hex_encode(state->ticket_key, sizeof(state->ticket_key), ticket);
        strcat(ticket, "\n");
        return respond(fd, ticket);
    }
    if (strncmp(request, "REPAIR ", 7) == 0) {
        char clone_id[33];
        if (strlen(request + 7) != 32 || read_clone_id(clone_id) < 0 ||
            strcmp(request + 7, clone_id) != 0 || repair_state(state) < 0) {
            return respond(fd, "failed\n");
        }
        return respond(fd, "repaired\n");
    }
    if (strncmp(request, "ACCEPT_TICKET ", 14) == 0) {
        char ticket[SECRET_BYTES * 2 + 1];
        hex_encode(state->ticket_key, sizeof(state->ticket_key), ticket);
        return respond(fd, strcmp(request + 14, ticket) == 0 ? "accepted\n" : "rejected\n");
    }
    if (strncmp(request, "CACHE ", 6) == 0) {
        size_t session_length = strlen(request + 6);
        if (session_length == 0 || session_length >= sizeof(state->cached_session)) {
            return respond(fd, "invalid\n");
        }
        memcpy(state->cached_session, request + 6, session_length + 1);
        return respond(fd, "stored\n");
    }
    if (strncmp(request, "CHECK ", 6) == 0) {
        return respond(fd, strcmp(request + 6, state->cached_session) == 0 ? "present\n" : "absent\n");
    }
    return respond(fd, "unknown\n");
}

static int write_pid(void) {
    char value[32];
    int length = snprintf(value, sizeof(value), "%ld\n", (long)getpid());
    int fd = open(PID_PATH, O_WRONLY | O_CREAT | O_TRUNC | O_CLOEXEC | O_NOFOLLOW, 0600);
    if (fd < 0) return -1;
    int result = length > 0 && (size_t)length < sizeof(value) && write_all(fd, value, (size_t)length) == 0 && fsync(fd) == 0 ? 0 : -1;
    int saved_errno = errno;
    close(fd);
    errno = saved_errno;
    return result;
}

static int signal_and_wait_for_repair(const char *expected_clone_id) {
    char value[32];
    char *end = NULL;
    int fd = open(PID_PATH, O_RDONLY | O_CLOEXEC | O_NOFOLLOW);
    if (fd < 0) return -1;
    ssize_t length = read(fd, value, sizeof(value) - 1U);
    int saved_errno = errno;
    close(fd);
    errno = saved_errno;
    if (length <= 0) return -1;
    value[length] = '\0';
    errno = 0;
    long parsed = strtol(value, &end, 10);
    if (errno != 0 || end == value || parsed <= 1 || parsed > INT32_MAX) return -1;
    if (kill((pid_t)parsed, SIGHUP) < 0) return -1;

    for (unsigned int attempt = 0; attempt < REPAIR_WAIT_ITERATIONS; attempt++) {
        char repaired[33];
        fd = open(REPAIRED_PATH, O_RDONLY | O_CLOEXEC | O_NOFOLLOW);
        if (fd >= 0) {
            length = read(fd, repaired, sizeof(repaired));
            close(fd);
            if (length >= 32 && memcmp(repaired, expected_clone_id, 32) == 0) {
                return 0;
            }
        }
        (void)sched_yield();
    }
    errno = ETIMEDOUT;
    return -1;
}

static int serve(void) {
    struct sigaction action = {0};
    struct workload_state state;
    struct sockaddr_un address = {0};
    int listener = -1;
    int result = EXIT_FAILURE;

    if (mkdir("/run/tarit", 0700) < 0 && errno != EEXIST) return EXIT_FAILURE;
    if (initialize_state(&state) < 0) return EXIT_FAILURE;
    action.sa_handler = request_repair;
    sigemptyset(&action.sa_mask);
    if (sigaction(SIGHUP, &action, NULL) < 0 || signal(SIGPIPE, SIG_IGN) == SIG_ERR) {
        return EXIT_FAILURE;
    }

    listener = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
    if (listener < 0) return EXIT_FAILURE;
    address.sun_family = AF_UNIX;
    memcpy(address.sun_path, SOCKET_PATH, sizeof(SOCKET_PATH));
    unlink(SOCKET_PATH);
    if (bind(listener, (struct sockaddr *)&address, sizeof(address)) < 0 ||
        chmod(SOCKET_PATH, 0600) < 0 || listen(listener, 16) < 0 || write_pid() < 0) {
        goto out;
    }

    for (;;) {
        if (repair_requested) {
            repair_requested = 0;
            if (repair_state(&state) < 0) goto out;
        }
        struct pollfd descriptor = {.fd = listener, .events = POLLIN};
        int ready = poll(&descriptor, 1, 100);
        if (ready < 0) {
            if (errno == EINTR) continue;
            goto out;
        }
        if (ready == 0) continue;
        int client = accept4(listener, NULL, NULL, SOCK_CLOEXEC);
        if (client < 0) {
            if (errno == EINTR) continue;
            goto out;
        }
        (void)handle_request(client, &state);
        close(client);
    }

out:
    memset(&state, 0, sizeof(state));
    if (listener >= 0) close(listener);
    unlink(SOCKET_PATH);
    unlink(PID_PATH);
    return result;
}

static int client(int argc, char **argv) {
    struct sockaddr_un address = {0};
    char request[MAX_REQUEST + 2];
    char response[1024];
    int socket_fd;
    int request_length;
    int expect_repaired = 0;

    if (argc < 2) return EXIT_FAILURE;
    if (strcmp(argv[1], "state") == 0) {
        request_length = snprintf(request, sizeof(request), "STATE\n");
    } else if (strcmp(argv[1], "issue") == 0) {
        request_length = snprintf(request, sizeof(request), "ISSUE\n");
    } else if (strcmp(argv[1], "ticket") == 0) {
        request_length = snprintf(request, sizeof(request), "TICKET\n");
    } else if (argc == 3 && strcmp(argv[1], "accept-ticket") == 0) {
        request_length = snprintf(request, sizeof(request), "ACCEPT_TICKET %s\n", argv[2]);
    } else if (argc == 3 && strcmp(argv[1], "repair") == 0) {
        request_length = snprintf(request, sizeof(request), "REPAIR %s\n", argv[2]);
        expect_repaired = 1;
    } else if (argc == 3 && strcmp(argv[1], "cache") == 0) {
        request_length = snprintf(request, sizeof(request), "CACHE %s\n", argv[2]);
    } else if (argc == 3 && strcmp(argv[1], "check") == 0) {
        request_length = snprintf(request, sizeof(request), "CHECK %s\n", argv[2]);
    } else {
        return EXIT_FAILURE;
    }
    if (request_length <= 0 || (size_t)request_length >= sizeof(request)) return EXIT_FAILURE;

    socket_fd = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
    if (socket_fd < 0) return EXIT_FAILURE;
    address.sun_family = AF_UNIX;
    memcpy(address.sun_path, SOCKET_PATH, sizeof(SOCKET_PATH));
    if (connect(socket_fd, (struct sockaddr *)&address, sizeof(address)) < 0 ||
        write_all(socket_fd, request, (size_t)request_length) < 0) {
        close(socket_fd);
        return EXIT_FAILURE;
    }
    ssize_t length = read(socket_fd, response, sizeof(response));
    close(socket_fd);
    if (length <= 0 || (expect_repaired &&
                        ((size_t)length != sizeof("repaired\n") - 1U ||
                         memcmp(response, "repaired\n", sizeof("repaired\n") - 1U) != 0)) ||
        write_all(STDOUT_FILENO, response, (size_t)length) < 0) {
        return EXIT_FAILURE;
    }
    return EXIT_SUCCESS;
}

int main(int argc, char **argv) {
    if (argc == 2 && strcmp(argv[1], "serve") == 0) return serve();
    if (argc == 3 && strcmp(argv[1], "repair-signal") == 0) {
        return strlen(argv[2]) == 32 && signal_and_wait_for_repair(argv[2]) == 0
                   ? EXIT_SUCCESS
                   : EXIT_FAILURE;
    }
    return client(argc, argv);
}
