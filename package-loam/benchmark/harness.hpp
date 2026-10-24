// What every C++ arm shares with generate.wado: the prompt, the step count,
// the warmup and timed runs, and the lines `core:benchmark` prints, which
// `benchmark/logs.ts` reads.
#pragma once

#include <chrono>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <fstream>
#include <sstream>
#include <string>
#include <vector>

namespace harness {

inline const char* const PROMPT = "Hello, my name is";
inline constexpr int STEPS = 8;
inline constexpr int WARMUP = 1;
inline constexpr int ITERATIONS = 3;

// The ids the JSON file `oracle` records for PROMPT: the `ids` list of the
// entry that holds the string PROMPT.
inline std::vector<int64_t> prompt_ids(const std::string& oracle) {
    std::ifstream file(oracle);
    if (!file) {
        std::fprintf(stderr, "cannot read %s\n", oracle.c_str());
        std::exit(1);
    }
    std::stringstream buf;
    buf << file.rdbuf();
    const std::string text = buf.str();
    // An entry is a flat object, so it spans the braces around the string.
    const std::size_t at = text.find("\"" + std::string(PROMPT) + "\"");
    const std::size_t from = at == std::string::npos ? at : text.rfind('{', at);
    const std::size_t to = text.find('}', at);
    const std::size_t open = text.find('[', text.find("\"ids\"", from));
    const std::size_t close = text.find(']', open);
    if (from == std::string::npos || to == std::string::npos || open > to || close > to) {
        std::fprintf(stderr, "%s records no ids for \"%s\"\n", oracle.c_str(), PROMPT);
        std::exit(1);
    }
    std::vector<int64_t> ids;
    std::stringstream list(text.substr(open + 1, close - open - 1));
    std::string item;
    while (std::getline(list, item, ',')) ids.push_back(std::stoll(item));
    return ids;
}

// The index of the greatest of `vocab` logits.
template <class Token>
Token greatest(const float* logits, Token vocab) {
    Token best = 0;
    for (Token v = 1; v < vocab; v++) {
        if (logits[v] > logits[best]) best = v;
    }
    return best;
}

inline void print_throughput(double elapsed_ns) {
    const double rate = STEPS * ITERATIONS / (elapsed_ns / 1e9);
    const double per_ms = elapsed_ns / ITERATIONS / 1e6;
    std::printf("generate: %.2f tokens/s   (%.3f ms/iter, %d iter)\n", rate, per_ms, ITERATIONS);
}

// Runs `continued`, which generates STEPS tokens, WARMUP times untimed and
// ITERATIONS times timed, and prints the throughput and the last run's tokens.
template <class Continued>
void time(Continued continued) {
    for (int i = 0; i < WARMUP; i++) continued();
    const auto started = std::chrono::steady_clock::now();
    auto generated = continued();
    for (int i = 1; i < ITERATIONS; i++) generated = continued();
    const auto elapsed = std::chrono::steady_clock::now() - started;
    print_throughput(static_cast<double>(std::chrono::duration_cast<std::chrono::nanoseconds>(elapsed).count()));
    std::printf("generated = [");
    for (std::size_t i = 0; i < generated.size(); i++) {
        std::printf(i ? ", %lld" : "%lld", static_cast<long long>(generated[i]));
    }
    std::printf("]\n");
}

}  // namespace harness
