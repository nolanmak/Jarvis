#define _GNU_SOURCE
#include <errno.h>
#include <stddef.h>
#include <stdint.h>
#include <linux/audit.h>
#include <linux/filter.h>
#include <linux/seccomp.h>
#include <linux/sched.h>
#include <sys/prctl.h>
#include <sys/ioctl.h>
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
static int jarvis_ci_install_unit_policy(int listener) {
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
        /* A later notification filter with its own listener could supersede
         * the original notifier. Trusted setup creates that listener first.
         * Ordinary stricter filters (including the relay's exec denial) may
         * still be installed; they cannot relax these ERRNO restrictions.
         */
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, SYS_seccomp, 0, 4),
        BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, args[1])),
        BPF_JUMP(BPF_JMP | BPF_JSET | BPF_K, SECCOMP_FILTER_FLAG_NEW_LISTENER, 0, 1),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ERRNO | EPERM),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
        /* clone3's flags live behind a mutable pointer. Refuse it with the
         * libc fallback errno, then inspect ordinary clone's value argument.
         */
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, SYS_clone3, 0, 1),
        BPF_STMT(BPF_RET | BPF_K, (listener ? SECCOMP_RET_USER_NOTIF : SECCOMP_RET_ERRNO | ENOSYS)),
        /* The exec relay retains the original child as a status/signal
         * proxy. It cannot release a vfork parent's shared address space at
         * the delegated tool's exec boundary. Refuse that primitive before
         * creation; runtimes such as CPython use their ordinary fork fallback.
         * Separate processes sharing an mm are likewise unsupported. Ordinary
         * fork and CLONE_THREAD threads remain available under the same caps.
         */
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, SYS_vfork, 0, 1),
        BPF_STMT(BPF_RET | BPF_K, (listener ? SECCOMP_RET_USER_NOTIF : SECCOMP_RET_ERRNO | ENOSYS)),
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, SYS_clone, 0, 12),
        BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, args[0]) + 4),
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, 0, 1, 0),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ERRNO | EPERM),
        BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, args[0])),
        BPF_JUMP(BPF_JMP | BPF_JSET | BPF_K,
                 CLONE_NEWNS | CLONE_NEWCGROUP | CLONE_NEWUTS | CLONE_NEWIPC |
                 CLONE_NEWUSER | CLONE_NEWPID | CLONE_NEWNET | CLONE_NEWTIME, 0, 1),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ERRNO | EPERM),
        BPF_JUMP(BPF_JMP | BPF_JSET | BPF_K, CLONE_VFORK, 0, 1),
        BPF_STMT(BPF_RET | BPF_K, (listener ? SECCOMP_RET_USER_NOTIF : SECCOMP_RET_ERRNO | ENOSYS)),
        BPF_JUMP(BPF_JMP | BPF_JSET | BPF_K, CLONE_VM, 0, 2),
        BPF_JUMP(BPF_JMP | BPF_JSET | BPF_K, CLONE_THREAD, 1, 0),
        BPF_STMT(BPF_RET | BPF_K, (listener ? SECCOMP_RET_USER_NOTIF : SECCOMP_RET_ERRNO | ENOSYS)),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, SYS_execve, 0, 1),
        BPF_STMT(BPF_RET | BPF_K, listener ? SECCOMP_RET_USER_NOTIF : SECCOMP_RET_ALLOW),
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, SYS_execveat, 0, 1),
        BPF_STMT(BPF_RET | BPF_K, listener ? SECCOMP_RET_USER_NOTIF : SECCOMP_RET_ALLOW),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW)
    };
#undef DENY_CALL
    struct sock_fprog program = {
        .len = (unsigned short)(sizeof(filter) / sizeof(filter[0])),
        .filter = (struct sock_filter *)filter
    };
    if (prctl(PR_SET_NO_NEW_PRIVS, 1UL, 0UL, 0UL, 0UL) != 0) return -1;
    long result = syscall(SYS_seccomp, SECCOMP_SET_MODE_FILTER,
                          listener ? SECCOMP_FILTER_FLAG_NEW_LISTENER : SECCOMP_FILTER_FLAG_TSYNC, &program);
    if (listener) return (int)result;
    if (result == 0) return 0;
    if (result > 0) errno = EBUSY;
    return -1;
#else
    errno = ENOSYS;
    return -1;
#endif
}

__attribute__((visibility("default"))) int jarvis_ci_restrict_unit(void) {
    return jarvis_ci_install_unit_policy(0);
}

/* Feasibility primitives only. The caller owns the original single-threaded
 * bootstrap, authenticated listener transfer, initial-exec authorization,
 * PTRACE_EXEC acknowledgement, cancellation and original descendant cleanup.
 * CONTINUE is never an authorization decision for untrusted pointer arguments.
 */
__attribute__((visibility("default"))) int jarvis_ci_exec_listener(void) {
#if defined(__linux__) && defined(__x86_64__)
    struct seccomp_notif_sizes sizes = {0};
    if (syscall(SYS_seccomp, SECCOMP_GET_NOTIF_SIZES, 0, &sizes) != 0) return -1;
    if (sizes.seccomp_notif != sizeof(struct seccomp_notif) ||
        sizes.seccomp_notif_resp != sizeof(struct seccomp_notif_resp) ||
        sizes.seccomp_data != sizeof(struct seccomp_data)) { errno = ENOTSUP; return -1; }
    return jarvis_ci_install_unit_policy(1);
#else
    errno = ENOSYS; return -1;
#endif
}

__attribute__((visibility("default"))) int jarvis_ci_exec_receive(
        int listener, uint64_t *identity, uint32_t *pid, int32_t *number) {
#if defined(__linux__) && defined(__x86_64__)
    if (!identity || !pid || !number) { errno = EINVAL; return -1; }
    struct seccomp_notif request = {0};
    if (ioctl(listener, SECCOMP_IOCTL_NOTIF_RECV, &request) != 0) return -1;
    if (request.flags || request.data.arch != AUDIT_ARCH_X86_64 ||
        (request.data.nr != SYS_execve && request.data.nr != SYS_execveat)) {
        errno = EPROTO; return -1;
    }
    *identity = request.id; *pid = request.pid; *number = request.data.nr;
    return 0;
#else
    (void)listener; (void)identity; (void)pid; (void)number;
    errno = ENOSYS; return -1;
#endif
}

/* Values are copied from seccomp_data, never from tracee pointer arguments.
 * 1 = exec; 2 = intentional clone3 fallback; 3 = unsupported vfork;
 * 4 = unsupported separate-process shared mm. This is an observed request,
 * not a committed disposition until NOTIF_SEND succeeds.
 */
__attribute__((visibility("default"))) int jarvis_ci_policy_receive(
        int listener, uint64_t *identity, uint32_t *pid, int32_t *number,
        uint64_t *flags, int32_t *kind) {
#if defined(__linux__) && defined(__x86_64__)
    if (!identity || !pid || !number || !flags || !kind) { errno = EINVAL; return -1; }
    struct seccomp_notif request = {0};
    if (ioctl(listener, SECCOMP_IOCTL_NOTIF_RECV, &request) != 0) return -1;
    if (request.flags || request.data.arch != AUDIT_ARCH_X86_64) { errno = EPROTO; return -1; }
    int classification;
    uint64_t value = 0;
    switch (request.data.nr) {
        case SYS_execve: case SYS_execveat: classification = 1; break;
        case SYS_clone3: classification = 2; break; /* Do not dereference clone_args. */
        case SYS_vfork: classification = 3; break;
        case SYS_clone:
            value = request.data.args[0];
            if (value >> 32 || value & (CLONE_NEWNS | CLONE_NEWCGROUP | CLONE_NEWUTS |
                    CLONE_NEWIPC | CLONE_NEWUSER | CLONE_NEWPID | CLONE_NEWNET | CLONE_NEWTIME)) {
                errno = EPROTO; return -1;
            }
            if (value & CLONE_VFORK) classification = 3;
            else if ((value & CLONE_VM) && !(value & CLONE_THREAD)) classification = 4;
            else { errno = EPROTO; return -1; }
            break;
        default: errno = EPROTO; return -1;
    }
    *identity = request.id; *pid = request.pid; *number = request.data.nr;
    *flags = value; *kind = classification;
    return 0;
#else
    (void)listener; (void)identity; (void)pid; (void)number; (void)flags; (void)kind;
    errno = ENOSYS; return -1;
#endif
}

/* Process creation has no CONTINUE path. The original listener supplies the
 * same ENOSYS as the standalone guard, now with a checked kernel request ID.
 */
__attribute__((visibility("default"))) int jarvis_ci_policy_deny(
        int listener, uint64_t identity) {
#if defined(__linux__) && defined(__x86_64__)
    if (ioctl(listener, SECCOMP_IOCTL_NOTIF_ID_VALID, &identity) != 0) return -1;
    struct seccomp_notif_resp response = {.id = identity, .val = 0, .error = -ENOSYS, .flags = 0};
    return ioctl(listener, SECCOMP_IOCTL_NOTIF_SEND, &response);
#else
    (void)listener; (void)identity; errno = ENOSYS; return -1;
#endif
}

__attribute__((visibility("default"))) int jarvis_ci_exec_respond(
        int listener, uint64_t identity, int allow_initial) {
#if defined(__linux__) && defined(__x86_64__)
    if (allow_initial != 0 && allow_initial != 1) { errno = EINVAL; return -1; }
    if (ioctl(listener, SECCOMP_IOCTL_NOTIF_ID_VALID, &identity) != 0) return -1;
    struct seccomp_notif_resp response = {
        .id = identity, .val = 0, .error = allow_initial ? 0 : -EPERM,
        .flags = allow_initial ? SECCOMP_USER_NOTIF_FLAG_CONTINUE : 0
    };
    return ioctl(listener, SECCOMP_IOCTL_NOTIF_SEND, &response);
#else
    (void)listener; (void)identity; (void)allow_initial;
    errno = ENOSYS; return -1;
#endif
}
