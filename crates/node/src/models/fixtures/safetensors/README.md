# Safetensors header fixtures

Each `<name>.json` is the JSON header of a real public safetensors file (the
bytes after its 8-byte length: the tensor table and `__metadata__`), used by
the classification tests in `models/image_catalog.rs`. No weights are stored;
the tests write a sparse file whose data length matches the offsets.

They were fetched with two HTTP `Range` requests per file (`bytes=0-7` for the
header length, then the header itself), so no weights were downloaded.

| Fixture | Category | Source |
|---|---|---|
| `z_image_diffusion` | diffusion | `Comfy-Org/z_image_turbo`, `z_image_turbo_bf16.safetensors` |
| `qwen_image_diffusion` | diffusion | `Comfy-Org/Qwen-Image_ComfyUI`, `split_files/diffusion_models/qwen_image_fp8_e4m3fn.safetensors` |
| `flux_diffusion` | diffusion | `Kijai/flux-fp8`, `flux1-schnell-fp8-e4m3fn.safetensors` |
| `qwen3_4b_text_encoder` | text_encoder | `Comfy-Org/z_image_turbo`, `qwen_3_4b.safetensors` |
| `t5xxl_text_encoder` | text_encoder | `comfyanonymous/flux_text_encoders`, `t5xxl_fp8_e4m3fn.safetensors` |
| `clip_l_text_encoder` | text_encoder | `comfyanonymous/flux_text_encoders`, `clip_l.safetensors` |
| `flux_ae_vae` | vae | `Comfy-Org/z_image_turbo`, `ae.safetensors` |
| `sdxl_vae` | vae | `stabilityai/sdxl-vae`, `sdxl_vae.safetensors` |
| `sdxl_checkpoint` | checkpoint | `stabilityai/stable-diffusion-xl-base-1.0`, `sd_xl_base_1.0.safetensors` |
| `sd15_checkpoint` | checkpoint | `Comfy-Org/stable-diffusion-v1-5-archive`, `v1-5-pruned-emaonly-fp16.safetensors` |
| `lora` | lora | `ByteDance/SDXL-Lightning`, `sdxl_lightning_4step_lora.safetensors` |
| `lora_peft` | lora | `alimama-creative/FLUX.1-Turbo-Alpha`, `diffusion_pytorch_model.safetensors` |
| `controlnet` | controlnet | `comfyanonymous/ControlNet-v1-1_fp16_safetensors`, `control_v11p_sd15_canny_fp16.safetensors` |
| `clip_vision` | clip_vision | `Comfy-Org/sigclip_vision_384`, `sigclip_vision_patch14_384.safetensors` |
| `esrgan_upscale` | upscale | `Comfy-Org/Real-ESRGAN_repackaged`, `RealESRGAN_x4plus.safetensors` |
| `minimax_h3_diffusion` | diffusion | `Comfy-Org/MiniMax-H3`, `diffusion_models/minimax_h3_fl2va_pruned_int8_convrot.safetensors` |
| `minimax_h3_video_vae` | vae | `Comfy-Org/MiniMax-H3`, `vae/minimax_h3_video_vae_int8_convrot.safetensors` |
| `minimax_h3_audio_vae` | vae | `Comfy-Org/MiniMax-H3`, `vae/minimax_h3_audio_vae_fp32.safetensors` |

All repositories are on Hugging Face (`https://huggingface.co/<repo>/resolve/main/<file>`).
