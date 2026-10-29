#include "Share.hpp"

#include "App.hpp"
#include "Server.hpp"
#include "Util.hpp"

#include <hyprland/src/desktop/view/Popup.hpp>
#include <hyprland/src/desktop/view/WLSurface.hpp>
#include <hyprland/src/desktop/view/Window.hpp>
#include <hyprland/src/output/Monitor.hpp>
#include <hyprland/src/protocols/XDGShell.hpp>
#include <hyprland/src/protocols/core/Compositor.hpp>
#include <hyprland/src/protocols/types/Buffer.hpp>

#include <drm_fourcc.h>
#include <sys/stat.h>

#include <algorithm>
#include <chrono>
#include <cmath>
#include <cstring>
#include <format>
#include <iterator>
#include <map>
#include <optional>
#include <ranges>
#include <unordered_map>
#include <unordered_set>
#include <utility>
#include <vector>

namespace World::Share {

namespace {

// A surface holding this many means the app is behind: its newest buffer
// waits for a `done` rather than its client running out of buffers.
constexpr long kMaxHeld = 3;

// One of a shared window's surfaces: its own (id 0), a subsurface, or a
// popup or one of the popup's subsurfaces.
struct SSurface {
    uint32_t               id = 0;
    WP<CWLSurfaceResource> surface;
    CHyprSignalListener    commit;
    // Where it is as of the last look at the window: its top left from the
    // window's, in logical px, and whether the window's box clips it, as it
    // does all but popups.
    Vector2D at;
    bool     clipped = true;
    // Drawn below the window's own surface.
    bool below = false;
    // Its newest buffer as the app sees it: whether the app can take it,
    // and its size in px. Judged as it comes, as Hyprland lets go of a buffer
    // in shared memory as soon as its commit's listeners have run.
    bool     shareable = false;
    Vector2D size;
    // It is YUV (NV12, P010), which the app draws on a surface's quad of its
    // own but never on the window's.
    bool yuv = false;
    // That buffer itself while Hyprland holds it too, a dmabuf until the
    // surface's next commit, to send again if its window is shared afresh.
    // Never one in shared memory: held past the app's copy it would stay
    // from its client until the next commit, and a client that draws into
    // one buffer would wait on it for good. That surface goes to the app
    // with its next commit, its window on capture until then.
    CHLBufferReference latest;
    // Its newest buffer's colour if that is a single-pixel buffer
    // (wp_single_pixel_buffer_v1), and whether the app still has to be
    // told it. Nothing is held for one: the colour goes in the line.
    std::optional<uint32_t> pixel;
    bool                    pixelDue = false;
    // Its newest buffer not sent yet: the socket was busy, or it held all
    // it may.
    CHLBufferReference pending;
};

struct SWindow {
    PHLWINDOWREF window;
    // As Hyprland draws them, bottom to top, the window's own first.
    std::vector<SSurface> surfaces;
    uint32_t              nextId = 1;
    // The `surfaces` line the app last had, empty for none.
    std::string layout;
    // The app has had a buffer of it, and no `unshared` since.
    bool shared = false;
    // While it is shared, the frame callbacks its surfaces' client waits
    // on, taken off them, and when the last went out.
    std::vector<SP<CWLCallbackResource>> frames;
    Time::steady_tp                      framed;
};

// A buffer the app has, kept from its client until the app is done, and
// when it went.
struct SHeld {
    CHLBufferReference buffer;
    uint64_t           window  = 0;
    uint32_t           surface = 0;
    Time::steady_tp    sent;
};

// Shared memory handed back since the app connected, and how long the app
// held it all told, for `hyprctl hypr-world`: it copies the pixels out and
// says `done` at once.
struct SShmHeld {
    uint64_t         count = 0;
    Time::steady_dur total{};
};

// A surface as Hyprland draws the window: where it is, from the window's
// top left in logical px, and whether the window's box clips it.
struct SNode {
    SP<CWLSurfaceResource> surface;
    Vector2D               at;
    bool                   clipped = true;
    bool                   below   = false;
};

std::map<uint32_t, std::unordered_set<uint64_t>> g_formats;
std::unordered_map<uint64_t, SWindow>            g_windows;
std::map<uint64_t, SHeld>                        g_held;
uint64_t                                         g_nextId = 1;

// The least time between each window's frames, in ms, as the app last said
// (`pace`): 0 is every refresh.
std::unordered_map<uint64_t, uint32_t> g_pace;
// Armed for when the next window's frame callbacks are due.
SP<CEventLoopTimer> g_frameTimer;
// Frame callbacks sent, for `hyprctl hypr-world`.
uint64_t g_framesSent = 0;
SShmHeld g_shmHeld;

// The buffer's planes are all one dmabuf, as Mesa hands them out, so one fd
// carries them.
bool onePlaneFile(const Aquamarine::SDMABUFAttrs& attrs) {
    struct stat first{};
    if (fstat(attrs.fds[0], &first) != 0)
        return false;
    for (int i = 1; i < attrs.planes; ++i) {
        struct stat plane{};
        if (fstat(attrs.fds[i], &plane) != 0 || plane.st_dev != first.st_dev || plane.st_ino != first.st_ino)
            return false;
    }
    return true;
}

// A single-pixel buffer's colour: shared memory to Hyprland, but with no
// pool behind it, one pixel of ARGB8888 (premultiplied, as the protocol
// gives it) at its data pointer.
std::optional<uint32_t> singlePixel(const CHLBufferReference& buffer) {
    if (!buffer || buffer->type() != Aquamarine::BUFFER_TYPE_SHM || buffer->size != Vector2D{1, 1} || buffer->shm().success)
        return std::nullopt;
    const auto [data, format, size] = buffer->beginDataPtr(0);
    std::optional<uint32_t> colour;
    if (data && format == DRM_FORMAT_ARGB8888 && size >= 4) {
        uint32_t value = 0;
        std::memcpy(&value, data, 4);
        colour = value;
    }
    buffer->endDataPtr();
    return colour;
}

// Whether the app can take `buffer` as a surface shows it, not turned (the
// app draws no buffer transform yet): a dmabuf of a format and modifier it
// said it samples, its planes in one file, or shared memory in one of those
// formats, which it copies out. Or a single-pixel buffer, shared memory with
// no pool behind it, whose colour goes in the line.
bool samples(const CHLBufferReference& buffer, const SP<CWLSurfaceResource>& surface) {
    if (!buffer || surface->m_current.transform != WL_OUTPUT_TRANSFORM_NORMAL)
        return false;
    if (singlePixel(buffer))
        return true;
    if (buffer->type() == Aquamarine::BUFFER_TYPE_SHM) {
        const auto attrs = buffer->shm();
        return attrs.success && g_formats.contains(attrs.format);
    }
    if (buffer->type() != Aquamarine::BUFFER_TYPE_DMABUF)
        return false;
    const auto attrs = buffer->dmabuf();
    if (!attrs.success || attrs.planes < 1 || attrs.planes > 4 || !onePlaneFile(attrs))
        return false;
    const auto format = g_formats.find(attrs.format);
    return format != g_formats.end() && format->second.contains(attrs.modifier);
}

// `buffer`, the surface's newest, taken note of as it comes, and the one to
// send next.
void noteBuffer(SSurface& entry, const SP<CWLSurfaceResource>& surface, const CHLBufferReference& buffer) {
    entry.shareable = buffer && samples(buffer, surface);
    entry.size      = buffer ? buffer->size : Vector2D{};
    entry.yuv       = buffer && buffer->type() == Aquamarine::BUFFER_TYPE_DMABUF &&
        (buffer->dmabuf().format == DRM_FORMAT_NV12 || buffer->dmabuf().format == DRM_FORMAT_P010);
    entry.pixel     = singlePixel(buffer);
    entry.pixelDue  = entry.pixel.has_value();
    entry.latest    = buffer && !buffer->isSynchronous() ? buffer : CHLBufferReference{};
    if (buffer)
        entry.pending = buffer;
}

// Drawn by Hyprland: a surface with a picture and a size.
bool drawn(const SP<CWLSurfaceResource>& surface) {
    return surface->m_current.texture && surface->m_current.size.x >= 1 && surface->m_current.size.y >= 1;
}

// The window's box in its own surface: from its xdg geometry's corner, or
// the surface's, as large as Hyprland has the window now, which is what the
// feed gives the app for its quad. Not the geometry's size: a client that
// cannot be as small as its tile (Firefox, with its least width) draws more
// than that, and Hyprland shows only the part in the tile.
CBox windowBox(const PHLWINDOW& window) {
    Vector2D corner;
    if (const auto xdg = window->m_xdgSurface.lock(); xdg && !xdg->m_current.geometry.empty())
        corner = xdg->m_current.geometry.pos();
    return {corner, window->size(Desktop::View::IGeometric::GEOMETRIC_CURRENT)};
}

// The window's surfaces, its own first, then the rest as Hyprland draws
// them, bottom to top: its subsurfaces below its own (`below`), those above
// it, then its popups, each with its subsurfaces. Positions
// are from the window's box, the xdg geometry's origin, as xdg-shell has
// them: a popup's is from its parent's geometry (`coordsRelativeToParent`).
// Hyprland itself draws the window's own surface uncut at the window's
// place, and its popups less the geometry's position, which only agree with
// this when the geometry starts at the surface's corner, as it does for a
// client drawing no shadows of its own.
std::optional<std::vector<SNode>> walk(const PHLWINDOW& window) {
    const auto main = window->wlSurface() ? window->wlSurface()->resource() : nullptr;
    if (!main)
        return std::nullopt;
    const CBox     box      = windowBox(window);
    const Vector2D geometry = box.pos();

    std::vector<SNode> nodes;
    bool               mainSeen = false;
    main->breadthfirst(
        [&](SP<CWLSurfaceResource> surface, const Vector2D& offset, void*) {
            if (surface == main) {
                nodes.insert(nodes.begin(), SNode{surface, -geometry, true});
                mainSeen = true;
            }
            // Hyprland walks those below the window's own surface first.
            else if (!drawn(surface))
                return;
            // One wholly outside the window's box shows nothing.
            else if (!CBox{offset - geometry, surface->m_current.size}.intersection({{0, 0}, box.size()}).empty()) {
                nodes.push_back({surface, offset - geometry, true, !mainSeen});
            }
        },
        nullptr);
    if (nodes.empty())
        return std::nullopt;

    if (window->m_popupHead) {
        window->m_popupHead->breadthfirst(
            [&](WP<Desktop::View::CPopup> popup, void*) {
                if (!popup || !popup->aliveAndVisible() || !popup->wlSurface() || !popup->wlSurface()->resource())
                    return;
                const Vector2D at = popup->coordsRelativeToParent();
                popup->wlSurface()->resource()->breadthfirst(
                    [&](SP<CWLSurfaceResource> surface, const Vector2D& offset, void*) {
                        if (drawn(surface))
                            nodes.push_back({surface, at + offset, false});
                    },
                    nullptr);
            },
            nullptr);
    }
    return nodes;
}

// What a surface shows of its buffer, `buffer` px, in the app: its box in
// logical px from the window's top left, cut to the window's box unless it
// is a popup, and the part of the buffer in that box, in buffer px. The
// viewport's source, in the surface's units before the buffer's scale, picks
// the part of the buffer the whole surface shows. None once cut down to
// nothing.
std::optional<std::pair<CBox, CBox>> placement(const SSurface& entry, const CBox& window, const Vector2D& buffer) {
    const auto surface = entry.surface.lock();
    if (!surface || buffer.x <= 0 || buffer.y <= 0)
        return std::nullopt;
    const auto& state  = surface->m_current;
    const CBox  box{entry.at, state.size};
    const CBox  source = state.viewport.hasSource ? CBox{state.viewport.source.pos() * state.scale, state.viewport.source.size() * state.scale} :
                                                    CBox{{0, 0}, buffer};
    const CBox  shown  = entry.clipped ? box.intersection({{0, 0}, window.size()}) : box;
    if (shown.empty() || box.empty())
        return std::nullopt;
    const Vector2D scale = source.size() / box.size();
    return std::pair{shown, CBox{source.pos() + (shown.pos() - box.pos()) * scale, shown.size() * scale}};
}

// A line for the app, and the file that goes with it.
struct SLine {
    std::string text;
    int         fd = -1;
};

// buffer <addr> <sid> <id> <w> <h> <fourcc> <modifier> <cx> <cy> <cw> <ch>
// <planes> <offset> <stride>..., with the dmabuf, or shm <addr> <sid> <id>
// <w> <h> <fourcc> <cx> <cy> <cw> <ch> <offset> <stride>, with the pool the
// shared memory is in: `buffer` of the window's surface `entry` as the app
// can take it, the crop what the surface shows of it, in buffer px. None if
// it cannot.
std::optional<SLine> line(uint64_t address, uint64_t id, const SWindow& window, const SSurface& entry, const CHLBufferReference& buffer) {
    const auto surface = entry.surface.lock();
    const auto w       = window.window.lock();
    const auto main    = window.surfaces.empty() ? nullptr : window.surfaces.front().surface.lock();
    if (!surface || !w || !main || !samples(buffer, surface))
        return std::nullopt;

    const auto placed = placement(entry, windowBox(w), buffer->size);
    if (!placed)
        return std::nullopt;
    const auto  crop   = [](double value, double limit) { return std::clamp<long>(std::lround(value), 0, std::lround(limit)); };
    const auto& part   = placed->second;
    const auto& size   = buffer->size;
    const long  cx     = crop(part.x, size.x);
    const long  cy     = crop(part.y, size.y);
    const long  cw     = crop(part.w, size.x - cx);
    const long  ch     = crop(part.h, size.y - cy);
    const long  width  = std::lround(size.x);
    const long  height = std::lround(size.y);
    if (cw <= 0 || ch <= 0)
        return std::nullopt;

    if (buffer->type() == Aquamarine::BUFFER_TYPE_SHM) {
        const auto attrs = buffer->shm();
        return SLine{std::format("shm 0x{:x} {} {} {} {} 0x{:x} {} {} {} {} {} {}", address, entry.id, id, width, height, attrs.format, cx, cy, cw, ch,
                                 attrs.offset, attrs.stride),
                     attrs.fd};
    }

    const auto attrs     = buffer->dmabuf();
    auto       text      = std::format("buffer 0x{:x} {} {} {} {} 0x{:x} 0x{:x} {} {} {} {} {}", address, entry.id, id, width, height, attrs.format,
                                       attrs.modifier, cx, cy, cw, ch, attrs.planes);
    for (int i = 0; i < attrs.planes; ++i)
        text += std::format(" {} {}", attrs.offsets[i], attrs.strides[i]);
    return SLine{std::move(text), attrs.fds[0]};
}

// The least time between a window's frames: the app's pace for it, or
// with none (or 0), a refresh of the app's monitor.
Time::steady_dur frameInterval(uint64_t address) {
    if (const auto pace = g_pace.find(address); pace != g_pace.end() && pace->second > 0)
        return std::chrono::milliseconds(pace->second);
    const auto   monitor = App::monitor();
    const double hz      = monitor && monitor->m_refreshRate > 0 ? monitor->m_refreshRate : 60.0;
    return std::chrono::duration_cast<Time::steady_dur>(std::chrono::duration<double>(1.0 / hz));
}

// Its client may draw again.
void sendFrames(SWindow& window, const Time::steady_tp& now) {
    for (const auto& frame : window.frames)
        frame->send(now);
    g_framesSent += window.frames.size();
    window.frames.clear();
    window.framed = now;
}

void onFrameTimer(SP<CEventLoopTimer> self, void* data);

// The timer set for the soonest window whose frame callbacks wait, or off.
void scheduleFrames() {
    std::optional<Time::steady_tp> next;
    for (const auto& [address, window] : g_windows) {
        if (window.frames.empty())
            continue;
        const auto due = window.framed + frameInterval(address);
        next           = next ? std::min(*next, due) : due;
    }

    if (!next) {
        if (g_frameTimer)
            g_frameTimer->updateTimeout(std::nullopt);
        return;
    }
    if (!g_frameTimer) {
        g_frameTimer = makeShared<CEventLoopTimer>(std::nullopt, onFrameTimer, nullptr);
        g_pEventLoopManager->addTimer(g_frameTimer);
    }
    g_frameTimer->updateTimeout(std::max(*next - Time::steadyNow(), Time::steady_dur::zero()));
}

void onFrameTimer(SP<CEventLoopTimer>, void*) {
    guarded("frames", [] {
        const auto now = Time::steadyNow();
        for (auto& [address, window] : g_windows) {
            if (!window.frames.empty() && now >= window.framed + frameInterval(address))
                sendFrames(window, now);
        }
        scheduleFrames();
    });
}

// The frame callbacks the window's client asked for on any of its surfaces,
// off them, so they go at the app's pace and not when Hyprland draws it.
// Hyprland applies a commit to the surface before its listeners run, and
// none of its own sends them, so every one of them is still there.
void takeFrames(SWindow& window) {
    bool took = false;
    for (const auto& entry : window.surfaces) {
        const auto surface = entry.surface.lock();
        if (!surface || surface->m_current.callbacks.empty())
            continue;
        auto& asked = surface->m_current.callbacks;
        window.frames.insert(window.frames.end(), std::make_move_iterator(asked.begin()), std::make_move_iterator(asked.end()));
        asked.clear();
        took = true;
    }
    if (took)
        scheduleFrames();
}

// The app has had a buffer of the window: its frames are the app's from
// here, those already waiting too.
void markShared(SWindow& window) {
    if (!window.shared) {
        window.shared = true;
        takeFrames(window);
    }
}

// Every frame callback held, sent now.
void sendAllFrames() {
    const auto now = Time::steadyNow();
    for (auto& [address, window] : g_windows) {
        if (!window.frames.empty())
            sendFrames(window, now);
    }
}

// Back on capture, and its client not left waiting on a frame. The app is
// told the surfaces afresh if it shares again.
void unshare(uint64_t address, SWindow& window) {
    for (auto& entry : window.surfaces) {
        entry.pending  = {};
        entry.pixelDue = false;
    }
    if (!window.frames.empty())
        sendFrames(window, Time::steadyNow());
    if (window.shared) {
        Server::send(std::format("unshared 0x{:x}", address));
        window.shared = false;
    }
    window.layout.clear();
}

// A surface's newest buffer to the app, if it can go now: then held. One
// the app cannot take puts the window back on capture.
void send(uint64_t address, SWindow& window, SSurface& entry) {
    // pixel <addr> <sid> <argb>: a single-pixel buffer's colour, held from
    // nobody, so with no id and no `done`.
    if (entry.pixel) {
        if (!entry.pixelDue)
            return;
        Server::send(std::format("pixel 0x{:x} {} {:08x}", address, entry.id, *entry.pixel));
        entry.pixelDue = false;
        entry.pending  = {};
        markShared(window);
        return;
    }
    if (!entry.pending)
        return;
    const auto held = std::ranges::count_if(g_held, [&](const auto& h) { return h.second.window == address && h.second.surface == entry.id; });
    if (held >= kMaxHeld)
        return;

    const uint64_t id  = g_nextId++;
    const auto     out = line(address, id, window, entry, entry.pending);
    if (!out) {
        unshare(address, window);
        return;
    }
    // Tried again on the next commit or tick.
    if (!Server::sendWithFd(out->text, out->fd))
        return;

    g_held.emplace(id, SHeld{.buffer = std::move(entry.pending), .window = address, .surface = entry.id, .sent = Time::steadyNow()});
    entry.pending = {};
    markShared(window);
}

void onCommit(uint64_t address, const WP<CWLSurfaceResource>& committed);

// The window's surfaces looked at again: each one it draws watched, those
// new to it taken on and those gone let go, then back to capture if they
// cannot all be shared, else the app told where they are when that
// changed, and their newest buffers sent. Watched whether or not they can
// be shared, so that a surface in shared memory is known by its next commit.
void refresh(uint64_t address, SWindow& window) {
    const auto w     = window.window.lock();
    const auto nodes = w ? walk(w) : std::nullopt;
    if (!nodes) {
        unshare(address, window);
        return;
    }

    std::vector<SSurface> surfaces;
    surfaces.reserve(nodes->size());
    for (const auto& node : *nodes) {
        auto known = std::ranges::find_if(window.surfaces, [&](const SSurface& entry) { return entry.surface.lock() == node.surface; });
        if (known != window.surfaces.end()) {
            surfaces.push_back(std::move(*known));
        } else {
            // New: what it shows now, so the app has it before its next
            // commit. The window's own surface is the first.
            auto& entry   = surfaces.emplace_back();
            entry.id      = &node == &nodes->front() ? 0 : window.nextId++;
            entry.surface = node.surface;
            noteBuffer(entry, node.surface, node.surface->m_current.buffer);
            entry.commit = node.surface->m_events.commit.listen(
                [address, weak = WP<CWLSurfaceResource>(node.surface)] { guarded("shared buffer", [&] { onCommit(address, weak); }); });
        }
        surfaces.back().at      = node.at;
        surfaces.back().clipped = node.clipped;
        surfaces.back().below   = node.below;
    }
    // The ones gone go with their listeners. Their buffers the app has stay
    // held until it is done with them.
    window.surfaces = std::move(surfaces);

    // Turned since its buffer came, it is no longer (`samples`). The
    // window's own surface (id 0) in YUV goes to capture.
    const auto shareable = [](const SSurface& entry) {
        const auto surface = entry.surface.lock();
        return surface && entry.shareable && !(entry.id == 0 && entry.yuv) && surface->m_current.transform == WL_OUTPUT_TRANSFORM_NORMAL;
    };
    if (!std::ranges::all_of(window.surfaces, shareable)) {
        unshare(address, window);
        return;
    }

    // surfaces <addr> [<sid> <x> <y> <w> <h>]...: those past its own, where
    // the app shows them, bottom to top, and its own among them (id 0) if
    // any is below it.
    const auto box  = windowBox(w);
    auto       text = std::format("surfaces 0x{:x}", address);
    const auto add  = [&](bool below) {
        for (const auto& entry : window.surfaces | std::views::drop(1)) {
            if (entry.below != below)
                continue;
            if (const auto placed = placement(entry, box, entry.size)) {
                const auto& [shown, part] = *placed;
                text += std::format(" {} {} {} {} {}", entry.id, std::lround(shown.x), std::lround(shown.y), std::lround(shown.w), std::lround(shown.h));
            }
        }
    };
    // Those below its own first, then its own (id 0, the window's box) to
    // say where they end, only when there are some, then the rest.
    add(true);
    if (std::ranges::any_of(window.surfaces, &SSurface::below))
        text += std::format(" 0 0 0 {} {}", std::lround(box.w), std::lround(box.h));
    add(false);
    // Once for every window shared, so the app lets go of any it still
    // shows from before, and again on every change.
    if (text != window.layout) {
        Server::send(text);
        window.layout = text;
    }

    // Shared afresh: what each shows now. Its own surface's first, then the
    // others, which go on it.
    for (auto& entry : window.surfaces) {
        if (!window.shared && !entry.pending)
            entry.pending = entry.latest;
        // A colour needs no buffer to tell again.
        if (!window.shared && entry.pixel)
            entry.pixelDue = true;
    }
    for (auto& entry : window.surfaces) {
        send(address, window, entry);
        if (!window.shared)
            return;
    }
}

void onCommit(uint64_t address, const WP<CWLSurfaceResource>& committed) {
    const auto it = g_windows.find(address);
    const auto surface = committed.lock();
    if (it == g_windows.end() || !surface)
        return;
    auto& window = it->second;
    auto  entry  = std::ranges::find_if(window.surfaces, [&](const SSurface& e) { return e.surface.lock() == surface; });
    if (entry == window.surfaces.end())
        return;
    // A commit without a new buffer changes nothing the app samples, but
    // may move the surfaces under it. A buffer in shared memory is the
    // surface's no more once its listeners have run.
    if (surface->m_current.updated.bits.buffer)
        noteBuffer(*entry, surface, surface->m_current.buffer);
    refresh(address, window);
    // Any commit may ask for frames.
    if (window.shared)
        takeFrames(window);
}

// Every window back on capture. The buffers the app has stay held until it
// is done with them.
void stopAll() {
    for (auto& [address, window] : g_windows)
        unshare(address, window);
    g_windows.clear();
    scheduleFrames();
}

} // namespace

void addFormat(uint32_t fourcc, std::vector<uint64_t> modifiers) {
    g_formats[fourcc].insert(modifiers.begin(), modifiers.end());
}

void setPace(std::unordered_map<uint64_t, uint32_t> pace) {
    g_pace = std::move(pace);
    scheduleFrames();
}

void done(uint64_t id) {
    const auto it = g_held.find(id);
    if (it == g_held.end())
        return;
    const auto [address, surface] = std::pair{it->second.window, it->second.surface};
    if (it->second.buffer && it->second.buffer->type() == Aquamarine::BUFFER_TYPE_SHM) {
        ++g_shmHeld.count;
        g_shmHeld.total += Time::steadyNow() - it->second.sent;
    }
    // Its client may draw into it again from here.
    g_held.erase(it);

    const auto window = g_windows.find(address);
    if (window == g_windows.end())
        return;
    if (auto entry = std::ranges::find_if(window->second.surfaces, [&](const SSurface& e) { return e.id == surface; }); entry != window->second.surfaces.end())
        send(address, window->second, *entry);
}

void tick(bool inView) {
    if (!inView || g_formats.empty() || !Server::connected()) {
        stopAll();
        return;
    }

    std::unordered_set<uint64_t> now;
    for (const auto& w : App::live()) {
        if (!w->wlSurface() || !w->wlSurface()->resource())
            continue;
        const auto address = reinterpret_cast<uint64_t>(w.get());
        now.insert(address);
        auto& window  = g_windows[address];
        window.window = w;
        refresh(address, window);
    }

    std::erase_if(g_windows, [&](auto& entry) {
        if (now.contains(entry.first))
            return false;
        unshare(entry.first, entry.second);
        return true;
    });
    scheduleFrames();
}

// No buffer held for the app nor anything it said: their clients have the
// buffers back, and their frames.
static void reset() {
    sendAllFrames();
    g_windows.clear();
    g_held.clear();
    g_formats.clear();
    g_pace.clear();
    g_shmHeld = {};
}

void appConnected(bool) {
    // An app that connects knows of no buffer, and one that went is done with
    // them all.
    reset();
    scheduleFrames();
}

void shutdown() {
    reset();
    removeTimer(g_frameTimer);
}

std::string describe() {
    size_t shared = 0, surfaces = 0, frames = 0;
    for (const auto& [address, window] : g_windows) {
        if (window.shared) {
            ++shared;
            surfaces += window.surfaces.size();
        }
        frames += window.frames.size();
    }
    const auto shm = g_shmHeld.count == 0 ? std::string{} :
                                            std::format(", shared memory held {:.2} ms on average over {}",
                                                        std::chrono::duration<double, std::milli>(g_shmHeld.total).count() / g_shmHeld.count, g_shmHeld.count);
    return std::format("{} formats, {} live, {} windows of {} surfaces, {} buffers held, {} frame callbacks held, {} sent{}",
                       g_formats.size(), App::live().size(), shared, surfaces, g_held.size(), frames, g_framesSent, shm);
}

} // namespace World::Share
