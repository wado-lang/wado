// GPT-2 (124M) on onnxruntime's own C++ API, against Microsoft's release as
// shipped (fetch_onnxruntime.sh): the program `onnxruntime.mjs` runs, with
// nothing between the generation loop and the session. A numeric argument caps
// onnxruntime's threads, which it otherwise picks itself.
//
//   mise run gpt2-124m   # builds and runs it

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

static const char* MODEL = "../example/gpt2-124m";
static const char* PROMPT = "Hello, my name is";
static const int STEPS = 8;
static const int WARMUP = 1;
static const int ITERATIONS = 3;

// The ids tokenize.json records for PROMPT: the `ids` list after the entry
// whose `text` it is.
static std::vector<int64_t> prompt_ids() {
    std::ifstream file(std::string(MODEL) + "/tokenize.json");
    std::stringstream buf;
    buf << file.rdbuf();
    const std::string text = buf.str();
    const std::size_t entry = text.find("\"" + std::string(PROMPT) + "\"");
    const std::size_t open = text.find('[', text.find("\"ids\"", entry));
    const std::size_t close = text.find(']', open);
    if (entry == std::string::npos || open == std::string::npos || close == std::string::npos) {
        std::fprintf(stderr, "tokenize.json records no ids for \"%s\"\n", PROMPT);
        std::exit(1);
    }
    std::vector<int64_t> ids;
    std::stringstream list(text.substr(open + 1, close - open - 1));
    std::string item;
    while (std::getline(list, item, ',')) ids.push_back(std::stoll(item));
    return ids;
}

static std::vector<int64_t> continued(Ort::Session& session, const std::vector<int64_t>& ids) {
    const Ort::MemoryInfo memory = Ort::MemoryInfo::CreateCpu(OrtArenaAllocator, OrtMemTypeDefault);
    const std::array<const char*, 2> inputs = {"input_ids", "attention_mask"};
    const std::array<const char*, 1> outputs = {"logits"};
    std::vector<int64_t> all = ids;
    for (int s = 0; s < STEPS; s++) {
        const int64_t n = static_cast<int64_t>(all.size());
        const std::array<int64_t, 2> shape = {1, n};
        std::vector<int64_t> mask(all.size(), 1);
        std::array<Ort::Value, 2> values = {
            Ort::Value::CreateTensor<int64_t>(memory, all.data(), all.size(), shape.data(), shape.size()),
            Ort::Value::CreateTensor<int64_t>(memory, mask.data(), mask.size(), shape.data(), shape.size()),
        };
        auto result = session.Run(Ort::RunOptions{nullptr}, inputs.data(), values.data(), values.size(),
                                  outputs.data(), outputs.size());
        const int64_t vocab = result[0].GetTensorTypeAndShapeInfo().GetShape()[2];
        const float* last = result[0].GetTensorData<float>() + (n - 1) * vocab;
        int64_t best = 0;
        for (int64_t v = 1; v < vocab; v++) {
            if (last[v] > last[best]) best = v;
        }
        all.push_back(best);
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
    Ort::Env env(ORT_LOGGING_LEVEL_WARNING, "gpt2-124m");
    Ort::SessionOptions options;
    if (argc > 1) {
        options.SetIntraOpNumThreads(std::atoi(argv[1]));
        options.SetInterOpNumThreads(1);
    }
    Ort::Session session(env, (std::string(MODEL) + "/model.onnx").c_str(), options);
    const std::vector<int64_t> ids = prompt_ids();

    std::vector<int64_t> generated;
    for (int i = 0; i < WARMUP; i++) generated = continued(session, ids);
    const auto started = std::chrono::steady_clock::now();
    for (int i = 0; i < ITERATIONS; i++) generated = continued(session, ids);
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
