cmake_minimum_required(VERSION 3.27)
project(hypr-world LANGUAGES CXX)

if(NOT CMAKE_BUILD_TYPE AND NOT CMAKE_CONFIGURATION_TYPES)
    set(CMAKE_BUILD_TYPE Release CACHE STRING "Build type" FORCE)
endif()

set(CMAKE_CXX_STANDARD 23)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_POSITION_INDEPENDENT_CODE ON)

# GNU-unique symbols make the library NODELETE, so a hot unload would leave
# the old code mapped and a reload would not run constructors again.
add_compile_options(-fno-gnu-unique)

# Never -fvisibility=hidden or -Wl,-Bsymbolic: the plugin would bind its own
# null copies of the compositor's globals (g_pCompositor and friends).

find_package(PkgConfig REQUIRED)
# hyprland.pc is the compositor's own headers, and pulls in everything they
# include (aquamarine, hyprutils, wayland-server, lua, ...).
pkg_check_modules(DEPS REQUIRED IMPORTED_TARGET hyprland pixman-1 libdrm xkbcommon)

add_library(hypr-world SHARED
    src/main.cpp
    src/Input.cpp
    src/Server.cpp
    src/Deliver.cpp
    src/App.cpp
    src/Feed.cpp
    src/Layers.cpp
    src/Scanout.cpp
    src/Share.cpp
)

# The app the world key launches when none is running: this tree's release
# build by default, the package's own when built by ./default.nix with `app`.
set(HYPR_WORLD_APP "${CMAKE_CURRENT_SOURCE_DIR}/../target/release/hypr-world"
    CACHE FILEPATH "The hypr-world app the world key launches")
cmake_path(NORMAL_PATH HYPR_WORLD_APP OUTPUT_VARIABLE HYPR_WORLD_APP_PATH)

target_link_libraries(hypr-world PRIVATE PkgConfig::DEPS)
target_compile_definitions(hypr-world PRIVATE HYPR_WORLD_APP="${HYPR_WORLD_APP_PATH}")
target_compile_options(hypr-world PRIVATE -Wall -Wextra -Wno-unused-parameter)

set_target_properties(hypr-world PROPERTIES
    PREFIX "lib"
    OUTPUT_NAME "hypr-world"
)
