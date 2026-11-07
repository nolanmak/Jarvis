/* Transport data only. No execution/admission authority is represented here. */
#ifndef JARVIS_CI_STATUS_CONTINUATION_H
#define JARVIS_CI_STATUS_CONTINUATION_H
#include <stdint.h>
#include <fcntl.h>
#define JARVIS_STATUS_SEALS (F_SEAL_WRITE|F_SEAL_GROW|F_SEAL_SHRINK|F_SEAL_SEAL)
struct jarvis_ci_status_continuation {
    char magic[8];
    uint32_t version;
    int32_t connection;
    uint32_t client_pid,client_uid,client_gid,server_pid,server_uid,server_gid;
    uint64_t cookie;
    int64_t deadline;
    uint64_t reserved;
};
_Static_assert(sizeof(struct jarvis_ci_status_continuation)==64,"continuation ABI");
struct jarvis_ci_status_prepared { int config_fd,connection_fd; };
int jarvis_ci_prepare_status_continuation(int connection,int64_t deadline,struct jarvis_ci_status_prepared *result);
void jarvis_ci_close_status_continuation(struct jarvis_ci_status_prepared *prepared);
int jarvis_ci_exec_status_proxy(const char *image,int connection,int64_t deadline);
#endif
