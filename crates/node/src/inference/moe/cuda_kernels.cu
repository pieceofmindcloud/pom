#include <cuda_runtime.h>
#include <cuda_fp16.h>

#include <algorithm>
#include <cmath>
#include <cstddef>
#include <cstdint>
#include <cstring>
#include <chrono>

// Os layouts e codebooks IQ são parte da convenção GGML. Incluí-los aqui
// evita uma segunda tabela que poderia divergir do decoder de referência;
// o executor e os kernels abaixo continuam sendo específicos deste motor.
#define GGML_COMMON_DECL_CUDA
#define GGML_COMMON_IMPL_CUDA
#include "../../../../../vendor/llama-cpp-sys-2/llama.cpp/ggml/src/ggml-common.h"

namespace {

constexpr int kBlockElements = 64;
constexpr int kSubblockElements = 16;
constexpr int kBlockBytes = 36;
constexpr int kPayloadHeaderBytes = 32;
constexpr int kKQuantPayloadHeaderBytes = 8;

struct CudaMoeCallTiming {
    uint64_t expert_calls;
    uint64_t kernel_launches;
    uint64_t sync_count;
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

__device__ __forceinline__ float round_to_f16(float value) {
    return __half2float(__float2half_rn(value));
}

__device__ __forceinline__ float payload_scale(const uint8_t * payload, int index) {
    const std::size_t offset = 8 + static_cast<std::size_t>(index) * sizeof(float);
    uint32_t bits = static_cast<uint32_t>(payload[offset]);
    bits |= static_cast<uint32_t>(payload[offset + 1]) << 8;
    bits |= static_cast<uint32_t>(payload[offset + 2]) << 16;
    bits |= static_cast<uint32_t>(payload[offset + 3]) << 24;
    return __uint_as_float(bits);
}

__global__ void quantize_q8_0_kernel(
    const float * input,
    int length,
    float * scales,
    int8_t * values,
    int vector_count
) {
    __shared__ float maxima[32];
    const int lane = threadIdx.x;
    const int blocks_per_vector = (length + 31) / 32;
    const int vector = blockIdx.x / blocks_per_vector;
    const int block = blockIdx.x % blocks_per_vector;
    if (vector >= vector_count) {
        return;
    }
    const int offset = block * 32 + lane;
    const int index = vector * length + offset;
    const float value = offset < length ? input[index] : 0.0f;
    maxima[lane] = fabsf(value);
    __syncthreads();

    for (int stride = 16; stride > 0; stride >>= 1) {
        if (lane < stride) {
            maxima[lane] = fmaxf(maxima[lane], maxima[lane + stride]);
        }
        __syncthreads();
    }

    const float max_scalar = maxima[0];
    const float unrounded_scale = max_scalar / 127.0f;
    const float scale = round_to_f16(unrounded_scale);
    if (lane == 0) {
        scales[vector * blocks_per_vector + block] = scale;
    }
    __syncthreads();

    if (offset < length) {
        const float inverse_scale = max_scalar != 0.0f ? 127.0f / max_scalar : 0.0f;
        int quantized = __float2int_rn(input[index] * inverse_scale);
        quantized = max(-128, min(127, quantized));
        values[index] = static_cast<int8_t>(quantized);
    }
}

__device__ float dot_nvfp4(
    const uint8_t * matrix,
    int row,
    int columns,
    const int8_t * input_values,
    const float * input_scales
) {
    const int row_bytes = (columns / kBlockElements) * kBlockBytes;
    const uint8_t * encoded = matrix + row * row_bytes;
    float result = 0.0f;
    for (int block = 0; block < columns / kBlockElements; ++block) {
        const uint8_t * packed = encoded + block * kBlockBytes;
        for (int subblock = 0; subblock < kBlockElements / kSubblockElements; ++subblock) {
            const float scale = ue4m3_to_f32(packed[subblock]);
            const uint8_t * values = packed + 4 + subblock * (kSubblockElements / 2);
            const int q8_block = block * 2 + subblock / 2;
            const int q8_start = q8_block * 32 + (subblock % 2) * kSubblockElements;
            const float input_scale = input_scales[q8_block];
            int sum_lo = 0;
            int sum_hi = 0;
            for (int index = 0; index < kSubblockElements / 2; ++index) {
                sum_lo += static_cast<int>(input_values[q8_start + index]) *
                    static_cast<int>(fp4_value(values[index]));
                sum_hi += static_cast<int>(input_values[q8_start + index + kSubblockElements / 2]) *
                    static_cast<int>(fp4_value(values[index] >> 4));
            }
            result += input_scale * scale * static_cast<float>(sum_lo + sum_hi);
        }
    }
    return result;
}

__global__ void gate_up_kernel(
    const uint8_t * payloads,
    std::size_t payload_stride,
    int batch,
    int intermediate,
    int hidden,
    const int8_t * hidden_values,
    const float * hidden_scales,
    float * gate_up_values
) {
    const int index = blockIdx.x * blockDim.x + threadIdx.x;
    const int rows_per_expert = 2 * intermediate;
    const int expert = index / rows_per_expert;
    const int row = index % rows_per_expert;
    if (expert >= batch) {
        return;
    }
    const uint8_t * payload = payloads + expert * payload_stride;
    const uint8_t * gate_up = payload + kPayloadHeaderBytes;
    const float gate_scale = payload_scale(payload, 0);
    const float up_scale = payload_scale(payload, 1);
    const bool is_up = row >= intermediate;
    const float value = dot_nvfp4(
        gate_up,
        row,
        hidden,
        hidden_values,
        hidden_scales
    );
    gate_up_values[expert * rows_per_expert + row] = value * (is_up ? up_scale : gate_scale);
}

__global__ void gate_up_resident_kernel(
    const uint8_t * const * payloads,
    int batch,
    int intermediate,
    int hidden,
    const int8_t * hidden_values,
    const float * hidden_scales,
    float * gate_up_values
) {
    const int index = blockIdx.x * blockDim.x + threadIdx.x;
    const int rows_per_expert = 2 * intermediate;
    const int expert = index / rows_per_expert;
    const int row = index % rows_per_expert;
    if (expert >= batch) {
        return;
    }
    const uint8_t * payload = payloads[expert];
    const uint8_t * gate_up = payload + kPayloadHeaderBytes;
    const float gate_scale = payload_scale(payload, 0);
    const float up_scale = payload_scale(payload, 1);
    const bool is_up = row >= intermediate;
    const float value = dot_nvfp4(
        gate_up,
        row,
        hidden,
        hidden_values,
        hidden_scales
    );
    gate_up_values[expert * rows_per_expert + row] = value * (is_up ? up_scale : gate_scale);
}

__device__ float ggml_gelu_f16(float value) {
    const float input = round_to_f16(value);
    const float gelu = 0.5f * input *
        (1.0f + tanhf(0.7978845608028654f * input *
            (1.0f + 0.044715f * input * input)));
    return round_to_f16(gelu);
}

__global__ void geglu_kernel(
    const float * gate_up_values,
    int batch,
    int intermediate,
    float * activated
) {
    const int index = blockIdx.x * blockDim.x + threadIdx.x;
    const int expert = index / intermediate;
    const int row = index % intermediate;
    if (expert >= batch) {
        return;
    }
    const float * gate_up = gate_up_values + expert * 2 * intermediate;
    const float gate = gate_up[row];
    const float up = gate_up[intermediate + row];
    const float activated_gate = gate <= -10.0f
        ? 0.0f
        : gate >= 10.0f
            ? gate
            : ggml_gelu_f16(gate);
    activated[expert * intermediate + row] = activated_gate * up;
}

__global__ void down_kernel(
    const uint8_t * payloads,
    std::size_t payload_stride,
    int batch,
    int hidden,
    int intermediate,
    const int8_t * activated_values,
    const float * activated_scales,
    float * output
) {
    const int index = blockIdx.x * blockDim.x + threadIdx.x;
    const int expert = index / hidden;
    const int row = index % hidden;
    if (expert >= batch) {
        return;
    }
    const uint8_t * payload = payloads + expert * payload_stride;
    const std::size_t gate_up_len = static_cast<std::size_t>(payload[0]) |
        (static_cast<std::size_t>(payload[1]) << 8) |
        (static_cast<std::size_t>(payload[2]) << 16) |
        (static_cast<std::size_t>(payload[3]) << 24);
    const uint8_t * down = payload + kPayloadHeaderBytes + gate_up_len;
    output[expert * hidden + row] = dot_nvfp4(
        down,
        row,
        intermediate,
        activated_values + expert * intermediate,
        activated_scales + expert * (intermediate / 32)
    ) * payload_scale(payload, 2);
}

__global__ void down_resident_kernel(
    const uint8_t * const * payloads,
    int batch,
    int hidden,
    int intermediate,
    const int8_t * activated_values,
    const float * activated_scales,
    float * output
) {
    const int index = blockIdx.x * blockDim.x + threadIdx.x;
    const int expert = index / hidden;
    const int row = index % hidden;
    if (expert >= batch) {
        return;
    }
    const uint8_t * payload = payloads[expert];
    const std::size_t gate_up_len = static_cast<std::size_t>(payload[0]) |
        (static_cast<std::size_t>(payload[1]) << 8) |
        (static_cast<std::size_t>(payload[2]) << 16) |
        (static_cast<std::size_t>(payload[3]) << 24);
    const uint8_t * down = payload + kPayloadHeaderBytes + gate_up_len;
    output[expert * hidden + row] = dot_nvfp4(
        down,
        row,
        intermediate,
        activated_values + expert * intermediate,
        activated_scales + expert * (intermediate / 32)
    ) * payload_scale(payload, 2);
}

__global__ void gate_up_route_batch_resident_kernel(
    const uint8_t * const * payloads,
    const uint32_t * token_ids,
    int pair_count,
    int intermediate,
    int hidden,
    const int8_t * hidden_values,
    const float * hidden_scales,
    float * gate_up_values
) {
    const int index = blockIdx.x * blockDim.x + threadIdx.x;
    const int rows_per_pair = 2 * intermediate;
    const int pair = index / rows_per_pair;
    const int row = index % rows_per_pair;
    if (pair >= pair_count) {
        return;
    }
    const uint8_t * payload = payloads[pair];
    const uint8_t * gate_up = payload + kPayloadHeaderBytes;
    const bool is_up = row >= intermediate;
    const int token = static_cast<int>(token_ids[pair]);
    const float value = dot_nvfp4(
        gate_up,
        row,
        hidden,
        hidden_values + token * hidden,
        hidden_scales + token * (hidden / 32)
    );
    gate_up_values[pair * rows_per_pair + row] =
        value * (is_up ? payload_scale(payload, 1) : payload_scale(payload, 0));
}

__global__ void down_route_batch_resident_kernel(
    const uint8_t * const * payloads,
    int pair_count,
    int hidden,
    int intermediate,
    const int8_t * activated_values,
    const float * activated_scales,
    float * pair_output
) {
    const int index = blockIdx.x * blockDim.x + threadIdx.x;
    const int pair = index / hidden;
    const int row = index % hidden;
    if (pair >= pair_count) {
        return;
    }
    const uint8_t * payload = payloads[pair];
    const std::size_t gate_up_len = static_cast<std::size_t>(payload[0]) |
        (static_cast<std::size_t>(payload[1]) << 8) |
        (static_cast<std::size_t>(payload[2]) << 16) |
        (static_cast<std::size_t>(payload[3]) << 24);
    const uint8_t * down = payload + kPayloadHeaderBytes + gate_up_len;
    pair_output[pair * hidden + row] = dot_nvfp4(
        down,
        row,
        intermediate,
        activated_values + pair * intermediate,
        activated_scales + pair * (intermediate / 32)
    ) * payload_scale(payload, 2);
}

__global__ void aggregate_route_batch_kernel(
    const float * pair_output,
    const float * weights,
    const std::size_t * token_offsets,
    int token_count,
    int hidden,
    float * output
) {
    const int index = blockIdx.x * blockDim.x + threadIdx.x;
    const int token = index / hidden;
    const int row = index % hidden;
    if (token >= token_count) {
        return;
    }
    float value = 0.0f;
    const std::size_t begin = token_offsets[token];
    const std::size_t end = token_offsets[token + 1];
    for (std::size_t pair = begin; pair < end; ++pair) {
        value += weights[pair] * pair_output[pair * hidden + row];
    }
    output[token * hidden + row] = value;
}

// K-quants usam superblocos de 256 valores. Estas rotinas reimplementam a
// convenção GGML a partir do layout do formato, sem depender de arquitetura
// ou de um modelo específico. A ponte começa pelos dtypes 12 e 14.
__device__ float k_f16(uint16_t bits) {
    const float sign = (bits & 0x8000u) != 0 ? -1.0f : 1.0f;
    const int exponent = (bits >> 10) & 0x1f;
    const uint32_t fraction = bits & 0x03ffu;
    if (exponent == 0) {
        return sign * static_cast<float>(fraction) * exp2f(-24.0f);
    }
    if (exponent == 31) {
        const float infinity = __int_as_float(0x7f800000);
        const float nan = __int_as_float(0x7fc00000);
        return fraction == 0 ? sign * infinity : nan;
    }
    return sign * (1.0f + static_cast<float>(fraction) / 1024.0f) *
        exp2f(static_cast<float>(exponent - 15));
}

__device__ uint16_t k_u16(const uint8_t *bytes) {
    return static_cast<uint16_t>(bytes[0]) |
        (static_cast<uint16_t>(bytes[1]) << 8);
}

__device__ float k_q4_scale(uint32_t index, const uint8_t *scales, bool minimum) {
    if (index < 4) {
        return static_cast<float>(minimum ? (scales[index + 4] & 63u) : (scales[index] & 63u));
    }
    return static_cast<float>(minimum
        ? ((scales[index + 4] >> 4) | ((scales[index] >> 6) << 4))
        : ((scales[index + 4] & 0x0fu) | ((scales[index - 4] >> 6) << 4)));
}

__device__ float k_dot_q4_k(
    const uint8_t *matrix,
    int row,
    int columns,
    const float *input
) {
    constexpr int block_bytes = 144;
    const int row_bytes = (columns / 256) * block_bytes;
    const uint8_t *encoded = matrix + row * row_bytes;
    float result = 0.0f;
    for (int block = 0; block < columns / 256; ++block) {
        const uint8_t *packed = encoded + block * block_bytes;
        const float d = k_f16(k_u16(packed));
        const float minimum = k_f16(k_u16(packed + 2));
        const uint8_t *scales = packed + 4;
        const uint8_t *quants = packed + 16;
        for (int chunk = 0; chunk < 4; ++chunk) {
            const uint8_t *values = quants + chunk * 32;
            const uint32_t scale_index = static_cast<uint32_t>(chunk * 2);
            const float scale0 = d * k_q4_scale(scale_index, scales, false);
            const float scale1 = d * k_q4_scale(scale_index + 1, scales, false);
            const float min0 = minimum * k_q4_scale(scale_index, scales, true);
            const float min1 = minimum * k_q4_scale(scale_index + 1, scales, true);
            const int input_base = block * 256 + chunk * 64;
            for (int lane = 0; lane < 32; ++lane) {
                result += input[input_base + lane] *
                    (scale0 * static_cast<float>(values[lane] & 0x0f) - min0);
                result += input[input_base + lane + 32] *
                    (scale1 * static_cast<float>(values[lane] >> 4) - min1);
            }
        }
    }
    return result;
}

__device__ float k_dot_q6_k(
    const uint8_t *matrix,
    int row,
    int columns,
    const float *input
) {
    constexpr int block_bytes = 210;
    const int row_bytes = (columns / 256) * block_bytes;
    const uint8_t *encoded = matrix + row * row_bytes;
    float result = 0.0f;
    for (int block = 0; block < columns / 256; ++block) {
        const uint8_t *packed = encoded + block * block_bytes;
        const float d = k_f16(k_u16(packed));
        const uint8_t *ql = packed + 2;
        const uint8_t *qh = packed + 130;
        const uint8_t *scales = packed + 194;
        for (int half = 0; half < 2; ++half) {
            const uint8_t *low = ql + half * 64;
            const uint8_t *high = qh + half * 32;
            const uint8_t *scale_group = scales + half * 8;
            const int input_base = block * 256 + half * 128;
            for (int lane = 0; lane < 32; ++lane) {
                const int group = lane / 16;
                const int q1 = static_cast<int>((low[lane] & 0x0f) | ((high[lane] & 3u) << 4)) - 32;
                const int q2 = static_cast<int>((low[lane + 32] & 0x0f) | (((high[lane] >> 2) & 3u) << 4)) - 32;
                const int q3 = static_cast<int>((low[lane] >> 4) | (((high[lane] >> 4) & 3u) << 4)) - 32;
                const int q4 = static_cast<int>((low[lane + 32] >> 4) | (((high[lane] >> 6) & 3u) << 4)) - 32;
                result += input[input_base + lane] * d * static_cast<float>(static_cast<int8_t>(scale_group[group])) * q1;
                result += input[input_base + lane + 32] * d * static_cast<float>(static_cast<int8_t>(scale_group[group + 2])) * q2;
                result += input[input_base + lane + 64] * d * static_cast<float>(static_cast<int8_t>(scale_group[group + 4])) * q3;
                result += input[input_base + lane + 96] * d * static_cast<float>(static_cast<int8_t>(scale_group[group + 6])) * q4;
            }
        }
    }
    return result;
}

__device__ float k_dot(
    uint32_t dtype,
    const uint8_t *matrix,
    int row,
    int columns,
    const float *input
) {
    return dtype == 12
        ? k_dot_q4_k(matrix, row, columns, input)
        : k_dot_q6_k(matrix, row, columns, input);
}

// IQ usa codebooks GGML por superbloco. Estas funções acessam um peso pela
// posição lógica, preservando a mesma ordem que dequantize_row_* usa no CPU.
__device__ __forceinline__ float iq2s_value(const uint8_t *matrix, int index) {
    constexpr int block_bytes = 82;
    const int block_index = index / 256;
    const int within = index % 256;
    const block_iq2_s *block = reinterpret_cast<const block_iq2_s *>(
        matrix + block_index * block_bytes);
    const int group = within / 32;
    const int lane = within % 32;
    const int packed_group = lane / 8;
    const int element = lane % 8;
    const uint8_t q = block->qs[group * 4 + packed_group];
    const int grid_index = q | ((static_cast<int>(block->qh[group]) <<
        (8 - 2 * packed_group)) & 0x300);
    const uint64_t grid = iq2s_grid[grid_index];
    const uint8_t signs = block->qs[32 + group * 4 + packed_group];
    const uint8_t code = static_cast<uint8_t>((grid >> (8 * element)) & 0xffu);
    const float scale = __half2float(block->d) *
        (0.5f + static_cast<float>((block->scales[group] >>
            (4 * (packed_group / 2))) & 0x0f)) * 0.25f;
    return scale * static_cast<float>(code) *
        ((signs & kmask_iq2xs[element]) != 0 ? -1.0f : 1.0f);
}

__device__ __forceinline__ float iq3s_value(const uint8_t *matrix, int index) {
    constexpr int block_bytes = 110;
    const int block_index = index / 256;
    const int within = index % 256;
    const block_iq3_s *block = reinterpret_cast<const block_iq3_s *>(
        matrix + block_index * block_bytes);
    const int group = within / 32;
    const int lane = within % 32;
    const int packed_group = lane / 8;
    const int element = lane % 8;
    const uint8_t *qs = block->qs + group * 8;
    const int pair = element / 4;
    const int in_pair = element % 4;
    const int q = qs[packed_group * 2 + pair];
    const int high_shift = pair == 0 ? (8 - 2 * packed_group) :
        (7 - 2 * packed_group);
    const int grid_index = q | ((static_cast<int>(block->qh[group]) <<
        high_shift) & 0x100);
    const uint32_t grid = iq3s_grid[grid_index];
    const uint8_t signs = block->signs[group * 4 + packed_group];
    const uint8_t code = static_cast<uint8_t>((grid >> (8 * in_pair)) & 0xffu);
    const float scale = __half2float(block->d) *
        (1.0f + 2.0f * static_cast<float>((block->scales[group / 2] >>
            (4 * (group % 2))) & 0x0f));
    return scale * static_cast<float>(code) *
        ((signs & kmask_iq2xs[element]) != 0 ? -1.0f : 1.0f);
}

__device__ __forceinline__ float iq4xs_value(const uint8_t *matrix, int index) {
    constexpr int block_bytes = 136;
    const int block_index = index / 256;
    const int within = index % 256;
    const block_iq4_xs *block = reinterpret_cast<const block_iq4_xs *>(
        matrix + block_index * block_bytes);
    const int group = within / 32;
    const int lane = within % 32;
    const uint8_t packed = block->qs[group * 16 + lane % 16];
    const int code = lane < 16 ? packed & 0x0f : packed >> 4;
    const int scale_code = ((block->scales_l[group / 2] >>
        (4 * (group % 2))) & 0x0f) |
        (((block->scales_h >> (2 * group)) & 0x03) << 4);
    return __half2float(block->d) * static_cast<float>(scale_code - 32) *
        static_cast<float>(kvalues_iq4nl[code]);
}

__device__ float iq_dot(
    uint32_t dtype,
    const uint8_t *matrix,
    int row,
    int columns,
    const float *input
) {
    const int row_bytes = (dtype == 21 ? 110 : dtype == 22 ? 82 : 136) *
        (columns / 256);
    const uint8_t *encoded = matrix + row * row_bytes;
    float result = 0.0f;
    for (int index = 0; index < columns; ++index) {
        const float weight = dtype == 21
            ? iq3s_value(encoded, index)
            : dtype == 22 ? iq2s_value(encoded, index) : iq4xs_value(encoded, index);
        result += weight * input[index];
    }
    return result;
}

__global__ void kquant_gate_up_route_kernel(
    uint32_t dtype,
    const uint8_t * const *payloads,
    const uint32_t *token_ids,
    int pair_count,
    int intermediate,
    int hidden,
    const float *hidden_values,
    float *gate_up_values
) {
    const int index = blockIdx.x * blockDim.x + threadIdx.x;
    const int rows_per_pair = 2 * intermediate;
    const int pair = index / rows_per_pair;
    const int row = index % rows_per_pair;
    if (pair >= pair_count) {
        return;
    }
    const uint8_t *payload = payloads[pair];
    const uint32_t gate_up_len = static_cast<uint32_t>(payload[0]) |
        (static_cast<uint32_t>(payload[1]) << 8) |
        (static_cast<uint32_t>(payload[2]) << 16) |
        (static_cast<uint32_t>(payload[3]) << 24);
    const float *input = hidden_values + static_cast<std::size_t>(token_ids[pair]) * hidden;
    gate_up_values[pair * rows_per_pair + row] = k_dot(
        dtype, payload + kKQuantPayloadHeaderBytes, row, hidden, input);
    (void)gate_up_len;
}

__global__ void kquant_silu_kernel(
    const float *gate_up_values,
    int pair_count,
    int intermediate,
    float *activated
) {
    const int index = blockIdx.x * blockDim.x + threadIdx.x;
    const int pair = index / intermediate;
    const int row = index % intermediate;
    if (pair >= pair_count) {
        return;
    }
    const float gate = gate_up_values[pair * 2 * intermediate + row];
    const float up = gate_up_values[pair * 2 * intermediate + intermediate + row];
    activated[pair * intermediate + row] =
        (gate / (1.0f + expf(-gate))) * up;
}

__global__ void kquant_down_route_kernel(
    uint32_t dtype,
    const uint8_t * const *payloads,
    int pair_count,
    int hidden,
    int intermediate,
    const float *activated,
    float *pair_output
) {
    const int index = blockIdx.x * blockDim.x + threadIdx.x;
    const int pair = index / hidden;
    const int row = index % hidden;
    if (pair >= pair_count) {
        return;
    }
    const uint8_t *payload = payloads[pair];
    const uint32_t gate_up_len = static_cast<uint32_t>(payload[0]) |
        (static_cast<uint32_t>(payload[1]) << 8) |
        (static_cast<uint32_t>(payload[2]) << 16) |
        (static_cast<uint32_t>(payload[3]) << 24);
    pair_output[pair * hidden + row] = k_dot(
        dtype,
        payload + kKQuantPayloadHeaderBytes + gate_up_len,
        row,
        intermediate,
        activated + pair * intermediate);
}

__global__ void iquant_gate_up_route_kernel(
    uint32_t dtype,
    const uint8_t * const *payloads,
    const uint32_t *token_ids,
    int pair_count,
    int intermediate,
    int hidden,
    const float *hidden_values,
    float *gate_up_values
) {
    const int index = blockIdx.x * blockDim.x + threadIdx.x;
    const int rows_per_pair = 2 * intermediate;
    const int pair = index / rows_per_pair;
    const int row = index % rows_per_pair;
    if (pair >= pair_count) {
        return;
    }
    const uint8_t *payload = payloads[pair];
    const uint32_t gate_up_len = static_cast<uint32_t>(payload[0]) |
        (static_cast<uint32_t>(payload[1]) << 8) |
        (static_cast<uint32_t>(payload[2]) << 16) |
        (static_cast<uint32_t>(payload[3]) << 24);
    const float *input = hidden_values + static_cast<std::size_t>(token_ids[pair]) * hidden;
    gate_up_values[pair * rows_per_pair + row] = iq_dot(
        dtype, payload + kKQuantPayloadHeaderBytes, row, hidden, input);
    (void)gate_up_len;
}

__global__ void iquant_down_route_kernel(
    uint32_t dtype,
    const uint8_t * const *payloads,
    int pair_count,
    int hidden,
    int intermediate,
    const float *activated,
    float *pair_output
) {
    const int index = blockIdx.x * blockDim.x + threadIdx.x;
    const int pair = index / hidden;
    const int row = index % hidden;
    if (pair >= pair_count) {
        return;
    }
    const uint8_t *payload = payloads[pair];
    const uint32_t gate_up_len = static_cast<uint32_t>(payload[0]) |
        (static_cast<uint32_t>(payload[1]) << 8) |
        (static_cast<uint32_t>(payload[2]) << 16) |
        (static_cast<uint32_t>(payload[3]) << 24);
    pair_output[pair * hidden + row] = iq_dot(
        dtype,
        payload + kKQuantPayloadHeaderBytes + gate_up_len,
        row,
        intermediate,
        activated + pair * intermediate);
}

int cuda_status(cudaError_t error) {
    return error == cudaSuccess ? 0 : static_cast<int>(error);
}

} // namespace

extern "C" int pom_moe_kquant_forward_route_batch_resident(
    uint32_t dtype,
    const uint8_t * const *payloads,
    std::size_t payload_count,
    const uint32_t *token_ids,
    const float *weights,
    const std::size_t *token_offsets,
    std::size_t token_offsets_len,
    std::size_t token_count,
    const float *hidden,
    std::size_t hidden_len,
    float *output,
    std::size_t output_len,
    std::size_t hidden_width,
    std::size_t intermediate_width,
    CudaMoeCallTiming *timing
) {
    if (timing != nullptr) {
        *timing = {};
        timing->expert_calls = static_cast<uint64_t>(payload_count);
        timing->kernel_launches = 4;
        timing->sync_count = 1;
    }
    if ((dtype != 12 && dtype != 14) || payloads == nullptr || token_ids == nullptr ||
        weights == nullptr || token_offsets == nullptr || hidden == nullptr ||
        output == nullptr || payload_count == 0 || token_count == 0 || hidden_width == 0 ||
        intermediate_width == 0 || hidden_width % 256 != 0 || intermediate_width % 256 != 0 ||
        token_offsets_len != token_count + 1 || hidden_len != token_count * hidden_width ||
        output_len != hidden_len || token_offsets[0] != 0 ||
        token_offsets[token_count] != payload_count) {
        return 1;
    }
    for (std::size_t pair = 0; pair < payload_count; ++pair) {
        if (payloads[pair] == nullptr || token_ids[pair] >= token_count ||
            !std::isfinite(weights[pair])) {
            return 1;
        }
    }
    for (std::size_t token = 0; token < token_count; ++token) {
        if (token_offsets[token] > token_offsets[token + 1]) {
            return 1;
        }
    }

    const int pair_count = static_cast<int>(payload_count);
    const int hidden_i = static_cast<int>(hidden_width);
    const int intermediate_i = static_cast<int>(intermediate_width);
    const int block_size = 128;
    const std::size_t hidden_bytes = hidden_width * token_count * sizeof(float);
    const std::size_t gate_up_bytes = 2 * intermediate_width * payload_count * sizeof(float);
    const std::size_t activated_bytes = intermediate_width * payload_count * sizeof(float);
    const std::size_t pair_output_bytes = hidden_width * payload_count * sizeof(float);

    float *d_hidden = nullptr;
    const uint8_t **d_payloads = nullptr;
    uint32_t *d_token_ids = nullptr;
    float *d_weights = nullptr;
    std::size_t *d_token_offsets = nullptr;
    float *d_gate_up = nullptr;
    float *d_activated = nullptr;
    float *d_pair_output = nullptr;
    float *d_output = nullptr;
    cudaEvent_t start = nullptr;
    cudaEvent_t end = nullptr;
    int result = 0;

    do {
        const auto alloc_start = Clock::now();
        result = cuda_status(cudaMalloc(reinterpret_cast<void **>(&d_hidden), hidden_bytes));
        if (result != 0) break;
        result = cuda_status(cudaMalloc(reinterpret_cast<void **>(&d_payloads),
            payload_count * sizeof(*d_payloads)));
        if (result != 0) break;
        result = cuda_status(cudaMalloc(reinterpret_cast<void **>(&d_token_ids),
            payload_count * sizeof(*d_token_ids)));
        if (result != 0) break;
        result = cuda_status(cudaMalloc(reinterpret_cast<void **>(&d_weights),
            payload_count * sizeof(*d_weights)));
        if (result != 0) break;
        result = cuda_status(cudaMalloc(reinterpret_cast<void **>(&d_token_offsets),
            token_offsets_len * sizeof(*d_token_offsets)));
        if (result != 0) break;
        result = cuda_status(cudaMalloc(reinterpret_cast<void **>(&d_gate_up), gate_up_bytes));
        if (result != 0) break;
        result = cuda_status(cudaMalloc(reinterpret_cast<void **>(&d_activated), activated_bytes));
        if (result != 0) break;
        result = cuda_status(cudaMalloc(reinterpret_cast<void **>(&d_pair_output), pair_output_bytes));
        if (result != 0) break;
        result = cuda_status(cudaMalloc(reinterpret_cast<void **>(&d_output), hidden_bytes));
        if (result != 0) break;
        if (timing != nullptr) {
            timing->alloc_ms = elapsed_ms(alloc_start, Clock::now());
            result = cuda_status(cudaEventCreate(&start));
            if (result != 0) break;
            result = cuda_status(cudaEventCreate(&end));
            if (result != 0) break;
        }

        const auto h2d_start = Clock::now();
        result = cuda_status(cudaMemcpy(d_hidden, hidden, hidden_bytes, cudaMemcpyHostToDevice));
        if (result != 0) break;
        result = cuda_status(cudaMemcpy(d_payloads, payloads,
            payload_count * sizeof(*d_payloads), cudaMemcpyHostToDevice));
        if (result != 0) break;
        result = cuda_status(cudaMemcpy(d_token_ids, token_ids,
            payload_count * sizeof(*d_token_ids), cudaMemcpyHostToDevice));
        if (result != 0) break;
        result = cuda_status(cudaMemcpy(d_weights, weights,
            payload_count * sizeof(*d_weights), cudaMemcpyHostToDevice));
        if (result != 0) break;
        result = cuda_status(cudaMemcpy(d_token_offsets, token_offsets,
            token_offsets_len * sizeof(*d_token_offsets), cudaMemcpyHostToDevice));
        if (timing != nullptr) {
            timing->h2d_ms = elapsed_ms(h2d_start, Clock::now());
            timing->h2d_bytes = static_cast<uint64_t>(
                hidden_bytes + payload_count * sizeof(*d_payloads) +
                payload_count * sizeof(*d_token_ids) + payload_count * sizeof(*d_weights) +
                token_offsets_len * sizeof(*d_token_offsets));
        }
        if (result != 0) break;

        const auto launch_start = Clock::now();
        if (timing != nullptr) {
            result = cuda_status(cudaEventRecord(start));
            if (result != 0) break;
        }
        kquant_gate_up_route_kernel<<<
            (2 * intermediate_i * pair_count + block_size - 1) / block_size,
            block_size>>>(dtype, d_payloads, d_token_ids, pair_count, intermediate_i,
                hidden_i, d_hidden, d_gate_up);
        result = cuda_status(cudaGetLastError());
        if (result != 0) break;
        kquant_silu_kernel<<<
            (intermediate_i * pair_count + block_size - 1) / block_size,
            block_size>>>(d_gate_up, pair_count, intermediate_i, d_activated);
        result = cuda_status(cudaGetLastError());
        if (result != 0) break;
        kquant_down_route_kernel<<<
            (hidden_i * pair_count + block_size - 1) / block_size,
            block_size>>>(dtype, d_payloads, pair_count, hidden_i, intermediate_i,
                d_activated, d_pair_output);
        result = cuda_status(cudaGetLastError());
        if (result != 0) break;
        aggregate_route_batch_kernel<<<
            (static_cast<int>(token_count) * hidden_i + block_size - 1) / block_size,
            block_size>>>(d_pair_output, d_weights, d_token_offsets,
                static_cast<int>(token_count), hidden_i, d_output);
        result = cuda_status(cudaGetLastError());
        if (timing != nullptr) {
            timing->launch_ms = elapsed_ms(launch_start, Clock::now());
            result = cuda_status(cudaEventRecord(end));
            if (result != 0) break;
        }
        const auto sync_start = Clock::now();
        result = cuda_status(cudaDeviceSynchronize());
        if (result != 0) break;
        if (timing != nullptr) {
            float kernel_ms = 0.0f;
            result = cuda_status(cudaEventElapsedTime(&kernel_ms, start, end));
            if (result != 0) break;
            timing->kernel_ms = kernel_ms;
            timing->sync_ms = std::max(0.0, elapsed_ms(sync_start, Clock::now()) -
                static_cast<double>(kernel_ms));
        }

        const auto d2h_start = Clock::now();
        result = cuda_status(cudaMemcpy(output, d_output, hidden_bytes, cudaMemcpyDeviceToHost));
        if (timing != nullptr) {
            timing->d2h_ms = elapsed_ms(d2h_start, Clock::now());
            timing->d2h_bytes = static_cast<uint64_t>(hidden_bytes);
        }
    } while (false);

    if (start != nullptr) cudaEventDestroy(start);
    if (end != nullptr) cudaEventDestroy(end);
    const auto free_start = Clock::now();
    cudaFree(d_output);
    cudaFree(d_pair_output);
    cudaFree(d_activated);
    cudaFree(d_gate_up);
    cudaFree(d_token_offsets);
    cudaFree(d_weights);
    cudaFree(d_token_ids);
    cudaFree(d_payloads);
    cudaFree(d_hidden);
    if (timing != nullptr) {
        timing->free_ms = elapsed_ms(free_start, Clock::now());
    }
    return result;
}

extern "C" int pom_moe_iquant_forward_route_batch_resident(
    uint32_t dtype,
    const uint8_t * const *payloads,
    std::size_t payload_count,
    const uint32_t *token_ids,
    const float *weights,
    const std::size_t *token_offsets,
    std::size_t token_offsets_len,
    std::size_t token_count,
    const float *hidden,
    std::size_t hidden_len,
    float *output,
    std::size_t output_len,
    std::size_t hidden_width,
    std::size_t intermediate_width,
    CudaMoeCallTiming *timing
) {
    if (timing != nullptr) {
        *timing = {};
        timing->expert_calls = static_cast<uint64_t>(payload_count);
        timing->kernel_launches = 4;
        timing->sync_count = 1;
    }
    if ((dtype != 21 && dtype != 22 && dtype != 23) || payloads == nullptr ||
        token_ids == nullptr || weights == nullptr || token_offsets == nullptr ||
        hidden == nullptr || output == nullptr || payload_count == 0 ||
        token_count == 0 || hidden_width == 0 || intermediate_width == 0 ||
        hidden_width % 256 != 0 || intermediate_width % 256 != 0 ||
        token_offsets_len != token_count + 1 || hidden_len != token_count * hidden_width ||
        output_len != hidden_len || token_offsets[0] != 0 ||
        token_offsets[token_count] != payload_count) {
        return 1;
    }
    for (std::size_t pair = 0; pair < payload_count; ++pair) {
        if (payloads[pair] == nullptr || token_ids[pair] >= token_count ||
            !std::isfinite(weights[pair])) {
            return 1;
        }
    }
    for (std::size_t token = 0; token < token_count; ++token) {
        if (token_offsets[token] > token_offsets[token + 1]) {
            return 1;
        }
    }

    const int pair_count = static_cast<int>(payload_count);
    const int hidden_i = static_cast<int>(hidden_width);
    const int intermediate_i = static_cast<int>(intermediate_width);
    const int block_size = 128;
    const std::size_t hidden_bytes = hidden_width * token_count * sizeof(float);
    const std::size_t gate_up_bytes = 2 * intermediate_width * payload_count * sizeof(float);
    const std::size_t activated_bytes = intermediate_width * payload_count * sizeof(float);
    const std::size_t pair_output_bytes = hidden_width * payload_count * sizeof(float);

    float *d_hidden = nullptr;
    const uint8_t **d_payloads = nullptr;
    uint32_t *d_token_ids = nullptr;
    float *d_weights = nullptr;
    std::size_t *d_token_offsets = nullptr;
    float *d_gate_up = nullptr;
    float *d_activated = nullptr;
    float *d_pair_output = nullptr;
    float *d_output = nullptr;
    cudaEvent_t start = nullptr;
    cudaEvent_t end = nullptr;
    int result = 0;

    do {
        const auto alloc_start = Clock::now();
        result = cuda_status(cudaMalloc(reinterpret_cast<void **>(&d_hidden), hidden_bytes));
        if (result != 0) break;
        result = cuda_status(cudaMalloc(reinterpret_cast<void **>(&d_payloads),
            payload_count * sizeof(*d_payloads)));
        if (result != 0) break;
        result = cuda_status(cudaMalloc(reinterpret_cast<void **>(&d_token_ids),
            payload_count * sizeof(*d_token_ids)));
        if (result != 0) break;
        result = cuda_status(cudaMalloc(reinterpret_cast<void **>(&d_weights),
            payload_count * sizeof(*d_weights)));
        if (result != 0) break;
        result = cuda_status(cudaMalloc(reinterpret_cast<void **>(&d_token_offsets),
            token_offsets_len * sizeof(*d_token_offsets)));
        if (result != 0) break;
        result = cuda_status(cudaMalloc(reinterpret_cast<void **>(&d_gate_up), gate_up_bytes));
        if (result != 0) break;
        result = cuda_status(cudaMalloc(reinterpret_cast<void **>(&d_activated), activated_bytes));
        if (result != 0) break;
        result = cuda_status(cudaMalloc(reinterpret_cast<void **>(&d_pair_output), pair_output_bytes));
        if (result != 0) break;
        result = cuda_status(cudaMalloc(reinterpret_cast<void **>(&d_output), hidden_bytes));
        if (result != 0) break;
        if (timing != nullptr) {
            timing->alloc_ms = elapsed_ms(alloc_start, Clock::now());
            result = cuda_status(cudaEventCreate(&start));
            if (result != 0) break;
            result = cuda_status(cudaEventCreate(&end));
            if (result != 0) break;
        }

        const auto h2d_start = Clock::now();
        result = cuda_status(cudaMemcpy(d_hidden, hidden, hidden_bytes, cudaMemcpyHostToDevice));
        if (result != 0) break;
        result = cuda_status(cudaMemcpy(d_payloads, payloads,
            payload_count * sizeof(*d_payloads), cudaMemcpyHostToDevice));
        if (result != 0) break;
        result = cuda_status(cudaMemcpy(d_token_ids, token_ids,
            payload_count * sizeof(*d_token_ids), cudaMemcpyHostToDevice));
        if (result != 0) break;
        result = cuda_status(cudaMemcpy(d_weights, weights,
            payload_count * sizeof(*d_weights), cudaMemcpyHostToDevice));
        if (result != 0) break;
        result = cuda_status(cudaMemcpy(d_token_offsets, token_offsets,
            token_offsets_len * sizeof(*d_token_offsets), cudaMemcpyHostToDevice));
        if (timing != nullptr) {
            timing->h2d_ms = elapsed_ms(h2d_start, Clock::now());
            timing->h2d_bytes = static_cast<uint64_t>(
                hidden_bytes + payload_count * sizeof(*d_payloads) +
                payload_count * sizeof(*d_token_ids) + payload_count * sizeof(*d_weights) +
                token_offsets_len * sizeof(*d_token_offsets));
        }
        if (result != 0) break;

        const auto launch_start = Clock::now();
        if (timing != nullptr) {
            result = cuda_status(cudaEventRecord(start));
            if (result != 0) break;
        }
        iquant_gate_up_route_kernel<<<
            (2 * intermediate_i * pair_count + block_size - 1) / block_size,
            block_size>>>(dtype, d_payloads, d_token_ids, pair_count, intermediate_i,
                hidden_i, d_hidden, d_gate_up);
        result = cuda_status(cudaGetLastError());
        if (result != 0) break;
        kquant_silu_kernel<<<
            (intermediate_i * pair_count + block_size - 1) / block_size,
            block_size>>>(d_gate_up, pair_count, intermediate_i, d_activated);
        result = cuda_status(cudaGetLastError());
        if (result != 0) break;
        iquant_down_route_kernel<<<
            (hidden_i * pair_count + block_size - 1) / block_size,
            block_size>>>(dtype, d_payloads, pair_count, hidden_i, intermediate_i,
                d_activated, d_pair_output);
        result = cuda_status(cudaGetLastError());
        if (result != 0) break;
        aggregate_route_batch_kernel<<<
            (static_cast<int>(token_count) * hidden_i + block_size - 1) / block_size,
            block_size>>>(d_pair_output, d_weights, d_token_offsets,
                static_cast<int>(token_count), hidden_i, d_output);
        result = cuda_status(cudaGetLastError());
        if (timing != nullptr) {
            timing->launch_ms = elapsed_ms(launch_start, Clock::now());
            result = cuda_status(cudaEventRecord(end));
            if (result != 0) break;
        }
        const auto sync_start = Clock::now();
        result = cuda_status(cudaDeviceSynchronize());
        if (result != 0) break;
        if (timing != nullptr) {
            float kernel_ms = 0.0f;
            result = cuda_status(cudaEventElapsedTime(&kernel_ms, start, end));
            if (result != 0) break;
            timing->kernel_ms = kernel_ms;
            timing->sync_ms = std::max(0.0, elapsed_ms(sync_start, Clock::now()) -
                static_cast<double>(kernel_ms));
        }

        const auto d2h_start = Clock::now();
        result = cuda_status(cudaMemcpy(output, d_output, hidden_bytes, cudaMemcpyDeviceToHost));
        if (timing != nullptr) {
            timing->d2h_ms = elapsed_ms(d2h_start, Clock::now());
            timing->d2h_bytes = static_cast<uint64_t>(hidden_bytes);
        }
    } while (false);

    if (start != nullptr) cudaEventDestroy(start);
    if (end != nullptr) cudaEventDestroy(end);
    const auto free_start = Clock::now();
    cudaFree(d_output);
    cudaFree(d_pair_output);
    cudaFree(d_activated);
    cudaFree(d_gate_up);
    cudaFree(d_token_offsets);
    cudaFree(d_weights);
    cudaFree(d_token_ids);
    cudaFree(d_payloads);
    cudaFree(d_hidden);
    if (timing != nullptr) {
        timing->free_ms = elapsed_ms(free_start, Clock::now());
    }
    return result;
}

extern "C" int pom_moe_nvfp4_forward_batch(
    const uint8_t * payload,
    std::size_t payload_len,
    std::size_t payload_count,
    const float * hidden,
    std::size_t hidden_len,
    float * output,
    std::size_t output_len,
    std::size_t hidden_width,
    std::size_t intermediate_width,
    CudaMoeCallTiming * timing
) {
    const uint64_t expert_calls = timing != nullptr ? static_cast<uint64_t>(payload_count) : 0;
    if (timing != nullptr) {
        *timing = {};
        timing->expert_calls = expert_calls;
        timing->kernel_launches = 5;
        timing->sync_count = 2;
    }
    if (payload == nullptr || hidden == nullptr || output == nullptr || payload_count == 0 ||
        hidden_width == 0 || intermediate_width == 0 ||
        hidden_width % kBlockElements != 0 || intermediate_width % kBlockElements != 0 ||
        hidden_len != hidden_width || output_len != hidden_width * payload_count ||
        payload_len < kPayloadHeaderBytes) {
        return 1;
    }

    const std::size_t gate_up_expected = 2 * intermediate_width * (hidden_width / kBlockElements) * kBlockBytes;
    const std::size_t down_expected = hidden_width * (intermediate_width / kBlockElements) * kBlockBytes;
    for (std::size_t expert = 0; expert < payload_count; ++expert) {
        const auto * header = payload + expert * payload_len;
        const std::size_t gate_up_len = static_cast<std::size_t>(header[0]) |
            (static_cast<std::size_t>(header[1]) << 8) |
            (static_cast<std::size_t>(header[2]) << 16) |
            (static_cast<std::size_t>(header[3]) << 24);
        const std::size_t down_len = static_cast<std::size_t>(header[4]) |
            (static_cast<std::size_t>(header[5]) << 8) |
            (static_cast<std::size_t>(header[6]) << 16) |
            (static_cast<std::size_t>(header[7]) << 24);
        if (gate_up_len != gate_up_expected || down_len != down_expected ||
            gate_up_len > payload_len - kPayloadHeaderBytes ||
            down_len != payload_len - kPayloadHeaderBytes - gate_up_len) {
            return 1;
        }
    }
    float * d_hidden = nullptr;
    float * d_hidden_scales = nullptr;
    int8_t * d_hidden_qs = nullptr;
    uint8_t * d_payloads = nullptr;
    float * d_gate_up_values = nullptr;
    float * d_activated = nullptr;
    float * d_activated_scales = nullptr;
    int8_t * d_activated_qs = nullptr;
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
        result = cuda_status(cudaMalloc(reinterpret_cast<void **>(&d_hidden_scales), hidden_width / 32 * sizeof(float)));
        if (result != 0) break;
        result = cuda_status(cudaMalloc(reinterpret_cast<void **>(&d_hidden_qs), hidden_width * sizeof(int8_t)));
        if (result != 0) break;
        result = cuda_status(cudaMalloc(reinterpret_cast<void **>(&d_payloads), payload_len * payload_count));
        if (result != 0) break;
        result = cuda_status(cudaMalloc(reinterpret_cast<void **>(&d_gate_up_values),
            2 * intermediate_width * payload_count * sizeof(float)));
        if (result != 0) break;
        result = cuda_status(cudaMalloc(reinterpret_cast<void **>(&d_activated),
            intermediate_width * payload_count * sizeof(float)));
        if (result != 0) break;
        result = cuda_status(cudaMalloc(reinterpret_cast<void **>(&d_activated_scales),
            intermediate_width / 32 * payload_count * sizeof(float)));
        if (result != 0) break;
        result = cuda_status(cudaMalloc(reinterpret_cast<void **>(&d_activated_qs),
            intermediate_width * payload_count * sizeof(int8_t)));
        if (result != 0) break;
        result = cuda_status(cudaMalloc(reinterpret_cast<void **>(&d_output),
            hidden_width * payload_count * sizeof(float)));
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
        result = cuda_status(cudaMemcpy(d_payloads, payload,
            payload_len * payload_count, cudaMemcpyHostToDevice));
        if (result != 0) break;
        if (timing != nullptr) {
            timing->h2d_ms = elapsed_ms(h2d_start, Clock::now());
            timing->h2d_bytes = static_cast<uint64_t>(hidden_width * sizeof(float) +
                payload_len * payload_count);
        }

        const int block_size = 128;
        const auto gate_launch_start = Clock::now();
        if (timing != nullptr) {
            result = cuda_status(cudaEventRecord(gate_start));
            if (result != 0) break;
        }
        quantize_q8_0_kernel<<<(hidden_width + 31) / 32, 32>>>(
            d_hidden, static_cast<int>(hidden_width), d_hidden_scales, d_hidden_qs, 1);
        result = cuda_status(cudaGetLastError());
        if (result != 0) break;
        gate_up_kernel<<<(2 * intermediate_width * payload_count + block_size - 1) / block_size, block_size>>>(
            d_payloads, payload_len, static_cast<int>(payload_count),
            static_cast<int>(intermediate_width), static_cast<int>(hidden_width),
            d_hidden_qs, d_hidden_scales, d_gate_up_values);
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
        geglu_kernel<<<(intermediate_width * payload_count + block_size - 1) / block_size, block_size>>>(
            d_gate_up_values, static_cast<int>(payload_count),
            static_cast<int>(intermediate_width), d_activated);
        result = cuda_status(cudaGetLastError());
        if (result != 0) break;
        quantize_q8_0_kernel<<<((intermediate_width + 31) / 32) * payload_count, 32>>>(
            d_activated, static_cast<int>(intermediate_width), d_activated_scales,
            d_activated_qs, static_cast<int>(payload_count));
        result = cuda_status(cudaGetLastError());
        if (result != 0) break;
        down_kernel<<<(hidden_width * payload_count + block_size - 1) / block_size, block_size>>>(
            d_payloads, payload_len, static_cast<int>(payload_count),
            static_cast<int>(hidden_width), static_cast<int>(intermediate_width),
            d_activated_qs, d_activated_scales, d_output);
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
        result = cuda_status(cudaMemcpy(output, d_output,
            hidden_width * payload_count * sizeof(float), cudaMemcpyDeviceToHost));
        if (timing != nullptr) {
            timing->d2h_ms = elapsed_ms(d2h_start, Clock::now());
            timing->d2h_bytes = static_cast<uint64_t>(hidden_width * payload_count * sizeof(float));
        }
    } while (false);

    if (gate_start != nullptr) cudaEventDestroy(gate_start);
    if (gate_end != nullptr) cudaEventDestroy(gate_end);
    if (down_start != nullptr) cudaEventDestroy(down_start);
    if (down_end != nullptr) cudaEventDestroy(down_end);
    const auto free_start = Clock::now();
    cudaFree(d_output);
    cudaFree(d_activated_qs);
    cudaFree(d_activated_scales);
    cudaFree(d_activated);
    cudaFree(d_gate_up_values);
    cudaFree(d_payloads);
    cudaFree(d_hidden_qs);
    cudaFree(d_hidden_scales);
    cudaFree(d_hidden);
    if (timing != nullptr) {
        timing->free_ms = elapsed_ms(free_start, Clock::now());
    }
    return result;
}

extern "C" int pom_moe_nvfp4_forward_batch_resident(
    const uint8_t * const * payloads,
    std::size_t payload_count,
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
        timing->expert_calls = static_cast<uint64_t>(payload_count);
        timing->kernel_launches = 5;
        timing->sync_count = 2;
    }
    if (payloads == nullptr || hidden == nullptr || output == nullptr || payload_count == 0 ||
        hidden_width == 0 || intermediate_width == 0 ||
        hidden_width % kBlockElements != 0 || intermediate_width % kBlockElements != 0 ||
        hidden_len != hidden_width || output_len != hidden_width * payload_count) {
        return 1;
    }
    for (std::size_t expert = 0; expert < payload_count; ++expert) {
        if (payloads[expert] == nullptr) {
            return 1;
        }
    }

    float * d_hidden = nullptr;
    float * d_hidden_scales = nullptr;
    int8_t * d_hidden_qs = nullptr;
    const uint8_t ** d_payloads = nullptr;
    float * d_gate_up_values = nullptr;
    float * d_activated = nullptr;
    float * d_activated_scales = nullptr;
    int8_t * d_activated_qs = nullptr;
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
        result = cuda_status(cudaMalloc(reinterpret_cast<void **>(&d_hidden_scales), hidden_width / 32 * sizeof(float)));
        if (result != 0) break;
        result = cuda_status(cudaMalloc(reinterpret_cast<void **>(&d_hidden_qs), hidden_width * sizeof(int8_t)));
        if (result != 0) break;
        result = cuda_status(cudaMalloc(reinterpret_cast<void **>(&d_payloads), payload_count * sizeof(*d_payloads)));
        if (result != 0) break;
        result = cuda_status(cudaMalloc(reinterpret_cast<void **>(&d_gate_up_values),
            2 * intermediate_width * payload_count * sizeof(float)));
        if (result != 0) break;
        result = cuda_status(cudaMalloc(reinterpret_cast<void **>(&d_activated),
            intermediate_width * payload_count * sizeof(float)));
        if (result != 0) break;
        result = cuda_status(cudaMalloc(reinterpret_cast<void **>(&d_activated_scales),
            intermediate_width / 32 * payload_count * sizeof(float)));
        if (result != 0) break;
        result = cuda_status(cudaMalloc(reinterpret_cast<void **>(&d_activated_qs),
            intermediate_width * payload_count * sizeof(int8_t)));
        if (result != 0) break;
        result = cuda_status(cudaMalloc(reinterpret_cast<void **>(&d_output),
            hidden_width * payload_count * sizeof(float)));
        if (result != 0) break;
        if (timing != nullptr) {
            timing->alloc_ms = elapsed_ms(alloc_start, Clock::now());
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
        result = cuda_status(cudaMemcpy(d_payloads, payloads,
            payload_count * sizeof(*d_payloads), cudaMemcpyHostToDevice));
        if (result != 0) break;
        if (timing != nullptr) {
            timing->h2d_ms = elapsed_ms(h2d_start, Clock::now());
            timing->h2d_bytes = static_cast<uint64_t>(hidden_width * sizeof(float) +
                payload_count * sizeof(*d_payloads));
        }

        const int block_size = 128;
        const auto gate_launch_start = Clock::now();
        if (timing != nullptr) {
            result = cuda_status(cudaEventRecord(gate_start));
            if (result != 0) break;
        }
        quantize_q8_0_kernel<<<(hidden_width + 31) / 32, 32>>>(
            d_hidden, static_cast<int>(hidden_width), d_hidden_scales, d_hidden_qs, 1);
        result = cuda_status(cudaGetLastError());
        if (result != 0) break;
        gate_up_resident_kernel<<<(2 * intermediate_width * payload_count + block_size - 1) / block_size, block_size>>>(
            d_payloads, static_cast<int>(payload_count),
            static_cast<int>(intermediate_width), static_cast<int>(hidden_width),
            d_hidden_qs, d_hidden_scales, d_gate_up_values);
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
        geglu_kernel<<<(intermediate_width * payload_count + block_size - 1) / block_size, block_size>>>(
            d_gate_up_values, static_cast<int>(payload_count),
            static_cast<int>(intermediate_width), d_activated);
        result = cuda_status(cudaGetLastError());
        if (result != 0) break;
        quantize_q8_0_kernel<<<((intermediate_width + 31) / 32) * payload_count, 32>>>(
            d_activated, static_cast<int>(intermediate_width), d_activated_scales,
            d_activated_qs, static_cast<int>(payload_count));
        result = cuda_status(cudaGetLastError());
        if (result != 0) break;
        down_resident_kernel<<<(hidden_width * payload_count + block_size - 1) / block_size, block_size>>>(
            d_payloads, static_cast<int>(payload_count),
            static_cast<int>(hidden_width), static_cast<int>(intermediate_width),
            d_activated_qs, d_activated_scales, d_output);
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
        result = cuda_status(cudaMemcpy(output, d_output,
            hidden_width * payload_count * sizeof(float), cudaMemcpyDeviceToHost));
        if (timing != nullptr) {
            timing->d2h_ms = elapsed_ms(d2h_start, Clock::now());
            timing->d2h_bytes = static_cast<uint64_t>(hidden_width * payload_count * sizeof(float));
        }
    } while (false);

    if (gate_start != nullptr) cudaEventDestroy(gate_start);
    if (gate_end != nullptr) cudaEventDestroy(gate_end);
    if (down_start != nullptr) cudaEventDestroy(down_start);
    if (down_end != nullptr) cudaEventDestroy(down_end);
    const auto free_start = Clock::now();
    cudaFree(d_output);
    cudaFree(d_activated_qs);
    cudaFree(d_activated_scales);
    cudaFree(d_activated);
    cudaFree(d_gate_up_values);
    cudaFree(d_payloads);
    cudaFree(d_hidden_qs);
    cudaFree(d_hidden_scales);
    cudaFree(d_hidden);
    if (timing != nullptr) {
        timing->free_ms = elapsed_ms(free_start, Clock::now());
    }
    return result;
}

extern "C" int pom_moe_nvfp4_forward_route_batch_resident(
    const uint8_t * const * payloads,
    std::size_t payload_count,
    const uint32_t * token_ids,
    const float * weights,
    const std::size_t * token_offsets,
    std::size_t token_offsets_len,
    std::size_t token_count,
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
        timing->expert_calls = static_cast<uint64_t>(payload_count);
        timing->kernel_launches = 6;
        timing->sync_count = 2;
    }
    if (payloads == nullptr || token_ids == nullptr || weights == nullptr ||
        token_offsets == nullptr || hidden == nullptr || output == nullptr ||
        payload_count == 0 || token_count == 0 || hidden_width == 0 ||
        intermediate_width == 0 || hidden_width % kBlockElements != 0 ||
        intermediate_width % kBlockElements != 0 || token_offsets_len != token_count + 1 ||
        hidden_len != token_count * hidden_width || output_len != hidden_len ||
        token_offsets[0] != 0 || token_offsets[token_count] != payload_count) {
        return 1;
    }
    for (std::size_t pair = 0; pair < payload_count; ++pair) {
        if (payloads[pair] == nullptr || token_ids[pair] >= token_count ||
            !std::isfinite(weights[pair])) {
            return 1;
        }
    }
    for (std::size_t token = 0; token < token_count; ++token) {
        if (token_offsets[token] > token_offsets[token + 1]) {
            return 1;
        }
    }

    float * d_hidden = nullptr;
    float * d_hidden_scales = nullptr;
    int8_t * d_hidden_qs = nullptr;
    const uint8_t ** d_payloads = nullptr;
    uint32_t * d_token_ids = nullptr;
    float * d_weights = nullptr;
    std::size_t * d_token_offsets = nullptr;
    float * d_gate_up_values = nullptr;
    float * d_activated = nullptr;
    float * d_activated_scales = nullptr;
    int8_t * d_activated_qs = nullptr;
    float * d_pair_output = nullptr;
    float * d_output = nullptr;
    cudaEvent_t gate_start = nullptr;
    cudaEvent_t gate_end = nullptr;
    cudaEvent_t down_start = nullptr;
    cudaEvent_t down_end = nullptr;
    int result = 0;

    do {
        const auto alloc_start = Clock::now();
        result = cuda_status(cudaMalloc(reinterpret_cast<void **>(&d_hidden),
            token_count * hidden_width * sizeof(float)));
        if (result != 0) break;
        result = cuda_status(cudaMalloc(reinterpret_cast<void **>(&d_hidden_scales),
            token_count * (hidden_width / 32) * sizeof(float)));
        if (result != 0) break;
        result = cuda_status(cudaMalloc(reinterpret_cast<void **>(&d_hidden_qs),
            token_count * hidden_width * sizeof(int8_t)));
        if (result != 0) break;
        result = cuda_status(cudaMalloc(reinterpret_cast<void **>(&d_payloads),
            payload_count * sizeof(*d_payloads)));
        if (result != 0) break;
        result = cuda_status(cudaMalloc(reinterpret_cast<void **>(&d_token_ids),
            payload_count * sizeof(*d_token_ids)));
        if (result != 0) break;
        result = cuda_status(cudaMalloc(reinterpret_cast<void **>(&d_weights),
            payload_count * sizeof(*d_weights)));
        if (result != 0) break;
        result = cuda_status(cudaMalloc(reinterpret_cast<void **>(&d_token_offsets),
            token_offsets_len * sizeof(*d_token_offsets)));
        if (result != 0) break;
        result = cuda_status(cudaMalloc(reinterpret_cast<void **>(&d_gate_up_values),
            2 * intermediate_width * payload_count * sizeof(float)));
        if (result != 0) break;
        result = cuda_status(cudaMalloc(reinterpret_cast<void **>(&d_activated),
            intermediate_width * payload_count * sizeof(float)));
        if (result != 0) break;
        result = cuda_status(cudaMalloc(reinterpret_cast<void **>(&d_activated_scales),
            payload_count * (intermediate_width / 32) * sizeof(float)));
        if (result != 0) break;
        result = cuda_status(cudaMalloc(reinterpret_cast<void **>(&d_activated_qs),
            intermediate_width * payload_count * sizeof(int8_t)));
        if (result != 0) break;
        result = cuda_status(cudaMalloc(reinterpret_cast<void **>(&d_pair_output),
            hidden_width * payload_count * sizeof(float)));
        if (result != 0) break;
        result = cuda_status(cudaMalloc(reinterpret_cast<void **>(&d_output),
            token_count * hidden_width * sizeof(float)));
        if (result != 0) break;
        if (timing != nullptr) {
            timing->alloc_ms = elapsed_ms(alloc_start, Clock::now());
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
        result = cuda_status(cudaMemcpy(d_hidden, hidden,
            token_count * hidden_width * sizeof(float), cudaMemcpyHostToDevice));
        if (result != 0) break;
        result = cuda_status(cudaMemcpy(d_payloads, payloads,
            payload_count * sizeof(*d_payloads), cudaMemcpyHostToDevice));
        if (result != 0) break;
        result = cuda_status(cudaMemcpy(d_token_ids, token_ids,
            payload_count * sizeof(*d_token_ids), cudaMemcpyHostToDevice));
        if (result != 0) break;
        result = cuda_status(cudaMemcpy(d_weights, weights,
            payload_count * sizeof(*d_weights), cudaMemcpyHostToDevice));
        if (result != 0) break;
        result = cuda_status(cudaMemcpy(d_token_offsets, token_offsets,
            token_offsets_len * sizeof(*d_token_offsets), cudaMemcpyHostToDevice));
        if (result != 0) break;
        if (timing != nullptr) {
            timing->h2d_ms = elapsed_ms(h2d_start, Clock::now());
            timing->h2d_bytes = static_cast<uint64_t>(
                token_count * hidden_width * sizeof(float) +
                payload_count * sizeof(*d_payloads) +
                payload_count * sizeof(*d_token_ids) +
                payload_count * sizeof(*d_weights) +
                token_offsets_len * sizeof(*d_token_offsets));
        }

        const int block_size = 128;
        const auto gate_launch_start = Clock::now();
        if (timing != nullptr) {
            result = cuda_status(cudaEventRecord(gate_start));
            if (result != 0) break;
        }
        quantize_q8_0_kernel<<<(token_count * ((hidden_width + 31) / 32)), 32>>>(
            d_hidden, static_cast<int>(hidden_width), d_hidden_scales, d_hidden_qs,
            static_cast<int>(token_count));
        result = cuda_status(cudaGetLastError());
        if (result != 0) break;
        gate_up_route_batch_resident_kernel<<<
            (2 * intermediate_width * payload_count + block_size - 1) / block_size,
            block_size>>>(
                d_payloads, d_token_ids, static_cast<int>(payload_count),
                static_cast<int>(intermediate_width), static_cast<int>(hidden_width),
                d_hidden_qs, d_hidden_scales, d_gate_up_values);
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
            timing->sync_ms += std::max(0.0, elapsed_ms(gate_sync_start, Clock::now()) -
                static_cast<double>(kernel_ms));
        }

        const auto down_launch_start = Clock::now();
        if (timing != nullptr) {
            result = cuda_status(cudaEventRecord(down_start));
            if (result != 0) break;
        }
        geglu_kernel<<<(intermediate_width * payload_count + block_size - 1) / block_size,
            block_size>>>(
                d_gate_up_values, static_cast<int>(payload_count),
                static_cast<int>(intermediate_width), d_activated);
        result = cuda_status(cudaGetLastError());
        if (result != 0) break;
        quantize_q8_0_kernel<<<
            ((intermediate_width + 31) / 32) * payload_count, 32>>>(
                d_activated, static_cast<int>(intermediate_width), d_activated_scales,
                d_activated_qs, static_cast<int>(payload_count));
        result = cuda_status(cudaGetLastError());
        if (result != 0) break;
        down_route_batch_resident_kernel<<<
            (hidden_width * payload_count + block_size - 1) / block_size, block_size>>>(
                d_payloads, static_cast<int>(payload_count), static_cast<int>(hidden_width),
                static_cast<int>(intermediate_width), d_activated_qs, d_activated_scales,
                d_pair_output);
        result = cuda_status(cudaGetLastError());
        if (result != 0) break;
        aggregate_route_batch_kernel<<<
            (token_count * hidden_width + block_size - 1) / block_size, block_size>>>(
                d_pair_output, d_weights, d_token_offsets, static_cast<int>(token_count),
                static_cast<int>(hidden_width), d_output);
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
            timing->sync_ms += std::max(0.0, elapsed_ms(down_sync_start, Clock::now()) -
                static_cast<double>(kernel_ms));
        }
        const auto d2h_start = Clock::now();
        result = cuda_status(cudaMemcpy(output, d_output,
            token_count * hidden_width * sizeof(float), cudaMemcpyDeviceToHost));
        if (timing != nullptr) {
            timing->d2h_ms = elapsed_ms(d2h_start, Clock::now());
            timing->d2h_bytes = static_cast<uint64_t>(
                token_count * hidden_width * sizeof(float));
        }
    } while (false);

    if (gate_start != nullptr) cudaEventDestroy(gate_start);
    if (gate_end != nullptr) cudaEventDestroy(gate_end);
    if (down_start != nullptr) cudaEventDestroy(down_start);
    if (down_end != nullptr) cudaEventDestroy(down_end);
    const auto free_start = Clock::now();
    cudaFree(d_output);
    cudaFree(d_pair_output);
    cudaFree(d_activated_qs);
    cudaFree(d_activated_scales);
    cudaFree(d_activated);
    cudaFree(d_gate_up_values);
    cudaFree(d_token_offsets);
    cudaFree(d_weights);
    cudaFree(d_token_ids);
    cudaFree(d_payloads);
    cudaFree(d_hidden_qs);
    cudaFree(d_hidden_scales);
    cudaFree(d_hidden);
    if (timing != nullptr) {
        timing->free_ms = elapsed_ms(free_start, Clock::now());
    }
    return result;
}

extern "C" int pom_moe_device_free_bytes(std::uint64_t * free_bytes) {
    if (free_bytes == nullptr) {
        return 1;
    }
    std::size_t free = 0;
    std::size_t total = 0;
    const auto status = cudaMemGetInfo(&free, &total);
    if (status != cudaSuccess) {
        return cuda_status(status);
    }
    *free_bytes = static_cast<std::uint64_t>(free);
    return 0;
}

extern "C" int pom_moe_device_upload(
    const std::uint8_t * payload,
    std::size_t payload_len,
    void ** out
) {
    if (payload == nullptr || payload_len == 0 || out == nullptr) {
        return 1;
    }
    *out = nullptr;
    auto status = cuda_status(cudaMalloc(out, payload_len));
    if (status != 0) {
        return status;
    }
    status = cuda_status(cudaMemcpy(*out, payload, payload_len, cudaMemcpyHostToDevice));
    if (status != 0) {
        cudaFree(*out);
        *out = nullptr;
    }
    return status;
}

extern "C" void pom_moe_device_free(void * handle) {
    if (handle != nullptr) {
        cudaFree(handle);
    }
}

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
    return pom_moe_nvfp4_forward_batch(
        payload,
        payload_len,
        1,
        hidden,
        hidden_len,
        output,
        output_len,
        hidden_width,
        intermediate_width,
        timing
    );
}
