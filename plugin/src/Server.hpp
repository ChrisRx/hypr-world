#pragma once

// The socket the app connects to, served from the compositor's own event
// loop. One app at a time: a new connection replaces the old one, so the app
// restarted after a hang takes over without waiting for the old socket to
// time out.

#include <string>
#include <string_view>

namespace World::Server {

struct SHandlers {
    // One complete line from the app, without the newline.
    void (*line)(std::string_view) = nullptr;
    // The app connected (true) or went away (false). Going away is told
    // from an idle callback, never from inside send(), so a handler can
    // send in the middle of a loop over the state a disconnect resets.
    void (*connection)(bool) = nullptr;
};

bool start(const std::string& path, SHandlers handlers);
void stop();

bool connected();

// The connected app's process id (SO_PEERCRED), or 0. Its window has the
// same one.
int peerPid();

// Queues one line (the newline is added). Never blocks: an app that stops
// reading is dropped once its backlog passes a limit.
void send(std::string_view line);

// Sends one line with a file attached to it (SCM_RIGHTS), straight away.
// False when it could not go now (output still queued, or the socket is
// full); the caller tries again later. The caller keeps its fd either way.
bool sendWithFd(std::string_view line, int fd);

} // namespace World::Server
