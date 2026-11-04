#define _GNU_SOURCE
#include <errno.h>
#include <sys/syscall.h>
#include <unistd.h>

/* A constructor that runs before application main or its relay constructor. */
__attribute__((constructor)) static void early_exec(void) {
    char *argv[] = {"true", NULL}, *environment[] = {NULL};
    if (syscall(SYS_execve, "/bin/true", argv, environment) != -1 || errno != EPERM) _exit(91);
    if (syscall(SYS_execveat, -100, "/bin/true", argv, environment, 0) != -1 || errno != EPERM) _exit(92);
    const char message[] = "EARLY_DENIED\n";
    if (write(1, message, sizeof(message) - 1) != sizeof(message) - 1) _exit(93);
}
