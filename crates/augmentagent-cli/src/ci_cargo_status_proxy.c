/* V3 post-ready transport continuation, not an execution/admission grant.
 * Runtime admission must bind this image and capsule to its actual root-owned
 * staging operation before native exec. Capsule fields alone never do that.
 */
#define _GNU_SOURCE
#define JARVIS_CARGO_PROTOCOL_V3 1
#include <fcntl.h>
#include <limits.h>
#include <stddef.h>
#include <stdlib.h>
#include <sys/prctl.h>
#include <sys/stat.h>
#include <sys/un.h>
#include "ci_cargo_status_wire.h"

extern int jarvis_ci_deny_exec(void);
#define REQUIRED_SEALS (F_SEAL_WRITE|F_SEAL_GROW|F_SEAL_SHRINK|F_SEAL_SEAL)
struct continuation {
    char magic[8];
    uint32_t version;
    int32_t connection;
    uint32_t client_pid,client_uid,client_gid,server_pid,server_uid,server_gid;
    uint64_t cookie;
    int64_t deadline;
    uint64_t reserved;
};
_Static_assert(sizeof(struct continuation)==64,"continuation ABI");

int main(int argc,char **argv,char **environment) {
    if (argc!=2 || *environment || getuid()==0 || getuid()!=geteuid()) return 125;
    /* Signals remain pending until the channel is verified and all forwarding
     * handlers exist. The original exec launcher must also block this interval.
     */
    sigset_t blocked;
    sigfillset(&blocked);
    if (sigprocmask(SIG_SETMASK,&blocked,NULL)<0 || jarvis_ci_deny_exec()!=0
            || prctl(PR_SET_DUMPABLE,0L,0L,0L,0L)!=0) return 125;
    unsigned number=0;
    if (!*argv[1]) return 125;
    for (const char *p=argv[1];*p;++p) {
        if (*p<'0' || *p>'9' || number>6553) return 125;
        number=number*10+(unsigned)(*p-'0');
    }
    if (number<3 || number>65535) return 125;
    int descriptor=(int)number;
    struct stat info;
    struct continuation value;
    int seals=fcntl(descriptor,F_GET_SEALS);
    if (fstat(descriptor,&info)!=0 || !S_ISREG(info.st_mode) || info.st_nlink!=0
            || (info.st_uid!=0 && info.st_uid!=geteuid()) || info.st_size!=sizeof(value)
            || seals<0 || (seals&REQUIRED_SEALS)!=REQUIRED_SEALS
            || pread(descriptor,&value,sizeof(value),0)!=sizeof(value)) return 125;
    if (memcmp(value.magic,"JARVPXY3",8) || value.version!=1 || value.reserved
            || value.connection<3 || value.connection>65535 || value.connection==descriptor
            || value.client_pid!=(uint32_t)getpid() || value.client_uid!=(uint32_t)geteuid()
            || !value.client_gid || !value.server_pid || value.server_pid>INT_MAX
            || value.server_pid==value.client_pid || !value.cookie) return 125;
    uid_t ruid,euid,suid;gid_t rgid,egid,sgid;
    if (getresuid(&ruid,&euid,&suid) || getresgid(&rgid,&egid,&sgid)
            || ruid!=value.client_uid || euid!=value.client_uid || suid!=value.client_uid
            || rgid!=value.client_gid || egid!=value.client_gid || sgid!=value.client_gid) return 125;
    int64_t now=milliseconds();
    if (now<0 || value.deadline<=now || value.deadline-now>MAX_TIMEOUT_MS) return 125;
    struct ucred peer;uint64_t cookie;int type;
    socklen_t size=sizeof(peer);
    if (fstat(value.connection,&info)!=0 || !S_ISSOCK(info.st_mode)
            || getsockopt(value.connection,SOL_SOCKET,SO_PEERCRED,&peer,&size)<0 || size!=sizeof(peer)
            || peer.pid!=(pid_t)value.server_pid || peer.uid!=value.server_uid || peer.gid!=value.server_gid) return 125;
    size=sizeof(type);
    if (getsockopt(value.connection,SOL_SOCKET,SO_TYPE,&type,&size)<0
            || size!=sizeof(type) || type!=SOCK_SEQPACKET) return 125;
    size=sizeof(cookie);
    if (getsockopt(value.connection,SOL_SOCKET,SO_COOKIE,&cookie,&size)<0
            || size!=sizeof(cookie) || cookie!=value.cookie) return 125;
    struct sockaddr_un address;
    size=sizeof(address);
    if (getpeername(value.connection,(struct sockaddr *)&address,&size)<0
            || size<sizeof(address.sun_family) || address.sun_family!=AF_UNIX) return 125;
    close(descriptor);
    if (install_signal_proxy(value.connection)<0) return 125;
    status_proxy_wait(value.connection,value.deadline);
}
