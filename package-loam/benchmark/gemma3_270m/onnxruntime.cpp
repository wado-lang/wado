// Gemma 3 (270M) on onnxruntime's own C++ API, against Microsoft's release as
// shipped (../fetch_onnxruntime.sh): the program generate.wado
// times. Each step feeds the new tokens and the attention cache of the rest,
// as the example's generate.mjs does.
//
//   mise run gemma3-270m   # builds and runs it

#include <onnxruntime_cxx_api.h>

#include <array>
#include <cstdint>
#include <string>
#include <vector>

#include "harness.hpp"

static const char* MODEL = "../example/gemma3-270m";
static const int LAYERS = 18;
static const int64_t KV_HEADS = 1;
static const int64_t DIM = 256;

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
    for (int s = 0; s < harness::STEPS; s++) {
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
        const int64_t best = harness::greatest(last, vocab);
        caches.clear();
        for (std::size_t i = 1; i < result.size(); i++) caches.push_back(std::move(result[i]));
        all.push_back(best);
        fresh = {best};
    }
    return std::vector<int64_t>(all.begin() + static_cast<std::ptrdiff_t>(ids.size()), all.end());
}

int main() {
    Ort::Env env(ORT_LOGGING_LEVEL_WARNING, "gemma3-270m");
    Ort::Session session(env, (std::string(MODEL) + "/build/model.onnx").c_str(), Ort::SessionOptions());
    const Names names;
    const std::vector<int64_t> ids = harness::prompt_ids(std::string(MODEL) + "/generate.json");
    harness::time([&] { return continued(session, names, ids); });
    return 0;
}
