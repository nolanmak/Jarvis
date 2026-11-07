/* Freestanding static probe image. This is not the V3 relay/status proxy. */
static long call(long number,long a,long b,long c) {
    long result;
    __asm__ volatile("syscall" : "=a"(result) : "a"(number),"D"(a),"S"(b),"d"(c)
        : "rcx","r11","memory");
    return result;
}
__attribute__((noreturn)) static void done(long status) {
    call(60,status,0,0);__builtin_unreachable();
}
static long descriptor(const char *value) {
    long result=0;
    if (!*value) done(125);
    for (;*value;++value) {
        if (*value<'0' || *value>'9' || result>6553) done(125);
        result=result*10+*value-'0';
    }
    if (result<3 || result>65535) done(125);
    return result;
}
__attribute__((used,noreturn)) void proxy_main(long *stack) {
    if (*stack!=5) done(125);
    char **argv=(char **)(stack+1);
    long ready=descriptor(argv[1]),hold=descriptor(argv[2]);
    long cloexec=descriptor(argv[3]),extra=descriptor(argv[4]);
    char token;
    if (call(72,cloexec,1,0)!=-9 || call(72,extra,1,0)<0
            || call(0,extra,(long)&token,1)!=1 || token!='+') done(125);
    static const char started[]="PROXY_READY\n", output[]="PROXY_OUTPUT\n",error[]="PROXY_ERROR\n";
    if (call(1,ready,(long)started,sizeof(started)-1)!=sizeof(started)-1) done(125);
    if (call(0,hold,(long)&token,1)!=1 || token!='x') done(125);
    if (call(1,1,(long)output,sizeof(output)-1)!=sizeof(output)-1
            || call(1,2,(long)error,sizeof(error)-1)!=sizeof(error)-1) done(125);
    done(23);
}
__asm__(".global _start\n_start:\nmov %rsp,%rdi\nandq $-16,%rsp\ncall proxy_main\nud2\n");
