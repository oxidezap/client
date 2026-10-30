// Give wait4 a child forked from a small process, so Python's resident memory
// before exec does not become the test executable's peak-RSS floor.
#define _DEFAULT_SOURCE
#include <stdio.h>
#include <sys/resource.h>
#include <sys/wait.h>
#include <unistd.h>
#include <errno.h>

int main(int argc, char **argv) {
    if (argc < 2) return 2;
    pid_t child = fork();
    if (child < 0) return 2;
    if (child == 0) {
        execv(argv[1], argv + 1);
        _exit(127);
    }
    int status;
    struct rusage usage;
    while (wait4(child, &status, 0, &usage) < 0) {
        if (errno != EINTR) return 2;
    }
    double cpu = usage.ru_utime.tv_sec + usage.ru_utime.tv_usec / 1e6
        + usage.ru_stime.tv_sec + usage.ru_stime.tv_usec / 1e6;
    fprintf(stderr, "attention child cpu_s=%.6f peak_rss_kib=%ld\n", cpu, usage.ru_maxrss);
    return WIFEXITED(status) ? WEXITSTATUS(status) : 1;
}
