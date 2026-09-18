#!/usr/bin/env python3
"""Gera um GGUF qwen4exp minimo e estruturalmente valido, para reproduzir o
defeito da tarefa sl-mtp-draft-attach (ver report.md na raiz da tarefa).

mode="draft": espelha o rascunho externo real da Unsloth ("shared MTP
module") - so o ultimo bloco (indice n_layer_all - 1) mais
token_embd/output/nextn.*, sem os tensores de nivel de modelo
output_hc_{norm,down,up}. Falha esperada: "check_tensor_dims: tensor
'output_hc_norm.weight' not found", igual ao log da ksia.

mode="draft_fixed": igual ao anterior, mas com output_hc_{norm,down,up}
tambem presentes. Falha esperada um passo depois, no primeiro tensor de
tronco ausente (blk.0.*) - prova que só adicionar os 3 tensores (hipotese 1)
nao resolve, porque o intervalo de tronco (blk.0..n_layer-1) continua sem
gate TENSOR_NOT_REQUIRED em `load_arch_tensors` (qwen4exp.cpp).

O fixture `qwen4exp-external-draft-sem-output-hc.gguf` neste diretorio é a
saida deste script em mode="draft"; regenere com:

    python3 gerar_gguf_sintetico.py qwen4exp-external-draft-sem-output-hc.gguf draft

Requer o gguf-py do fork vendorizado no PYTHONPATH e numpy instalado.
"""
import sys
sys.path.insert(0, "/home/tiago/projects/rdma-p2p/llama.cpp/gguf-py")

import numpy as np
import gguf

N_EMBD = 8
N_VOCAB = 6
HC = 2
HC_LOW_RANK = 4
HC_DIM = HC * N_EMBD
SSM_D_CONV = 2
SSM_D_INNER = 4
SSM_D_STATE = 2
SSM_DT_RANK = 2
SSM_N_GROUP = 1
IDX_HEADS = 1
IDX_HEAD_DIM = 2
IDX_TOPK = 1
N_HEAD = 2
N_HEAD_KV = 2
N_EMBD_HEAD = N_EMBD // N_HEAD
N_FF = 8
N_FF_SHEXP = 8
N_EXPERT = 2
N_EXPERT_USED = 1
BLOCK_COUNT = 2          # n_layer_all: blk.0 (trunk-only) + blk.1 (nextn block)
NEXTN_PREDICT_LAYERS = 1  # n_layer() = BLOCK_COUNT - NEXTN_PREDICT_LAYERS = 1


def f32(*shape):
    return np.zeros(shape, dtype=np.float32)


def write(path: str, mode: str):
    w = gguf.GGUFWriter(path, "qwen4exp")
    w.add_type(gguf.GGUFType.MODEL)
    w.add_name("pom-synthetic-qwen4exp")
    w.add_vocab_size(N_VOCAB)
    w.add_context_length(64)
    w.add_embedding_length(N_EMBD)
    w.add_block_count(BLOCK_COUNT)
    w.add_feed_forward_length(N_FF)
    w.add_head_count(N_HEAD)
    w.add_head_count_kv(N_HEAD_KV)
    w.add_layer_norm_rms_eps(1e-5)
    w.add_rope_dimension_count(N_EMBD_HEAD)
    w.add_rope_dimension_sections([1, 1, 1, 1])
    w.add_file_type(gguf.LlamaFileType.ALL_F32)

    w.add_expert_count(N_EXPERT)
    w.add_expert_used_count(N_EXPERT_USED)
    w.add_expert_shared_feed_forward_length(N_FF_SHEXP)

    w.add_ssm_conv_kernel(SSM_D_CONV)
    w.add_ssm_inner_size(SSM_D_INNER)
    w.add_ssm_state_size(SSM_D_STATE)
    w.add_ssm_time_step_rank(SSM_DT_RANK)
    w.add_ssm_group_count(SSM_N_GROUP)

    w.add_hyper_connection_count(HC)
    w.add_hyper_connection_low_rank(HC_LOW_RANK)

    w.add_indexer_head_count(IDX_HEADS)
    w.add_indexer_key_length(IDX_HEAD_DIM)
    w.add_indexer_top_k(IDX_TOPK)

    w.add_uint32("qwen4exp.nextn_predict_layers", NEXTN_PREDICT_LAYERS)

    w.add_tokenizer_model("none")
    w.add_token_list([f"<t{i}>" for i in range(N_VOCAB)])
    w.add_token_types([int(gguf.TokenType.NORMAL)] * N_VOCAB)

    # --- tensors -----------------------------------------------------
    w.add_tensor("token_embd.weight", f32(N_VOCAB, N_EMBD))
    w.add_tensor("output.weight", f32(N_VOCAB, N_EMBD))

    if mode in ("draft_fixed",):
        w.add_tensor("output_hc_norm.weight", f32(HC, N_EMBD))
        w.add_tensor("output_hc_down.weight", f32(HC_LOW_RANK, HC_DIM))
        w.add_tensor("output_hc_up.weight", f32(HC_DIM, HC_LOW_RANK))

    il = BLOCK_COUNT - 1  # 1: the only block present, mirrors "blk.48" in the real draft
    key_dim = SSM_D_STATE * SSM_N_GROUP
    value_dim = SSM_D_STATE * SSM_DT_RANK
    conv_dim = key_dim * 2 + value_dim

    def blk(name, *shape):
        w.add_tensor(f"blk.{il}.{name}", f32(*shape))

    blk("attn_k.weight", N_EMBD, N_EMBD_HEAD * N_HEAD_KV)
    blk("attn_k_norm.weight", N_EMBD_HEAD)
    blk("attn_output.weight", N_EMBD_HEAD * N_HEAD, N_EMBD)
    blk("attn_q.weight", N_EMBD, N_EMBD_HEAD * N_HEAD * 2)
    blk("attn_q_norm.weight", N_EMBD_HEAD)
    blk("attn_v.weight", N_EMBD, N_EMBD_HEAD * N_HEAD_KV)
    blk("ffn_down_exps.weight", N_FF, N_EMBD, N_EXPERT)
    blk("ffn_down_shexp.weight", N_FF_SHEXP, N_EMBD)
    blk("ffn_gate_exps.weight", N_EMBD, N_FF, N_EXPERT)
    blk("ffn_gate_inp.weight", N_EMBD, N_EXPERT)
    blk("ffn_gate_inp_shexp.weight", N_EMBD)
    blk("ffn_gate_shexp.weight", N_EMBD, N_FF_SHEXP)
    blk("ffn_up_exps.weight", N_EMBD, N_FF, N_EXPERT)
    blk("ffn_up_shexp.weight", N_EMBD, N_FF_SHEXP)
    blk("hc_attn_down.weight", HC_DIM, HC_LOW_RANK)
    blk("hc_attn_inject.weight", HC_DIM, HC)
    blk("hc_attn_norm.weight", HC, N_EMBD)
    blk("hc_attn_up.weight", HC_LOW_RANK, HC_DIM)
    blk("hc_ffn_down.weight", HC_DIM, HC_LOW_RANK)
    blk("hc_ffn_inject.weight", HC_DIM, HC)
    blk("hc_ffn_norm.weight", HC, N_EMBD)
    blk("hc_ffn_up.weight", HC_LOW_RANK, HC_DIM)
    blk("indexer.k_norm.weight", IDX_HEAD_DIM)
    blk("indexer.k_proj.weight", N_EMBD, IDX_HEAD_DIM)
    blk("indexer.q_norm.weight", IDX_HEAD_DIM)
    blk("indexer.q_proj.weight", N_EMBD, IDX_HEADS * IDX_HEAD_DIM)
    blk("nextn.eh_proj.weight", 2 * N_EMBD, N_EMBD)
    blk("nextn.enorm.weight", N_EMBD)
    blk("nextn.hc_head_down.weight", HC_LOW_RANK, HC_DIM)
    blk("nextn.hc_head_norm.weight", HC, N_EMBD)
    blk("nextn.hc_head_up.weight", HC_DIM, HC_LOW_RANK)
    blk("nextn.hnorm.weight", N_EMBD)

    w.write_header_to_file()
    w.write_kv_data_to_file()
    w.write_tensors_to_file()
    w.close()


if __name__ == "__main__":
    out, mode = sys.argv[1], sys.argv[2]
    write(out, mode)
    print(f"wrote {out} (mode={mode})")
