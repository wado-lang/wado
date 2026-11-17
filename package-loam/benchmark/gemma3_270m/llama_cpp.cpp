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

#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <vector>

#include "harness.hpp"

// `tokens` at the positions from `past` on, with logits for the last alone.
static void decode(llama_context* ctx, llama_batch_ext* batch, const std::vector<llama_token>& tokens, llama_pos past) {
    llama_batch_ext_clear(batch);
    for (std::size_t i = 0; i < tokens.size(); i++) {
        const int32_t at = llama_batch_ext_add_token(batch, 0, tokens[i]);
        const llama_pos pos = past + static_cast<llama_pos>(i);
        if (at < 0 || !llama_batch_ext_set_pos(batch, at, &pos) ||
            !llama_batch_ext_set_output_logits(batch, at, i + 1 == tokens.size())) {
            std::fprintf(stderr, "cannot add a token to the batch\n");
            std::exit(1);
        }
    }
    if (llama_process(ctx, LLAMA_PROCESS_TYPE_DECODE, batch) != 0) {
        std::fprintf(stderr, "llama_process failed\n");
        std::exit(1);
    }
}

static std::vector<llama_token> continued(llama_context* ctx, llama_batch_ext* batch, int32_t vocab,
                                          const std::vector<llama_token>& ids) {
    llama_memory_clear(llama_get_memory(ctx), true);
    std::vector<llama_token> fresh = ids;
    std::vector<llama_token> generated;
    llama_pos past = 0;
    for (int s = 0; s < harness::STEPS; s++) {
        decode(ctx, batch, fresh, past);
        past += static_cast<llama_pos>(fresh.size());
        const llama_token next = harness::greatest(llama_get_logits_ith(ctx, -1), vocab);
        generated.push_back(next);
        fresh = {next};
    }
    return generated;
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
    const std::vector<int64_t> oracle = harness::prompt_ids("../example/gemma3-270m/generate.json");
    const std::vector<llama_token> ids(oracle.begin(), oracle.end());
    llama_context_params ctx_params = llama_context_default_params();
    ctx_params.n_ctx = static_cast<uint32_t>(ids.size() + harness::STEPS);
    ctx_params.n_batch = static_cast<uint32_t>(ids.size());
    ctx_params.n_threads = std::atoi(argv[4]);
    ctx_params.n_threads_batch = ctx_params.n_threads;
    llama_context* ctx = llama_init_from_model(model, ctx_params);
    if (ctx == nullptr) {
        std::fprintf(stderr, "cannot create a context\n");
        return 1;
    }
    const int32_t vocab = llama_vocab_n_tokens(llama_model_get_vocab(model));
    llama_batch_ext* batch = llama_batch_ext_init(ctx);
    harness::time([&] { return continued(ctx, batch, vocab, ids); });

    llama_batch_ext_free(batch);
    llama_free(ctx);
    llama_model_free(model);
    llama_backend_free();
    return 0;
}
