// What the onnxruntime CUDA arms share: a decoder continued greedily, each step
// feeding the new tokens and the attention cache of the rest, as each
// example's generate.mjs does through tools/greedy.mjs.
#pragma once

#include <onnxruntime_cxx_api.h>

#include <array>
#include <cstdint>
#include <string>
#include <vector>

#include "harness.hpp"

namespace decoder {

struct Shape {
    int layers;
    int64_t kv_heads;
    int64_t dim;
    // Whether the graph takes `use_cache_branch`, as a merged decoder does:
    // true once a step is given a past.
    bool branched;
};

struct Names {
    std::vector<std::string> inputs;
    std::vector<std::string> outputs;

    // `past_key_values.l.kind` and `present.l.kind` in one order, so the
    // outputs after `logits` feed the next step's caches position for position.
    explicit Names(const Shape& shape) {
        inputs = {"input_ids", "attention_mask"};
        if (shape.branched) inputs.push_back("use_cache_branch");
        outputs = {"logits"};
        for (int l = 0; l < shape.layers; l++) {
            for (const char* kind : {"key", "value"}) {
                inputs.push_back("past_key_values." + std::to_string(l) + "." + kind);
                outputs.push_back("present." + std::to_string(l) + "." + kind);
            }
        }
    }
};

// A session on the CUDA provider.
inline Ort::SessionOptions on_cuda() {
    Ort::SessionOptions options;
    options.AppendExecutionProvider_CUDA(OrtCUDAProviderOptions{});
    return options;
}

// The STEPS tokens greedy decoding picks after `ids`. Only the logits come
// back to the host; the caches stay on the device.
inline std::vector<int64_t> continued(Ort::Session& session, const Shape& shape, const Names& names,
                                      const std::vector<int64_t>& ids) {
    const Ort::MemoryInfo host = Ort::MemoryInfo::CreateCpu(OrtArenaAllocator, OrtMemTypeDefault);
    const Ort::MemoryInfo device("Cuda", OrtDeviceAllocator, 0, OrtMemTypeDefault);
    Ort::AllocatorWithDefaultOptions allocator;
    std::vector<Ort::Value> caches;
    const std::array<int64_t, 4> empty = {1, shape.kv_heads, 0, shape.dim};
    for (int i = 0; i < 2 * shape.layers; i++) {
        caches.push_back(Ort::Value::CreateTensor<float>(allocator, empty.data(), empty.size()));
    }
    Ort::IoBinding binding(session);
    std::vector<int64_t> all = ids;
    std::vector<int64_t> fresh = ids;
    for (int s = 0; s < harness::STEPS; s++) {
        const std::array<int64_t, 2> fresh_shape = {1, static_cast<int64_t>(fresh.size())};
        std::vector<int64_t> mask(all.size(), 1);
        const std::array<int64_t, 2> mask_shape = {1, static_cast<int64_t>(mask.size())};
        bool past[1] = {s > 0};
        const std::array<int64_t, 1> past_shape = {1};
        binding.ClearBoundInputs();
        binding.ClearBoundOutputs();
        std::size_t at = 0;
        binding.BindInput(names.inputs[at++].c_str(),
                          Ort::Value::CreateTensor<int64_t>(host, fresh.data(), fresh.size(), fresh_shape.data(), 2));
        binding.BindInput(names.inputs[at++].c_str(),
                          Ort::Value::CreateTensor<int64_t>(host, mask.data(), mask.size(), mask_shape.data(), 2));
        if (shape.branched) {
            binding.BindInput(names.inputs[at++].c_str(),
                              Ort::Value::CreateTensor<bool>(host, past, 1, past_shape.data(), 1));
        }
        for (const Ort::Value& cache : caches) binding.BindInput(names.inputs[at++].c_str(), cache);
        binding.BindOutput(names.outputs[0].c_str(), host);
        for (std::size_t i = 1; i < names.outputs.size(); i++) {
            binding.BindOutput(names.outputs[i].c_str(), device);
        }
        session.Run(Ort::RunOptions{nullptr}, binding);
        std::vector<Ort::Value> result = binding.GetOutputValues();
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

}  // namespace decoder
