// GPT-2 (124M) on onnxruntime's own C++ API, against Microsoft's release as
// shipped (../fetch_onnxruntime.sh): the program generate.wado times. A numeric
// argument caps onnxruntime's threads, which it otherwise picks itself.
//
//   mise run gpt2-124m   # builds and runs it

#include <onnxruntime_cxx_api.h>

#include <array>
#include <cstdint>
#include <cstdlib>
#include <string>
#include <vector>

#include "harness.hpp"

static const char* MODEL = "../example/gpt2-124m";

static std::vector<int64_t> continued(Ort::Session& session, const std::vector<int64_t>& ids) {
    const Ort::MemoryInfo memory = Ort::MemoryInfo::CreateCpu(OrtArenaAllocator, OrtMemTypeDefault);
    const std::array<const char*, 2> inputs = {"input_ids", "attention_mask"};
    const std::array<const char*, 1> outputs = {"logits"};
    std::vector<int64_t> all = ids;
    for (int s = 0; s < harness::STEPS; s++) {
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
        all.push_back(harness::greatest(result[0].GetTensorData<float>() + (n - 1) * vocab, vocab));
    }
    return std::vector<int64_t>(all.begin() + static_cast<std::ptrdiff_t>(ids.size()), all.end());
}

int main(int argc, char** argv) {
    Ort::Env env(ORT_LOGGING_LEVEL_WARNING, "gpt2-124m");
    Ort::SessionOptions options;
    if (argc > 1) {
        options.SetIntraOpNumThreads(std::atoi(argv[1]));
        options.SetInterOpNumThreads(1);
    }
    Ort::Session session(env, (std::string(MODEL) + "/model.onnx").c_str(), options);
    const std::vector<int64_t> ids = harness::prompt_ids(std::string(MODEL) + "/tokenize.json");
    harness::time([&] { return continued(session, ids); });
    return 0;
}
