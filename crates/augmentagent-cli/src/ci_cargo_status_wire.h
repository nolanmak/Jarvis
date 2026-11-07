/* Shared post-ready wire behavior. No execution/admission authority. */
#ifndef JARVIS_CARGO_STATUS_WIRE_H
#define JARVIS_CARGO_STATUS_WIRE_H
#include <errno.h>
#include <poll.h>
#include <signal.h>
#include <stdint.h>
#include <string.h>
#include <sys/socket.h>
#include <time.h>
#include <unistd.h>
#ifdef JARVIS_CARGO_PROTOCOL_V3
#define PROTOCOL_DIGIT '3'
#define RESPONSE_MAGIC "JARVRSP3"
#define ERROR_STAGE 4
#define FINISHED_STAGE 3
#define SIGNALED_STAGE 5
#define MAX_TIMEOUT_MS 900000U
#else
#define PROTOCOL_DIGIT '2'
#define RESPONSE_MAGIC "JARVRSP2"
#define ERROR_STAGE 3
#define FINISHED_STAGE 2
#define SIGNALED_STAGE 4
#define MAX_TIMEOUT_MS 60000U
#endif
struct response { char magic[8]; uint32_t stage, value; };
_Static_assert(sizeof(struct response)==16,"response ABI");
static volatile sig_atomic_t proxy_connection=-1;
static volatile sig_atomic_t proxy_signal_failure;
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
/* Staging-ready has already been consumed by the original caller. */
__attribute__((noreturn)) static void status_proxy_wait(int connection,int64_t deadline) {
    struct response response;
    if (receive_response(connection,deadline,&response)<0) _exit(125);
#ifdef JARVIS_CARGO_PROTOCOL_V3
    if (response.stage!=2 || response.value!=0) _exit(125);
    if (receive_response(connection,deadline,&response)<0) _exit(125);
#endif
    if (response.stage==SIGNALED_STAGE) {close(connection);exit_by_signal(response.value);}
    if (response.stage!=FINISHED_STAGE || response.value>255) _exit(125);
    close(connection);_exit((int)response.value);
}
#endif
