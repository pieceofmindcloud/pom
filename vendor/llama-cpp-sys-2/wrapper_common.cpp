#include "wrapper_common.h"

#include <cstdlib>
#include <cstring>
#include <exception>
#include <map>
#include <memory>
#include <string>
#include <stdint.h>
#include <vector>

#include "llama.cpp/common/common.h"
// Cabeçalhos internos: `llama_model` (`tensors_by_name`, `dev_layer`) e o
// `llama-ext.h` de estágio (`llama_get_memory_breakdown`). Nada disso está no
// `llama.h` público, e é justamente o que responde onde cada camada ficou.
#include "llama.cpp/src/llama-ext.h"
#include "llama.cpp/src/llama-model.h"
#include "llama.cpp/ggml/include/ggml-backend.h"
#include "llama.cpp/common/fit.h"
#include "llama.cpp/common/json-schema-to-grammar.h"
#include "llama.cpp/common/speculative.h"
#include "llama.cpp/include/llama.h"
#include "wrapper_utils.h"

#include <nlohmann/json.hpp>

extern "C" llama_rs_status llama_rs_json_schema_to_grammar(
    const char * schema_json,
    bool force_gbnf,
    char ** out_grammar) {
    if (!schema_json || !out_grammar) {
        return LLAMA_RS_STATUS_INVALID_ARGUMENT;
    }

    *out_grammar = nullptr;
    try {
        const auto schema = nlohmann::ordered_json::parse(schema_json);
        const auto grammar = json_schema_to_grammar(schema, force_gbnf);
        *out_grammar = llama_rs_dup_string(grammar);
        return *out_grammar ? LLAMA_RS_STATUS_OK : LLAMA_RS_STATUS_ALLOCATION_FAILED;
    } catch (const std::exception &) {
        return LLAMA_RS_STATUS_EXCEPTION;
    }
}

extern "C" void llama_rs_string_free(char * ptr) {
    if (ptr) {
        std::free(ptr);
    }
}

extern "C" struct llama_sampler * llama_rs_sampler_init_grammar(
    const struct llama_vocab * vocab,
    const char * grammar_str,
    const char * grammar_root) {
    try {
        return llama_sampler_init_grammar(vocab, grammar_str, grammar_root);
    } catch (...) {
        return nullptr;
    }
}

extern "C" struct llama_sampler * llama_rs_sampler_init_grammar_lazy(
    const struct llama_vocab * vocab,
    const char * grammar_str,
    const char * grammar_root,
    const char ** trigger_words,
    size_t num_trigger_words,
    const llama_token * trigger_tokens,
    size_t num_trigger_tokens) {
    try {
        std::vector<std::string> trigger_patterns;
        trigger_patterns.reserve(num_trigger_words);
        for (size_t i = 0; i < num_trigger_words; ++i) {
            const char * word = trigger_words ? trigger_words[i] : nullptr;
            if (word && word[0] != '\0') {
                trigger_patterns.push_back(regex_escape(word));
            }
        }
        std::vector<const char *> trigger_patterns_c;
        trigger_patterns_c.reserve(trigger_patterns.size());
        for (const auto & pattern : trigger_patterns) {
            trigger_patterns_c.push_back(pattern.c_str());
        }
        return llama_sampler_init_grammar_lazy_patterns(
            vocab,
            grammar_str,
            grammar_root,
            trigger_patterns_c.data(),
            trigger_patterns_c.size(),
            trigger_tokens,
            num_trigger_tokens);
    } catch (...) {
        return nullptr;
    }
}

extern "C" struct llama_sampler * llama_rs_sampler_init_grammar_lazy_patterns(
    const struct llama_vocab * vocab,
    const char * grammar_str,
    const char * grammar_root,
    const char ** trigger_patterns,
    size_t num_trigger_patterns,
    const llama_token * trigger_tokens,
    size_t num_trigger_tokens) {
    try {
        return llama_sampler_init_grammar_lazy_patterns(
            vocab,
            grammar_str,
            grammar_root,
            trigger_patterns,
            num_trigger_patterns,
            trigger_tokens,
            num_trigger_tokens);
    } catch (...) {
        return nullptr;
    }
}

extern "C" llama_rs_status llama_rs_sampler_accept(struct llama_sampler * sampler, llama_token token) {
    if (!sampler) {
        return LLAMA_RS_STATUS_INVALID_ARGUMENT;
    }
    try {
        llama_sampler_accept(sampler, token);
        return LLAMA_RS_STATUS_OK;
    } catch (const std::exception &) {
        return LLAMA_RS_STATUS_EXCEPTION;
    } catch (...) {
        return LLAMA_RS_STATUS_EXCEPTION;
    }
}

// Thin pass-through to llama.cpp's common_fit_params (a C++ symbol in libcommon).
// Returns common_params_fit_status as an int: 0 = success, 1 = failure, 2 = error.
extern "C" int llama_rs_fit_params(
    const char * path_model,
    struct llama_model_params * mparams,
    struct llama_context_params * cparams,
    float * tensor_split,
    struct llama_model_tensor_buft_override * tensor_buft_overrides,
    size_t * margins,
    uint32_t n_ctx_min,
    enum ggml_log_level log_level) {
    return static_cast<int>(common_fit_params(
        path_model,
        mparams,
        cparams,
        tensor_split,
        tensor_buft_overrides,
        margins,
        n_ctx_min,
        log_level));
}

extern "C" void llama_rs_memory_breakdown_print(const struct llama_context * ctx) {
    common_memory_breakdown_print(ctx);
}

// ── Onde o modelo ficou ───────────────────────────────────────────────────────

// O bloco a que um tensor pertence, ou -1 quando ele não é de bloco nenhum
// (embeddings, norma de saída, cabeça). Os nomes do GGUF são `blk.<i>.<resto>`.
static int llama_rs_tensor_block(const std::string & name) {
    static const std::string prefix = "blk.";
    if (name.compare(0, prefix.size(), prefix) != 0) {
        return -1;
    }
    const size_t dot = name.find('.', prefix.size());
    if (dot == std::string::npos) {
        return -1;
    }
    try {
        return std::stoi(name.substr(prefix.size(), dot - prefix.size()));
    } catch (const std::exception &) {
        return -1;
    }
}

// Quantos bytes de um grupo de tensores foram parar em cada buffer.
// `map` ordenado só para a saída sair estável entre execuções.
using llama_rs_buffer_bytes = std::map<std::string, size_t>;

static void llama_rs_account(llama_rs_buffer_bytes & into, const ggml_tensor * t) {
    if (!t || !t->buffer) {
        return;
    }
    into[ggml_backend_buffer_name(t->buffer)] += ggml_nbytes(t);
}

static nlohmann::ordered_json llama_rs_buffers_json(const llama_rs_buffer_bytes & bytes) {
    auto out = nlohmann::ordered_json::array();
    for (const auto & [name, n] : bytes) {
        out.push_back({{"buffer", name}, {"bytes", n}});
    }
    return out;
}

extern "C" llama_rs_status llama_rs_model_placement_json(const struct llama_model * model, char ** out_json) {
    if (!model || !out_json) {
        return LLAMA_RS_STATUS_INVALID_ARGUMENT;
    }
    *out_json = nullptr;

    try {
        const int n_layer = llama_model_n_layer(model);

        std::vector<llama_rs_buffer_bytes> per_layer(n_layer > 0 ? (size_t) n_layer : 0);
        llama_rs_buffer_bytes other;
        llama_rs_buffer_bytes totals;

        for (const auto & [name, tensor] : model->tensors_by_name) {
            const int il = llama_rs_tensor_block(name);
            if (il >= 0 && il < n_layer) {
                llama_rs_account(per_layer[(size_t) il], tensor);
            } else {
                llama_rs_account(other, tensor);
            }
            llama_rs_account(totals, tensor);
        }

        auto layers = nlohmann::ordered_json::array();
        for (int il = 0; il < n_layer; ++il) {
            // O dispositivo é a atribuição que o llama.cpp fez ao bloco; os
            // buffers são onde os pesos dele estão. Os dois divergem
            // exatamente no caso que interessa ver.
            ggml_backend_dev_t dev = model->dev_layer(il);
            layers.push_back({
                {"layer",   il},
                {"device",  dev ? ggml_backend_dev_name(dev) : "CPU"},
                {"buffers", llama_rs_buffers_json(per_layer[(size_t) il])},
            });
        }

        ggml_backend_dev_t dev_out = model->dev_output();
        auto devices = nlohmann::ordered_json::array();
        for (int i = 0; i < llama_model_n_devices(model); ++i) {
            ggml_backend_dev_t dev = llama_model_get_device(model, i);
            if (!dev) {
                continue;
            }
            devices.push_back({
                {"name",        ggml_backend_dev_name(dev)},
                {"description", ggml_backend_dev_description(dev)},
            });
        }

        const nlohmann::ordered_json out = {
            {"n_layer",              n_layer},
            {"n_gpu_layers",         model->n_gpu_layers()},
            {"has_tensor_overrides", model->has_tensor_overrides()},
            {"devices",              devices},
            {"layers",               layers},
            {"output", {
                {"device",  dev_out ? ggml_backend_dev_name(dev_out) : "CPU"},
                {"buffers", llama_rs_buffers_json(other)},
            }},
            {"totals", llama_rs_buffers_json(totals)},
        };

        *out_json = llama_rs_dup_string(out.dump());
        return *out_json ? LLAMA_RS_STATUS_OK : LLAMA_RS_STATUS_ALLOCATION_FAILED;
    } catch (const std::exception &) {
        return LLAMA_RS_STATUS_EXCEPTION;
    }
}

extern "C" llama_rs_status llama_rs_context_memory_json(const struct llama_context * ctx, char ** out_json) {
    if (!ctx || !out_json) {
        return LLAMA_RS_STATUS_INVALID_ARGUMENT;
    }
    *out_json = nullptr;

    try {
        auto buffers = nlohmann::ordered_json::array();
        for (const auto & [buft, data] : llama_get_memory_breakdown(ctx)) {
            if (!buft) {
                continue;
            }
            ggml_backend_dev_t dev = ggml_backend_buft_get_device(buft);
            buffers.push_back({
                {"buffer",  ggml_backend_buft_name(buft)},
                {"device",  dev ? ggml_backend_dev_name(dev) : "CPU"},
                {"model",   data.model},
                // `context` é o cache KV (mais o estado recorrente, quando o
                // modelo tem um). É esta linha que responde onde o KV ficou.
                {"context", data.context},
                {"compute", data.compute},
            });
        }

        const nlohmann::ordered_json out = {{"buffers", buffers}};
        *out_json = llama_rs_dup_string(out.dump());
        return *out_json ? LLAMA_RS_STATUS_OK : LLAMA_RS_STATUS_ALLOCATION_FAILED;
    } catch (const std::exception &) {
        return LLAMA_RS_STATUS_EXCEPTION;
    }
}

struct llama_rs_mtp_speculative {
    common_params_speculative params;
    common_speculative * spec = nullptr;
    std::vector<llama_token> prompt;
    std::vector<llama_token> draft;
    size_t last_draft_len = 0;
    bool draft_pending = false;
};

static constexpr llama_seq_id LLAMA_RS_MTP_SEQ_ID = 0;

static bool llama_rs_mtp_batch_compatible(const struct llama_batch & batch) {
    if (batch.n_tokens <= 0 || !batch.token || batch.embd || !batch.pos || !batch.n_seq_id ||
        !batch.seq_id) {
        return false;
    }
    for (int32_t k = 0; k < batch.n_tokens; ++k) {
        if (batch.n_seq_id[k] != 1 || !batch.seq_id[k] ||
            batch.seq_id[k][0] != LLAMA_RS_MTP_SEQ_ID) {
            return false;
        }
    }
    return true;
}

static void llama_rs_assign_tokens(
    std::vector<llama_token> & dst,
    const llama_token * tokens,
    size_t count) {
    if (count == 0) {
        dst.clear();
        return;
    }
    dst.assign(tokens, tokens + count);
}

extern "C" struct llama_rs_mtp_speculative * llama_rs_mtp_speculative_init(
    struct llama_context * ctx_tgt,
    struct llama_context * ctx_dft,
    int32_t n_max,
    int32_t n_min,
    float p_min) {
    if (!ctx_tgt || !ctx_dft || n_max <= 0 || n_min < 0 || n_min > n_max) {
        return nullptr;
    }

    try {
        auto wrapper = std::make_unique<llama_rs_mtp_speculative>();
        wrapper->params.types = { COMMON_SPECULATIVE_TYPE_DRAFT_MTP };
        wrapper->params.draft.ctx_tgt = ctx_tgt;
        wrapper->params.draft.ctx_dft = ctx_dft;
        wrapper->params.draft.n_max = n_max;
        wrapper->params.draft.n_min = n_min;
        wrapper->params.draft.p_min = p_min;

        wrapper->spec = common_speculative_init(wrapper->params, 1);
        if (!wrapper->spec) {
            return nullptr;
        }

        return wrapper.release();
    } catch (...) {
        return nullptr;
    }
}

extern "C" void llama_rs_mtp_speculative_free(struct llama_rs_mtp_speculative * spec) {
    if (!spec) {
        return;
    }
    if (spec->spec) {
        common_speculative_free(spec->spec);
        spec->spec = nullptr;
    }
    delete spec;
}

extern "C" llama_rs_status llama_rs_mtp_speculative_begin(
    struct llama_rs_mtp_speculative * spec,
    const llama_token * prompt_tokens,
    size_t prompt_tokens_count) {
    if (!spec || !spec->spec || (!prompt_tokens && prompt_tokens_count > 0)) {
        return LLAMA_RS_STATUS_INVALID_ARGUMENT;
    }

    try {
        llama_rs_assign_tokens(spec->prompt, prompt_tokens, prompt_tokens_count);
        spec->last_draft_len = 0;
        spec->draft_pending = false;
        common_speculative_begin(spec->spec, LLAMA_RS_MTP_SEQ_ID, spec->prompt);
        return LLAMA_RS_STATUS_OK;
    } catch (...) {
        return LLAMA_RS_STATUS_EXCEPTION;
    }
}

extern "C" llama_rs_status llama_rs_mtp_speculative_process(
    struct llama_rs_mtp_speculative * spec,
    const struct llama_batch * batch) {
    if (!spec || !spec->spec || !batch) {
        return LLAMA_RS_STATUS_INVALID_ARGUMENT;
    }
    if (!llama_rs_mtp_batch_compatible(*batch)) {
        return LLAMA_RS_STATUS_INVALID_ARGUMENT;
    }

    try {
        return common_speculative_process(spec->spec, *batch)
            ? LLAMA_RS_STATUS_OK
            : LLAMA_RS_STATUS_EXCEPTION;
    } catch (...) {
        return LLAMA_RS_STATUS_EXCEPTION;
    }
}

extern "C" llama_rs_status llama_rs_mtp_speculative_draft(
    struct llama_rs_mtp_speculative * spec,
    llama_pos n_past,
    llama_token id_last,
    const llama_token * prompt_tokens,
    size_t prompt_tokens_count,
    llama_token * out_tokens,
    size_t out_tokens_capacity,
    size_t * out_tokens_count) {
    if (!spec || !spec->spec || (!prompt_tokens && prompt_tokens_count > 0) ||
        !out_tokens_count || n_past < 0) {
        return LLAMA_RS_STATUS_INVALID_ARGUMENT;
    }

    try {
        if (spec->draft_pending) {
            return LLAMA_RS_STATUS_INVALID_ARGUMENT;
        }
        llama_rs_assign_tokens(spec->prompt, prompt_tokens, prompt_tokens_count);
        spec->draft.clear();
        spec->last_draft_len = 0;

        auto & params = common_speculative_get_draft_params(spec->spec, LLAMA_RS_MTP_SEQ_ID);
        params = {
            true,
            spec->params.draft.n_max,
            n_past,
            id_last,
            &spec->prompt,
            &spec->draft,
        };

        common_speculative_draft(spec->spec);

        *out_tokens_count = spec->draft.size();
        if (spec->draft.size() > out_tokens_capacity) {
            return LLAMA_RS_STATUS_ALLOCATION_FAILED;
        }
        if (!spec->draft.empty() && !out_tokens) {
            return LLAMA_RS_STATUS_INVALID_ARGUMENT;
        }
        if (!spec->draft.empty()) {
            std::memcpy(out_tokens, spec->draft.data(), spec->draft.size() * sizeof(llama_token));
        }
        spec->last_draft_len = spec->draft.size();
        spec->draft_pending = !spec->draft.empty();
        return LLAMA_RS_STATUS_OK;
    } catch (...) {
        return LLAMA_RS_STATUS_EXCEPTION;
    }
}

extern "C" llama_rs_status llama_rs_mtp_speculative_accept(
    struct llama_rs_mtp_speculative * spec,
    uint16_t n_accepted) {
    if (!spec || !spec->spec) {
        return LLAMA_RS_STATUS_INVALID_ARGUMENT;
    }
    if (!spec->draft_pending || n_accepted > spec->last_draft_len) {
        return LLAMA_RS_STATUS_INVALID_ARGUMENT;
    }

    try {
        common_speculative_accept(spec->spec, LLAMA_RS_MTP_SEQ_ID, n_accepted);
        spec->last_draft_len = 0;
        spec->draft_pending = false;
        return LLAMA_RS_STATUS_OK;
    } catch (...) {
        return LLAMA_RS_STATUS_EXCEPTION;
    }
}
