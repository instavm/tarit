#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdarg.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <termios.h>
#include <unistd.h>
#ifdef __linux__
#include <linux/vm_sockets.h>
#include <poll.h>
#include <pty.h>
#include <sys/mount.h>
#include <sys/sysmacros.h>
#endif

#define EXEC_PREFIX "VMM_EXEC:"
#define EXEC_PREFIX_LEN 9

/* Host-side vsock port the agent dials for the exec channel. The VMM bridges
 * (guest_cid, this port) → a per-VM host Unix socket the controller accepts on.
 * vsock gives exec its own framed, per-connection stream that never interleaves
 * with kernel console output on ttyS0, so exec can't desync under IRQ load and
 * a dead connection after restore is cleanly re-dialed. Serial stays as a
 * fallback for kernels/hosts without a virtio-vsock device. */
#define VMM_EXEC_VSOCK_PORT 1024
#define VMM_PTY_VSOCK_PORT 1025
#define LINE_MAX_LEN 4096
#define VSOCK_RECONNECT_BACKOFF_INITIAL_US 10000U
#define VSOCK_RECONNECT_BACKOFF_MAX_US 1000000U

#define PTY_FRAME_DATA 0
#define PTY_FRAME_RESIZE 1
#define PTY_FRAME_EXIT 2
#define PTY_FRAME_ERROR 3
#define PTY_FRAME_START 4
#define PTY_MAX_FRAME_LEN (16U * 1024U * 1024U)

/* A sane default PATH so commands (node, python, ...) resolve when we run as
 * init on an OCI-derived rootfs where no login shell exported one. */
#define DEFAULT_PATH "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"

static int write_all(int fd, const void *buf, size_t len) {
    const unsigned char *p = (const unsigned char *)buf;
    while (len > 0) {
        ssize_t n = write(fd, p, len);
        if (n < 0) {
            if (errno == EINTR) {
                continue;
            }
            return -1;
        }
        if (n == 0) {
            errno = EIO;
            return -1;
        }
        p += (size_t)n;
        len -= (size_t)n;
    }
    return 0;
}

static int serial_write(int fd, const void *buf, size_t len) {
    if (write_all(fd, buf, len) < 0) {
        return -1;
    }
    /* Drain only real ttys; on a vsock socket tcdrain returns ENOTTY and the
     * bytes are already handed to the transport, so that is not an error. */
    while (tcdrain(fd) < 0) {
        if (errno == EINTR) {
            continue;
        }
        if (errno == ENOTTY || errno == EINVAL || errno == ENOSYS) {
            break;
        }
        return -1;
    }
    return 0;
}

static void serial_printf(int fd, const char *fmt, ...) {
    char buf[128];
    va_list ap;
    va_start(ap, fmt);
    int n = vsnprintf(buf, sizeof(buf), fmt, ap);
    va_end(ap);
    if (n < 0) {
        return;
    }
    if ((size_t)n >= sizeof(buf)) {
        n = (int)sizeof(buf) - 1;
    }
    (void)serial_write(fd, buf, (size_t)n);
}

#ifdef __linux__
/* Mount one pseudo-filesystem, creating its mountpoint first. Best-effort. */
static void mount_pseudo(const char *src, const char *target, const char *fstype,
                         unsigned long flags, const void *data) {
    (void)mkdir(target, 0755);
    (void)mount(src, target, fstype, flags, data);
}

/* Ensure an essential device node exists (fallback when devtmpfs is absent). */
static void ensure_node(const char *path, mode_t mode, unsigned major, unsigned minor) {
    if (access(path, F_OK) == 0) {
        return;
    }
    (void)mknod(path, mode, makedev(major, minor));
}

/* PID 1 setup for booting an OCI-derived (initless) rootfs directly: bring up
 * the pseudo-filesystems a normal init would, so /dev/urandom, /dev/null, /proc
 * etc. exist for the workload (node reads /dev/urandom at startup). Must run
 * before we open the serial device, since /dev/ttyS0 lives on devtmpfs. */
static void setup_as_init(void) {
    mount_pseudo("proc", "/proc", "proc", MS_NOSUID | MS_NOEXEC | MS_NODEV, NULL);
    mount_pseudo("sysfs", "/sys", "sysfs", MS_NOSUID | MS_NOEXEC | MS_NODEV, NULL);
    /* devtmpfs auto-populates /dev with the kernel's device nodes (ttyS0,
     * null, urandom, ...). If the kernel lacks devtmpfs, fall back to a tmpfs
     * plus the handful of nodes programs actually need. */
    if (mount("devtmpfs", "/dev", "devtmpfs", MS_NOSUID, "mode=0755") != 0) {
        mount_pseudo("tmpfs", "/dev", "tmpfs", MS_NOSUID, "mode=0755");
        ensure_node("/dev/null", S_IFCHR | 0666, 1, 3);
        ensure_node("/dev/zero", S_IFCHR | 0666, 1, 5);
        ensure_node("/dev/full", S_IFCHR | 0666, 1, 7);
        ensure_node("/dev/random", S_IFCHR | 0666, 1, 8);
        ensure_node("/dev/urandom", S_IFCHR | 0666, 1, 9);
        ensure_node("/dev/tty", S_IFCHR | 0666, 5, 0);
        ensure_node("/dev/console", S_IFCHR | 0600, 5, 1);
        ensure_node("/dev/ttyS0", S_IFCHR | 0660, 4, 64);
    }
    mount_pseudo("devpts", "/dev/pts", "devpts", MS_NOSUID | MS_NOEXEC, "mode=0620,gid=5");
    mount_pseudo("tmpfs", "/run", "tmpfs", MS_NOSUID | MS_NODEV, "mode=0755");
    mount_pseudo("tmpfs", "/tmp", "tmpfs", MS_NOSUID | MS_NODEV, "mode=1777");

    /* Give children a usable PATH regardless of the image's shell profile. */
    if (getenv("PATH") == NULL) {
        setenv("PATH", DEFAULT_PATH, 1);
    }
}
#else
static void setup_as_init(void) {
    if (getenv("PATH") == NULL) {
        setenv("PATH", DEFAULT_PATH, 1);
    }
}
#endif

/* Reap any orphaned children we inherited as PID 1, without blocking. Called
 * between commands; the synchronous exec child is already waited for in
 * run_command, so this only collects double-forked strays. */
static void reap_orphans(void) {
    while (waitpid(-1, NULL, WNOHANG) > 0) {
    }
}

static void make_raw(int fd) {
    struct termios tio;
    if (tcgetattr(fd, &tio) < 0) {
        return;
    }

    tio.c_iflag &= (tcflag_t)~(IGNBRK | BRKINT | PARMRK | ISTRIP | INLCR | IGNCR | ICRNL | IXON);
    tio.c_oflag &= (tcflag_t)~OPOST;
    tio.c_lflag &= (tcflag_t)~(ECHO | ECHONL | ICANON | ISIG | IEXTEN);
    tio.c_cflag &= (tcflag_t)~(CSIZE | PARENB);
    tio.c_cflag |= CS8 | CREAD | CLOCAL;
    tio.c_cc[VMIN] = 1;
    tio.c_cc[VTIME] = 0;

    (void)tcsetattr(fd, TCSANOW, &tio);
}

static int open_serial(void) {
    int fd = open("/dev/ttyS0", O_RDWR | O_NOCTTY);
    if (fd < 0) {
        fd = open("/dev/console", O_RDWR | O_NOCTTY);
    }
    if (fd >= 0) {
        make_raw(fd);
    }
    return fd;
}

static int read_line(int fd, char *line, size_t cap, bool eof_disconnect) {
    size_t len = 0;
    bool overflow = false;

    for (;;) {
        char c;
        ssize_t n = read(fd, &c, 1);
        if (n < 0) {
            if (errno == EINTR) {
                continue;
            }
            return -1;
        }
        if (n == 0) {
            /* A stream (vsock) EOF means the peer closed → reconnect. A serial
             * tty can momentarily return 0 without a real hangup, so there we
             * keep waiting instead of tearing the channel down. */
            if (eof_disconnect) {
                return -1;
            }
            continue;
        }
        if (c == '\r') {
            continue;
        }
        if (c == '\n') {
            if (cap > 0) {
                line[len] = '\0';
            }
            return overflow ? 1 : 0;
        }
        if (len + 1 < cap) {
            line[len++] = c;
        } else {
            overflow = true;
        }
    }
}

static int status_to_exit_code(int status) {
    if (WIFEXITED(status)) {
        return WEXITSTATUS(status);
    }
    if (WIFSIGNALED(status)) {
        return 128 + WTERMSIG(status);
    }
    return 1;
}

static int wait_for_child(pid_t pid) {
    int status;
    for (;;) {
        if (waitpid(pid, &status, 0) >= 0) {
            return status_to_exit_code(status);
        }
        if (errno != EINTR) {
            return 127;
        }
    }
}

static void run_command(int serial_fd, const char *command) {
    int pipefd[2];
    bool wrote_output = false;
    bool output_ended_with_newline = true;

    (void)serial_write(serial_fd, "VMM_EXEC_START\n", 15);

    if (pipe(pipefd) < 0) {
        serial_printf(serial_fd, "vmm-agent: pipe failed: %s\n", strerror(errno));
        serial_printf(serial_fd, "VMM_EXEC_EXIT=%d\n", 127);
        return;
    }

    pid_t pid = fork();
    if (pid < 0) {
        serial_printf(serial_fd, "vmm-agent: fork failed: %s\n", strerror(errno));
        close(pipefd[0]);
        close(pipefd[1]);
        serial_printf(serial_fd, "VMM_EXEC_EXIT=%d\n", 127);
        return;
    }

    if (pid == 0) {
        close(pipefd[0]);
        if (dup2(pipefd[1], STDOUT_FILENO) < 0 || dup2(pipefd[1], STDERR_FILENO) < 0) {
            _exit(127);
        }
        close(pipefd[1]);
        execl("/bin/sh", "sh", "-c", command, (char *)NULL);
        _exit(127);
    }

    close(pipefd[1]);

    for (;;) {
        char buf[1024];
        ssize_t n = read(pipefd[0], buf, sizeof(buf));
        if (n < 0) {
            if (errno == EINTR) {
                continue;
            }
            serial_printf(serial_fd, "vmm-agent: read failed: %s\n", strerror(errno));
            break;
        }
        if (n == 0) {
            break;
        }
        wrote_output = true;
        output_ended_with_newline = (buf[n - 1] == '\n');
        (void)serial_write(serial_fd, buf, (size_t)n);
    }

    close(pipefd[0]);
    int exit_code = wait_for_child(pid);

    if (wrote_output && !output_ended_with_newline) {
        (void)serial_write(serial_fd, "\n", 1);
    }
    serial_printf(serial_fd, "VMM_EXEC_EXIT=%d\n", exit_code);
}

#ifdef __linux__
/* Dial the host exec channel over vsock (guest -> host CID 2, fixed port). The
 * VMM bridges this to a per-VM host Unix socket the controller accepts on. */
static int vsock_connect_host(void) {
    int fd = socket(AF_VSOCK, SOCK_STREAM, 0);
    if (fd < 0) {
        return -1;
    }
    struct sockaddr_vm addr;
    memset(&addr, 0, sizeof(addr));
    addr.svm_family = AF_VSOCK;
    addr.svm_cid = VMADDR_CID_HOST; /* 2 */
    addr.svm_port = VMM_EXEC_VSOCK_PORT;
    if (connect(fd, (struct sockaddr *)&addr, sizeof(addr)) < 0) {
        close(fd);
        return -1;
    }
    return fd;
}

static void sleep_us(unsigned int usec) {
    struct timespec ts;
    ts.tv_sec = (time_t)(usec / 1000000U);
    ts.tv_nsec = (long)(usec % 1000000U) * 1000L;
    while (nanosleep(&ts, &ts) < 0 && errno == EINTR) {
    }
}

static int read_exact_fd(int fd, void *buf, size_t len) {
    unsigned char *p = (unsigned char *)buf;
    while (len > 0) {
        ssize_t n = read(fd, p, len);
        if (n < 0) {
            if (errno == EINTR) {
                continue;
            }
            return -1;
        }
        if (n == 0) {
            return 1;
        }
        p += (size_t)n;
        len -= (size_t)n;
    }
    return 0;
}

static int read_pty_frame(int fd, uint8_t *type, unsigned char **payload, uint32_t *len) {
    unsigned char header[5];
    int rc = read_exact_fd(fd, header, sizeof(header));
    if (rc != 0) {
        return rc;
    }

    *type = header[0];
    *len = ((uint32_t)header[1] << 24) | ((uint32_t)header[2] << 16) |
           ((uint32_t)header[3] << 8) | (uint32_t)header[4];
    if (*len > PTY_MAX_FRAME_LEN) {
        errno = EMSGSIZE;
        return -1;
    }

    *payload = (unsigned char *)malloc((size_t)*len + 1U);
    if (*payload == NULL) {
        return -1;
    }
    rc = read_exact_fd(fd, *payload, *len);
    if (rc != 0) {
        free(*payload);
        *payload = NULL;
        return rc;
    }
    (*payload)[*len] = '\0';
    return 0;
}

static int write_pty_frame(int fd, uint8_t type, const void *payload, uint32_t len) {
    unsigned char header[5];
    header[0] = type;
    header[1] = (unsigned char)((len >> 24) & 0xffU);
    header[2] = (unsigned char)((len >> 16) & 0xffU);
    header[3] = (unsigned char)((len >> 8) & 0xffU);
    header[4] = (unsigned char)(len & 0xffU);
    if (write_all(fd, header, sizeof(header)) < 0) {
        return -1;
    }
    if (len > 0 && write_all(fd, payload, len) < 0) {
        return -1;
    }
    return 0;
}

static int send_pty_error(int fd, const char *msg) {
    return write_pty_frame(fd, PTY_FRAME_ERROR, msg, (uint32_t)strlen(msg));
}

static int send_pty_exit(int fd, int exit_code) {
    char json[64];
    int n = snprintf(json, sizeof(json), "{\"exit_code\":%d}", exit_code);
    if (n < 0) {
        return -1;
    }
    if ((size_t)n >= sizeof(json)) {
        n = (int)sizeof(json) - 1;
    }
    return write_pty_frame(fd, PTY_FRAME_EXIT, json, (uint32_t)n);
}

static const char *json_value_for_key(const char *json, const char *key) {
    char needle[32];
    int n = snprintf(needle, sizeof(needle), "\"%s\"", key);
    if (n < 0 || (size_t)n >= sizeof(needle)) {
        return NULL;
    }
    const char *p = strstr(json, needle);
    if (p == NULL) {
        return NULL;
    }
    p = strchr(p + n, ':');
    if (p == NULL) {
        return NULL;
    }
    p++;
    while (*p == ' ' || *p == '\t' || *p == '\r' || *p == '\n') {
        p++;
    }
    return p;
}

static bool json_get_u16(const char *json, const char *key, uint16_t *out) {
    const char *p = json_value_for_key(json, key);
    if (p == NULL) {
        return false;
    }
    char *end = NULL;
    unsigned long v = strtoul(p, &end, 10);
    if (end == p || v > 65535UL) {
        return false;
    }
    *out = (uint16_t)v;
    return true;
}

static char *json_get_shell(const char *json) {
    const char *p = json_value_for_key(json, "shell");
    if (p == NULL || strncmp(p, "null", 4) == 0) {
        return NULL;
    }
    if (*p != '"') {
        return NULL;
    }
    p++;

    size_t cap = strlen(p) + 1U;
    char *out = (char *)malloc(cap);
    if (out == NULL) {
        return NULL;
    }
    size_t j = 0;
    while (*p != '\0' && *p != '"') {
        if (*p == '\\') {
            p++;
            switch (*p) {
            case '"':
            case '\\':
            case '/':
                out[j++] = *p++;
                break;
            case 'b':
                out[j++] = '\b';
                p++;
                break;
            case 'f':
                out[j++] = '\f';
                p++;
                break;
            case 'n':
                out[j++] = '\n';
                p++;
                break;
            case 'r':
                out[j++] = '\r';
                p++;
                break;
            case 't':
                out[j++] = '\t';
                p++;
                break;
            case 'u':
                out[j++] = '?';
                p++;
                for (int i = 0; i < 4 && ((*p >= '0' && *p <= '9') ||
                                           (*p >= 'a' && *p <= 'f') ||
                                           (*p >= 'A' && *p <= 'F'));
                     i++) {
                    p++;
                }
                break;
            case '\0':
                out[j] = '\0';
                return out;
            default:
                out[j++] = *p++;
                break;
            }
        } else {
            out[j++] = *p++;
        }
    }
    out[j] = '\0';
    if (out[0] == '\0') {
        free(out);
        return NULL;
    }
    return out;
}

static void set_pty_winsize(int master_fd, uint16_t cols, uint16_t rows) {
    struct winsize ws;
    memset(&ws, 0, sizeof(ws));
    ws.ws_col = cols ? cols : 80;
    ws.ws_row = rows ? rows : 24;
    (void)ioctl(master_fd, TIOCSWINSZ, &ws);
}

static void terminate_pty_child(pid_t pid) {
    (void)kill(-pid, SIGHUP);
    (void)kill(pid, SIGHUP);
    (void)wait_for_child(pid);
}

static void drain_pty_output(int fd, int master_fd) {
    for (;;) {
        struct pollfd pfd;
        pfd.fd = master_fd;
        pfd.events = POLLIN;
        pfd.revents = 0;
        int prc = poll(&pfd, 1, 0);
        if (prc <= 0 || (pfd.revents & (POLLIN | POLLHUP | POLLERR)) == 0) {
            return;
        }
        char buf[4096];
        ssize_t n = read(master_fd, buf, sizeof(buf));
        if (n <= 0) {
            return;
        }
        if (write_pty_frame(fd, PTY_FRAME_DATA, buf, (uint32_t)n) < 0) {
            return;
        }
    }
}

static void relay_pty_session(int fd, int master_fd, pid_t child) {
    for (;;) {
        int status = 0;
        pid_t wr = waitpid(child, &status, WNOHANG);
        if (wr == child) {
            drain_pty_output(fd, master_fd);
            (void)send_pty_exit(fd, status_to_exit_code(status));
            return;
        }

        struct pollfd pfds[2];
        pfds[0].fd = master_fd;
        pfds[0].events = POLLIN;
        pfds[0].revents = 0;
        pfds[1].fd = fd;
        pfds[1].events = POLLIN;
        pfds[1].revents = 0;

        int prc = poll(pfds, 2, 250);
        if (prc < 0) {
            if (errno == EINTR) {
                continue;
            }
            terminate_pty_child(child);
            return;
        }
        if (prc == 0) {
            continue;
        }

        if ((pfds[0].revents & (POLLIN | POLLHUP | POLLERR)) != 0) {
            char buf[4096];
            ssize_t n = read(master_fd, buf, sizeof(buf));
            if (n > 0) {
                if (write_pty_frame(fd, PTY_FRAME_DATA, buf, (uint32_t)n) < 0) {
                    terminate_pty_child(child);
                    return;
                }
            } else if (n < 0 && (errno == EINTR || errno == EAGAIN || errno == EWOULDBLOCK)) {
                continue;
            } else {
                int exit_code = wait_for_child(child);
                (void)send_pty_exit(fd, exit_code);
                return;
            }
        }

        if ((pfds[1].revents & (POLLIN | POLLHUP | POLLERR)) != 0) {
            uint8_t type = 0;
            uint32_t len = 0;
            unsigned char *payload = NULL;
            int rc = read_pty_frame(fd, &type, &payload, &len);
            if (rc != 0) {
                terminate_pty_child(child);
                return;
            }

            if (type == PTY_FRAME_DATA) {
                if (len > 0 && write_all(master_fd, payload, len) < 0) {
                    free(payload);
                    terminate_pty_child(child);
                    return;
                }
            } else if (type == PTY_FRAME_RESIZE) {
                uint16_t cols = 80;
                uint16_t rows = 24;
                (void)json_get_u16((const char *)payload, "cols", &cols);
                (void)json_get_u16((const char *)payload, "rows", &rows);
                set_pty_winsize(master_fd, cols, rows);
            } else if (type == PTY_FRAME_ERROR) {
                free(payload);
                terminate_pty_child(child);
                return;
            }
            free(payload);
        }
    }
}

static void handle_pty_client(int fd) {
    uint8_t type = 0;
    uint32_t len = 0;
    unsigned char *payload = NULL;
    int rc = read_pty_frame(fd, &type, &payload, &len);
    if (rc != 0) {
        return;
    }
    if (type != PTY_FRAME_START) {
        free(payload);
        (void)send_pty_error(fd, "expected START frame");
        return;
    }

    uint16_t cols = 80;
    uint16_t rows = 24;
    (void)json_get_u16((const char *)payload, "cols", &cols);
    (void)json_get_u16((const char *)payload, "rows", &rows);
    char *shell = json_get_shell((const char *)payload);
    free(payload);

    struct winsize ws;
    memset(&ws, 0, sizeof(ws));
    ws.ws_col = cols ? cols : 80;
    ws.ws_row = rows ? rows : 24;

    int master_fd = -1;
    int slave_fd = -1;
    if (openpty(&master_fd, &slave_fd, NULL, NULL, &ws) < 0) {
        free(shell);
        (void)send_pty_error(fd, strerror(errno));
        return;
    }

    pid_t pid = fork();
    if (pid < 0) {
        free(shell);
        close(master_fd);
        close(slave_fd);
        (void)send_pty_error(fd, strerror(errno));
        return;
    }

    if (pid == 0) {
        close(master_fd);
        close(fd);
        if (setsid() < 0) {
            _exit(127);
        }
        (void)ioctl(slave_fd, TIOCSCTTY, 0);
        if (dup2(slave_fd, STDIN_FILENO) < 0 || dup2(slave_fd, STDOUT_FILENO) < 0 ||
            dup2(slave_fd, STDERR_FILENO) < 0) {
            _exit(127);
        }
        if (slave_fd > STDERR_FILENO) {
            close(slave_fd);
        }
        if (getenv("PATH") == NULL) {
            setenv("PATH", DEFAULT_PATH, 1);
        }
        const char *chosen = shell;
        if (chosen == NULL || chosen[0] == '\0') {
            chosen = getenv("SHELL");
        }
        if (chosen == NULL || chosen[0] == '\0') {
            chosen = (access("/bin/bash", X_OK) == 0) ? "/bin/bash" : "/bin/sh";
        }
        execlp(chosen, chosen, (char *)NULL);
        execl("/bin/sh", "sh", (char *)NULL);
        _exit(127);
    }

    close(slave_fd);
    free(shell);
    relay_pty_session(fd, master_fd, pid);
    close(master_fd);
}

static int listen_pty_vsock(void) {
    int fd = socket(AF_VSOCK, SOCK_STREAM, 0);
    if (fd < 0) {
        return -1;
    }
    struct sockaddr_vm addr;
    memset(&addr, 0, sizeof(addr));
    addr.svm_family = AF_VSOCK;
    addr.svm_cid = VMADDR_CID_ANY;
    addr.svm_port = VMM_PTY_VSOCK_PORT;
    if (bind(fd, (struct sockaddr *)&addr, sizeof(addr)) < 0) {
        close(fd);
        return -1;
    }
    if (listen(fd, 16) < 0) {
        close(fd);
        return -1;
    }
    return fd;
}

static void reap_pty_clients(void) {
    while (waitpid(-1, NULL, WNOHANG) > 0) {
    }
}

static void serve_pty_forever(void) {
    for (;;) {
        int listen_fd = listen_pty_vsock();
        if (listen_fd < 0) {
            sleep_us(VSOCK_RECONNECT_BACKOFF_MAX_US);
            continue;
        }

        for (;;) {
            reap_pty_clients();
            struct pollfd pfd;
            pfd.fd = listen_fd;
            pfd.events = POLLIN;
            pfd.revents = 0;
            int prc = poll(&pfd, 1, 1000);
            if (prc < 0) {
                if (errno == EINTR) {
                    continue;
                }
                break;
            }
            if (prc == 0) {
                continue;
            }

            int fd = accept(listen_fd, NULL, NULL);
            if (fd < 0) {
                if (errno == EINTR || errno == EAGAIN || errno == EWOULDBLOCK) {
                    continue;
                }
                break;
            }

            pid_t pid = fork();
            if (pid == 0) {
                close(listen_fd);
                handle_pty_client(fd);
                close(fd);
                _exit(0);
            }
            close(fd);
        }

        close(listen_fd);
        sleep_us(VSOCK_RECONNECT_BACKOFF_INITIAL_US);
    }
}

/* Serve exec over vsock forever: (re)connect to the host, announce readiness,
 * then run VMM_EXEC: commands until the connection drops, and reconnect. Runs in
 * a forked child so the serial loop remains an independent fallback. If there is
 * no virtio-vsock device (older kernel/host), connect just keeps failing and
 * this backs off to 1 Hz while serial handles exec. */
static void serve_vsock_forever(void) {
    char line[LINE_MAX_LEN];
    unsigned int reconnect_backoff_us = VSOCK_RECONNECT_BACKOFF_INITIAL_US;
    for (;;) {
        int fd = vsock_connect_host();
        if (fd < 0) {
            sleep_us(reconnect_backoff_us);
            if (reconnect_backoff_us < VSOCK_RECONNECT_BACKOFF_MAX_US / 2U) {
                reconnect_backoff_us *= 2U;
            } else {
                reconnect_backoff_us = VSOCK_RECONNECT_BACKOFF_MAX_US;
            }
            continue;
        }
        reconnect_backoff_us = VSOCK_RECONNECT_BACKOFF_INITIAL_US;
        (void)serial_write(fd, "VMM_AGENT_READY\n", 16);
        for (;;) {
            int rc = read_line(fd, line, sizeof(line), true);
            if (rc < 0) {
                break; /* peer closed (e.g. after restore) -> reconnect */
            }
            if (rc > 0 || line[0] == '\0') {
                continue;
            }
            if (strncmp(line, EXEC_PREFIX, EXEC_PREFIX_LEN) == 0) {
                run_command(fd, line + EXEC_PREFIX_LEN);
            }
        }
        close(fd);
    }
}
#endif

int main(void) {
    signal(SIGPIPE, SIG_IGN);

    /* When the kernel execs us directly as PID 1 (init=/usr/sbin/vmm-agent on
     * an OCI-derived rootfs with no init system), bring up the pseudo-fs a real
     * init would before touching /dev. When started as a systemd service on a
     * distro rootfs we are not PID 1 and skip all of this. */
    bool is_init = (getpid() == 1);
    if (is_init) {
        setup_as_init();
    }

#ifdef __linux__
    /* Fork a dedicated vsock exec server; the parent keeps serving serial as a
     * fallback. The host uses whichever channel it opened, so only one runs a
     * given command. Fork a second vsock server for host-initiated PTY sessions
     * on port 1025, so interactive shells never affect exec/serial fallback. */
    pid_t vsock_pid = fork();
    if (vsock_pid == 0) {
        serve_vsock_forever();
        _exit(0);
    }
    pid_t pty_pid = fork();
    if (pty_pid == 0) {
        serve_pty_forever();
        _exit(0);
    }
#endif

    int serial_fd = open_serial();
    if (serial_fd < 0) {
        return 1;
    }

    /* Readiness banner: lets the host know the agent is up and listening
     * (the controller can wait for this before sending the first VMM_EXEC),
     * and doubles as a diagnostic that the agent started + serial output works. */
    (void)serial_write(serial_fd, "VMM_AGENT_READY\n", 16);

    char line[LINE_MAX_LEN];
    for (;;) {
        if (is_init) {
            reap_orphans();
        }
        int rc = read_line(serial_fd, line, sizeof(line), false);
        if (rc < 0) {
            close(serial_fd);
            sleep(1);
            serial_fd = open_serial();
            if (serial_fd < 0) {
                sleep(1);
            }
            continue;
        }
        if (rc > 0 || line[0] == '\0') {
            continue;
        }
        if (strncmp(line, EXEC_PREFIX, EXEC_PREFIX_LEN) == 0) {
            run_command(serial_fd, line + EXEC_PREFIX_LEN);
        }
    }
}
