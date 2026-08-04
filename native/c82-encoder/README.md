# C82 native encoder

This helper is Rayline’s narrow process boundary around
`davidvgilmore/llama.cpp` commit
`8c5d694fe7e28e8973349b634a72fe7683ecc940`, a minimal fork of upstream
`ggml-org/llama.cpp` tag `b10153` (commit
`b77d646751d01c0962bc203b6809e9d94f7d50b7`).

It uses libllama’s Qwen3.5 tokenization, recurrent state, token embeddings,
Metal backend, and CUDA backend. The fork adds cumulative FP32 mean pooling
and opaque snapshot/restore APIs because upstream graph-level
`LLAMA_POOLING_TYPE_MEAN` is batch-local and does not preserve the policy
contract across incremental recurrent decode calls. Rayline checkpoints that
pooling state beside libllama model memory. Frozen Metal and CUDA parity gate
every release of this helper. The fork is intentionally limited to this
upstream gap and remains PR-ready for later submission.

Flash attention is deliberately disabled. Both `b9585` and the current
`b10153` Metal backend returned a first non-finite token embedding at token
31,936 for the frozen repeated-token probe, while the upstream non-flash
libllama path remained finite. The manifest and health response pin this choice
so an upstream upgrade cannot silently re-enable the failing path. A 512-token
physical micro-batch also avoids the upstream Metal page fault observed at the
262,144-token boundary with a 2,048-token micro-batch; the logical incremental
checkpoint grid remains 8,192 tokens.
CUDA builds also disable NCCL: C82 owns one user GPU, so a collective
communication dependency would add no value and would make the portable helper
require a separately installed `libnccl`.

Startup executes a real forward and audits libllama’s scheduled buffers.
Readiness requires selected-device model compute and zero compute nodes on any
other device. The health record separately reports the observed host-side token
lookup and metadata views as boundary staging, rather than hiding them as GPU
compute or misclassifying them as fallback.

Configure an audited checkout with:

```sh
cmake -S native/c82-encoder -B build/c82-encoder \
  -DLLAMA_CPP_SOURCE=/path/to/llama.cpp \
  -DGGML_METAL=ON
cmake --build build/c82-encoder --target rayline-c82-encoder -j
```

For CUDA, use `-DGGML_CUDA=ON` and disable Metal. CMake rejects a Git checkout
whose HEAD differs from the pinned revision. Release manifests additionally
pin the helper binary and lossless BF16 GGUF by SHA-256.
