// GPT-2 (124M) on onnxruntime's CUDA provider, through its own C++ API,
// against Microsoft's release as shipped (../fetch_onnxruntime_cuda.sh): the
// program generate.wado times.
//
//   mise run gpt2-124m   # builds and runs it

#include <onnxruntime_cxx_api.h>

#include <cstdint>
#include <string>
#include <vector>

#include "decoder.hpp"
#include "harness.hpp"

static const char* MODEL = "../example/gpt2-124m";

int main() {
    Ort::Env env(ORT_LOGGING_LEVEL_WARNING, "gpt2-124m");
    Ort::Session session(env, (std::string(MODEL) + "/model.onnx").c_str(), decoder::on_cuda());
    const decoder::Shape shape = {12, 12, 64, true};
    const decoder::Names names(shape);
    const std::vector<int64_t> ids = harness::prompt_ids(std::string(MODEL) + "/tokenize.json");
    harness::time([&] { return decoder::continued(session, shape, names, ids); });
    return 0;
}
