#include "Layers.hpp"

#include "Feed.hpp"
#include "Server.hpp"
#include "Util.hpp"

#include <hyprland/src/desktop/view/LayerSurface.hpp>
#include <hyprland/src/desktop/view/WLSurface.hpp>
#include <hyprland/src/output/Monitor.hpp>
#include <hyprland/src/protocols/LayerShell.hpp>
#include <hyprland/src/protocols/core/Compositor.hpp>
#include <hyprland/src/render/Renderer.hpp>
#include <hyprland/src/render/gl/GLFramebuffer.hpp>

#include <hyprutils/utils/ScopeGuard.hpp>

#include <sys/mman.h>
#include <unistd.h>

#include <algorithm>
#include <chrono>
#include <format>
#include <optional>
#include <unordered_map>
#include <unordered_set>
#include <utility>
#include <vector>

namespace World::Layers {

namespace {

using Clock = std::chrono::steady_clock;

// A layer that keeps drawing (a meter, a clock with seconds) is taken at
// most this often: each take is a render and a readback, and a meter in a
// frame on the wall needs no more than one a second.
constexpr auto kMinInterval = std::chrono::seconds(1);

// A layer's level and its current (animated) box as last seen,
// monitor-local logical px.
struct SPlace {
    uint32_t level = 0;
    long     x = 0, y = 0, w = 0, h = 0;

    bool operator==(const SPlace&) const = default;
};

struct SLayer {
    SPlace place;
    // Drew something, moved or resized since it was last sent.
    bool              dirty = true;
    bool              sent  = false;
    Clock::time_point taken;
    // Its surface and subsurfaces, with a listener on their commits.
    Feed::SWatched surfaces;
};

std::unordered_map<uint64_t, SLayer> g_layers;

// The layers the app says it sees (`layers`). Until it says, every layer
// counts as seen while the app is in view.
std::optional<std::unordered_set<uint64_t>> g_seen;

void watch(const PHLLS& layer, uint64_t address, SLayer& entry) {
    const auto surface = layer->wlSurface() ? layer->wlSurface()->resource() : nullptr;
    const auto drew    = [address] {
        if (const auto it = g_layers.find(address); it != g_layers.end())
            it->second.dirty = true;
    };
    if (Feed::watchTree(entry.surfaces, surface, drew))
        entry.dirty = true;
}

// layer <addr> <level> <x> <y> <w> <h> <pw> <ph> <namespace>, with a file
// of pw x ph pixels: the layer drawn as it is on the desktop, the part of
// the monitor its box covers, RGBA, premultiplied, the top row first.
bool send(uint64_t address, const SPlace& place, const PHLLS& layer, const PHLMONITOR& monitor) {
    const double scale  = monitor->m_scale;
    const long   width  = px(monitor->m_pixelSize.x);
    const long   height = px(monitor->m_pixelSize.y);
    const long   left   = std::clamp(px(place.x * scale), 0L, width);
    const long   top    = std::clamp(px(place.y * scale), 0L, height);
    const long   pw     = std::clamp(px((place.x + place.w) * scale), 0L, width) - left;
    const long   ph     = std::clamp(px((place.y + place.h) * scale), 0L, height) - top;
    if (pw <= 0 || ph <= 0)
        return false;

    if (Render::GL::g_pHyprOpenGL)
        Render::GL::g_pHyprOpenGL->makeEGLCurrent();

    // Not faded: under the app's fullscreen window Hyprland fades the top
    // layer out, and the snapshot draws a layer at its fade.
    auto&       fade = layer->alpha()[Desktop::View::LS_ALPHA_FADE]->value();
    const float was  = fade;
    fade             = 1.F;
    const auto snapshot = [&] {
        Hyprutils::Utils::CScopeGuard restore([&] { fade = was; });
        return g_pHyprRenderer->makeSnapshotFB(layer);
    }();

    auto* framebuffer = dynamic_cast<Render::GL::CGLFramebuffer*>(snapshot.get());
    if (!framebuffer)
        return false;

    const int fd = memfd_create("hypr-world-layer", MFD_CLOEXEC);
    if (fd < 0)
        return false;
    Hyprutils::Utils::CScopeGuard closeFd([fd] { close(fd); });

    const size_t bytes = static_cast<size_t>(pw) * static_cast<size_t>(ph) * 4;
    if (ftruncate(fd, static_cast<off_t>(bytes)) < 0)
        return false;
    void* pixels = mmap(nullptr, bytes, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (pixels == MAP_FAILED)
        return false;

    // Hyprland's framebuffers keep the top row first, as its own readbacks
    // for screencopy assume.
    glBindFramebuffer(GL_READ_FRAMEBUFFER, framebuffer->getFBID());
    glPixelStorei(GL_PACK_ALIGNMENT, 4);
    glReadPixels(left, top, pw, ph, GL_RGBA, GL_UNSIGNED_BYTE, pixels);
    glBindFramebuffer(GL_READ_FRAMEBUFFER, 0);
    munmap(pixels, bytes);

    return Server::sendWithFd(
        std::format("layer 0x{:x} {} {} {} {} {} {} {} {}", address, place.level, place.x, place.y, place.w, place.h, pw, ph, layer->m_namespace), fd);
}

} // namespace

void tick(const PHLMONITOR& monitor, bool inView) {
    // Dropped earlier in this tick: the reset comes once it is over.
    if (!Server::connected())
        return;

    const auto                   origin = monitor->logicalBox().pos();
    const auto                   size   = monitor->logicalBox().size();
    const auto                   now    = Clock::now();
    std::unordered_set<uint64_t> present;

    // Under the windows and over them. The overlay layer (launchers,
    // notifications, the lock screen) comes and goes over everything, and
    // is left out.
    for (uint32_t level = ZWLR_LAYER_SHELL_V1_LAYER_BACKGROUND; level <= ZWLR_LAYER_SHELL_V1_LAYER_TOP; ++level) {
        for (const auto& ref : monitor->m_layerSurfaceLayers[level]) {
            const auto layer = ref.lock();
            if (!layer || !layer->visible())
                continue;

            const auto pos = layer->position(Desktop::View::IGeometric::GEOMETRIC_CURRENT) - origin;
            const auto box = layer->size(Desktop::View::IGeometric::GEOMETRIC_CURRENT);
            // Off the monitor, as a dock that slid away, it is not shown.
            if (box.x < 1 || box.y < 1 || pos.x >= size.x || pos.y >= size.y || pos.x + box.x <= 0 || pos.y + box.y <= 0)
                continue;

            const auto address = reinterpret_cast<uint64_t>(layer.get());
            present.insert(address);

            auto&        entry = g_layers[address];
            const SPlace place{.level = level, .x = px(pos.x), .y = px(pos.y), .w = px(box.x), .h = px(box.y)};
            if (entry.place != place) {
                entry.place = place;
                entry.dirty = true;
            }
            watch(layer, address, entry);

            // A layer nobody sees in the app, out of view or in no frame on
            // screen, is not taken. Only one the app has never had goes out,
            // so the frames are whole when it comes into view.
            const bool seen = inView && (!g_seen || g_seen->contains(address));
            if (!entry.dirty || (entry.sent && !seen) || now - entry.taken < kMinInterval)
                continue;

            entry.taken     = now;
            const bool sent = send(address, entry.place, layer, monitor);
            // A failed write dropped the app: no more snapshots for it.
            if (!Server::connected())
                return;
            if (sent) {
                entry.dirty = false;
                entry.sent  = true;
            }
        }
    }

    for (auto it = g_layers.begin(); it != g_layers.end();) {
        if (present.contains(it->first)) {
            ++it;
            continue;
        }
        if (it->second.sent)
            Server::send(std::format("layer-gone 0x{:x}", it->first));
        it = g_layers.erase(it);
    }
}

void setSeen(const std::vector<uint64_t>& addresses) {
    g_seen = std::unordered_set<uint64_t>(addresses.begin(), addresses.end());
}

void reset() {
    g_layers.clear();
    g_seen.reset();
}

} // namespace World::Layers
