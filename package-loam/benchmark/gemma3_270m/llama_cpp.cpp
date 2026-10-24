// Gemma 3 (270M) on llama.cpp's own C API, against its release as shipped
// (../fetch_llama_cpp.sh): the program generate.wado times, from the prompt ids
// the example's generate.json records. Each run clears the attention cache,
// reads the prompt in one batch, then each new token alone against the cache.
//
//   llama_cpp <model.gguf> <backends dir> <gpu layers> <threads>
//
// `mise run gemma3-270m` builds and runs it.

#include <ggml-backend.h>
#include <llama.h>

#include <chrono>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <fstream>
#include <sstream>
#include <string>
#include <vector>

static const char* ORACLE = "../example/gemma3-270m/generate.json";
static const char* PROMPT = "Hello, my name is";
static const int STEPS = 8;
static const int WARMUP = 1;
static const int ITERATIONS = 3;

// The ids generate.json records for PROMPT: the `ids` list after the entry
// whose `prompt` it is.
static std::vector<llama_token> prompt_ids() {
    std::ifstream file(ORACLE);
    std::stringstream buf;
    buf << file.rdbuf();
    const std::string text = buf.str();
    const std::size_t entry = text.find("\"" + std::string(PROMPT) + "\"");
    const std::size_t open = text.find('[', text.find("\"ids\"", entry));
    const std::size_t close = text.find(']', open);
    if (entry == std::string::npos || open == std::string::npos || close == std::string::npos) {
        std::fprintf(stderr, "generate.json records no ids for \"%s\"\n", PROMPT);
        std::exit(1);
    }
    std::vector<llama_token> ids;
    std::stringstream list(text.substr(open + 1, close - open - 1));
    std::string item;
    while (std::getline(list, item, ',')) ids.push_back(std::stoi(item));
    return ids;
}

static void decode(llama_context* ctx, std::vector<llama_token>& tokens) {
    if (llama_decode(ctx, llama_batch_get_one(tokens.data(), static_cast<int32_t>(tokens.size()))) != 0) {
        std::fprintf(stderr, "llama_decode failed\n");
        std::exit(1);
    }
}

// The token the last decode ranks first.
static llama_token greatest(llama_context* ctx, int32_t vocab) {
    const float* logits = llama_get_logits_ith(ctx, -1);
    llama_token best = 0;
    for (llama_token v = 1; v < vocab; v++) {
        if (logits[v] > logits[best]) best = v;
    }
    return best;
}

static std::vector<llama_token> continued(llama_context* ctx, int32_t vocab, const std::vector<llama_token>& ids) {
    llama_memory_clear(llama_get_memory(ctx), true);
    std::vector<llama_token> fresh = ids;
    std::vector<llama_token> generated;
    for (int s = 0; s < STEPS; s++) {
        decode(ctx, fresh);
        const llama_token next = greatest(ctx, vocab);
        generated.push_back(next);
        fresh = {next};
    }
    return generated;
}

// The throughput line `core:benchmark` prints, which `logs.ts` reads.
static void print_throughput(const char* label, int work_per_iter, int n, double elapsed_ns, const char* unit) {
    const double secs = elapsed_ns / 1e9;
    const double rate = secs > 0 ? static_cast<double>(work_per_iter) * n / secs : 0;
    const double per_ms = elapsed_ns / n / 1e6;
    char rbuf[64];
    if (rate >= 1e9) std::snprintf(rbuf, sizeof rbuf, "%.2f G %s/s", rate / 1e9, unit);
    else if (rate >= 1e6) std::snprintf(rbuf, sizeof rbuf, "%.2f M %s/s", rate / 1e6, unit);
    else if (rate >= 1e3) std::snprintf(rbuf, sizeof rbuf, "%.2f k %s/s", rate / 1e3, unit);
    else std::snprintf(rbuf, sizeof rbuf, "%.2f %s/s", rate, unit);
    std::printf("%s: %s   (%.3f ms/iter, %d iter)\n", label, rbuf, per_ms, n);
}

static void quiet(ggml_log_level level, const char* text, void*) {
    if (level >= GGML_LOG_LEVEL_WARN) std::fputs(text, stderr);
}

int main(int argc, char** argv) {
    if (argc != 5) {
        std::fprintf(stderr, "usage: %s <model.gguf> <backends dir> <gpu layers> <threads>\n", argv[0]);
        return 1;
    }
    llama_log_set(quiet, nullptr);
    ggml_backend_load_all_from_path(argv[2]);
    llama_backend_init();

    llama_model_params model_params = llama_model_default_params();
    model_params.n_gpu_layers = std::atoi(argv[3]);
    llama_model* model = llama_model_load_from_file(argv[1], model_params);
    if (model == nullptr) {
        std::fprintf(stderr, "cannot load %s\n", argv[1]);
        return 1;
    }
    const std::vector<llama_token> ids = prompt_ids();
    llama_context_params ctx_params = llama_context_default_params();
    ctx_params.n_ctx = static_cast<uint32_t>(ids.size() + STEPS);
    ctx_params.n_batch = static_cast<uint32_t>(ids.size());
    ctx_params.n_threads = std::atoi(argv[4]);
    ctx_params.n_threads_batch = ctx_params.n_threads;
    llama_context* ctx = llama_init_from_model(model, ctx_params);
    if (ctx == nullptr) {
        std::fprintf(stderr, "cannot create a context\n");
        return 1;
    }
    const int32_t vocab = llama_vocab_n_tokens(llama_model_get_vocab(model));

    std::vector<llama_token> generated;
    for (int i = 0; i < WARMUP; i++) generated = continued(ctx, vocab, ids);
    const auto started = std::chrono::steady_clock::now();
    for (int i = 0; i < ITERATIONS; i++) generated = continued(ctx, vocab, ids);
    const auto elapsed = std::chrono::steady_clock::now() - started;
    print_throughput("generate", STEPS, ITERATIONS,
                     static_cast<double>(std::chrono::duration_cast<std::chrono::nanoseconds>(elapsed).count()),
                     "tokens");
    std::printf("generated = [");
    for (std::size_t i = 0; i < generated.size(); i++) std::printf(i ? ", %d" : "%d", generated[i]);
    std::printf("]\n");

    llama_free(ctx);
    llama_model_free(model);
    llama_backend_free();
    return 0;
}
