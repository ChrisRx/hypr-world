#pragma once

// Windows' own buffers, sent to the app as their clients commit them in
// place of captured frames. A window the app shows (its `live` set) has
// each committed buffer of each of its surfaces sent with its dmabuf
// attached, and held, so its client cannot draw into it again, until the
// app says `done`: its own surface, its
// subsurfaces and its popups with theirs, where they are told in a
// `surfaces` line, as long as every one draws into a dmabuf the app said
// it can sample, or into shared memory in one of those formats (`shm`, the
// pool attached, which the app copies out and hands back at once), or is
// a single-pixel buffer (`pixel`, its color in the line, nothing held).
// YUV dmabufs (NV12, P010) go on surfaces past the window's own, never on
// its own. Everything else stays on capture.
//
// While the app has a window's buffers, its frame callbacks are the
// plugin's to send: taken off its surfaces on each commit and sent at the
// rate the app asks for it (`pace`), every refresh if it asks none.
// Hyprland sends them only when it draws a window, so one on a hidden
// workspace would get none, and with no copies of it none come from capture
// either. Whenever a window stops being shared, those held go at once.

#include <cstdint>
#include <string>
#include <unordered_map>
#include <vector>

namespace World::Share {

// samples <fourcc> <modifier>...: a format the app samples, through these
// modifiers.
void addFormat(uint32_t fourcc, std::vector<uint64_t> modifiers);

// done <id>: the app is finished with a buffer, its GPU too.
void done(uint64_t id);

// pace [<addr> <ms>]...: the least time between the frames of each window the
// app shows, 0 for every refresh. As a whole: a window it leaves out goes
// back to every refresh.
void setPace(std::unordered_map<uint64_t, uint32_t> pace);

// Each feed tick, the app in view or not: which windows are shared, and
// sends that could not go out at once.
void tick(bool inView);

void appConnected(bool connected);
void shutdown();

// For `hyprctl hypr-world`.
std::string describe();

} // namespace World::Share
