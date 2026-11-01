#include "Compat.hpp"

#include <hyprland/src/output/Monitor.hpp>
#include <hyprland/src/state/WorkspaceState.hpp>

#if HYPR_WORLD_HYPRLAND_AT_LEAST(0, 57)
#include <hyprland/src/workspace/query/Query.hpp>
#else
#include <hyprland/src/helpers/MiscFunctions.hpp>
#endif

#include <limits>
#include <string>
#include <unordered_map>

namespace World::Compat {

#if HYPR_WORLD_HYPRLAND_AT_LEAST(0, 57)

namespace {

using ::Workspace::eWorkspaceType;
using ::Workspace::SWorkspaceNumberedID;
using ::Workspace::SWorkspaceSpecialID;

// The ids given to named workspaces, by address, and the next one.
std::unordered_map<std::string, int64_t> g_named;
int64_t                                  g_nextNamed = -1337;

std::optional<SWorkspaceNumberedID> numbered(int64_t id) {
    if (id <= 0 || id > std::numeric_limits<::Workspace::WorkspaceIDContainer>::max())
        return std::nullopt;
    return SWorkspaceNumberedID{static_cast<::Workspace::WorkspaceIDContainer>(id)};
}

} // namespace

bool isSpecial(const PHLWORKSPACE& workspace) {
    return workspace->type() == eWorkspaceType::SPECIAL;
}

int64_t id(const PHLWORKSPACE& workspace) {
    if (isSpecial(workspace))
        return 0;
    if (const auto number = workspace->numberedID())
        return *number;

    const auto [it, added] = g_named.try_emplace(workspace->addressableName(), g_nextNamed);
    if (added)
        --g_nextNamed;
    return it->second;
}

const std::string& name(const PHLWORKSPACE& workspace) {
    return workspace->displayName();
}

PHLWORKSPACE find(int64_t id) {
    if (const auto number = numbered(id))
        return State::workspaceState()->query().numbered(*number).run();

    for (const auto& [address, named] : g_named) {
        if (named == id)
            return State::workspaceState()->query().identity(SWorkspaceSpecialID{}, address, eWorkspaceType::NORMAL).run();
    }
    return nullptr;
}

PHLWORKSPACE createNumbered(int64_t id, const PHLMONITOR& monitor) {
    const auto number = numbered(id);
    if (!number || !monitor)
        return nullptr;
    return State::workspaceState()->createNumbered(*number, monitor, std::to_string(id));
}

PHLWORKSPACE special(std::string_view name, const PHLMONITOR& monitor) {
    const auto address = "special:" + std::string(name);
    if (auto workspace = State::workspaceState()->query().identity(SWorkspaceSpecialID{}, address, eWorkspaceType::SPECIAL).run())
        return workspace;
    if (!monitor)
        return nullptr;
    return State::workspaceState()->createSpecial(address, monitor);
}

std::optional<std::string> selector(int64_t id) {
    if (numbered(id))
        return std::to_string(id);
    if (const auto workspace = find(id))
        return ::Workspace::selector(*workspace);
    return std::nullopt;
}

#else

bool isSpecial(const PHLWORKSPACE& workspace) {
    return workspace->m_isSpecialWorkspace;
}

int64_t id(const PHLWORKSPACE& workspace) {
    return workspace->m_isSpecialWorkspace ? 0 : workspace->m_id;
}

const std::string& name(const PHLWORKSPACE& workspace) {
    return workspace->m_name;
}

PHLWORKSPACE find(int64_t id) {
    return State::workspaceState()->query().id(id).run();
}

PHLWORKSPACE createNumbered(int64_t id, const PHLMONITOR& monitor) {
    if (id <= 0 || !monitor)
        return nullptr;
    return State::workspaceState()->create(id, monitor->m_id, std::to_string(id));
}

PHLWORKSPACE special(std::string_view name, const PHLMONITOR& monitor) {
    const auto [id, wsName, isAutoID] = getWorkspaceIDNameFromString("special:" + std::string(name));
    if (id == WORKSPACE_INVALID)
        return nullptr;

    if (auto workspace = State::workspaceState()->query().id(id).run())
        return workspace;
    if (!monitor)
        return nullptr;
    return State::workspaceState()->create(id, monitor->m_id, wsName);
}

std::optional<std::string> selector(int64_t id) {
    return std::to_string(id);
}

#endif

} // namespace World::Compat
