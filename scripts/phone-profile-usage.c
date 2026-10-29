#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <sys/resource.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>
int main(int argc, char **argv) {
    if (argc < 2) return 2;
    struct timespec a,b;
    clock_gettime(CLOCK_MONOTONIC,&a);
    pid_t pid=fork();
    if(pid<0) return 3;
    if(!pid){execv(argv[1],argv+1); _exit(127);}
    struct rusage u; int status;
    if(wait4(pid,&status,0,&u)<0) return 4;
    clock_gettime(CLOCK_MONOTONIC,&b);
    fprintf(stderr,"usage wall_ns=%lld cpu_us=%lld maxrss_kib=%ld\n",(long long)(b.tv_sec-a.tv_sec)*1000000000+b.tv_nsec-a.tv_nsec,(long long)(u.ru_utime.tv_sec+u.ru_stime.tv_sec)*1000000+u.ru_utime.tv_usec+u.ru_stime.tv_usec,u.ru_maxrss);
    return WIFEXITED(status)?WEXITSTATUS(status):5;
}
