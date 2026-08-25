#include <cuda_runtime.h>

#include <algorithm>
#include <cmath>
#include <cstddef>
#include <cstdint>
#include <cstring>
#include <chrono>

namespace {

constexpr int kBlockElements = 64;
constexpr int kSubblockElements = 16;
constexpr int kBlockBytes = 36;
constexpr int kPayloadHeaderBytes = 32;

struct CudaMoeCallTiming {
    double payload_validation_ms;
    double alloc_ms;
    double h2d_ms;
    uint64_t h2d_bytes;
    double kernel_ms;
    double launch_ms;
    double sync_ms;
    double d2h_ms;
    uint64_t d2h_bytes;
    double free_ms;
};

using Clock = std::chrono::steady_clock;

double elapsed_ms(Clock::time_point start, Clock::time_point end) {
    return std::chrono::duration<double, std::milli>(end - start).count();
}

__device__ float fp4_value(uint8_t value) {
    constexpr float values[16] = {
        0.0f, 1.0f, 2.0f, 3.0f, 4.0f, 6.0f, 8.0f, 12.0f,
        0.0f, -1.0f, -2.0f, -3.0f, -4.0f, -6.0f, -8.0f, -12.0f,
    };
    return values[value & 0x0f];
}

__device__ float ue4m3_to_f32(uint8_t value) {
    if (value == 0 || value == 0x7f) {
        return 0.0f;
    }
    const int exponent = (value >> 3) & 0x0f;
    const float mantissa = static_cast<float>(value & 0x07);
    const float raw = exponent == 0
        ? mantissa * exp2f(-9.0f)
        : (1.0f + mantissa / 8.0f) * exp2f(static_cast<float>(exponent - 7));
    return raw * 0.5f;
}

__device__ float dot_nvfp4(
    const uint8_t * matrix,
    int row,
    int columns,
    const float * input
) {
    const int row_bytes = (columns / kBlockElements) * kBlockBytes;
    const uint8_t * encoded = matrix + row * row_bytes;
    float result = 0.0f;
    for (int block = 0; block < columns / kBlockElements; ++block) {
        const uint8_t * packed = encoded + block * kBlockBytes;
        for (int subblock = 0; subblock < kBlockElements / kSubblockElements; ++subblock) {
            const float scale = ue4m3_to_f32(packed[subblock]);
            const uint8_t * values = packed + 4 + subblock * (kSubblockElements / 2);
            int input_start = block * kBlockElements + subblock * kSubblockElements;
            for (int index = 0; index < kSubblockElements / 2; ++index) {
                result += scale * fp4_value(values[index]) * input[input_start + index];
                result += scale * fp4_value(values[index] >> 4) * input[input_start + index + kSubblockElements / 2];
            }
        }
    }
    return result;
}

__global__ void gate_up_kernel(
    const uint8_t * gate_up,
    int intermediate,
    int hidden,
    const float * hidden_values,
    float gate_scale,
    float up_scale,
    float * gate_up_values
) {
    const int row = blockIdx.x * blockDim.x + threadIdx.x;
    if (row >= 2 * intermediate) {
        return;
    }
    const bool is_up = row >= intermediate;
    const float value = dot_nvfp4(
        gate_up,
        row,
        hidden,
        hidden_values
    );
    gate_up_values[row] = value * (is_up ? up_scale : gate_scale);
}

__device__ float gelu(float value) {
    return 0.5f * value * (1.0f + tanhf(0.7978846f * (value + 0.044715f * value * value * value)));
}

__global__ void down_kernel(
    const uint8_t * down,
    int hidden,
    int intermediate,
    const float * gate_up_values,
    float down_scale,
    float * output
) {
    const int row = blockIdx.x * blockDim.x + threadIdx.x;
    if (row >= hidden) {
        return;
    }
    const int row_bytes = (intermediate / kBlockElements) * kBlockBytes;
    const uint8_t * encoded = down + row * row_bytes;
    float result = 0.0f;
    for (int block = 0; block < intermediate / kBlockElements; ++block) {
        const uint8_t * packed = encoded + block * kBlockBytes;
        for (int subblock = 0; subblock < kBlockElements / kSubblockElements; ++subblock) {
            const float scale = ue4m3_to_f32(packed[subblock]);
            const uint8_t * values = packed + 4 + subblock * (kSubblockElements / 2);
            int input_start = block * kBlockElements + subblock * kSubblockElements;
            for (int index = 0; index < kSubblockElements / 2; ++index) {
                const float first = gelu(gate_up_values[input_start]) * gate_up_values[intermediate + input_start];
                const float second = gelu(gate_up_values[input_start + kSubblockElements / 2]) * gate_up_values[intermediate + input_start + kSubblockElements / 2];
                result += scale * fp4_value(values[index]) * first;
                result += scale * fp4_value(values[index] >> 4) * second;
                ++input_start;
            }
        }
    }
    output[row] = result * down_scale;
}

int cuda_status(cudaError_t error) {
    return error == cudaSuccess ? 0 : static_cast<int>(error);
}

} // namespace

extern "C" int pom_moe_nvfp4_forward(
    const uint8_t * payload,
    std::size_t payload_len,
    const float * hidden,
    std::size_t hidden_len,
    float * output,
    std::size_t output_len,
    std::size_t hidden_width,
    std::size_t intermediate_width,
    CudaMoeCallTiming * timing
) {
    if (timing != nullptr) {
        *timing = {};
    }
    if (payload == nullptr || hidden == nullptr || output == nullptr ||
        hidden_width == 0 || intermediate_width == 0 ||
        hidden_width % kBlockElements != 0 || intermediate_width % kBlockElements != 0 ||
        hidden_len != hidden_width || output_len != hidden_width || payload_len < kPayloadHeaderBytes) {
        return 1;
    }

    const auto * header = payload;
    const std::size_t gate_up_len = static_cast<std::size_t>(header[0]) |
        (static_cast<std::size_t>(header[1]) << 8) |
        (static_cast<std::size_t>(header[2]) << 16) |
        (static_cast<std::size_t>(header[3]) << 24);
    const std::size_t down_len = static_cast<std::size_t>(header[4]) |
        (static_cast<std::size_t>(header[5]) << 8) |
        (static_cast<std::size_t>(header[6]) << 16) |
        (static_cast<std::size_t>(header[7]) << 24);
    const std::size_t gate_up_expected = 2 * intermediate_width * (hidden_width / kBlockElements) * kBlockBytes;
    const std::size_t down_expected = hidden_width * (intermediate_width / kBlockElements) * kBlockBytes;
    if (gate_up_len != gate_up_expected || down_len != down_expected ||
        gate_up_len > payload_len - kPayloadHeaderBytes ||
        down_len != payload_len - kPayloadHeaderBytes - gate_up_len) {
        return 1;
    }

    const auto * gate_up = payload + kPayloadHeaderBytes;
    const auto * down = gate_up + gate_up_len;
    float * d_hidden = nullptr;
    uint8_t * d_gate_up = nullptr;
    uint8_t * d_down = nullptr;
    float * d_gate_up_values = nullptr;
    float * d_output = nullptr;
    cudaEvent_t gate_start = nullptr;
    cudaEvent_t gate_end = nullptr;
    cudaEvent_t down_start = nullptr;
    cudaEvent_t down_end = nullptr;
    int result = 0;

    do {
        const auto alloc_start = Clock::now();
        result = cuda_status(cudaMalloc(reinterpret_cast<void **>(&d_hidden), hidden_width * sizeof(float)));
        if (result != 0) break;
        result = cuda_status(cudaMalloc(reinterpret_cast<void **>(&d_gate_up), gate_up_len));
        if (result != 0) break;
        result = cuda_status(cudaMalloc(reinterpret_cast<void **>(&d_down), down_len));
        if (result != 0) break;
        result = cuda_status(cudaMalloc(reinterpret_cast<void **>(&d_gate_up_values), 2 * intermediate_width * sizeof(float)));
        if (result != 0) break;
        result = cuda_status(cudaMalloc(reinterpret_cast<void **>(&d_output), hidden_width * sizeof(float)));
        if (result != 0) break;
        if (timing != nullptr) {
            timing->alloc_ms = elapsed_ms(alloc_start, Clock::now());
        }

        if (timing != nullptr) {
            result = cuda_status(cudaEventCreate(&gate_start));
            if (result != 0) break;
            result = cuda_status(cudaEventCreate(&gate_end));
            if (result != 0) break;
            result = cuda_status(cudaEventCreate(&down_start));
            if (result != 0) break;
            result = cuda_status(cudaEventCreate(&down_end));
            if (result != 0) break;
        }

        const auto h2d_start = Clock::now();
        result = cuda_status(cudaMemcpy(d_hidden, hidden, hidden_width * sizeof(float), cudaMemcpyHostToDevice));
        if (result != 0) break;
        result = cuda_status(cudaMemcpy(d_gate_up, gate_up, gate_up_len, cudaMemcpyHostToDevice));
        if (result != 0) break;
        result = cuda_status(cudaMemcpy(d_down, down, down_len, cudaMemcpyHostToDevice));
        if (result != 0) break;
        if (timing != nullptr) {
            timing->h2d_ms = elapsed_ms(h2d_start, Clock::now());
            timing->h2d_bytes = static_cast<uint64_t>(hidden_width * sizeof(float) + gate_up_len + down_len);
        }

        float scales_host[6];
        std::memcpy(scales_host, payload + 8, sizeof(scales_host));

        const int block_size = 128;
        const auto gate_launch_start = Clock::now();
        if (timing != nullptr) {
            result = cuda_status(cudaEventRecord(gate_start));
            if (result != 0) break;
        }
        gate_up_kernel<<<(2 * intermediate_width + block_size - 1) / block_size, block_size>>>(
            d_gate_up, static_cast<int>(intermediate_width), static_cast<int>(hidden_width), d_hidden,
            scales_host[0], scales_host[1], d_gate_up_values);
        result = cuda_status(cudaGetLastError());
        if (result != 0) break;
        if (timing != nullptr) {
            timing->launch_ms += elapsed_ms(gate_launch_start, Clock::now());
            result = cuda_status(cudaEventRecord(gate_end));
            if (result != 0) break;
        }
        const auto gate_sync_start = Clock::now();
        result = cuda_status(cudaDeviceSynchronize());
        if (result != 0) break;
        if (timing != nullptr) {
            float kernel_ms = 0.0f;
            result = cuda_status(cudaEventElapsedTime(&kernel_ms, gate_start, gate_end));
            if (result != 0) break;
            timing->kernel_ms += kernel_ms;
            timing->sync_ms += std::max(0.0, elapsed_ms(gate_sync_start, Clock::now()) - static_cast<double>(kernel_ms));
        }

        const auto down_launch_start = Clock::now();
        if (timing != nullptr) {
            result = cuda_status(cudaEventRecord(down_start));
            if (result != 0) break;
        }
        down_kernel<<<(hidden_width + block_size - 1) / block_size, block_size>>>(
            d_down, static_cast<int>(hidden_width), static_cast<int>(intermediate_width), d_gate_up_values,
            scales_host[2], d_output);
        result = cuda_status(cudaGetLastError());
        if (result != 0) break;
        if (timing != nullptr) {
            timing->launch_ms += elapsed_ms(down_launch_start, Clock::now());
            result = cuda_status(cudaEventRecord(down_end));
            if (result != 0) break;
        }
        const auto down_sync_start = Clock::now();
        result = cuda_status(cudaDeviceSynchronize());
        if (result != 0) break;
        if (timing != nullptr) {
            float kernel_ms = 0.0f;
            result = cuda_status(cudaEventElapsedTime(&kernel_ms, down_start, down_end));
            if (result != 0) break;
            timing->kernel_ms += kernel_ms;
            timing->sync_ms += std::max(0.0, elapsed_ms(down_sync_start, Clock::now()) - static_cast<double>(kernel_ms));
        }
        const auto d2h_start = Clock::now();
        result = cuda_status(cudaMemcpy(output, d_output, hidden_width * sizeof(float), cudaMemcpyDeviceToHost));
        if (timing != nullptr) {
            timing->d2h_ms = elapsed_ms(d2h_start, Clock::now());
            timing->d2h_bytes = static_cast<uint64_t>(hidden_width * sizeof(float));
        }
    } while (false);

    if (gate_start != nullptr) cudaEventDestroy(gate_start);
    if (gate_end != nullptr) cudaEventDestroy(gate_end);
    if (down_start != nullptr) cudaEventDestroy(down_start);
    if (down_end != nullptr) cudaEventDestroy(down_end);
    const auto free_start = Clock::now();
    cudaFree(d_output);
    cudaFree(d_gate_up_values);
    cudaFree(d_down);
    cudaFree(d_gate_up);
    cudaFree(d_hidden);
    if (timing != nullptr) {
        timing->free_ms = elapsed_ms(free_start, Clock::now());
    }
    return result;
}
