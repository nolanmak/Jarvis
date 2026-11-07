#define _GNU_SOURCE
/* Disposable actual-VM fixture; never a controller or production launcher. */
#include <assert.h>
#include <fcntl.h>
#include <poll.h>
#include <signal.h>
#include <spawn.h>
#include <stdio.h>
#include <string.h>
#include <sys/resource.h>
#include <sys/wait.h>
#include <unistd.h>
extern char **environ;

static int run(const char *file, char *const argv[]) {
    pid_t child;
    int status;
    assert(posix_spawn(&child,file,NULL,NULL,argv,environ)==0);
    assert(waitpid(child,&status,0)==child);
    return status;
}

static void line(int fd, const char *expected) {
    char value[32];
    size_t position=0;
    do {
        struct pollfd ready={.fd=fd,.events=POLLIN};
        assert(poll(&ready,1,5000)>0);
        assert(position<sizeof(value)-1 && read(fd,value+position,1)==1);
    } while (value[position++]!='\n');
    value[position]=0;
    assert(strcmp(value,expected)==0);
}

int main(void) {
    struct rlimit no_core={0,0};
    assert(setrlimit(RLIMIT_CORE,&no_core)==0);
    sigset_t blocked;
    assert(sigemptyset(&blocked)==0 && sigaddset(&blocked,SIGUSR1)==0);
    assert(sigprocmask(SIG_BLOCK,&blocked,NULL)==0);
    assert(signal(SIGUSR2,SIG_IGN)!=SIG_ERR);
    char *state[]={"python3","-I","-c",
        "import signal;assert signal.SIGUSR1 in signal.pthread_sigmask(signal.SIG_BLOCK,[]);assert signal.getsignal(signal.SIGUSR2)==signal.SIG_IGN",NULL};
    int status=run("/usr/bin/python3",state);
    assert(WIFEXITED(status) && WEXITSTATUS(status)==0);
    char *ordinary[]={"sh","-c","exit 143",NULL};
    status=run("/bin/sh",ordinary);
    assert(WIFEXITED(status) && WEXITSTATUS(status)==143);
    char *terminated[]={"sh","-c","kill -TERM $$",NULL};
    status=run("/bin/sh",terminated);
    assert(WIFSIGNALED(status) && WTERMSIG(status)==SIGTERM);
    int output[2];
    assert(pipe2(output,O_CLOEXEC)==0);
    posix_spawn_file_actions_t actions;
    assert(posix_spawn_file_actions_init(&actions)==0);
    assert(posix_spawn_file_actions_adddup2(&actions,output[1],STDOUT_FILENO)==0);
    assert(posix_spawn_file_actions_addclose(&actions,output[0])==0);
    assert(posix_spawn_file_actions_addclose(&actions,output[1])==0);
    char *handled[]={"python3","-I","-c",
        "import signal,sys,time;signal.signal(signal.SIGUSR2,lambda *_:(print('HANDLED',flush=True),sys.exit(0)));print('READY',flush=True);time.sleep(30)",NULL};
    pid_t child;
    assert(posix_spawn(&child,"/usr/bin/python3",&actions,NULL,handled,environ)==0);
    assert(posix_spawn_file_actions_destroy(&actions)==0);
    close(output[1]);
    line(output[0],"READY\n");
    assert(kill(child,SIGUSR2)==0);
    line(output[0],"HANDLED\n");
    close(output[0]);
    assert(waitpid(child,&status,0)==child);
    assert(WIFEXITED(status) && WEXITSTATUS(status)==0);
    puts("RELAY_GUEST_SIGNALS_COMPLETE");
    return 0;
}
