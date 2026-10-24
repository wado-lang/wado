// Gemma 3 (270M) on onnxruntime's own C++ API, against Microsoft's release as
// shipped (../fetch_onnxruntime.sh): the program generate.wado
// times. Each step feeds the new tokens and the attention cache of the rest,
// as the example's generate.mjs does. A numeric argument caps onnxruntime's
// threads, which it otherwise picks itself.
//
//   mise run gemma3-270m   # builds and runs it

#include <onnxruntime_cxx_api.h>

#include <array>
#include <chrono>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <fstream>
#include <sstream>
#include <string>
#include <vector>

static const char* MODEL = "../example/gemma3-270m";
static const char* PROMPT = "Hello, my name is";
static const int STEPS = 8;
static const int WARMUP = 1;
static const int ITERATIONS = 3;
static const int LAYERS = 18;
static const int64_t KV_HEADS = 1;
static const int64_t DIM = 256;

// The ids generate.json records for PROMPT: the `ids` list after the entry
// whose `prompt` it is.
static std::vector<int64_t> prompt_ids() {
    std::ifstream file(std::string(MODEL) + "/generate.json");
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
    std::vector<int64_t> ids;
    std::stringstream list(text.substr(open + 1, close - open - 1));
    std::string item;
    while (std::getline(list, item, ',')) ids.push_back(std::stoll(item));
    return ids;
}

struct Names {
    std::vector<std::string> inputs;
    std::vector<std::string> outputs;
    std::vector<const char*> input_ptrs;
    std::vector<const char*> output_ptrs;

    // `past_key_values.l.kind` and `present.l.kind` in one order, so the
    // outputs after `logits` feed the next step's caches position for position.
    Names() {
        inputs = {"input_ids", "attention_mask"};
        outputs = {"logits"};
        for (int l = 0; l < LAYERS; l++) {
            for (const char* kind : {"key", "value"}) {
                inputs.push_back("past_key_values." + std::to_string(l) + "." + kind);
                outputs.push_back("present." + std::to_string(l) + "." + kind);
            }
        }
        for (const std::string& n : inputs) input_ptrs.push_back(n.c_str());
        for (const std::string& n : outputs) output_ptrs.push_back(n.c_str());
    }
};

static std::vector<int64_t> continued(Ort::Session& session, const Names& names, const std::vector<int64_t>& ids) {
    const Ort::MemoryInfo memory = Ort::MemoryInfo::CreateCpu(OrtArenaAllocator, OrtMemTypeDefault);
    Ort::AllocatorWithDefaultOptions allocator;
    std::vector<Ort::Value> caches;
    const std::array<int64_t, 4> empty = {1, KV_HEADS, 0, DIM};
    for (int i = 0; i < 2 * LAYERS; i++) {
        caches.push_back(Ort::Value::CreateTensor<float>(allocator, empty.data(), empty.size()));
    }
    std::vector<int64_t> all = ids;
    std::vector<int64_t> fresh = ids;
    for (int s = 0; s < STEPS; s++) {
        const std::array<int64_t, 2> fresh_shape = {1, static_cast<int64_t>(fresh.size())};
        std::vector<int64_t> mask(all.size(), 1);
        const std::array<int64_t, 2> mask_shape = {1, static_cast<int64_t>(mask.size())};
        std::vector<Ort::Value> feeds;
        feeds.push_back(Ort::Value::CreateTensor<int64_t>(memory, fresh.data(), fresh.size(), fresh_shape.data(), 2));
        feeds.push_back(Ort::Value::CreateTensor<int64_t>(memory, mask.data(), mask.size(), mask_shape.data(), 2));
        for (Ort::Value& cache : caches) feeds.push_back(std::move(cache));
        auto result = session.Run(Ort::RunOptions{nullptr}, names.input_ptrs.data(), feeds.data(), feeds.size(),
                                  names.output_ptrs.data(), names.output_ptrs.size());
        const int64_t vocab = result[0].GetTensorTypeAndShapeInfo().GetShape()[2];
        const float* last = result[0].GetTensorData<float>() + (static_cast<int64_t>(fresh.size()) - 1) * vocab;
        int64_t best = 0;
        for (int64_t v = 1; v < vocab; v++) {
            if (last[v] > last[best]) best = v;
        }
        caches.clear();
        for (std::size_t i = 1; i < result.size(); i++) caches.push_back(std::move(result[i]));
        all.push_back(best);
        fresh = {best};
    }
    return std::vector<int64_t>(all.begin() + static_cast<std::ptrdiff_t>(ids.size()), all.end());
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

int main(int argc, char** argv) {
    Ort::Env env(ORT_LOGGING_LEVEL_WARNING, "gemma3-270m");
    Ort::SessionOptions options;
    if (argc > 1) {
        options.SetIntraOpNumThreads(std::atoi(argv[1]));
        options.SetInterOpNumThreads(1);
    }
    Ort::Session session(env, (std::string(MODEL) + "/build/model.onnx").c_str(), options);
    const Names names;
    const std::vector<int64_t> ids = prompt_ids();

    std::vector<int64_t> generated;
    for (int i = 0; i < WARMUP; i++) generated = continued(session, names, ids);
    const auto started = std::chrono::steady_clock::now();
    for (int i = 0; i < ITERATIONS; i++) generated = continued(session, names, ids);
    const auto elapsed = std::chrono::steady_clock::now() - started;
    print_throughput("generate", STEPS, ITERATIONS,
                     static_cast<double>(std::chrono::duration_cast<std::chrono::nanoseconds>(elapsed).count()),
                     "tokens");
    std::printf("generated = [");
    for (std::size_t i = 0; i < generated.size(); i++) {
        std::printf(i ? ", %lld" : "%lld", static_cast<long long>(generated[i]));
    }
    std::printf("]\n");
    return 0;
}
