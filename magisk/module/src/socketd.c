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

#define SOCKET_PATH "/dev/socket/land.sock"
#define MAX_CLIENTS 2
#define BUF_SIZE (4 * 1024 * 1024)

static int running = 1;
static void handle_signal(int s) { running = 0; }

int main(void) {
    signal(SIGINT, handle_signal);
    signal(SIGTERM, handle_signal);
    signal(SIGPIPE, SIG_IGN);

    unlink(SOCKET_PATH);

    int listen_fd = socket(AF_UNIX, SOCK_STREAM, 0);
    if (listen_fd < 0) { perror("socket"); return 1; }

    struct sockaddr_un addr = { .sun_family = AF_UNIX };
    strncpy(addr.sun_path, SOCKET_PATH, sizeof(addr.sun_path) - 1);

    if (bind(listen_fd, (struct sockaddr *)&addr, sizeof(addr)) < 0) {
        perror("bind"); close(listen_fd); return 1;
    }
    chmod(SOCKET_PATH, 0666);
    listen(listen_fd, 3);

    fprintf(stderr, "[socketd] listening on %s\n", SOCKET_PATH);

    int fds[MAX_CLIENTS] = { -1, -1 };
    int count = 0;

    while (running && count < MAX_CLIENTS) {
        struct pollfd pfd = { .fd = listen_fd, .events = POLLIN };
        if (poll(&pfd, 1, 1000) <= 0) continue;
        int client = accept(listen_fd, NULL, NULL);
        if (client < 0) continue;
        fds[count++] = client;
    }

    close(listen_fd);
    if (count < MAX_CLIENTS) {
        for (int i = 0; i < count; i++) close(fds[i]);
        return 1;
    }

    char buf1[BUF_SIZE], buf2[BUF_SIZE];
    struct pollfd pfds[2] = {
        { .fd = fds[0], .events = POLLIN },
        { .fd = fds[1], .events = POLLIN },
    };

    while (running) {
        int ret = poll(pfds, 2, 1000);
        if (ret <= 0) continue;
        for (int i = 0; i < 2; i++) {
            if (pfds[i].revents & POLLIN) {
                int src = fds[i], dst = fds[1 - i];
                char *buf = (i == 0) ? buf1 : buf2;
                ssize_t n = read(src, buf, BUF_SIZE);
                if (n <= 0) { running = 0; break; }
                struct iovec iov = { .iov_base = buf, .iov_len = (size_t)n };
                struct msghdr msg = { .msg_iov = &iov, .msg_iovlen = 1 };
                sendmsg(dst, &msg, MSG_NOSIGNAL);
            }
        }
    }

    for (int i = 0; i < MAX_CLIENTS; i++) close(fds[i]);
    unlink(SOCKET_PATH);
    return 0;
}
