#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <linux/memfd.h>
#include <poll.h>
#include <spawn.h>
#include <signal.h>
#include <sys/wait.h>
#include <stddef.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <sys/un.h>
#include <time.h>
#include <unistd.h>

/* Same-UID interface experiment, not credential or metadata isolation.
 * The controller-created configuration must be sealed. Fork-child exec hooks
 * use no allocation/dlsym; a missed hook stays denied by the kernel filter.
 */
extern int jarvis_ci_deny_exec(void);
extern char **environ;
#define MAX_BODY (8U * 1024U * 1024U)
#define MAX_STRING (1024U * 1024U)
#define SEALS (F_SEAL_WRITE | F_SEAL_GROW | F_SEAL_SHRINK | F_SEAL_SEAL)
#ifdef JARVIS_CARGO_PROTOCOL_V3
#define PROTOCOL_DIGIT '3'
#define CONFIG_MAGIC "JARVCFG3"
#define ARGUMENT_MAGIC "JARVARG3"
#define RESPONSE_MAGIC "JARVRSP3"
#define ERROR_STAGE 4
#define FINISHED_STAGE 3
#define SIGNALED_STAGE 5
#define MAX_TIMEOUT_MS 900000U
#else
#define PROTOCOL_DIGIT '2'
#define CONFIG_MAGIC "JARVCFG2"
#define ARGUMENT_MAGIC "JARVARG2"
#define RESPONSE_MAGIC "JARVRSP2"
#define ERROR_STAGE 3
#define FINISHED_STAGE 2
#define SIGNALED_STAGE 4
#define MAX_TIMEOUT_MS 60000U
#endif
struct configuration {
    char magic[8];
    uint32_t uid, pid, timeout_ms;
    unsigned char token[32];
    char path[108];
};
struct request {
    char magic[8];
    unsigned char token[32];
    uint32_t count;
    int32_t numbers[64];
};
struct response { char magic[8]; uint32_t stage, value; };
struct linux_dirent64 {
    uint64_t ino;
    int64_t off;
    unsigned short length;
    unsigned char type;
    char name[];
};
_Static_assert(sizeof(struct configuration) == 160, "configuration ABI");
_Static_assert(sizeof(struct request) == 300, "request ABI");
_Static_assert(sizeof(struct response) == 16, "response ABI");
static struct configuration config;
static int initialized;
static volatile sig_atomic_t proxy_connection = -1;
static volatile sig_atomic_t proxy_signal_failure;
struct signal_state { sigset_t mask; uint64_t ignored, blocked; };

static int capture_signals(struct signal_state *state) {
    state->ignored = state->blocked = 0;
    if (sigprocmask(SIG_SETMASK, NULL, &state->mask) < 0) return -1;
    for (int number=1; number<=64; ++number) {
        if (sigismember(&state->mask, number)==1) state->blocked |= UINT64_C(1) << (number-1);
        struct sigaction action;
        if (sigaction(number, NULL, &action)<0) {
            if (errno==EINVAL && (number==32 || number==33)) continue;
            return -1;
        }
        if (action.sa_handler==SIG_IGN) state->ignored |= UINT64_C(1) << (number-1);
    }
    uint64_t unsupported=(UINT64_C(1)<<8)|(UINT64_C(1)<<18)|(UINT64_C(1)<<31)|(UINT64_C(1)<<32);
    if ((state->ignored|state->blocked)&unsupported) {errno=ENOTSUP;return -1;}
    return 0;
}

static void forward_signal(int number, siginfo_t *information, void *context) {
    (void)context;
    int saved_errno=errno;
    /* A fault in this proxy is infrastructure failure, not a tool signal. */
    if (information && information->si_code>0
        && (number==SIGSEGV || number==SIGILL || number==SIGFPE || number==SIGBUS
            || number==SIGTRAP || number==SIGSYS)) _exit(125);
    struct response frame={.magic={'J','A','R','V','S','I','G',PROTOCOL_DIGIT},.stage=(uint32_t)number,.value=0};
    if (proxy_connection<0 || send(proxy_connection,&frame,sizeof(frame),MSG_DONTWAIT|MSG_NOSIGNAL)!=(ssize_t)sizeof(frame))
        proxy_signal_failure=1;
    errno=saved_errno;
}

static int install_signal_proxy(int connection) {
    proxy_connection=connection;
    proxy_signal_failure=0;
    struct sigaction action={.sa_sigaction=forward_signal,.sa_flags=SA_SIGINFO};
    sigfillset(&action.sa_mask);
    for (int number=1;number<=64;++number) {
        if (number==SIGKILL || number==SIGSTOP || number==32 || number==33) continue;
        if (sigaction(number,&action,NULL)<0) return -1;
    }
    sigset_t unblocked;
    sigemptyset(&unblocked);
    /* The actual tool owns its requested mask/dispositions. The proxy must
     * receive even initially ignored/blocked signals to route later changes. */
    return sigprocmask(SIG_SETMASK,&unblocked,NULL);
}

static void exit_by_signal(uint32_t number) {
    if (!number || number>64 || number==32 || number==33 || number==SIGSTOP
        || number==SIGTSTP || number==SIGTTIN || number==SIGTTOU || number==SIGCONT
        || number==SIGCHLD || number==SIGURG || number==SIGWINCH) _exit(125);
    sigset_t blocked, selected;
    sigfillset(&blocked);
    if (sigprocmask(SIG_SETMASK,&blocked,NULL)<0) _exit(125);
    struct sigaction action={.sa_handler=SIG_DFL};
    sigemptyset(&action.sa_mask);
    if (number!=SIGKILL && sigaction((int)number,&action,NULL)<0) _exit(125);
    sigemptyset(&selected);sigaddset(&selected,(int)number);
    if (sigprocmask(SIG_UNBLOCK,&selected,NULL)<0) _exit(125);
    kill(getpid(),(int)number);
    _exit(125);
}

static const char *environment_value(const char *name) {
    size_t length = strlen(name);
    for (char **entry = environ; entry && *entry; ++entry)
        if (strncmp(*entry, name, length) == 0 && (*entry)[length] == '=')
            return *entry + length + 1;
    return NULL;
}

__attribute__((constructor)) static void initialize_relay(void) {
    const char *value = environment_value("JARVIS_CARGO_RELAY_FD");
    if (geteuid() == 0 || getuid() != geteuid() || !value || !*value) _exit(125);
#ifdef JARVIS_CARGO_PROTOCOL_V3
    uid_t real_uid,effective_uid,saved_uid;
    gid_t real_gid,effective_gid,saved_gid;
    if (getresuid(&real_uid,&effective_uid,&saved_uid) || getresgid(&real_gid,&effective_gid,&saved_gid)
        || real_uid!=1001 || effective_uid!=1001 || saved_uid!=1001
        || real_gid!=1001 || effective_gid!=1001 || saved_gid!=1001 || getgroups(0,NULL)!=0) _exit(125);
#endif
    unsigned long number = 0;
    for (const char *digit = value; *digit; ++digit) {
        if (*digit < '0' || *digit > '9' || number > 6553) _exit(125);
        number = number * 10 + (unsigned)(*digit - '0');
    }
    if (number < 3 || number > 65535) _exit(125);
    int fd = (int)number;
    struct stat info;
    int seals = fcntl(fd, F_GET_SEALS);
    if (fstat(fd, &info) != 0 || !S_ISREG(info.st_mode) || info.st_size != sizeof(config)
        || seals < 0 || (seals & SEALS) != SEALS
        || pread(fd, &config, sizeof(config), 0) != sizeof(config)) _exit(125);
    close(fd);
    if (memcmp(config.magic, CONFIG_MAGIC, 8)
#ifdef JARVIS_CARGO_PROTOCOL_V3
        || config.uid!=0 || info.st_uid!=0
#else
        || config.uid != geteuid()
#endif
        || config.pid == 0 || config.pid > INT_MAX || config.timeout_ms < 1000
        || config.timeout_ms > MAX_TIMEOUT_MS || config.path[0] != '/'
        || !memchr(config.path, 0, sizeof(config.path))) _exit(125);
    if (jarvis_ci_deny_exec() != 0) _exit(125);
    initialized = 1;
}

static int write_bytes(int fd, const void *data, size_t length, uint32_t *total) {
    if (length > MAX_BODY - *total) { errno = E2BIG; return -1; }
    const unsigned char *cursor = data;
    size_t left = length;
    while (left) {
        ssize_t count = write(fd, cursor, left);
        if (count < 0 && errno == EINTR) continue;
        if (count <= 0) return -1;
        cursor += count;
        left -= (size_t)count;
    }
    *total += (uint32_t)length;
    return 0;
}
static int write_string(int fd, const char *value, uint32_t *total) {
    if (!value) { errno = EFAULT; return -1; }
    size_t length = strnlen(value, MAX_STRING + 1);
    if (length > MAX_STRING) { errno = E2BIG; return -1; }
    uint32_t size = (uint32_t)length;
    if (write_bytes(fd, &size, sizeof(size), total) < 0) return -1;
    return write_bytes(fd, value, length, total);
}
static int vector_length(char *const values[], uint32_t *result) {
    *result = 0;
    if (!values) return 0;
    while (values[*result]) {
        if (*result == 4096) { errno = E2BIG; return -1; }
        ++*result;
    }
    return 0;
}
static int payload_fd(const char *file, char *const argv[], char *const envp[], int lookup,
                       const struct signal_state *signals) {
    uint32_t argc, envc;
    if (vector_length(argv, &argc) < 0 || vector_length(envp, &envc) < 0) return -1;
    if (!argc) { errno = EINVAL; return -1; }
    char cwd[4096];
    if (syscall(SYS_getcwd, cwd, sizeof(cwd)) < 0) return -1;
    int fd = (int)syscall(SYS_memfd_create, "jarvis-relay-arguments", MFD_CLOEXEC | MFD_ALLOW_SEALING);
    if (fd < 0) return -1;
    uint32_t total = 0, header[3] = {(uint32_t)lookup, argc, envc};
    const char *path = lookup ? environment_value("PATH") : "";
    if (!path) path = "/bin:/usr/bin";
    uint64_t dispositions[2]={signals->ignored,signals->blocked};
    if (write_bytes(fd, ARGUMENT_MAGIC, 8, &total) < 0
        || write_bytes(fd, header, sizeof(header), &total) < 0
        || write_bytes(fd, dispositions, sizeof(dispositions), &total) < 0
        || write_string(fd, file, &total) < 0 || write_string(fd, cwd, &total) < 0
        || write_string(fd, path, &total) < 0) goto failed;
    for (uint32_t i = 0; i < argc; ++i)
        if (write_string(fd, argv[i], &total) < 0) goto failed;
    for (uint32_t i = 0; i < envc; ++i)
        if (write_string(fd, envp[i], &total) < 0) goto failed;
    if (fcntl(fd, F_ADD_SEALS, SEALS) < 0) goto failed;
    return fd;
failed:;
    int error = errno;
    close(fd);
    errno = error;
    return -1;
}

/* Enumerate the actual FD table. Only non-CLOEXEC FIFO extras are delegated.
 * After START, release CLOEXEC copies including Rust's exec-error pipe.
 */
static int walk_descriptors(int close_mode, int connection, int payload,
                            struct request *request, int passed[65]) {
    int directory = (int)syscall(SYS_openat, AT_FDCWD, "/proc/self/fd", O_RDONLY | O_DIRECTORY | O_CLOEXEC);
    if (directory < 0) return -1;
    union { uint64_t alignment; char bytes[4096]; } buffer;
    int result = -1;
    for (;;) {
        long length = syscall(SYS_getdents64, directory, buffer.bytes, sizeof(buffer.bytes));
        if (length < 0 && errno == EINTR) continue;
        if (length < 0) goto done;
        if (length == 0) { result = 0; goto done; }
        for (long offset = 0; offset < length;) {
            struct linux_dirent64 *entry = (void *)(buffer.bytes + offset);
            if (length - offset < 20 || entry->length < 20 || entry->length > length - offset) {
                errno = EIO; goto done;
            }
            size_t available = entry->length - offsetof(struct linux_dirent64, name);
            unsigned number = 0;
            int valid = 1, ended = 0;
            for (size_t i = 0; i < available; ++i) {
                unsigned char digit = (unsigned char)entry->name[i];
                if (!digit) { ended = 1; break; }
                if (digit < '0' || digit > '9' || number > (INT_MAX-9U)/10U) { valid = 0; break; }
                number = number*10U + digit-'0';
            }
            offset += entry->length;
            if (!valid || !ended || number < 3) continue;
            int fd = (int)number;
            if (fd == directory || fd == connection || fd == payload) continue;
            int flags = fcntl(fd, F_GETFD);
            if (flags < 0) goto done;
            if (close_mode) {
                if ((flags & FD_CLOEXEC) && close(fd) < 0) goto done;
                continue;
            }
            if (flags & FD_CLOEXEC) continue;
            struct stat info;
            if (fstat(fd, &info) < 0) goto done;
            if (!S_ISFIFO(info.st_mode) || fd > 65535 || request->count == 64) {
                errno = ENOTSUP; goto done;
            }
            request->numbers[request->count] = fd;
            passed[1 + request->count] = fd;
            ++request->count;
        }
    }
done:;
    int error = errno;
    close(directory);
    errno = error;
    return result;
}
static int64_t milliseconds(void) {
    struct timespec time;
    if (clock_gettime(CLOCK_MONOTONIC, &time) != 0) return -1;
    return (int64_t)time.tv_sec*1000 + time.tv_nsec/1000000;
}
static int receive_response(int connection, int64_t deadline, struct response *response) {
    for (;;) {
        if (proxy_signal_failure) {errno=EIO;return -1;}
        int64_t now = milliseconds();
        if (now < 0) return -1;
        if (now >= deadline) { errno = ETIMEDOUT; return -1; }
        struct pollfd descriptor = {.fd = connection, .events = POLLIN};
        int ready = poll(&descriptor, 1, (int)(deadline-now));
        if (ready < 0 && errno == EINTR) continue;
        if (ready <= 0) { if (!ready) errno = ETIMEDOUT; return -1; }
        struct iovec vector = {.iov_base = response, .iov_len = sizeof(*response)};
        struct msghdr message = {.msg_iov = &vector, .msg_iovlen = 1};
        ssize_t length = recvmsg(connection, &message, 0);
        if (length < 0 && errno == EINTR) continue;
        if (length != sizeof(*response) || message.msg_flags & (MSG_TRUNC | MSG_CTRUNC)
            || memcmp(response->magic, RESPONSE_MAGIC, 8)) { errno = EIO; return -1; }
        return 0;
    }
}
static int delegate_exec(const char *file, char *const argv[], char *const envp[], int lookup) {
    if (!initialized) { errno = EPERM; return -1; }
    struct signal_state signals;
    if (capture_signals(&signals)<0) return -1;
    int payload = payload_fd(file, argv, envp, lookup, &signals);
    if (payload < 0) return -1;
    int connection = socket(AF_UNIX, SOCK_SEQPACKET | SOCK_CLOEXEC, 0);
    if (connection < 0) { int error=errno; close(payload); errno=error; return -1; }
    int signals_blocked=0;
    sigset_t blocked;
    sigfillset(&blocked);
    if (sigprocmask(SIG_SETMASK,&blocked,NULL)<0) goto failed;
    signals_blocked=1;
    struct timeval timeout = {.tv_sec = config.timeout_ms/1000,
                              .tv_usec = (config.timeout_ms%1000)*1000};
    if (setsockopt(connection, SOL_SOCKET, SO_SNDTIMEO, &timeout, sizeof(timeout)) < 0) goto failed;
    struct sockaddr_un address = {.sun_family = AF_UNIX};
    memcpy(address.sun_path, config.path, sizeof(address.sun_path));
    if (connect(connection, (struct sockaddr *)&address, sizeof(address)) < 0) goto failed;
    struct ucred peer;
    socklen_t size = sizeof(peer);
    if (getsockopt(connection, SOL_SOCKET, SO_PEERCRED, &peer, &size) < 0) goto failed;
    if (size != sizeof(peer) || peer.uid != config.uid || peer.pid != (pid_t)config.pid) {
        errno = EPERM; goto failed;
    }
    struct request request = {.magic = {'J','A','R','V','R','E','Q',PROTOCOL_DIGIT}, .count = 3, .numbers = {0,1,2}};
    memcpy(request.token, config.token, sizeof(request.token));
    int passed[65] = {payload,0,1,2};
    if (walk_descriptors(0, connection, payload, &request, passed) < 0) goto failed;
    char ancillary[CMSG_SPACE(sizeof(passed))] = {0};
    struct iovec vector = {.iov_base = &request, .iov_len = sizeof(request)};
    struct msghdr message = {.msg_iov = &vector, .msg_iovlen = 1,
                             .msg_control = ancillary, .msg_controllen = CMSG_SPACE((request.count+1)*sizeof(int))};
    struct cmsghdr *rights = CMSG_FIRSTHDR(&message);
    rights->cmsg_level = SOL_SOCKET;
    rights->cmsg_type = SCM_RIGHTS;
    rights->cmsg_len = CMSG_LEN((request.count+1)*sizeof(int));
    memcpy(CMSG_DATA(rights), passed, (request.count+1)*sizeof(int));
    if (sendmsg(connection, &message, MSG_NOSIGNAL) != sizeof(request)) goto failed;
    close(payload); payload = -1;
    int64_t now = milliseconds();
    if (now < 0) goto failed;
    int64_t deadline = now + config.timeout_ms;
    struct response response;
    if (receive_response(connection, deadline, &response) < 0) goto failed;
    if (response.stage == ERROR_STAGE && response.value >= 1 && response.value <= 4095) {
        errno = (int)response.value; goto failed;
    }
    if (response.stage != 1 || response.value != 0) { errno = EIO; goto failed; }
    if (install_signal_proxy(connection)<0) _exit(125);
    if (walk_descriptors(1, connection, -1, NULL, NULL) < 0) _exit(125);
    if (receive_response(connection, deadline, &response) < 0) _exit(125);
#ifdef JARVIS_CARGO_PROTOCOL_V3
    /* Staging readiness closes Cargo's exec-start pipe so it can feed stdin.
     * Only the controller's later positive exec observation starts the tool. */
    if (response.stage!=2 || response.value!=0) _exit(125);
    if (receive_response(connection,deadline,&response)<0) _exit(125);
#endif
    if (response.stage==SIGNALED_STAGE) {close(connection);exit_by_signal(response.value);}
    if (response.stage != FINISHED_STAGE || response.value > 255) _exit(125);
    close(connection);
    _exit((int)response.value);
failed:;
    int error = errno;
    if (payload >= 0) close(payload);
    close(connection);
    if (signals_blocked) sigprocmask(SIG_SETMASK,&signals.mask,NULL);
    errno = error;
    return -1;
}
int execve(const char *file, char *const argv[], char *const envp[]) {
    return delegate_exec(file, argv, envp, 0);
}
int execv(const char *file, char *const argv[]) { return delegate_exec(file, argv, environ, 0); }
int execvp(const char *file, char *const argv[]) { return delegate_exec(file, argv, environ, 1); }
int execvpe(const char *file, char *const argv[], char *const envp[]) {
    return delegate_exec(file, argv, envp, 1);
}

/* Keep our own bounded action records through the public init/add/destroy
 * interface. No glibc-private action layout is decoded in the fork child. */
#define ACTION_MAGIC UINT64_C(0x4a41525641435431)
struct action { int kind, source, target; char *path; };
struct action_state { size_t count; struct action actions[64]; };
struct action_reference { uint64_t magic; struct action_state *state; };
_Static_assert(sizeof(posix_spawn_file_actions_t) >= sizeof(struct action_reference), "action handle ABI");
static struct action_state *actions_state(const posix_spawn_file_actions_t *actions) {
    struct action_reference reference;
    memcpy(&reference, actions, sizeof(reference));
    return reference.magic == ACTION_MAGIC ? reference.state : NULL;
}
int posix_spawn_file_actions_init(posix_spawn_file_actions_t *actions) {
    struct action_state *state = calloc(1, sizeof(*state));
    if (!state) return ENOMEM;
    struct action_reference reference = {.magic=ACTION_MAGIC, .state=state};
    memset(actions, 0, sizeof(*actions));
    memcpy(actions, &reference, sizeof(reference));
    return 0;
}
int posix_spawn_file_actions_destroy(posix_spawn_file_actions_t *actions) {
    struct action_state *state = actions_state(actions);
    if (!state) return EINVAL;
    for (size_t i=0; i<state->count; ++i) free(state->actions[i].path);
    free(state);
    memset(actions, 0, sizeof(*actions));
    return 0;
}
static int add_action(posix_spawn_file_actions_t *actions, int kind, int source, int target, const char *path) {
    struct action_state *state = actions_state(actions);
    if (!state || source < 0 || target < 0) return EINVAL;
    if (state->count == 64) return E2BIG;
    char *copy = NULL;
    if (path) {
        size_t length = strnlen(path, 4097);
        if (length > 4096) return ENAMETOOLONG;
        copy = strdup(path);
        if (!copy) return ENOMEM;
    }
    state->actions[state->count++] = (struct action){.kind=kind,.source=source,.target=target,.path=copy};
    return 0;
}
int posix_spawn_file_actions_addclose(posix_spawn_file_actions_t *actions, int fd) {
    return add_action(actions, 1, fd, 0, NULL);
}
int posix_spawn_file_actions_adddup2(posix_spawn_file_actions_t *actions, int source, int target) {
    return add_action(actions, 2, source, target, NULL);
}
int posix_spawn_file_actions_addchdir_np(posix_spawn_file_actions_t *actions, const char *path) {
    return add_action(actions, 3, 0, 0, path);
}
int posix_spawn_file_actions_addfchdir_np(posix_spawn_file_actions_t *actions, int fd) {
    return add_action(actions, 4, fd, 0, NULL);
}
/* Opaque/unimplemented operations never reach a native spawn fallback. */
int posix_spawn_file_actions_addopen(posix_spawn_file_actions_t *restrict actions,
        int fd, const char *restrict path, int flags, mode_t mode) {
    (void)actions; (void)fd; (void)path; (void)flags; (void)mode;
    return ENOTSUP;
}
int posix_spawn_file_actions_addclosefrom_np(posix_spawn_file_actions_t *actions, int from) {
    (void)actions; (void)from; return ENOTSUP;
}
int posix_spawn_file_actions_addtcsetpgrp_np(posix_spawn_file_actions_t *actions, int fd) {
    (void)actions; (void)fd; return ENOTSUP;
}
static int spawn_relay(pid_t *pid, const char *file, const posix_spawn_file_actions_t *actions,
                        const posix_spawnattr_t *attributes, char *const argv[], char *const envp[], int lookup) {
    struct action_state *state = actions ? actions_state(actions) : NULL;
    if (actions && !state) return EINVAL;
    short flags = 0;
    sigset_t mask, defaults;
    if (attributes) {
        int error = posix_spawnattr_getflags(attributes, &flags);
        if (error) return error;
        if (flags & ~(POSIX_SPAWN_SETSIGDEF | POSIX_SPAWN_SETSIGMASK | POSIX_SPAWN_USEVFORK)) return ENOTSUP;
        if ((flags & POSIX_SPAWN_SETSIGMASK) && (error=posix_spawnattr_getsigmask(attributes,&mask))) return error;
        if ((flags & POSIX_SPAWN_SETSIGDEF) && (error=posix_spawnattr_getsigdefault(attributes,&defaults))) return error;
    }
    int pipe[2];
    if (pipe2(pipe, O_CLOEXEC) < 0) return errno;
    int floor = pipe[0] > pipe[1] ? pipe[0] : pipe[1];
    if (state) for (size_t i=0; i<state->count; ++i) {
        if (state->actions[i].source > floor) floor=state->actions[i].source;
        if (state->actions[i].target > floor) floor=state->actions[i].target;
    }
    if (floor >= 65535) { close(pipe[0]); close(pipe[1]); return ENOTSUP; }
    int report = fcntl(pipe[1], F_DUPFD_CLOEXEC, floor+1);
    close(pipe[1]);
    if (report < 0) { int error=errno; close(pipe[0]); return error; }
    pid_t child = fork();
    if (child < 0) { int error=errno; close(pipe[0]); close(report); return error; }
    if (!child) {
        close(pipe[0]);
        if (flags & POSIX_SPAWN_SETSIGDEF) {
            struct sigaction action = {.sa_handler=SIG_DFL};
            sigemptyset(&action.sa_mask);
            for (int signal=1; signal<NSIG; ++signal)
                if (sigismember(&defaults,signal)==1 && sigaction(signal,&action,NULL)<0) goto child_failed;
        }
        if ((flags & POSIX_SPAWN_SETSIGMASK) && sigprocmask(SIG_SETMASK,&mask,NULL)<0) goto child_failed;
        if (state) for (size_t i=0; i<state->count; ++i) {
            struct action *action=&state->actions[i];
            if (action->kind==1) {
                if (close(action->source)<0 && errno!=EBADF) goto child_failed;
            } else if (action->kind==2) {
                if (dup2(action->source,action->target)<0) goto child_failed;
                if (fcntl(action->target,F_SETFD,0)<0) goto child_failed;
            } else if (action->kind==3) {
                if (chdir(action->path)<0) goto child_failed;
            } else if (action->kind==4) {
                if (fchdir(action->source)<0) goto child_failed;
            } else { errno=ENOTSUP; goto child_failed; }
        }
        delegate_exec(file,argv,envp,lookup);
child_failed:;
        int error=errno;
        const char *cursor=(const char *)&error;
        size_t left=sizeof(error);
        while (left) {
            ssize_t written=write(report,cursor,left);
            if (written<0 && errno==EINTR) continue;
            if (written<=0) break;
            cursor+=written; left-=(size_t)written;
        }
        _exit(127);
    }
    close(report);
    int error=0;
    char *cursor=(char *)&error;
    size_t received=0;
    for (;;) {
        ssize_t count=read(pipe[0],cursor+received,sizeof(error)-received);
        if (count<0 && errno==EINTR) continue;
        if (count<0) { error=EIO; break; }
        if (!count) break;
        received+=(size_t)count;
        if (received==sizeof(error)) break;
    }
    close(pipe[0]);
    if (received || error) {
        int status;
        while (waitpid(child,&status,0)<0 && errno==EINTR) {}
        return received==sizeof(error) && error>0 ? error : EIO;
    }
    *pid=child;
    return 0;
}
int posix_spawn(pid_t *restrict pid, const char *restrict file,
                const posix_spawn_file_actions_t *actions, const posix_spawnattr_t *restrict attributes,
                char *const argv[restrict], char *const envp[restrict]) {
    return spawn_relay(pid,file,actions,attributes,argv,envp,0);
}
int posix_spawnp(pid_t *restrict pid, const char *restrict file,
                 const posix_spawn_file_actions_t *actions, const posix_spawnattr_t *restrict attributes,
                 char *const argv[restrict], char *const envp[restrict]) {
    return spawn_relay(pid,file,actions,attributes,argv,envp,1);
}
