#include "Server.hpp"

#include <hyprland/src/Compositor.hpp>

#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>

#include <cerrno>
#include <cstring>
#include <memory>

namespace World::Server {

namespace {

// A line longer than this without a newline is a broken app.
constexpr size_t kMaxLine = 64 * 1024;

// Unsent output above this means the app stopped reading.
constexpr size_t kMaxBacklog = 4 * 1024 * 1024;

struct SClient {
    int              fd     = -1;
    wl_event_source* source = nullptr;
    int              pid    = 0;
    std::string      in, out;
};

SHandlers                g_handlers;
std::string              g_path;
int                      g_listenFd     = -1;
wl_event_source*         g_listenSource = nullptr;
std::unique_ptr<SClient> g_client;
// A dropped app not yet told to the handlers.
wl_event_source* g_dropNotice = nullptr;

void onDropNotice(void*) {
    // An idle source is gone once it has run.
    g_dropNotice = nullptr;

    if (g_handlers.connection)
        g_handlers.connection(false);
}

// Tells the handlers now of an app dropped earlier, if one is waiting.
void tellDropped() {
    if (!g_dropNotice)
        return;
    wl_event_source_remove(g_dropNotice);
    onDropNotice(nullptr);
}

// Closes the app's connection. The handlers hear of it from an idle
// callback, not from here: a failed write drops the app from inside
// send(), which a handler may be calling in the middle of a loop over the
// very state its disconnect resets (the feed walking its windows).
void dropClient() {
    if (!g_client)
        return;

    auto client = std::move(g_client);

    // The loop polls a dup of the fd, so the source goes first; closing the
    // fd does not remove it.
    if (client->source)
        wl_event_source_remove(client->source);
    if (client->fd >= 0)
        close(client->fd);

    if (!g_dropNotice)
        g_dropNotice = wl_event_loop_add_idle(g_pCompositor->m_wlEventLoop, onDropNotice, nullptr);
    if (!g_dropNotice && g_handlers.connection)
        g_handlers.connection(false);
}

void updateMask() {
    if (g_client && g_client->source)
        wl_event_source_fd_update(g_client->source,
                                  WL_EVENT_READABLE | (g_client->out.empty() ? 0 : WL_EVENT_WRITABLE));
}

void flush() {
    while (g_client && !g_client->out.empty()) {
        const ssize_t n = ::send(g_client->fd, g_client->out.data(), g_client->out.size(), MSG_NOSIGNAL);

        if (n > 0) {
            g_client->out.erase(0, static_cast<size_t>(n));
            continue;
        }
        if (n < 0 && errno == EINTR)
            continue;
        if (n < 0 && (errno == EAGAIN || errno == EWOULDBLOCK))
            break;

        dropClient();
        return;
    }

    updateMask();
}

// Hands every complete line to the handler. Lines are copied out first: a
// handler may queue output or drop the client, and the buffer goes with it.
void dispatchLines(SClient* client) {
    size_t start = 0;

    while (g_client.get() == client) {
        const size_t newline = client->in.find('\n', start);
        if (newline == std::string::npos)
            break;

        std::string line = client->in.substr(start, newline - start);
        start            = newline + 1;

        if (!line.empty() && line.back() == '\r')
            line.pop_back();

        if (g_handlers.line && !line.empty())
            g_handlers.line(line);
    }

    if (g_client.get() != client)
        return;

    client->in.erase(0, start);

    if (client->in.size() > kMaxLine)
        dropClient();
}

int onClient(int fd, uint32_t mask, void*) {
    try {
        SClient* client = g_client.get();
        if (!client || client->fd != fd)
            return 0;

        if (mask & WL_EVENT_READABLE) {
            char buffer[4096];

            // Bounded: a flooding app cannot starve the compositor.
            for (int i = 0; i < 16; ++i) {
                const ssize_t n = recv(fd, buffer, sizeof(buffer), 0);

                if (n > 0) {
                    client->in.append(buffer, static_cast<size_t>(n));
                    continue;
                }
                if (n == 0) {
                    dispatchLines(client);
                    if (g_client.get() == client)
                        dropClient();
                    return 0;
                }
                if (errno == EINTR)
                    continue;
                if (errno == EAGAIN || errno == EWOULDBLOCK)
                    break;

                dropClient();
                return 0;
            }

            dispatchLines(client);
        }

        if (g_client.get() != client)
            return 0;

        if (mask & (WL_EVENT_HANGUP | WL_EVENT_ERROR)) {
            dropClient();
            return 0;
        }

        if (mask & WL_EVENT_WRITABLE)
            flush();
    } catch (...) { dropClient(); }

    return 0;
}

int onListen(int, uint32_t mask, void*) {
    try {
        if (mask & (WL_EVENT_HANGUP | WL_EVENT_ERROR))
            return 0;

        const int fd = accept4(g_listenFd, nullptr, nullptr, SOCK_NONBLOCK | SOCK_CLOEXEC);
        if (fd < 0)
            return 0;

        dropClient(); // the newest app wins
        // The handlers hear of the old app going before the new one
        // comes, and while no app is connected.
        tellDropped();

        auto client    = std::make_unique<SClient>();
        client->fd     = fd;
        client->source = wl_event_loop_add_fd(g_pCompositor->m_wlEventLoop, fd, WL_EVENT_READABLE, onClient, nullptr);

        ucred     credentials{};
        socklen_t length = sizeof(credentials);
        if (getsockopt(fd, SOL_SOCKET, SO_PEERCRED, &credentials, &length) == 0)
            client->pid = credentials.pid;

        if (!client->source) {
            close(fd);
            return 0;
        }

        g_client = std::move(client);

        if (g_handlers.connection)
            g_handlers.connection(true);
    } catch (...) {}

    return 0;
}

} // namespace

bool start(const std::string& path, SHandlers handlers) {
    g_handlers = handlers;
    g_path     = path;

    sockaddr_un address{};
    address.sun_family = AF_UNIX;
    if (path.size() >= sizeof(address.sun_path))
        return false;
    std::memcpy(address.sun_path, path.c_str(), path.size() + 1);

    g_listenFd = socket(AF_UNIX, SOCK_STREAM | SOCK_NONBLOCK | SOCK_CLOEXEC, 0);
    if (g_listenFd < 0)
        return false;

    // A socket file left by a previous load or a crashed session.
    unlink(path.c_str());

    if (bind(g_listenFd, reinterpret_cast<sockaddr*>(&address), sizeof(address)) < 0 || listen(g_listenFd, 4) < 0) {
        close(g_listenFd);
        g_listenFd = -1;
        return false;
    }

    g_listenSource = wl_event_loop_add_fd(g_pCompositor->m_wlEventLoop, g_listenFd, WL_EVENT_READABLE, onListen, nullptr);
    return g_listenSource != nullptr;
}

void stop() {
    // Teardown: nothing above this layer is listening any more.
    g_handlers = {};

    dropClient();
    // No callback may outlive the plugin's code.
    tellDropped();

    if (g_listenSource) {
        wl_event_source_remove(g_listenSource);
        g_listenSource = nullptr;
    }
    if (g_listenFd >= 0) {
        close(g_listenFd);
        g_listenFd = -1;
    }
    if (!g_path.empty()) {
        unlink(g_path.c_str());
        g_path.clear();
    }
}

bool connected() {
    return g_client != nullptr;
}

int peerPid() {
    return g_client ? g_client->pid : 0;
}

void send(std::string_view line) {
    if (!g_client)
        return;

    g_client->out.append(line);
    g_client->out.push_back('\n');

    if (g_client->out.size() > kMaxBacklog) {
        dropClient();
        return;
    }

    flush();
}

bool sendWithFd(std::string_view line, int fd) {
    if (!g_client)
        return false;

    // The file rides on the line's first byte, so it has to go out now, not
    // queue behind unsent output.
    flush();
    if (!g_client || !g_client->out.empty())
        return false;

    std::string data{line};
    data.push_back('\n');

    iovec io{.iov_base = data.data(), .iov_len = data.size()};

    alignas(cmsghdr) char control[CMSG_SPACE(sizeof(int))] = {};
    msghdr                message{};
    message.msg_iov        = &io;
    message.msg_iovlen     = 1;
    message.msg_control    = control;
    message.msg_controllen = sizeof(control);

    cmsghdr* header    = CMSG_FIRSTHDR(&message);
    header->cmsg_level = SOL_SOCKET;
    header->cmsg_type  = SCM_RIGHTS;
    header->cmsg_len   = CMSG_LEN(sizeof(int));
    std::memcpy(CMSG_DATA(header), &fd, sizeof(int));

    ssize_t n = -1;
    do {
        n = sendmsg(g_client->fd, &message, MSG_NOSIGNAL);
    } while (n < 0 && errno == EINTR);

    if (n < 0) {
        if (errno == EAGAIN || errno == EWOULDBLOCK)
            return false;
        dropClient();
        return false;
    }

    // The file went with the first byte; the rest of the line, if the
    // socket took only part of it, queues as usual.
    if (static_cast<size_t>(n) < data.size()) {
        g_client->out.append(data, static_cast<size_t>(n));
        updateMask();
    }
    return true;
}

} // namespace World::Server
