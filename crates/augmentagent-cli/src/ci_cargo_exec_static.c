/* Freestanding static ELF: no interpreter, libc or preload constructor. */
static long call(long number, long a, long b, long c, long d, long e) {
    register long r10 __asm__("r10") = d;
    register long r8 __asm__("r8") = e;
    long result;
    __asm__ volatile("syscall" : "=a"(result)
        : "a"(number), "D"(a), "S"(b), "d"(c), "r"(r10), "r"(r8)
        : "rcx", "r11", "memory");
    return result;
}
__attribute__((noreturn)) void _start(void) {
    static const char path[] = "/bin/true";
    static const char message[] = "STATIC_DENIED\n";
    const char *argv[] = {path, 0}, *environment[] = {0};
    long first = call(59, (long)path, (long)argv, (long)environment, 0, 0);
    long second = call(322, -100, (long)path, (long)argv, (long)environment, 0);
    long status = first == -1 && second == -1 ? 0 : 94; /* -EPERM */
    if (!status && call(1, 1, (long)message, sizeof(message) - 1, 0, 0) != sizeof(message) - 1) status = 95;
    call(60, status, 0, 0, 0, 0);
    __builtin_unreachable();
}
