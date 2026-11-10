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

int main(void) {
    assert(pipe(ready_pipe) == 0);
    struct agent_worker workers[] = {
        {.name = "exec", .serve = worker},
        {.name = "PTY", .serve = worker},
        {.name = "serial", .serve = worker},
    };
    const size_t count = sizeof(workers) / sizeof(workers[0]);
    for (size_t i = 0; i < count; ++i) {
        assert(worker_start(&workers[i], 1000, fork) == 1);
        await_ready(workers[i].pid);
        assert(worker_start(&workers[i], 1001, fail_fork) == 0);
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
        assert(worker_start(&workers[i], 1199, fail_fork) == 0);
        assert(worker_start(&workers[i], 1200, fork) == 1);
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
        assert(worker_start(&failing, now, fail_fork) == -1);
        assert(failing.pid == 0 && failing.retry_at_ms == now + expected);
        assert(worker_start(&failing, failing.retry_at_ms - 1, fail_fork) == 0);
        now = failing.retry_at_ms;
        expected = expected < WORKER_RETRY_MAX_MS / 2U ? expected * 2U : WORKER_RETRY_MAX_MS;
    }
    assert(fork_attempts == 12);
    assert(worker_start(&failing, now, fork) == 1);
    await_ready(failing.pid);
    assert(kill(failing.pid, SIGTERM) == 0);
    assert(waitpid(failing.pid, NULL, 0) == failing.pid);
    assert(waitpid(-1, NULL, WNOHANG) == -1 && errno == ECHILD);
    close(ready_pipe[0]);
    close(ready_pipe[1]);
    puts("worker supervisor: crash/restart, orphan isolation, fork failure/backoff, cleanup passed");
    return 0;
}
