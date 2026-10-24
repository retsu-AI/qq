#define _GNU_SOURCE
#include <errno.h>
#include <linux/keyctl.h>
#include <linux/fscrypt.h>
#include <fcntl.h>
#include <mqueue.h>
#include <sys/ioctl.h>
#include <sys/random.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/shm.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <unistd.h>

/* Synthetic secrets only. Mark shared memory for removal while attached, so
 * kernel cleanup also works when the test kills its fixture process. */
static const char secret[] = "qq-synthetic-kernel-secret";

static int seed(void) {
    int id = shmget(IPC_PRIVATE, 4096, IPC_CREAT | 0600);
    if (id < 0) { perror("shmget"); return 1; }
    char *memory = shmat(id, NULL, 0);
    if (memory == (void *)-1) { shmctl(id, IPC_RMID, NULL); return 1; }
    strcpy(memory, secret);
    if (shmctl(id, IPC_RMID, NULL) != 0) return 1;
    long key = 0;
    if (syscall(SYS_keyctl, KEYCTL_JOIN_SESSION_KEYRING, NULL) >= 0) {
        key = syscall(SYS_add_key, "user", "qq-synthetic-probe", secret,
                      sizeof(secret), KEY_SPEC_SESSION_KEYRING);
        if (key > 0) {
            syscall(SYS_keyctl, KEYCTL_SETPERM, key, 0x3f3f0000);
        } else { key = 0; }
    }
    printf("%ld %d\n", key, id);
    fflush(stdout);
    (void)getchar();
    if (key > 0) syscall(SYS_keyctl, KEYCTL_REVOKE, key);
    shmdt(memory);
    return 0;
}

static void report(const char *name, long value) {
    printf("%s errno=%d\n", name, value < 0 ? errno : 0);
}

int main(int argc, char **argv) {
    if (argc == 2 && strcmp(argv[1], "seed") == 0) return seed();
#ifdef __x86_64__
    if (argc == 2 && strcmp(argv[1], "compat") == 0) {
        long result;
        puts("compat-attempt");
        fflush(stdout);
        __asm__ volatile("int $0x80" : "=a"(result) : "0"(20) : "memory");
        printf("compat-returned %ld\n", result);
        return 0;
    }
#endif
    if (argc != 4) return 2;
    long key = strtol(argv[2], NULL, 10);
    int id = (int)strtol(argv[3], NULL, 10);
    char *memory = shmat(id, NULL, SHM_RDONLY);
    if (memory != (void *)-1) {
        printf("host-shm=%s\n", memory);
        shmdt(memory);
    }
    char value[128] = {0};
    if (key > 0 && syscall(SYS_keyctl, KEYCTL_READ, key, value, sizeof(value)) > 0)
        printf("host-key=%s\n", value);

    /* Invalid operations exercise the syscall filter without creating keys or
     * invoking any host request-key upcall. */
    report("keyctl", syscall(SYS_keyctl, 0x7fffffff, 0, 0, 0, 0));
    report("add_key", syscall(SYS_add_key, "", "", NULL, 0, 0));
    report("request_key", syscall(SYS_request_key, "", "", NULL, 0));
    int crypto = socket(AF_ALG, SOCK_SEQPACKET, 0);
    report("af-alg", crypto);
    if (crypto >= 0) close(crypto);
    int rxrpc = socket(AF_RXRPC, SOCK_DGRAM, AF_INET);
    report("af-rxrpc", rxrpc);
    if (rxrpc >= 0) close(rxrpc);
    report("bpf", syscall(SYS_bpf, -1, NULL, 0));
    report("io_uring_setup", syscall(SYS_io_uring_setup, 0, NULL));
    report("io_uring_enter", syscall(SYS_io_uring_enter, -1, 0, 0, 0, NULL, 0));
    report("io_uring_register", syscall(SYS_io_uring_register, -1, 0, NULL, 0));
    int ordinary = open("ioctl-probe", O_CREAT | O_RDWR, 0600);
    if (ordinary < 0) return 6;
    report("key-ioctl", ioctl(ordinary, FS_IOC_ADD_ENCRYPTION_KEY, NULL));
    close(ordinary);
    int channel[2], available = 0;
    if (pipe(channel) || write(channel[1], "x", 1) != 1 ||
        ioctl(channel[0], FIONREAD, &available) || available != 1) return 7;
    close(channel[0]); close(channel[1]);
    puts("ordinary-ioctl-works");
#ifdef __x86_64__
    const long x32 = 0x40000000;
    report("x32-keyctl", syscall(SYS_keyctl | x32, 0x7fffffff, 0, 0, 0, 0));
    report("x32-add_key", syscall(SYS_add_key | x32, "", "", NULL, 0, 0));
    report("x32-request_key", syscall(SYS_request_key | x32, "", "", NULL, 0));
    report("x32-af-alg", syscall(SYS_socket | x32, AF_ALG, SOCK_SEQPACKET, 0));
#endif
    int local = shmget(IPC_PRIVATE, 4096, IPC_CREAT | 0600);
    if (local < 0) return 3;
    memory = shmat(local, NULL, 0);
    if (memory == (void *)-1) { shmctl(local, IPC_RMID, NULL); return 4; }
    if (shmctl(local, IPC_RMID, NULL) != 0) return 5;
    strcpy(memory, "private-ipc-works");
    puts(memory);
    shmdt(memory);
    unsigned long nonce;
    if (getrandom(&nonce, sizeof(nonce), 0) != sizeof(nonce)) return 8;
    char name[80];
    snprintf(name, sizeof(name), "/qq-native-probe-%lx", nonce);
    struct mq_attr attr = { .mq_maxmsg = 1, .mq_msgsize = 128 };
    mqd_t mq = mq_open(name, O_CREAT | O_EXCL | O_RDWR, 0600, &attr);
    if (mq == (mqd_t)-1) { perror("mq_open"); return 9; }
    if (mq_unlink(name) != 0) { mq_close(mq); return 10; }
    if (mq_send(mq, "private-mq-works", sizeof("private-mq-works"), 0) != 0 ||
        mq_receive(mq, value, sizeof(value), NULL) < 0) { mq_close(mq); return 11; }
    puts(value);
    mq_close(mq);
    return 0;
}
