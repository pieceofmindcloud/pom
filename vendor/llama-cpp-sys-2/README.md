# llama-cpp-sys

Raw bindings to llama.cpp with cuda support.

See [llama-cpp-2](https://crates.io/crates/llama-cpp-2) for a safe API.

O `wrapper_common` também expõe as sondagens que o POM usa para ajustar a
memória: a reserva seca do segundo contexto MTP por dispositivo e o diagnóstico
dos buffers de contexto. O recuo para RAM usa um modelo irmão CPU-only para
manter todo o grafo `nextn` no host e desliga o offload do cache MTP. Um
override apenas nos tensores `nextn` foi investigado, mas não basta porque o
scheduler pode seguir a colocação da camada e do output head. Isto pertence à
ponte do POM; o checkout de `llama.cpp` continua sem alterações.
