typedef unsigned short u16;
typedef unsigned int u32;
typedef long i64;

#define SYS_read 0
#define SYS_write 1
#define SYS_close 3
#define SYS_nanosleep 35
#define SYS_socket 41
#define SYS_connect 42
#define SYS_accept 43
#define SYS_bind 49
#define SYS_listen 50
#define SYS_exit 60

#define AF_VSOCK 40
#define SOCK_STREAM 1
#define VMADDR_CID_HOST 2u
#define VMADDR_CID_ANY 0xffffffffu
#define HOST_PORT 5252u
#define GUEST_PORT 5253u

struct sockaddr_vm {
    u16 family;
    u16 reserved;
    u32 port;
    u32 cid;
    unsigned char rest[4];
};

struct timespec_k {
    i64 sec;
    i64 nsec;
};

static long sc2(long n, long a, long b) {
    long ret;
    __asm__ volatile("syscall" : "=a"(ret) : "a"(n), "D"(a), "S"(b) : "rcx", "r11", "memory");
    return ret;
}

static long sc3(long n, long a, long b, long c) {
    long ret;
    __asm__ volatile("syscall"
                     : "=a"(ret)
                     : "a"(n), "D"(a), "S"(b), "d"(c)
                     : "rcx", "r11", "memory");
    return ret;
}

static void sleep_ms(int ms) {
    struct timespec_k ts;
    ts.sec = ms / 1000;
    ts.nsec = (i64)(ms % 1000) * 1000000L;
    sc2(SYS_nanosleep, (long)&ts, 0);
}

static int write_all(int fd, const char *buf, int len) {
    int off = 0;
    while (off < len) {
        long n = sc3(SYS_write, fd, (long)(buf + off), (long)(len - off));
        if (n <= 0) {
            return -1;
        }
        off += (int)n;
    }
    return 0;
}

static int read_full(int fd, char *buf, int len) {
    int off = 0;
    while (off < len) {
        long n = sc3(SYS_read, fd, (long)(buf + off), (long)(len - off));
        if (n <= 0) {
            return -1;
        }
        off += (int)n;
    }
    return 0;
}

static void say(const char *s) {
    int len = 0;
    while (s[len] != 0) {
        len++;
    }
    write_all(1, s, len);
}

static void fail(void) {
    say("kitsune-vsock-fail\n");
    sc3(SYS_exit, 1, 0, 0);
    for (;;) {
    }
}

static void fill_addr(struct sockaddr_vm *addr, u32 cid, u32 port) {
    int i;
    unsigned char *raw = (unsigned char *)addr;
    for (i = 0; i < (int)sizeof(*addr); i++) {
        raw[i] = 0;
    }
    addr->family = AF_VSOCK;
    addr->port = port;
    addr->cid = cid;
}

static int open_sock(void) {
    long fd = sc3(SYS_socket, AF_VSOCK, SOCK_STREAM, 0);
    if (fd < 0) {
        return -1;
    }
    return (int)fd;
}

static int connect_host(void) {
    int attempt;
    for (attempt = 0; attempt < 50; attempt++) {
        int fd = open_sock();
        struct sockaddr_vm addr;
        long rc;
        if (fd < 0) {
            sleep_ms(100);
            continue;
        }
        fill_addr(&addr, VMADDR_CID_HOST, HOST_PORT);
        rc = sc3(SYS_connect, fd, (long)&addr, (long)sizeof(addr));
        if (rc == 0) {
            return fd;
        }
        sc3(SYS_close, fd, 0, 0);
        sleep_ms(100);
    }
    return -1;
}

static int listen_guest(void) {
    int fd = open_sock();
    struct sockaddr_vm addr;
    if (fd < 0) {
        return -1;
    }
    fill_addr(&addr, VMADDR_CID_ANY, GUEST_PORT);
    if (sc3(SYS_bind, fd, (long)&addr, (long)sizeof(addr)) < 0) {
        sc3(SYS_close, fd, 0, 0);
        return -1;
    }
    if (sc2(SYS_listen, fd, 1) < 0) {
        sc3(SYS_close, fd, 0, 0);
        return -1;
    }
    return fd;
}

void _start(void) {
    int fd;
    int client;
    char buf[16];
    struct sockaddr_vm peer;
    unsigned int peer_len = sizeof(peer);

    fd = connect_host();
    if (fd < 0) {
        fail();
    }
    if (write_all(fd, "guest-to-host\n", 14) < 0 || read_full(fd, buf, 9) < 0) {
        fail();
    }
    if (buf[0] != 'h' || buf[5] != 'a') {
        fail();
    }
    say("kitsune-vsock-tx-ok\n");
    sc3(SYS_close, fd, 0, 0);

    fd = listen_guest();
    if (fd < 0) {
        fail();
    }
    say("kitsune-vsock-listen\n");
    client = (int)sc3(SYS_accept, fd, (long)&peer, (long)&peer_len);
    if (client < 0) {
        fail();
    }
    if (read_full(client, buf, 10) < 0 || write_all(client, "guest-pong\n", 11) < 0) {
        fail();
    }
    say("kitsune-vsock-rx-ok\n");
    sc3(SYS_close, client, 0, 0);
    sc3(SYS_close, fd, 0, 0);
    sc3(SYS_exit, 0, 0, 0);
}
