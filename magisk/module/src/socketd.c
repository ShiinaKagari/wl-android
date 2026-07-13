#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <signal.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <sys/poll.h>
#include <sys/stat.h>
#include <errno.h>

#define BUF_SIZE (4 * 1024 * 1024)

static int running = 1;
static void handle_signal(int s) { running = 0; }

int main(void) {
    const char *socket_path = getenv("LAND_SOCKET");
    if (!socket_path) socket_path = "/data/local/tmp/land.sock";

    signal(SIGINT, handle_signal);
    signal(SIGTERM, handle_signal);
    signal(SIGPIPE, SIG_IGN);

    unlink(socket_path);

    int listen_fd = socket(AF_UNIX, SOCK_STREAM, 0);
    if (listen_fd < 0) { perror("socket"); return 1; }

    struct sockaddr_un addr = { .sun_family = AF_UNIX };
    strncpy(addr.sun_path, socket_path, sizeof(addr.sun_path) - 1);

    if (bind(listen_fd, (struct sockaddr *)&addr, sizeof(addr)) < 0) {
        perror("bind"); close(listen_fd); return 1;
    }
    chmod(socket_path, 0666);
    listen(listen_fd, 3);

    fprintf(stderr, "[socketd] listening on %s\n", socket_path);

    int fds[2] = { -1, -1 };
    int count = 0;
    while (running && count < 2) {
        struct pollfd pfd = { .fd = listen_fd, .events = POLLIN };
        if (poll(&pfd, 1, 1000) <= 0) continue;
        int client = accept(listen_fd, NULL, NULL);
        if (client < 0) continue;
        fds[count++] = client;
    }
    close(listen_fd);
    if (count < 2) { for (int i = 0; i < count; i++) close(fds[i]); return 1; }

    char *buf0 = malloc(BUF_SIZE), *buf1 = malloc(BUF_SIZE);
    if (!buf0 || !buf1) { free(buf0); free(buf1); return 1; }

    struct pollfd pfds[2] = {
        { .fd = fds[0], .events = POLLIN },
        { .fd = fds[1], .events = POLLIN },
    };

    while (running) {
        int ret = poll(pfds, 2, 1000);
        if (ret <= 0) continue;
        for (int i = 0; i < 2; i++) {
            if (!(pfds[i].revents & POLLIN)) continue;
            int src = fds[i], dst = fds[1 - i];
            char *buf = (i == 0) ? buf0 : buf1;
            ssize_t n = read(src, buf, BUF_SIZE);
            if (n <= 0) { running = 0; break; }
            struct iovec iov = { .iov_base = buf, .iov_len = (size_t)n };
            struct msghdr msg = { .msg_iov = &iov, .msg_iovlen = 1 };
            sendmsg(dst, &msg, MSG_NOSIGNAL);
        }
    }

    free(buf0); free(buf1);
    for (int i = 0; i < 2; i++) close(fds[i]);
    unlink(socket_path);
    return 0;
}
