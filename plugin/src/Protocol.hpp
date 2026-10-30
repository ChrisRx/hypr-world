#pragma once

// The line protocol between the plugin and the app: one message per line,
// space-separated tokens, verb first, numbers in C locale. Unknown verbs are
// ignored by both sides.

#include <charconv>
#include <cstdint>
#include <optional>
#include <string_view>
#include <system_error>
#include <vector>

namespace World::Protocol {

// The protocol's version, said in each side's hello. The plugin and the app
// are built from the same tree, so they must match exactly: bump it with any
// change to the lines either side sends.
inline constexpr int kVersion = 3;

// The tokens of one line, as views into it.
inline std::vector<std::string_view> tokens(std::string_view line) {
    std::vector<std::string_view> out;

    size_t i = 0;
    while (i < line.size()) {
        while (i < line.size() && line[i] == ' ')
            ++i;
        const size_t start = i;
        while (i < line.size() && line[i] != ' ')
            ++i;
        if (i > start)
            out.push_back(line.substr(start, i - start));
    }

    return out;
}

// A whole token as a number, or nothing if any of it is not one.
template <typename T>
std::optional<T> number(std::string_view token) {
    T value{};
    const auto* end = token.data() + token.size();
    const auto [ptr, ec] = std::from_chars(token.data(), end, value);
    if (ec != std::errc{} || ptr != end)
        return std::nullopt;
    return value;
}

// An address as `hyprctl clients` prints it (`0x` and hex), or nothing if
// the token is not one.
inline std::optional<uint64_t> address(std::string_view token) {
    if (token.starts_with("0x"))
        token.remove_prefix(2);

    uint64_t   value = 0;
    const auto end   = token.data() + token.size();
    if (const auto [ptr, ec] = std::from_chars(token.data(), end, value, 16); ec != std::errc{} || ptr != end)
        return std::nullopt;
    return value;
}

// The addresses after a line's verb. Malformed ones are skipped.
inline std::vector<uint64_t> addresses(const std::vector<std::string_view>& tokens) {
    std::vector<uint64_t> out;

    for (size_t i = 1; i < tokens.size(); ++i) {
        if (const auto value = address(tokens[i]))
            out.push_back(*value);
    }

    return out;
}

} // namespace World::Protocol
