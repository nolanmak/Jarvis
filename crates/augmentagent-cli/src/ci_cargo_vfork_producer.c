/* Linux x86-64 kernel feasibility fixture, never a production adapter.
 * Use the raw ABI so the deliberate shared-memory observation is not a claim
 * about portable POSIX vfork C semantics. No fork emulation is involved.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

static volatile int shared;
int main(int argc, char **argv) {
    if (argc!=8) return 110;
    int cloexec=atoi(argv[4]), release=atoi(argv[6]);
    if (cloexec<3 || release<3 || fcntl(cloexec,F_SETFD,FD_CLOEXEC)<0
            || fcntl(release,F_SETFD,FD_CLOEXEC)<0) return 111;
    char *arguments[]={argv[1],argv[2],argv[3],argv[4],argv[5],NULL};
    char *environment[]={NULL};
    pid_t parent=getpid();
    long child;
    __asm__ volatile("syscall" : "=a"(child) : "a"((long)SYS_vfork)
        : "rcx","r11","memory");
    if (child<0) return 112;
    if (!child) {
        shared=41;
        execve(argv[1],arguments,environment);
        _exit((errno==EPERM || errno==ENOSYS) ? 93 : 94);
    }
    int32_t entered[]={0x4a415256,(int32_t)parent,(int32_t)child,shared};
    if (write(release,entered,sizeof(entered))!=sizeof(entered)) return 113;
    int status;
    if (waitpid((pid_t)child,&status,0)!=child) return 114;
    int32_t finished[]={0x52534c54,(int32_t)parent,(int32_t)child,status};
    if (write(release,finished,sizeof(finished))!=sizeof(finished)) return 115;
    if (shared!=41 || fcntl(cloexec,F_GETFD)!=FD_CLOEXEC) return 116;
    if (!strcmp(argv[7],"signal")) return WIFSIGNALED(status) && WTERMSIG(status)==SIGTERM ? 0 : 117;
    if (!strcmp(argv[7],"wrong-image")) return WIFSIGNALED(status) && WTERMSIG(status)==SIGKILL ? 0 : 118;
    int expected=(!strcmp(argv[7],"cancel") || !strcmp(argv[7],"listener-loss")) ? 93 : 23;
    return WIFEXITED(status) && WEXITSTATUS(status)==expected ? 0 : 119;
}
