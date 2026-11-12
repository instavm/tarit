#ifndef VMM_WORKER_SUPERVISOR_H
#define VMM_WORKER_SUPERVISOR_H

#include <stdbool.h>
#include <stdint.h>
#include <stddef.h>
#include <signal.h>
#include <sys/types.h>
#include <unistd.h>

#define WORKER_RETRY_INITIAL_MS 100U
#define WORKER_RETRY_MAX_MS 5000U
#define WORKER_STABLE_MS 10000U

struct agent_worker {
    const char *name;
    void (*serve)(void);
    pid_t pid;
    uint64_t started_ms;
    uint64_t retry_at_ms;
    unsigned int retry_ms;
};

static inline void worker_retry(struct agent_worker *worker, uint64_t now) {
    unsigned int delay = worker->retry_ms ? worker->retry_ms : WORKER_RETRY_INITIAL_MS;
    worker->retry_at_ms = now + delay;
    worker->retry_ms = delay < WORKER_RETRY_MAX_MS / 2U ? delay * 2U : WORKER_RETRY_MAX_MS;
    worker->pid = 0;
}

/* The sole parent reaper calls this for every child, including adopted command
 * orphans. Never turn an unrelated command PID into a replacement service. */
static inline bool worker_reaped(struct agent_worker *workers, size_t count,
                                 pid_t pid, uint64_t now) {
    for (size_t i = 0; i < count; ++i) {
        if (workers[i].pid == pid && pid > 0) {
            if (now - workers[i].started_ms >= WORKER_STABLE_MS) {
                workers[i].retry_ms = WORKER_RETRY_INITIAL_MS;
            }
            worker_retry(&workers[i], now);
            return true;
        }
    }
    return false;
}

/* The signals the supervisor blocks and waits for. As PID 1 it leaves SIGTERM
 * and SIGINT alone: the kernel drops them for init, and taking one would let
 * init exit, which panics the guest. */
static inline void worker_wait_signals(sigset_t *set, bool is_init) {
    sigemptyset(set);
    sigaddset(set, SIGCHLD);
    if (!is_init) {
        sigaddset(set, SIGTERM);
        sigaddset(set, SIGINT);
    }
}

/* Milliseconds until the earliest pending restart, or -1 when every worker is
 * running: the supervisor then sleeps until a signal instead of polling. */
static inline int64_t worker_next_wait_ms(const struct agent_worker *workers,
                                          size_t count, uint64_t now) {
    int64_t wait = -1;
    for (size_t i = 0; i < count; ++i) {
        if (workers[i].pid > 0) {
            continue;
        }
        uint64_t due = workers[i].retry_at_ms > now ? workers[i].retry_at_ms - now : 0;
        if (wait < 0 || due < (uint64_t)wait) {
            wait = (int64_t)due;
        }
    }
    return wait;
}

/* fork_fn is injectable so a real fork failure can be tested without exhausting
 * the guest process limit. No threads are created before these forks. The
 * child installs child_mask, the mask from before the supervisor blocked its
 * wait signals, so workers and the commands they run never inherit them. */
static inline int worker_start(struct agent_worker *worker, uint64_t now,
                                pid_t (*fork_fn)(void), const sigset_t *child_mask) {
    if (worker->pid > 0 || now < worker->retry_at_ms) {
        return 0;
    }
    pid_t pid = fork_fn();
    if (pid < 0) {
        worker_retry(worker, now);
        return -1;
    }
    if (pid == 0) {
        if (sigprocmask(SIG_SETMASK, child_mask, NULL) < 0) {
            _exit(1);
        }
        worker->serve();
        _exit(1); /* These workers must never return normally. */
    }
    worker->pid = pid;
    worker->started_ms = now;
    return 1;
}
#endif
