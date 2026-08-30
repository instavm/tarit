#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <dirent.h>
#include <arpa/inet.h>
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
#include <linux/random.h>
#include <linux/vm_sockets.h>
#include <poll.h>
#include <pty.h>
#include <sys/mount.h>
#include <sys/random.h>
#include <sys/sysmacros.h>
#include <sys/vfs.h>
#endif

#define EXEC_PREFIX "VMM_EXEC:"
#define EXEC_PREFIX_LEN 9
#define PROBE_PREFIX "VMM_PROBE:"
#define PROBE_PREFIX_LEN 10
#define REPAIR_NET_PREFIX "VMM_REPAIR_NET:"
#define REPAIR_NET_PREFIX_LEN 15

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
#define EXEC_FRAME_MAGIC "VEX2"
#define EXEC_FRAME_MAGIC_LEN 4U
#define EXEC_FRAME_VERSION 2U
#define EXEC_FRAME_REQUEST 1U
#define EXEC_FRAME_START 2U
#define EXEC_FRAME_STDOUT 3U
#define EXEC_FRAME_STDERR 4U
#define EXEC_FRAME_EXIT 5U
#define EXEC_FRAME_ERROR 6U
#define EXEC_FRAME_MAX_PAYLOAD (1024U * 1024U)
#define EXEC_STREAM_CHUNK_BYTES (16U * 1024U)
#define EXEC_PROTOCOL_LINE "VMM_VSOCK_EXEC_PROTO=2\n"
#define CLONE_REPAIR_PREFIX "__TARIT_CLONE_REPAIR_V2__"
#define CLONE_REPAIR_PREFIX_LEN (sizeof(CLONE_REPAIR_PREFIX) - 1U)
#define CLONE_REPAIR_SEED_BYTES 32U
#define CLONE_REPAIR_ID_BYTES 16U
#define CLONE_REPAIR_NONCE_BYTES (CLONE_REPAIR_SEED_BYTES + CLONE_REPAIR_ID_BYTES)
#define CLONE_REPAIR_OK "TARIT_CLONE_REPAIR_V2_OK"
#define POST_FORK_HOOK_PATH "/usr/libexec/tarit/post-fork"

/* A sane default PATH so commands (node, python, ...) resolve when we run as
 * init on an OCI-derived rootfs where no login shell exported one. */
#define DEFAULT_PATH "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"

static const char *json_value_for_key(const char *json, const char *key);
static bool json_get_u16(const char *json, const char *key, uint16_t *out);
#ifdef __linux__
static int read_line_with_prefix(int fd, const unsigned char *prefix, size_t prefix_len,
                                 char *line, size_t cap, bool ignore_overflow);
static int read_exact_fd(int fd, void *buf, size_t len);
#endif

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

#ifdef __linux__
static int hex_nibble(unsigned char value) {
    if (value >= '0' && value <= '9') return value - '0';
    if (value >= 'a' && value <= 'f') return value - 'a' + 10;
    if (value >= 'A' && value <= 'F') return value - 'A' + 10;
    return -1;
}

static int decode_clone_repair_nonce(const char *command,
                                     unsigned char nonce[CLONE_REPAIR_NONCE_BYTES]) {
    size_t prefix_len = strlen(CLONE_REPAIR_PREFIX);
    size_t expected_len = prefix_len + CLONE_REPAIR_NONCE_BYTES * 2U;
    if (prefix_len != CLONE_REPAIR_PREFIX_LEN || strlen(command) != expected_len ||
        memcmp(command, CLONE_REPAIR_PREFIX, prefix_len) != 0) {
        errno = EINVAL;
        return -1;
    }
    for (size_t i = 0; i < CLONE_REPAIR_NONCE_BYTES; i++) {
        int high = hex_nibble((unsigned char)command[prefix_len + i * 2U]);
        int low = hex_nibble((unsigned char)command[prefix_len + i * 2U + 1U]);
        if (high < 0 || low < 0) {
            errno = EINVAL;
            return -1;
        }
        nonce[i] = (unsigned char)((high << 4) | low);
    }
    return 0;
}

static int run_post_fork_hook(const char *clone_id) {
    struct stat st;
    int hook_fd = open(POST_FORK_HOOK_PATH, O_RDONLY | O_CLOEXEC | O_NOFOLLOW);
    if (hook_fd < 0) {
        return errno == ENOENT ? 0 : -1;
    }
    if (fstat(hook_fd, &st) < 0 || !S_ISREG(st.st_mode) || st.st_uid != 0 ||
        (st.st_mode & 0022) != 0 || (st.st_mode & 0111) == 0) {
        close(hook_fd);
        errno = EPERM;
        return -1;
    }
    if (fcntl(hook_fd, F_SETFD, 0) < 0) {
        close(hook_fd);
        return -1;
    }

    char fd_path[64];
    char clone_env[64];
    int fd_path_len = snprintf(fd_path, sizeof(fd_path), "/proc/self/fd/%d", hook_fd);
    int clone_env_len = snprintf(clone_env, sizeof(clone_env), "TARIT_CLONE_ID=%s", clone_id);
    if (fd_path_len <= 0 || (size_t)fd_path_len >= sizeof(fd_path) || clone_env_len != 47) {
        close(hook_fd);
        errno = EOVERFLOW;
        return -1;
    }
    char *const argv[] = { (char *)POST_FORK_HOOK_PATH, NULL };
    char *const envp[] = {
        clone_env,
        (char *)"TARIT_POST_FORK=1",
        (char *)"PATH=" DEFAULT_PATH,
        NULL,
    };

    pid_t child = fork();
    if (child < 0) {
        close(hook_fd);
        return -1;
    }
    if (child == 0) {
        int null_fd = open("/dev/null", O_RDWR);
        if (null_fd >= 0) {
            (void)dup2(null_fd, STDIN_FILENO);
            (void)dup2(null_fd, STDOUT_FILENO);
            (void)dup2(null_fd, STDERR_FILENO);
        }
        for (int fd = 3; fd < 1024; fd++) {
            if (fd != hook_fd) close(fd);
        }
        execve(fd_path, argv, envp);
        _exit(126);
    }
    close(hook_fd);

    int status;
    while (waitpid(child, &status, 0) < 0) {
        if (errno != EINTR) return -1;
    }
    if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) {
        errno = ECANCELED;
        return -1;
    }
    return 0;
}

static int repair_clone_entropy(const char *command, char clone_id[33]) {
    unsigned char nonce[CLONE_REPAIR_NONCE_BYTES];
    struct {
        int entropy_count;
        int buf_size;
        unsigned char buf[CLONE_REPAIR_SEED_BYTES];
    } pool;
    int random_fd = -1;
    int marker = -1;
    int boot_id_file = -1;
    int rc = -1;

    if (decode_clone_repair_nonce(command, nonce) < 0) {
        goto out;
    }

    pool.entropy_count = (int)(CLONE_REPAIR_SEED_BYTES * 8U);
    pool.buf_size = (int)CLONE_REPAIR_SEED_BYTES;
    memcpy(pool.buf, nonce, CLONE_REPAIR_SEED_BYTES);
    random_fd = open("/dev/random", O_RDWR | O_CLOEXEC);
    if (random_fd < 0 || ioctl(random_fd, RNDADDENTROPY, &pool) < 0 ||
        ioctl(random_fd, RNDRESEEDCRNG, 0) < 0) {
        goto out;
    }
    close(random_fd);
    random_fd = -1;

    /* The clone ID is a separate, non-secret part of the host nonce. Echoing
     * it proves that the exact repair request was consumed before admission. */
    for (size_t i = 0; i < CLONE_REPAIR_ID_BYTES; i++) {
        static const char hex[] = "0123456789abcdef";
        unsigned char value = nonce[CLONE_REPAIR_SEED_BYTES + i];
        clone_id[i * 2] = hex[value >> 4];
        clone_id[i * 2 + 1] = hex[value & 0x0fU];
    }
    clone_id[32] = '\0';

    if (mkdir("/run/tarit", 0700) < 0 && errno != EEXIST) {
        goto out;
    }
    marker = open("/run/tarit/clone-id", O_WRONLY | O_CREAT | O_TRUNC | O_CLOEXEC, 0600);
    if (marker < 0 || write_all(marker, clone_id, 32) < 0 || write_all(marker, "\n", 1) < 0) {
        goto out;
    }
    close(marker);
    marker = -1;

    char boot_id[38];
    int boot_id_len = snprintf(boot_id, sizeof(boot_id),
                               "%.8s-%.4s-%.4s-%.4s-%.12s\n", clone_id,
                               clone_id + 8, clone_id + 12, clone_id + 16,
                               clone_id + 20);
    if (boot_id_len != 37) {
        errno = EOVERFLOW;
        goto out;
    }
    boot_id_file = open("/run/tarit/boot-id", O_WRONLY | O_CREAT | O_TRUNC | O_CLOEXEC, 0444);
    if (boot_id_file < 0 || write_all(boot_id_file, boot_id, (size_t)boot_id_len) < 0 ||
        fsync(boot_id_file) < 0) {
        goto out;
    }
    close(boot_id_file);
    boot_id_file = -1;

    /* A kernel boot_id is immutable and remains captured in VM RAM. Overlay it
     * with this incarnation ID before admission. On descendants the existing
     * bind mount observes the rewritten tmpfs inode, so mounts do not stack. */
    char current_boot_id[38];
    int current = open("/proc/sys/kernel/random/boot_id", O_RDONLY | O_CLOEXEC);
    ssize_t current_len = current >= 0 ? read(current, current_boot_id, sizeof(current_boot_id)) : -1;
    if (current >= 0) close(current);
    if (current_len != boot_id_len || memcmp(current_boot_id, boot_id, (size_t)boot_id_len) != 0) {
        if (mount("/run/tarit/boot-id", "/proc/sys/kernel/random/boot_id", NULL, MS_BIND, NULL) < 0 ||
            mount(NULL, "/proc/sys/kernel/random/boot_id", NULL,
                  MS_BIND | MS_REMOUNT | MS_RDONLY, NULL) < 0) {
            goto out;
        }
    }
    if (run_post_fork_hook(clone_id) < 0) {
        goto out;
    }
    rc = 0;

out:
    {
        int saved_errno = errno;
        if (random_fd >= 0) close(random_fd);
        if (marker >= 0) close(marker);
        if (boot_id_file >= 0) close(boot_id_file);
        memset(nonce, 0, sizeof(nonce));
        memset(&pool, 0, sizeof(pool));
        errno = saved_errno;
    }
    return rc;
}
#endif

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

/* Some small kernels expose the block devices through sysfs but provide only
 * a plain tmpfs at /dev. devtmpfs and tmpfs have the same statfs magic, so the
 * fallback above cannot safely infer that device nodes were populated. Build
 * the missing block nodes from the kernel-owned sysfs class instead. */
static void ensure_block_nodes(void) {
    DIR *directory = opendir("/sys/class/block");
    if (directory == NULL) {
        return;
    }
    struct dirent *entry;
    while ((entry = readdir(directory)) != NULL) {
        const char *name = entry->d_name;
        if (name[0] == '.' || strchr(name, '/') != NULL || strlen(name) > 127U) {
            continue;
        }
        char sysfs_path[256];
        char device_path[256];
        if (snprintf(sysfs_path, sizeof(sysfs_path), "/sys/class/block/%s/dev", name) < 0 ||
            snprintf(device_path, sizeof(device_path), "/dev/%s", name) < 0) {
            continue;
        }
        FILE *device = fopen(sysfs_path, "re");
        if (device == NULL) {
            continue;
        }
        unsigned device_major = 0;
        unsigned device_minor = 0;
        int parsed = fscanf(device, "%u:%u", &device_major, &device_minor);
        fclose(device);
        if (parsed == 2) {
            ensure_node(device_path, S_IFBLK | 0600, device_major, device_minor);
        }
    }
    closedir(directory);
}

/* devtmpfs and tmpfs share TMPFS_MAGIC. At this early PID-1 boundary, an
 * existing tmpfs mount on /dev is the kernel's CONFIG_DEVTMPFS_MOUNT result;
 * the OCI rootfs cannot have established a mount before init runs. */
static bool dev_filesystem_is_mounted(void) {
    struct statfs state;
    return statfs("/dev", &state) == 0 &&
           (unsigned long)state.f_type == 0x01021994UL;
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
    if (mount("devtmpfs", "/dev", "devtmpfs", MS_NOSUID, "mode=0755") != 0 &&
        !dev_filesystem_is_mounted()) {
        mount_pseudo("tmpfs", "/dev", "tmpfs", MS_NOSUID, "mode=0755");
    }
    ensure_node("/dev/null", S_IFCHR | 0666, 1, 3);
    ensure_node("/dev/zero", S_IFCHR | 0666, 1, 5);
    ensure_node("/dev/full", S_IFCHR | 0666, 1, 7);
    ensure_node("/dev/random", S_IFCHR | 0666, 1, 8);
    ensure_node("/dev/urandom", S_IFCHR | 0666, 1, 9);
    ensure_node("/dev/tty", S_IFCHR | 0666, 5, 0);
    ensure_node("/dev/console", S_IFCHR | 0600, 5, 1);
    ensure_node("/dev/ttyS0", S_IFCHR | 0660, 4, 64);
    ensure_block_nodes();
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

#ifdef __linux__
static int read_line_with_prefix(int fd, const unsigned char *prefix, size_t prefix_len,
                                 char *line, size_t cap, bool ignore_overflow) {
    size_t len = 0;
    bool overflow = false;
    for (size_t i = 0; i < prefix_len; i++) {
        unsigned char c = prefix[i];
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
            line[len++] = (char)c;
        } else {
            overflow = true;
        }
    }

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
            return -1;
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
        if (overflow && !ignore_overflow) {
            return 1;
        }
    }
}
#endif

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

    /* An empty command is the transport-level readiness probe. It proves the
     * agent can receive and answer requests without requiring /bin/sh, which
     * intentionally does not exist in distroless OCI images. */
    if (command[0] == '\0') {
        serial_printf(serial_fd, "VMM_EXEC_EXIT=%d\n", 0);
        return;
    }
#ifdef __linux__
    if (strncmp(command, CLONE_REPAIR_PREFIX, CLONE_REPAIR_PREFIX_LEN) == 0) {
        char clone_id[33];
        if (repair_clone_entropy(command, clone_id) == 0) {
            serial_printf(serial_fd, "%s %s\n", CLONE_REPAIR_OK, clone_id);
            serial_printf(serial_fd, "VMM_EXEC_EXIT=%d\n", 0);
        } else {
            serial_printf(serial_fd, "vmm-agent: clone repair failed: %s\n", strerror(errno));
            serial_printf(serial_fd, "VMM_EXEC_EXIT=%d\n", 1);
        }
        return;
    }
#endif

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

static void run_probe(int serial_fd, const char *token) {
    if (strlen(token) != 16U) {
        return;
    }
    for (size_t i = 0; i < 16U; i++) {
        unsigned char c = (unsigned char)token[i];
        if (!((c >= '0' && c <= '9') || (c >= 'a' && c <= 'f'))) {
            return;
        }
    }
    serial_printf(serial_fd, "VMM_PROBE_OK:%s\n", token);
}

#ifdef __linux__
static void write_be64(unsigned char *out, uint64_t value) {
    for (int i = 7; i >= 0; i--) {
        out[i] = (unsigned char)(value & 0xffU);
        value >>= 8;
    }
}

static uint64_t read_be64(const unsigned char *in) {
    uint64_t value = 0;
    for (size_t i = 0; i < 8; i++) {
        value = (value << 8) | (uint64_t)in[i];
    }
    return value;
}

static int write_exec_frame(int fd, uint8_t kind, const void *payload, uint32_t len) {
    unsigned char header[10];
    if (len > EXEC_FRAME_MAX_PAYLOAD) {
        errno = EMSGSIZE;
        return -1;
    }
    memcpy(header, EXEC_FRAME_MAGIC, EXEC_FRAME_MAGIC_LEN);
    header[4] = EXEC_FRAME_VERSION;
    header[5] = kind;
    header[6] = (unsigned char)((len >> 24) & 0xffU);
    header[7] = (unsigned char)((len >> 16) & 0xffU);
    header[8] = (unsigned char)((len >> 8) & 0xffU);
    header[9] = (unsigned char)(len & 0xffU);
    if (write_all(fd, header, sizeof(header)) < 0) {
        return -1;
    }
    if (len > 0 && write_all(fd, payload, len) < 0) {
        return -1;
    }
    return 0;
}

static int send_exec_start(int fd, uint64_t request_id) {
    unsigned char payload[8];
    write_be64(payload, request_id);
    return write_exec_frame(fd, EXEC_FRAME_START, payload, sizeof(payload));
}

static int send_exec_chunk(int fd, uint8_t kind, uint64_t request_id, const void *bytes, uint32_t len) {
    unsigned char *payload = (unsigned char *)malloc(8U + len);
    if (payload == NULL) {
        return -1;
    }
    write_be64(payload, request_id);
    if (len > 0) {
        memcpy(payload + 8, bytes, len);
    }
    int rc = write_exec_frame(fd, kind, payload, 8U + len);
    free(payload);
    return rc;
}

static int send_exec_exit(int fd, uint64_t request_id, int exit_code) {
    unsigned char payload[12];
    write_be64(payload, request_id);
    payload[8] = (unsigned char)((exit_code >> 24) & 0xff);
    payload[9] = (unsigned char)((exit_code >> 16) & 0xff);
    payload[10] = (unsigned char)((exit_code >> 8) & 0xff);
    payload[11] = (unsigned char)(exit_code & 0xff);
    return write_exec_frame(fd, EXEC_FRAME_EXIT, payload, sizeof(payload));
}

static int read_exec_request_frame(int fd, const unsigned char magic[4], uint64_t *request_id, char **command_out) {
    unsigned char header[6];
    if (memcmp(magic, EXEC_FRAME_MAGIC, EXEC_FRAME_MAGIC_LEN) != 0) {
        errno = EPROTO;
        return -1;
    }
    if (read_exact_fd(fd, header, sizeof(header)) != 0) {
        return -1;
    }
    if (header[0] != EXEC_FRAME_VERSION || header[1] != EXEC_FRAME_REQUEST) {
        errno = EPROTO;
        return -1;
    }
    uint32_t len = ((uint32_t)header[2] << 24) | ((uint32_t)header[3] << 16) |
                   ((uint32_t)header[4] << 8) | (uint32_t)header[5];
    if (len < 12U || len > EXEC_FRAME_MAX_PAYLOAD) {
        errno = EMSGSIZE;
        return -1;
    }
    unsigned char *payload = (unsigned char *)malloc(len + 1U);
    if (payload == NULL) {
        return -1;
    }
    int rc = read_exact_fd(fd, payload, len);
    if (rc != 0) {
        free(payload);
        return -1;
    }
    *request_id = read_be64(payload);
    uint32_t command_len = ((uint32_t)payload[8] << 24) | ((uint32_t)payload[9] << 16) |
                           ((uint32_t)payload[10] << 8) | (uint32_t)payload[11];
    if (command_len != len - 12U) {
        free(payload);
        errno = EPROTO;
        return -1;
    }
    payload[len] = '\0';
    *command_out = (char *)payload;
    memmove(*command_out, payload + 12, command_len);
    (*command_out)[command_len] = '\0';
    return 0;
}

static void run_command_chunked(int fd, uint64_t request_id, const char *command) {
    int stdout_pipe[2] = { -1, -1 };
    int stderr_pipe[2] = { -1, -1 };
    if (send_exec_start(fd, request_id) < 0) {
        return;
    }
    /* Keep readiness independent of guest shell/userspace availability. */
    if (command[0] == '\0') {
        (void)send_exec_exit(fd, request_id, 0);
        return;
    }
    if (strncmp(command, CLONE_REPAIR_PREFIX, CLONE_REPAIR_PREFIX_LEN) == 0) {
        char clone_id[33];
        if (repair_clone_entropy(command, clone_id) == 0) {
            char response[96];
            int len = snprintf(response, sizeof(response), "%s %s\n", CLONE_REPAIR_OK, clone_id);
            if (len > 0 && (size_t)len < sizeof(response)) {
                (void)send_exec_chunk(fd, EXEC_FRAME_STDOUT, request_id, response, (uint32_t)len);
                (void)send_exec_exit(fd, request_id, 0);
                return;
            }
            errno = EOVERFLOW;
        }
        char message[160];
        int len = snprintf(message, sizeof(message), "vmm-agent: clone repair failed: %s\n",
                           strerror(errno));
        if (len > 0 && (size_t)len < sizeof(message)) {
            (void)send_exec_chunk(fd, EXEC_FRAME_STDERR, request_id, message, (uint32_t)len);
        }
        (void)send_exec_exit(fd, request_id, 1);
        return;
    }
    if (pipe(stdout_pipe) < 0 || pipe(stderr_pipe) < 0) {
        char message[128];
        snprintf(message, sizeof(message), "vmm-agent: pipe failed: %s", strerror(errno));
        (void)send_exec_chunk(fd, EXEC_FRAME_STDERR, request_id, message, (uint32_t)strlen(message));
        (void)send_exec_chunk(fd, EXEC_FRAME_STDERR, request_id, "\n", 1U);
        (void)send_exec_exit(fd, request_id, 127);
        if (stdout_pipe[0] >= 0) close(stdout_pipe[0]);
        if (stdout_pipe[1] >= 0) close(stdout_pipe[1]);
        if (stderr_pipe[0] >= 0) close(stderr_pipe[0]);
        if (stderr_pipe[1] >= 0) close(stderr_pipe[1]);
        return;
    }

    pid_t pid = fork();
    if (pid < 0) {
        char message[128];
        snprintf(message, sizeof(message), "vmm-agent: fork failed: %s", strerror(errno));
        (void)send_exec_chunk(fd, EXEC_FRAME_STDERR, request_id, message, (uint32_t)strlen(message));
        (void)send_exec_chunk(fd, EXEC_FRAME_STDERR, request_id, "\n", 1U);
        (void)send_exec_exit(fd, request_id, 127);
        close(stdout_pipe[0]);
        close(stdout_pipe[1]);
        close(stderr_pipe[0]);
        close(stderr_pipe[1]);
        return;
    }

    if (pid == 0) {
        close(stdout_pipe[0]);
        close(stderr_pipe[0]);
        if (dup2(stdout_pipe[1], STDOUT_FILENO) < 0 || dup2(stderr_pipe[1], STDERR_FILENO) < 0) {
            _exit(127);
        }
        close(stdout_pipe[1]);
        close(stderr_pipe[1]);
        execl("/bin/sh", "sh", "-c", command, (char *)NULL);
        _exit(127);
    }

    close(stdout_pipe[1]);
    close(stderr_pipe[1]);

    struct pollfd pfds[2];
    pfds[0].fd = stdout_pipe[0];
    pfds[0].events = POLLIN;
    pfds[0].revents = 0;
    pfds[1].fd = stderr_pipe[0];
    pfds[1].events = POLLIN;
    pfds[1].revents = 0;

    while (pfds[0].fd >= 0 || pfds[1].fd >= 0) {
        int prc = poll(pfds, 2, -1);
        if (prc < 0) {
            if (errno == EINTR) {
                continue;
            }
            break;
        }
        for (size_t i = 0; i < 2; i++) {
            if (pfds[i].fd < 0 || !(pfds[i].revents & (POLLIN | POLLHUP | POLLERR))) {
                continue;
            }
            char buf[EXEC_STREAM_CHUNK_BYTES];
            ssize_t n = read(pfds[i].fd, buf, sizeof(buf));
            if (n < 0) {
                if (errno == EINTR || errno == EAGAIN) {
                    continue;
                }
                close(pfds[i].fd);
                pfds[i].fd = -1;
                continue;
            }
            if (n == 0) {
                close(pfds[i].fd);
                pfds[i].fd = -1;
                continue;
            }
            (void)send_exec_chunk(fd, i == 0 ? EXEC_FRAME_STDOUT : EXEC_FRAME_STDERR,
                                  request_id, buf, (uint32_t)n);
        }
    }

    if (stdout_pipe[0] >= 0) {
        close(stdout_pipe[0]);
    }
    if (stderr_pipe[0] >= 0) {
        close(stderr_pipe[0]);
    }
    (void)send_exec_exit(fd, request_id, wait_for_child(pid));
}
#endif

static bool json_get_string_field(const char *json, const char *key, char *out, size_t cap) {
    const char *p = json_value_for_key(json, key);
    if (p == NULL || cap == 0 || *p != '"') {
        return false;
    }
    p++;
    size_t j = 0;
    while (*p != '\0' && *p != '"') {
        char ch = *p++;
        if (ch == '\\') {
            ch = *p++;
            switch (ch) {
            case '"':
            case '\\':
            case '/':
                break;
            case 'b':
                ch = '\b';
                break;
            case 'f':
                ch = '\f';
                break;
            case 'n':
                ch = '\n';
                break;
            case 'r':
                ch = '\r';
                break;
            case 't':
                ch = '\t';
                break;
            default:
                return false;
            }
        }
        if (j + 1 >= cap) {
            return false;
        }
        out[j++] = ch;
    }
    if (*p != '"') {
        return false;
    }
    out[j] = '\0';
    return j > 0;
}

static size_t json_get_string_array(const char *json, const char *key, char out[][64], size_t max_items) {
    const char *p = json_value_for_key(json, key);
    if (p == NULL || *p != '[') {
        return 0;
    }
    p++;
    size_t count = 0;
    for (;;) {
        while (*p == ' ' || *p == '\t' || *p == '\r' || *p == '\n') {
            p++;
        }
        if (*p == ']') {
            return count;
        }
        if (*p != '"' || count >= max_items) {
            return 0;
        }
        p++;
        size_t j = 0;
        while (*p != '\0' && *p != '"') {
            char ch = *p++;
            if (ch == '\\') {
                ch = *p++;
                if (!(ch == '"' || ch == '\\' || ch == '/')) {
                    return 0;
                }
            }
            if (j + 1 >= sizeof(out[0])) {
                return 0;
            }
            out[count][j++] = ch;
        }
        if (*p != '"') {
            return 0;
        }
        out[count][j] = '\0';
        count++;
        p++;
        while (*p == ' ' || *p == '\t' || *p == '\r' || *p == '\n') {
            p++;
        }
        if (*p == ',') {
            p++;
            continue;
        }
        if (*p == ']') {
            return count;
        }
        return 0;
    }
}

static int run_argv_and_stream_output(int serial_fd, char *const argv[]) {
    pid_t pid = fork();
    if (pid < 0) {
        serial_printf(serial_fd, "fork failed: %s\n", strerror(errno));
        return 127;
    }
    if (pid == 0) {
        if (dup2(serial_fd, STDOUT_FILENO) < 0 || dup2(serial_fd, STDERR_FILENO) < 0) {
            _exit(127);
        }
        execvp(argv[0], argv);
        _exit(127);
    }
    return wait_for_child(pid);
}

static int write_resolv_conf(char dns[][64], size_t dns_count) {
    int fd = open("/etc/resolv.conf", O_WRONLY | O_CREAT | O_TRUNC, 0644);
    if (fd < 0) {
        return -1;
    }
    for (size_t i = 0; i < dns_count; i++) {
        if (dprintf(fd, "nameserver %s\n", dns[i]) < 0) {
            close(fd);
            return -1;
        }
    }
    if (fsync(fd) < 0) {
        close(fd);
        return -1;
    }
    return close(fd);
}

static void run_guest_network_repair(int serial_fd, const char *json) {
    char addr[64];
    char gateway[64];
    char dns[4][64];
    uint16_t prefix_u16 = 0;
    size_t dns_count = 0;
    struct in_addr ignored;

    (void)serial_write(serial_fd, "VMM_REPAIR_NET_START\n", 21);
    if (!json_get_string_field(json, "addr", addr, sizeof(addr)) ||
        !json_get_u16(json, "prefix", &prefix_u16) ||
        !json_get_string_field(json, "gateway", gateway, sizeof(gateway))) {
        (void)serial_write(serial_fd, "invalid network repair payload\n", 31);
        serial_printf(serial_fd, "VMM_REPAIR_NET_EXIT=%d\n", 64);
        return;
    }
    if (prefix_u16 > 32U ||
        inet_pton(AF_INET, addr, &ignored) != 1 ||
        inet_pton(AF_INET, gateway, &ignored) != 1) {
        (void)serial_write(serial_fd, "invalid IPv4 network repair settings\n", 37);
        serial_printf(serial_fd, "VMM_REPAIR_NET_EXIT=%d\n", 65);
        return;
    }
    dns_count = json_get_string_array(json, "dns_servers", dns, 4);
    for (size_t i = 0; i < dns_count; i++) {
        if (inet_pton(AF_INET, dns[i], &ignored) != 1) {
            (void)serial_write(serial_fd, "invalid DNS server in network repair payload\n", 45);
            serial_printf(serial_fd, "VMM_REPAIR_NET_EXIT=%d\n", 66);
            return;
        }
    }

    char prefix[4];
    snprintf(prefix, sizeof(prefix), "%u", (unsigned)prefix_u16);
    char cidr[80];
    snprintf(cidr, sizeof(cidr), "%s/%s", addr, prefix);

    char *flush_argv[] = {"ip", "addr", "flush", "dev", "eth0", "scope", "global", NULL};
    int exit_code = run_argv_and_stream_output(serial_fd, flush_argv);
    if (exit_code != 0) {
        serial_printf(serial_fd, "VMM_REPAIR_NET_EXIT=%d\n", exit_code);
        return;
    }

    char *addr_argv[] = {"ip", "addr", "add", cidr, "dev", "eth0", NULL};
    exit_code = run_argv_and_stream_output(serial_fd, addr_argv);
    if (exit_code != 0) {
        serial_printf(serial_fd, "VMM_REPAIR_NET_EXIT=%d\n", exit_code);
        return;
    }

    char *link_argv[] = {"ip", "link", "set", "eth0", "up", NULL};
    exit_code = run_argv_and_stream_output(serial_fd, link_argv);
    if (exit_code != 0) {
        serial_printf(serial_fd, "VMM_REPAIR_NET_EXIT=%d\n", exit_code);
        return;
    }

    char *route_argv[] = {"ip", "route", "replace", "default", "via", gateway, NULL};
    exit_code = run_argv_and_stream_output(serial_fd, route_argv);
    if (exit_code != 0) {
        serial_printf(serial_fd, "VMM_REPAIR_NET_EXIT=%d\n", exit_code);
        return;
    }

    if (dns_count > 0 && write_resolv_conf(dns, dns_count) < 0) {
        serial_printf(serial_fd, "write /etc/resolv.conf failed: %s\n", strerror(errno));
        serial_printf(serial_fd, "VMM_REPAIR_NET_EXIT=%d\n", 74);
        return;
    }

    serial_printf(serial_fd, "VMM_REPAIR_NET_EXIT=%d\n", 0);
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
        if (shell != NULL && shell[0] != '\0' && strpbrk(shell, " \t;|&<>()$`\"'*?[]{}~#\\") != NULL) {
            /* Command line, not a bare program path: run via sh -c. */
            execl("/bin/sh", "sh", "-c", shell, (char *)NULL);
            _exit(127);
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
        (void)serial_write(fd, EXEC_PROTOCOL_LINE, sizeof(EXEC_PROTOCOL_LINE) - 1U);
        for (;;) {
            unsigned char prefix[EXEC_FRAME_MAGIC_LEN];
            int rc = read_exact_fd(fd, prefix, sizeof(prefix));
            if (rc < 0) {
                break; /* peer closed (e.g. after restore) -> reconnect */
            }
            if (rc > 0) {
                break;
            }
            if (memcmp(prefix, EXEC_FRAME_MAGIC, EXEC_FRAME_MAGIC_LEN) == 0) {
                uint64_t request_id = 0;
                char *command = NULL;
                if (read_exec_request_frame(fd, prefix, &request_id, &command) < 0) {
                    break;
                }
                run_command_chunked(fd, request_id, command);
                free(command);
                continue;
            }
            rc = read_line_with_prefix(fd, prefix, sizeof(prefix), line, sizeof(line), true);
            if (rc < 0) {
                break;
            }
            if (rc > 0 || line[0] == '\0') {
                continue;
            }
            if (strncmp(line, EXEC_PREFIX, EXEC_PREFIX_LEN) == 0) {
                run_command(fd, line + EXEC_PREFIX_LEN);
            } else if (strncmp(line, PROBE_PREFIX, PROBE_PREFIX_LEN) == 0) {
                run_probe(fd, line + PROBE_PREFIX_LEN);
            } else if (strncmp(line, REPAIR_NET_PREFIX, REPAIR_NET_PREFIX_LEN) == 0) {
                run_guest_network_repair(fd, line + REPAIR_NET_PREFIX_LEN);
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
        } else if (strncmp(line, PROBE_PREFIX, PROBE_PREFIX_LEN) == 0) {
            run_probe(serial_fd, line + PROBE_PREFIX_LEN);
        } else if (strncmp(line, REPAIR_NET_PREFIX, REPAIR_NET_PREFIX_LEN) == 0) {
            run_guest_network_repair(serial_fd, line + REPAIR_NET_PREFIX_LEN);
        }
    }
}
