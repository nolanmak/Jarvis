/* Allocation-free pre-exec preparation. It does not authorize native exec.
 * The unchanged original kernel listener must refuse unsupported callers.
 * Shared-mm safety and root live-admission proof remain caller requirements.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <limits.h>
#include <linux/memfd.h>
#include <stddef.h>
#include <signal.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/un.h>
#include <time.h>
#include <unistd.h>
#include "ci_cargo_status_continuation.h"

__attribute__((visibility("default"))) void jarvis_ci_close_status_continuation(struct jarvis_ci_status_prepared *value) {
    int saved=errno;
    if (value) {
        if (value->config_fd>=0) close(value->config_fd);
        if (value->connection_fd>=0 && value->connection_fd!=value->config_fd) close(value->connection_fd);
        value->config_fd=value->connection_fd=-1;
    }
    errno=saved;
}

__attribute__((visibility("default"))) int jarvis_ci_prepare_status_continuation(int connection,int64_t deadline,struct jarvis_ci_status_prepared *result) {
    if (!result) {errno=EINVAL;return -1;}
    result->config_fd=result->connection_fd=-1;
    if (connection<3 || connection>65535) {errno=EINVAL;return -1;}
    struct timespec now;
    if (clock_gettime(CLOCK_MONOTONIC,&now)!=0) return -1;
    int64_t current=(int64_t)now.tv_sec*1000+now.tv_nsec/1000000;
    if (deadline<=current || deadline-current>900000) {errno=ETIMEDOUT;return -1;}
    struct jarvis_ci_status_continuation value={.magic={'J','A','R','V','P','X','Y','3'},.version=1,.deadline=deadline};
    uid_t ruid,euid,suid;gid_t rgid,egid,sgid;
    if (getresuid(&ruid,&euid,&suid) || getresgid(&rgid,&egid,&sgid)) return -1;
    if (!ruid || !rgid || ruid!=euid || ruid!=suid || rgid!=egid || rgid!=sgid) {errno=EPERM;return -1;}
    value.client_pid=(uint32_t)getpid();value.client_uid=ruid;value.client_gid=rgid;
    struct ucred peer;int type;socklen_t size=sizeof(peer);
    if (getsockopt(connection,SOL_SOCKET,SO_PEERCRED,&peer,&size)!=0) return -1;
    if (size!=sizeof(peer) || peer.pid<=0 || peer.pid==getpid()) {errno=EPROTO;return -1;}
    value.server_pid=(uint32_t)peer.pid;value.server_uid=peer.uid;value.server_gid=peer.gid;
    size=sizeof(type);
    if (getsockopt(connection,SOL_SOCKET,SO_TYPE,&type,&size)!=0) return -1;
    if (size!=sizeof(type) || type!=SOCK_SEQPACKET) {errno=EPROTOTYPE;return -1;}
    struct sockaddr_un address;size=sizeof(address);
    if (getpeername(connection,(struct sockaddr *)&address,&size)!=0) return -1;
    if (size<sizeof(address.sun_family) || address.sun_family!=AF_UNIX) {errno=EPROTOTYPE;return -1;}
    size=sizeof(value.cookie);
    if (getsockopt(connection,SOL_SOCKET,SO_COOKIE,&value.cookie,&size)!=0) return -1;
    if (size!=sizeof(value.cookie) || !value.cookie) {errno=EPROTO;return -1;}
    result->connection_fd=fcntl(connection,F_DUPFD,3);
    if (result->connection_fd<0) return -1;
    if (result->connection_fd>65535) {errno=EMFILE;goto failed;}
    value.connection=result->connection_fd;
    int temporary=(int)syscall(SYS_memfd_create,"jarvis-status-continuation",MFD_CLOEXEC|MFD_ALLOW_SEALING);
    if (temporary<0) goto failed;
    result->config_fd=fcntl(temporary,F_DUPFD,3);
    int saved=errno;close(temporary);errno=saved;
    if (result->config_fd<0) goto failed;
    if (result->config_fd>65535) {errno=EMFILE;goto failed;}
    const unsigned char *cursor=(const void *)&value;size_t left=sizeof(value);
    while (left) {
        ssize_t written=write(result->config_fd,cursor,left);
        if (written<0 && errno==EINTR) continue;
        if (written<=0) {if (!written) errno=EIO;goto failed;}
        cursor+=written;left-=(size_t)written;
    }
    if (fcntl(result->config_fd,F_ADD_SEALS,JARVIS_STATUS_SEALS)<0) goto failed;
    return 0;
failed:
    jarvis_ci_close_status_continuation(result);return -1;
}

static int exec_status_proxy(const char *image,int image_fd,int connection,int64_t deadline) {
    int original_errno=errno;
    sigset_t blocked,original_mask;
    sigfillset(&blocked);
    if (sigprocmask(SIG_SETMASK,&blocked,&original_mask)<0) return -1;
    struct jarvis_ci_status_prepared prepared;
    if (jarvis_ci_prepare_status_continuation(connection,deadline,&prepared)<0) goto failed;
    char number[6];unsigned value=(unsigned)prepared.config_fd;size_t count=0;
    do {number[count++]=(char)('0'+value%10);value/=10;} while(value);
    for (size_t i=0;i<count/2;++i) {char c=number[i];number[i]=number[count-i-1];number[count-i-1]=c;}
    number[count]=0;
    char *arguments[]={(char *)image,number,NULL};char *environment[]={NULL};
    errno=original_errno;
    if (image_fd>=0) syscall(SYS_execveat,image_fd,"",arguments,environment,AT_EMPTY_PATH);
    else syscall(SYS_execve,image,arguments,environment);
    jarvis_ci_close_status_continuation(&prepared);
failed:;
    int saved=errno;
    if (sigprocmask(SIG_SETMASK,&original_mask,NULL)<0) return -1;
    errno=saved;return -1;
}

__attribute__((visibility("default"))) int jarvis_ci_exec_status_proxy(const char *image,int connection,int64_t deadline) {
    if (!image || !*image) {errno=EINVAL;return -1;}
    return exec_status_proxy(image,-1,connection,deadline);
}

__attribute__((visibility("default"))) int jarvis_ci_exec_status_proxy_fd(int image,int connection,int64_t deadline) {
    int original_errno=errno;
    if (image<3 || image>65535 || image==connection) {errno=EINVAL;return -1;}
    struct stat info;
    int flags=fcntl(image,F_GETFD),seals=fcntl(image,F_GET_SEALS);
    /* Shape checks only. The original root operation must still authenticate
     * this exact image and request before allowing the kernel notification.
     * The borrowed FD is never closed or changed, including on failed exec. */
    if (fstat(image,&info)<0) return -1;
    if (!S_ISREG(info.st_mode) || info.st_nlink!=0 || (info.st_mode&07777)!=0555
            || (info.st_uid!=0 && info.st_uid!=geteuid()) || info.st_size<64
            || info.st_size>2*1024*1024 || flags<0 || !(flags&FD_CLOEXEC)
            || seals<0 || (seals&(JARVIS_STATUS_SEALS|0x20))!=(JARVIS_STATUS_SEALS|0x20)) {
        errno=EPERM;return -1;
    }
    errno=original_errno;
    return exec_status_proxy("ci-cargo-status-proxy",image,connection,deadline);
}
