#define _GNU_SOURCE
#include <errno.h>
#include <stddef.h>
#include <linux/audit.h>
#include <linux/filter.h>
#include <linux/seccomp.h>
#include <linux/sched.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <unistd.h>

/* Process-local primitives, not CI authorization. No constructor activates
 * either policy. The controller installs them at distinct execution boundaries.
 */
__attribute__((visibility("default"))) int jarvis_ci_deny_exec(void) {
#if defined(__linux__) && defined(__x86_64__)
    const struct sock_filter filter[] = {
        BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, arch)),
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, AUDIT_ARCH_X86_64, 1, 0),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_KILL_PROCESS),
        BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, nr)),
        BPF_JUMP(BPF_JMP | BPF_JSET | BPF_K, 0x40000000U, 0, 1),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_KILL_PROCESS),
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, SYS_execve, 0, 1),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ERRNO | EPERM),
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, SYS_execveat, 0, 1),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ERRNO | EPERM),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW)
    };
    struct sock_fprog program = {
        .len = (unsigned short)(sizeof(filter) / sizeof(filter[0])),
        .filter = (struct sock_filter *)filter
    };
    if (prctl(PR_SET_NO_NEW_PRIVS, 1UL, 0UL, 0UL, 0UL) != 0) return -1;
    long result = syscall(SYS_seccomp, SECCOMP_SET_MODE_FILTER,
                          SECCOMP_FILTER_FLAG_TSYNC, &program);
    if (result == 0) return 0;
    if (result > 0) errno = EBUSY; /* TSYNC can return an unsynchronized TID. */
    return -1;
#else
    errno = ENOSYS;
    return -1;
#endif
}

/* Install after trusted namespace setup, before the first untrusted exec.
 * Stopping every producer task is only a write barrier if kernel asynchronous
 * submissions cannot continue after the tasks stop. Namespace changes would
 * also invalidate the controller's namespace-based attribution. This policy
 * supplies those two prerequisites; it does not mediate the first exec or
 * establish complete input coverage.
 */
__attribute__((visibility("default"))) int jarvis_ci_restrict_unit(void) {
#if defined(__linux__) && defined(__x86_64__)
#define DENY_CALL(number) \
    BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, (number), 0, 1), \
    BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ERRNO | EPERM)
    const struct sock_filter filter[] = {
        BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, arch)),
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, AUDIT_ARCH_X86_64, 1, 0),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_KILL_PROCESS),
        BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, nr)),
        BPF_JUMP(BPF_JMP | BPF_JSET | BPF_K, 0x40000000U, 0, 1),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_KILL_PROCESS),
        DENY_CALL(SYS_unshare),
        DENY_CALL(SYS_setns),
        DENY_CALL(SYS_mount),
        DENY_CALL(SYS_umount2),
        DENY_CALL(SYS_pivot_root),
        DENY_CALL(SYS_chroot),
        DENY_CALL(SYS_open_tree),
        DENY_CALL(SYS_move_mount),
        DENY_CALL(SYS_fsopen),
        DENY_CALL(SYS_fsconfig),
        DENY_CALL(SYS_fsmount),
        DENY_CALL(SYS_fspick),
        DENY_CALL(SYS_mount_setattr),
        DENY_CALL(SYS_io_uring_setup),
        DENY_CALL(SYS_io_uring_register),
        DENY_CALL(SYS_io_uring_enter),
        DENY_CALL(SYS_io_setup),
        DENY_CALL(SYS_io_submit),
        DENY_CALL(SYS_io_cancel),
        DENY_CALL(SYS_io_getevents),
        DENY_CALL(SYS_io_pgetevents),
        DENY_CALL(SYS_io_destroy),
        /* clone3's flags live behind a mutable pointer. Refuse it with the
         * libc fallback errno, then inspect ordinary clone's value argument.
         */
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, SYS_clone3, 0, 1),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ERRNO | ENOSYS),
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, SYS_clone, 0, 7),
        BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, args[0]) + 4),
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, 0, 1, 0),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ERRNO | EPERM),
        BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, args[0])),
        BPF_JUMP(BPF_JMP | BPF_JSET | BPF_K,
                 CLONE_NEWNS | CLONE_NEWCGROUP | CLONE_NEWUTS | CLONE_NEWIPC |
                 CLONE_NEWUSER | CLONE_NEWPID | CLONE_NEWNET | CLONE_NEWTIME, 0, 1),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ERRNO | EPERM),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW)
    };
#undef DENY_CALL
    struct sock_fprog program = {
        .len = (unsigned short)(sizeof(filter) / sizeof(filter[0])),
        .filter = (struct sock_filter *)filter
    };
    if (prctl(PR_SET_NO_NEW_PRIVS, 1UL, 0UL, 0UL, 0UL) != 0) return -1;
    long result = syscall(SYS_seccomp, SECCOMP_SET_MODE_FILTER,
                          SECCOMP_FILTER_FLAG_TSYNC, &program);
    if (result == 0) return 0;
    if (result > 0) errno = EBUSY;
    return -1;
#else
    errno = ENOSYS;
    return -1;
#endif
}
