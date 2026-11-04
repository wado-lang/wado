// Gemma 3 (270M) on onnxruntime's CUDA provider, through its own C++ API,
// against Microsoft's release as shipped (../fetch_onnxruntime_cuda.sh): the
// program generate.wado times.
//
//   mise run gemma3-270m   # builds and runs it

#include <onnxruntime_cxx_api.h>

#include <cstdint>
#include <string>
#include <vector>

#include "decoder.hpp"
#include "harness.hpp"

static const char* MODEL = "../example/gemma3-270m";

int main() {
    Ort::Env env(ORT_LOGGING_LEVEL_WARNING, "gemma3-270m");
    Ort::Session session(env, (std::string(MODEL) + "/build/model.onnx").c_str(), decoder::on_cuda());
    const decoder::Shape shape = {18, 1, 256, false};
    const decoder::Names names(shape);
    const std::vector<int64_t> ids = harness::prompt_ids(std::string(MODEL) + "/generate.json");
    harness::time([&] { return decoder::continued(session, shape, names, ids); });
    return 0;
}
