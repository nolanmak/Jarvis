/* Combined Linux ABI conformance fixture, not a production admission path. */
#define _GNU_SOURCE
#include <errno.h>
#include <signal.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>
#include "ci_cargo_status_continuation.h"
extern int jarvis_ci_deny_exec(void);
static volatile int shared;
int main(int argc,char **argv) {
    if (argc!=8) return 110;
    int connection=atoi(argv[2]),release=atoi(argv[3]),cloexec=atoi(argv[6]);
    int64_t deadline=strtoll(argv[4],NULL,10);
    int image_fd=!strcmp(argv[5],"fd-finish") ? atoi(argv[1]) : -1;
    if (image_fd>=0 && (image_fd<3 || fcntl(image_fd,F_SETFD,FD_CLOEXEC)<0)) return 122;
    if (connection<3 || release<3 || cloexec<3 || atoi(argv[7])<3
            || fcntl(connection,F_SETFD,FD_CLOEXEC)<0 || fcntl(release,F_SETFD,FD_CLOEXEC)<0
            || fcntl(cloexec,F_SETFD,FD_CLOEXEC)<0) return 111;
    /* Same irreversible primitive installed by the production relay constructor.
     * Install after the initial fixture exec, before the actual vfork. */
    if (!strcmp(argv[5],"inherited-exec-deny") && jarvis_ci_deny_exec()!=0) return 121;
    pid_t parent=getpid();long child;
    __asm__ volatile("syscall" : "=a"(child) : "a"((long)SYS_vfork) : "rcx","r11","memory");
    if (child<0) return 112;
    if (!child) {
        shared=41;
        if (image_fd>=0) jarvis_ci_exec_status_proxy_fd(image_fd,connection,deadline);
        else jarvis_ci_exec_status_proxy(argv[1],connection,deadline);
        _exit((errno==EPERM || errno==ENOSYS) ? 93 : 94);
    }
    int32_t entered[]={0x4a415256,parent,(int32_t)child,shared};
    if (write(release,entered,sizeof(entered))!=sizeof(entered)) return 113;
    int status;
    if (waitpid((pid_t)child,&status,0)!=child) return 114;
    int32_t finished[]={0x52534c54,parent,(int32_t)child,status};
    if (write(release,finished,sizeof(finished))!=sizeof(finished)) return 115;
    if (shared!=41 || fcntl(connection,F_GETFD)!=FD_CLOEXEC || fcntl(cloexec,F_GETFD)!=FD_CLOEXEC) return 116;
    if (!strcmp(argv[5],"signal")) return WIFSIGNALED(status) && WTERMSIG(status)==SIGUSR1 ? 0 : 117;
    if (!strcmp(argv[5],"cancel-after-start")) return WIFSIGNALED(status) && WTERMSIG(status)==SIGTERM ? 0 : 118;
    if (!strcmp(argv[5],"wrong-image") || !strcmp(argv[5],"swapped-channel"))
        return WIFSIGNALED(status) && WTERMSIG(status)==SIGKILL ? 0 : 119;
    int expected=(!strcmp(argv[5],"cancel-before-exec") || !strcmp(argv[5],"inherited-exec-deny")) ? 93 : 23;
    return WIFEXITED(status) && WEXITSTATUS(status)==expected ? 0 : 120;
}
