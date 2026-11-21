/* Untimed verification only. Run the packaged entry as the same uid and
 * propagate its exit status. ru_maxrss on Linux is in KiB; cgroup memory.peak
 * also accounts for memory charged to the group outside the child process. */
#define _DEFAULT_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <errno.h>
#include <sys/resource.h>
#include <sys/wait.h>
#include <unistd.h>

int main(int argc, char **argv) {
    if (argc < 2) return 2;
    pid_t pid = fork();
    if (pid == -1) { perror("fork"); return 1; }
    if (pid == 0) {
        execv(argv[1], argv + 1);
        perror("execv");
        _exit(127);
    }
    int status;
    struct rusage usage;
    while (wait4(pid, &status, 0, &usage) == -1) {
        if (errno == EINTR) continue;
        perror("wait4");
        return 1;
    }
#ifdef __linux__
    fprintf(stderr, "memory-budget: child peak RSS %ld KiB\n", usage.ru_maxrss);
    FILE *peak = fopen("/sys/fs/cgroup/memory.peak", "r");
    if (peak) {
        unsigned long long bytes;
        if (fscanf(peak, "%llu", &bytes) == 1)
            fprintf(stderr, "memory-budget: cgroup v2 peak %llu bytes (includes charged page cache)\n", bytes);
        fclose(peak);
    }
#endif
    return WIFEXITED(status) ? WEXITSTATUS(status) : (WIFSIGNALED(status) ? 128 + WTERMSIG(status) : 1);
}
