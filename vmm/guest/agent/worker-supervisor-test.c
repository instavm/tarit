#define _POSIX_C_SOURCE 200809L
#include <assert.h>
#include <errno.h>
#include <poll.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/wait.h>
#include "worker-supervisor.h"

static int ready_pipe[2];
static unsigned int fork_attempts;
static sigset_t child_mask;

/* Reports whether a supervisor wait signal is still blocked in the worker. */
static void mask_worker(void) {
    sigset_t blocked;
    assert(sigprocmask(SIG_SETMASK, NULL, &blocked) == 0);
    int blocked_any = sigismember(&blocked, SIGCHLD) || sigismember(&blocked, SIGTERM) ||
                        sigismember(&blocked, SIGINT);
    assert(write(ready_pipe[1], &blocked_any, sizeof(blocked_any)) == sizeof(blocked_any));
    for (;;) pause();
}

static void worker(void) {
    pid_t pid = getpid();
    assert(write(ready_pipe[1], &pid, sizeof(pid)) == sizeof(pid));
    for (;;) pause();
}

static pid_t fail_fork(void) {
    ++fork_attempts;
    errno = EAGAIN;
    return -1;
}

static void await_ready(pid_t expected) {
    struct pollfd pfd = {.fd = ready_pipe[0], .events = POLLIN};
    assert(poll(&pfd, 1, 2000) == 1);
    pid_t actual;
    assert(read(ready_pipe[0], &actual, sizeof(actual)) == sizeof(actual));
    assert(actual == expected);
}

static void test_wait_signals_and_deadline(void) {
    /* As PID 1 SIGTERM/SIGINT must never be taken: init exiting panics the guest. */
    sigset_t set;
    worker_wait_signals(&set, true);
    assert(sigismember(&set, SIGCHLD) == 1);
    assert(sigismember(&set, SIGTERM) == 0 && sigismember(&set, SIGINT) == 0);
    worker_wait_signals(&set, false);
    assert(sigismember(&set, SIGCHLD) == 1);
    assert(sigismember(&set, SIGTERM) == 1 && sigismember(&set, SIGINT) == 1);

    /* All workers running: no timeout, so an idle supervisor never wakes. */
    struct agent_worker idle[] = {
        {.name = "a", .pid = 10},
        {.name = "b", .pid = 11},
    };
    assert(worker_next_wait_ms(idle, 2, 5000) == -1);
    /* A pending restart sets the timeout; an overdue one fires at once. */
    idle[1].pid = 0;
    idle[1].retry_at_ms = 5400;
    assert(worker_next_wait_ms(idle, 2, 5000) == 400);
    idle[0].pid = 0;
    idle[0].retry_at_ms = 5100;
    assert(worker_next_wait_ms(idle, 2, 5000) == 100);
    assert(worker_next_wait_ms(idle, 2, 6000) == 0);
}

/* Workers (and so every command they run) get the pre-supervisor signal mask,
 * not the supervisor's blocked wait signals. */
static void test_worker_mask_is_restored(void) {
    sigset_t wait_set;
    sigset_t saved;
    worker_wait_signals(&wait_set, false);
    assert(sigprocmask(SIG_BLOCK, &wait_set, &saved) == 0);
    struct agent_worker worker = {.name = "mask", .serve = mask_worker};
    assert(worker_start(&worker, 0, fork, &child_mask) == 1);
    struct pollfd pfd = {.fd = ready_pipe[0], .events = POLLIN};
    assert(poll(&pfd, 1, 2000) == 1);
    int blocked_any = 1;
    assert(read(ready_pipe[0], &blocked_any, sizeof(blocked_any)) == sizeof(blocked_any));
    assert(blocked_any == 0);
    assert(kill(worker.pid, SIGKILL) == 0);
    assert(waitpid(worker.pid, NULL, 0) == worker.pid);
    assert(sigprocmask(SIG_SETMASK, &saved, NULL) == 0);
}

int main(void) {
    assert(pipe(ready_pipe) == 0);
    /* Start from an empty mask so the worker mask check is exact. */
    assert(sigemptyset(&child_mask) == 0);
    assert(sigprocmask(SIG_SETMASK, &child_mask, NULL) == 0);
    test_wait_signals_and_deadline();
    test_worker_mask_is_restored();
    struct agent_worker workers[] = {
        {.name = "exec", .serve = worker},
        {.name = "PTY", .serve = worker},
        {.name = "serial", .serve = worker},
    };
    const size_t count = sizeof(workers) / sizeof(workers[0]);
    for (size_t i = 0; i < count; ++i) {
        assert(worker_start(&workers[i], 1000, fork, &child_mask) == 1);
        await_ready(workers[i].pid);
        assert(worker_start(&workers[i], 1001, fail_fork, &child_mask) == 0);
    }
    assert(fork_attempts == 0);
    /* Simultaneous worker deaths, as with guest kill -9 -1, all restart. */
    for (size_t i = 0; i < count; ++i) assert(kill(workers[i].pid, SIGKILL) == 0);
    for (size_t i = 0; i < count; ++i) {
        pid_t previous = workers[i].pid;
        int status;
        assert(waitpid(previous, &status, 0) == previous);
        assert(WIFSIGNALED(status) && WTERMSIG(status) == SIGKILL);
        assert(worker_reaped(workers, count, previous, 1100));
        assert(workers[i].pid == 0 && workers[i].retry_at_ms == 1200);
        assert(worker_start(&workers[i], 1199, fail_fork, &child_mask) == 0);
        assert(worker_start(&workers[i], 1200, fork, &child_mask) == 1);
        await_ready(workers[i].pid);
    }
    /* Reaping an adopted command must not create or retire a service slot. */
    pid_t orphan = fork();
    assert(orphan >= 0);
    if (orphan == 0) _exit(17);
    assert(waitpid(orphan, NULL, 0) == orphan);
    assert(!worker_reaped(workers, count, orphan, 1300));
    for (size_t i = 0; i < count; ++i) {
        assert(workers[i].pid > 0);
        assert(kill(workers[i].pid, SIGTERM) == 0);
        assert(waitpid(workers[i].pid, NULL, 0) == workers[i].pid);
        assert(worker_reaped(workers, count, workers[i].pid, 12000));
        assert(workers[i].retry_at_ms == 12100); /* stable lifetime resets delay */
    }
    assert(waitpid(-1, NULL, WNOHANG) == -1 && errno == ECHILD);
    /* Initial and repeated fork failures retain the slot, cap rate, retry. */
    struct agent_worker failing = {.name = "failed", .serve = worker};
    uint64_t now = 0;
    unsigned int expected = WORKER_RETRY_INITIAL_MS;
    for (unsigned int i = 0; i < 12; ++i) {
        assert(worker_start(&failing, now, fail_fork, &child_mask) == -1);
        assert(failing.pid == 0 && failing.retry_at_ms == now + expected);
        assert(worker_start(&failing, failing.retry_at_ms - 1, fail_fork, &child_mask) == 0);
        now = failing.retry_at_ms;
        expected = expected < WORKER_RETRY_MAX_MS / 2U ? expected * 2U : WORKER_RETRY_MAX_MS;
    }
    assert(fork_attempts == 12);
    assert(worker_start(&failing, now, fork, &child_mask) == 1);
    await_ready(failing.pid);
    assert(kill(failing.pid, SIGTERM) == 0);
    assert(waitpid(failing.pid, NULL, 0) == failing.pid);
    assert(waitpid(-1, NULL, WNOHANG) == -1 && errno == ECHILD);
    close(ready_pipe[0]);
    close(ready_pipe[1]);
    puts("worker supervisor: signal wait, PID 1 signals, worker mask, crash/restart, orphan isolation, fork failure/backoff, cleanup passed");
    return 0;
}
