#include "encoder.hpp"

#include "ggml-backend.h"

#include <algorithm>
#include <cctype>
#include <cmath>
#include <cstdio>
#include <limits>
#include <stdexcept>
#include <thread>

using json = nlohmann::json;

namespace {

constexpr const char * encoder_model = "Qwen/Qwen3.5-0.8B";
constexpr const char * encoder_revision =
    "2fc06364715b967f1860aea9cf38778875588b17";
constexpr size_t expected_embedding_dimension = 1024;

bool starts_with(
    const std::vector<llama_token> & value,
    const std::vector<llama_token> & prefix) {
    return value.size() >= prefix.size()
        && std::equal(prefix.begin(), prefix.end(), value.begin());
}

std::string lowercase(std::string value) {
    std::transform(
        value.begin(),
        value.end(),
        value.begin(),
        [](unsigned char ch) { return static_cast<char>(std::tolower(ch)); });
    return value;
}

struct resolved_backend {
    std::string name;
    ggml_backend_dev_t device = nullptr;
};

resolved_backend resolve_device(const std::string & requested) {
    const std::string normalized = lowercase(requested);
    if (normalized != "auto" && normalized != "metal"
        && normalized != "cuda" && normalized != "cpu") {
        throw std::runtime_error("device must be one of: auto, metal, cuda, cpu");
    }

    ggml_backend_dev_t metal = nullptr;
    ggml_backend_dev_t cuda = nullptr;
    ggml_backend_dev_t cpu = nullptr;
    for (size_t index = 0; index < ggml_backend_dev_count(); ++index) {
        const auto device = ggml_backend_dev_get(index);
        const std::string name = lowercase(ggml_backend_dev_name(device));
        const std::string description =
            lowercase(ggml_backend_dev_description(device));
        const bool gpu =
            ggml_backend_dev_type(device) == GGML_BACKEND_DEVICE_TYPE_GPU;
        if (metal == nullptr
            && gpu && (name.find("metal") != std::string::npos
                || description.find("metal") != std::string::npos
                || description.find("apple") != std::string::npos)) {
            metal = device;
        }
        if (cuda == nullptr
            && gpu && (name.find("cuda") != std::string::npos
                || description.find("cuda") != std::string::npos
                || description.find("nvidia") != std::string::npos)) {
            cuda = device;
        }
        if (cpu == nullptr
            && ggml_backend_dev_type(device) == GGML_BACKEND_DEVICE_TYPE_CPU) {
            cpu = device;
        }
    }

    if (normalized == "metal" && metal == nullptr) {
        throw std::runtime_error("Metal was requested but no Metal backend is available");
    }
    if (normalized == "cuda" && cuda == nullptr) {
        throw std::runtime_error("CUDA was requested but no CUDA backend is available");
    }
    if (normalized == "cpu" && cpu == nullptr) {
        throw std::runtime_error("CPU was requested but no CPU backend is available");
    }
    if (normalized == "metal") {
        return {"metal", metal};
    }
    if (normalized == "cuda") {
        return {"cuda", cuda};
    }
    if (normalized == "cpu") {
        return {"cpu", cpu};
    }
    if (metal != nullptr) {
        return {"metal", metal};
    }
    if (cuda != nullptr) {
        return {"cuda", cuda};
    }
    return {"cpu", cpu};
}

size_t checked_u32(size_t value, const char * label) {
    if (value > std::numeric_limits<uint32_t>::max()) {
        throw std::runtime_error(std::string(label) + " exceeds libllama limits");
    }
    return value;
}

void native_log_callback(
    ggml_log_level level,
    const char * text,
    void *) {
    if (level == GGML_LOG_LEVEL_ERROR) {
        std::fputs(text, stderr);
    }
}

} // namespace

void native_encoder::model_deleter::operator()(llama_model * model) const {
    if (model != nullptr) {
        llama_model_free(model);
    }
}

void native_encoder::context_deleter::operator()(llama_context * context) const {
    if (context != nullptr) {
        llama_free(context);
    }
}

void native_encoder::batch_deleter::operator()(llama_batch * batch) const {
    if (batch != nullptr) {
        llama_batch_free(*batch);
        delete batch;
    }
}

bool native_encoder::backend_eval_callback(
    ggml_tensor * tensor,
    bool ask,
    void * user_data) {
    if (!ask
        || tensor == nullptr
        || tensor->op == GGML_OP_NONE
        || tensor->buffer == nullptr
        || user_data == nullptr) {
        return false;
    }
    auto * runtime = static_cast<native_encoder *>(user_data);
    const auto buffer_type = ggml_backend_buffer_get_type(tensor->buffer);
    const auto buffer_device = ggml_backend_buft_get_device(buffer_type);
    const bool selected =
        buffer_device == runtime->device_
        || (runtime->resolved_device_ == "cpu"
            && ggml_backend_buffer_is_host(tensor->buffer));
    if (selected) {
        ++runtime->selected_device_compute_nodes_;
    } else if (
        runtime->resolved_device_ != "cpu"
        && (tensor->op == GGML_OP_VIEW
            || (tensor->op == GGML_OP_GET_ROWS
                && std::string(tensor->name) == "model.input_embed"))) {
        // libllama materializes token lookup and metadata-only views at the
        // host/device boundary. They are observed staging nodes, not a
        // scheduler fallback for model compute.
        ++runtime->host_boundary_nodes_;
    } else {
        ++runtime->other_device_compute_nodes_;
        const char * device_name =
            buffer_device == nullptr
                ? "host"
                : ggml_backend_dev_name(buffer_device);
        const std::string observed =
            std::string(tensor->name)
            + " (" + ggml_op_name(tensor->op)
            + " on " + device_name + ")";
        if (runtime->first_other_device_node_.empty()) {
            runtime->first_other_device_node_ = observed;
        } else if (
            runtime->first_other_device_node_.size() < 2'048
            && runtime->first_other_device_node_.find(observed)
                == std::string::npos) {
            runtime->first_other_device_node_ += "; " + observed;
        }
    }
    return false;
}

native_encoder::native_encoder(native_encoder_options options)
    : options_(std::move(options)),
      started_at_(std::chrono::steady_clock::now()) {
    if (options_.model_path.empty()) {
        throw std::runtime_error("--model is required");
    }
    if (options_.max_tokens < 16
        || options_.checkpoint_tokens == 0
        || options_.physical_batch_tokens == 0
        || options_.physical_batch_tokens > options_.checkpoint_tokens
        || options_.checkpoint_tokens % options_.physical_batch_tokens != 0) {
        throw std::runtime_error(
            "token limits require physical-batch <= checkpoint and an exact divisor");
    }
    if (options_.session_budget_tokens == 0
        || options_.process_budget_tokens == 0
        || options_.session_budget_tokens > options_.process_budget_tokens
        || options_.max_sessions == 0
        || options_.idle_ttl_seconds <= 0.0) {
        throw std::runtime_error("invalid native C82 cache limits");
    }
    if (options_.max_sessions
        >= static_cast<size_t>(std::numeric_limits<llama_seq_id>::max())) {
        throw std::runtime_error("too many native C82 session slots");
    }

    llama_log_set(native_log_callback, nullptr);
    ggml_backend_load_all();
    llama_backend_init();
    const auto resolved = resolve_device(options_.device);
    resolved_device_ = resolved.name;
    device_ = resolved.device;
    if (device_ == nullptr) {
        throw std::runtime_error("native C82 could not resolve a backend device");
    }
    ggml_backend_dev_memory(
        device_,
        &device_initial_free_bytes_,
        &device_total_bytes_);

    auto model_params = llama_model_default_params();
    model_params.n_gpu_layers = resolved_device_ == "cpu" ? 0 : -1;
    if (resolved_device_ != "cpu") {
        selected_model_devices_ = {device_, nullptr};
        model_params.devices = selected_model_devices_.data();
    }
    model_.reset(llama_model_load_from_file(
        options_.model_path.c_str(),
        model_params));
    if (!model_) {
        throw std::runtime_error("failed to load the pinned C82 GGUF model");
    }

    embedding_dimension_ =
        static_cast<size_t>(llama_model_n_embd_out(model_.get()));
    if (embedding_dimension_ != expected_embedding_dimension) {
        throw std::runtime_error(
            "native C82 model has an incompatible embedding dimension");
    }

    auto context_params = llama_context_default_params();
    const size_t sequence_count = options_.max_sessions;
    if (options_.max_tokens
        > std::numeric_limits<size_t>::max() - options_.checkpoint_tokens) {
        throw std::runtime_error("native C82 context token budget overflowed");
    }
    const size_t per_sequence_tokens =
        options_.max_tokens + options_.checkpoint_tokens;
    if (per_sequence_tokens
        > std::numeric_limits<size_t>::max() / sequence_count) {
        throw std::runtime_error("native C82 context token budget overflowed");
    }
    context_params.n_ctx = static_cast<uint32_t>(checked_u32(
        per_sequence_tokens * sequence_count,
        "context token budget"));
    context_params.n_batch = static_cast<uint32_t>(checked_u32(
        options_.physical_batch_tokens,
        "physical batch token budget"));
    context_params.n_ubatch = context_params.n_batch;
    context_params.n_seq_max = static_cast<uint32_t>(checked_u32(
        sequence_count,
        "sequence budget"));
    context_params.n_outputs_max = context_params.n_batch;
    const unsigned hardware_threads = std::thread::hardware_concurrency();
    const int32_t default_threads = static_cast<int32_t>(
        std::max(1U, hardware_threads == 0 ? 4U : hardware_threads));
    context_params.n_threads =
        options_.threads > 0 ? options_.threads : default_threads;
    context_params.n_threads_batch = context_params.n_threads;
    // Rayline's pinned libllama fork retains an exact FP32 sum/count across
    // decode calls. The opaque pooling state is checkpointed with recurrent
    // model memory below so incremental episodes preserve the frozen contract.
    context_params.pooling_type = LLAMA_POOLING_TYPE_MEAN_CUMULATIVE;
    context_params.attention_type = LLAMA_ATTENTION_TYPE_CAUSAL;
    // Upstream Metal flash attention becomes non-finite on the frozen
    // repeated-token probe at token 31,936; keep the upstream non-flash graph
    // until an upgraded pin passes the same probe.
    context_params.flash_attn_type = LLAMA_FLASH_ATTN_TYPE_DISABLED;
    context_params.embeddings = true;
    context_params.offload_kqv = resolved_device_ != "cpu";
    context_params.op_offload = resolved_device_ != "cpu";
    context_params.kv_unified = false;
    context_params.swa_full = false;
    context_params.no_perf = true;
    context_params.type_k = GGML_TYPE_BF16;
    context_params.type_v = GGML_TYPE_BF16;
    context_params.cb_eval = backend_eval_callback;
    context_params.cb_eval_user_data = this;
    context_.reset(llama_init_from_model(model_.get(), context_params));
    if (!context_) {
        throw std::runtime_error("failed to initialize the native C82 context");
    }
    validate_device();
    size_t free_bytes = 0;
    size_t total_bytes = 0;
    ggml_backend_dev_memory(device_, &free_bytes, &total_bytes);
    const size_t allocated_bytes =
        device_initial_free_bytes_ > free_bytes
            ? device_initial_free_bytes_ - free_bytes
            : 0;
    if (options_.memory_budget_bytes > 0
        && allocated_bytes > options_.memory_budget_bytes) {
        throw std::runtime_error(
            "native C82 model and context exceed --memory-budget-gib");
    }

    serializer_ = std::make_unique<token_block_serializer>(
        llama_model_get_vocab(model_.get()),
        options_.max_tokens,
        1,
        64);
    free_sequence_ids_.reserve(options_.max_sessions);
    for (size_t index = options_.max_sessions; index > 0; --index) {
        free_sequence_ids_.push_back(static_cast<llama_seq_id>(index - 1));
    }
    const auto probe = serializer_->tokenize(json::array({
        {
            {"role", "user"},
            {"text", "C82 native backend readiness probe."},
        },
    }));
    static_cast<void>(encode_without_session(probe.input_ids));
    if (selected_device_compute_nodes_ == 0) {
        throw std::runtime_error(
            "native C82 readiness probe observed no selected-device compute");
    }
    if (resolved_device_ != "cpu" && other_device_compute_nodes_ > 0) {
        throw std::runtime_error(
            "native C82 readiness probe observed mixed-device compute at "
            + first_other_device_node_);
    }
}

native_encoder::~native_encoder() {
    sessions_.clear();
    context_.reset();
    model_.reset();
    llama_backend_free();
}

void native_encoder::validate_device() const {
    if (resolved_device_ != "cpu" && !llama_supports_gpu_offload()) {
        throw std::runtime_error(
            "native C82 accelerator was requested but GPU offload is unavailable");
    }
    if (llama_n_ctx_seq(context_.get()) < options_.max_tokens) {
        throw std::runtime_error(
            "native C82 context does not expose the required per-sequence window");
    }
}

json native_encoder::health() {
    reclaim_idle();
    size_t free_bytes = 0;
    size_t total_bytes = 0;
    ggml_backend_dev_memory(device_, &free_bytes, &total_bytes);
    const size_t allocated_bytes =
        device_initial_free_bytes_ > free_bytes
            ? device_initial_free_bytes_ - free_bytes
            : 0;
    return {
        {"status", "ready"},
        {"backend", "libllama"},
        {"backend_revision", RAYLINE_LLAMA_COMMIT},
        {"device", resolved_device_},
        {"backend_active", selected_device_compute_nodes_ > 0},
        {"bf16_supported", true},
        {"mixed_device_fallback",
            resolved_device_ != "cpu" && other_device_compute_nodes_ > 0},
        {"selected_device_compute_nodes", selected_device_compute_nodes_},
        {"host_boundary_nodes", host_boundary_nodes_},
        {"other_device_compute_nodes", other_device_compute_nodes_},
        {"first_other_device_node",
            first_other_device_node_.empty()
                ? json(nullptr)
                : json(first_other_device_node_)},
        {"encoder_model", encoder_model},
        {"encoder_revision", encoder_revision},
        {"encoder_dimension", embedding_dimension_},
        {"max_tokens", options_.max_tokens},
        {"pooling", "masked_mean"},
        {"serialization", "mtrouter-token-blocks-v2"},
        {"kv_chunk_tokens", options_.checkpoint_tokens},
        {"flash_attention", false},
        {"physical_batch_tokens", options_.physical_batch_tokens},
        {"max_sessions", options_.max_sessions},
        {"kv_cache_type", "BF16"},
        {"kv_unified", false},
        {"swa_full", false},
        {"cuda_nccl", RAYLINE_CUDA_NCCL != 0},
        {"kv_sessions", sessions_.size()},
        {"kv_resident_tokens", resident_tokens()},
        {"kv_session_budget_tokens", options_.session_budget_tokens},
        {"kv_process_budget_tokens", options_.process_budget_tokens},
        {"kv_evictions", evictions_},
        {"requests", requests_},
        {"uptime_seconds", std::chrono::duration<double>(
            std::chrono::steady_clock::now() - started_at_).count()},
        {"device_free_bytes", free_bytes},
        {"device_total_bytes", total_bytes},
        {"device_allocated_bytes", allocated_bytes},
        {"memory_budget_bytes",
            options_.memory_budget_bytes > 0
                ? json(options_.memory_budget_bytes)
                : json(nullptr)},
    };
}

json native_encoder::tokenize_for_parity(const json & turns) const {
    const auto tokenized = serializer_->tokenize(turns);
    return {
        {"input_ids", tokenized.input_ids},
        {"full_history_tokens", tokenized.full_tokens},
        {"truncated_tokens", tokenized.truncated_tokens},
    };
}

json native_encoder::encode(
    const std::string & episode_id,
    const json & turns) {
    if (episode_id.empty() || episode_id.size() > 512) {
        throw std::runtime_error("episode_id must contain 1..512 characters");
    }
    if (!turns.is_array() || turns.empty()) {
        throw std::runtime_error("turns must be a non-empty array");
    }
    static const std::vector<std::string> valid_roles = {
        "user", "assistant", "system", "tool",
    };
    for (size_t index = 0; index < turns.size(); ++index) {
        const auto & turn = turns.at(index);
        if (!turn.is_object()
            || !turn.contains("role")
            || !turn.at("role").is_string()
            || !turn.contains("text")
            || !turn.at("text").is_string()) {
            throw std::runtime_error(
                "each turn must contain string role and text fields");
        }
        const std::string role = turn.at("role").get<std::string>();
        if (std::find(valid_roles.begin(), valid_roles.end(), role)
            == valid_roles.end()) {
            throw std::runtime_error("turn has an unsupported role");
        }
    }

    ++requests_;
    reclaim_idle();
    auto tokenized = serializer_->tokenize(turns);
    encode_result result;
    if (tokenized.truncated_tokens > 0) {
        evict(episode_id);
        prepare_capacity("", tokenized.input_ids.size());
        result.embedding = encode_without_session(tokenized.input_ids);
        result.mode = "full_truncation_fallback";
    } else if (tokenized.input_ids.size() < options_.checkpoint_tokens) {
        evict(episode_id);
        prepare_capacity("", tokenized.input_ids.size());
        result.embedding = encode_without_session(tokenized.input_ids);
        result.mode = "full_sub_chunk_fallback";
    } else {
        result = encode_incremental(episode_id, tokenized.input_ids);
    }

    if (result.embedding.size() != embedding_dimension_) {
        throw std::runtime_error("native C82 encoder returned the wrong dimension");
    }
    for (float value : result.embedding) {
        if (!std::isfinite(value)) {
            throw std::runtime_error("native C82 encoder returned a non-finite value");
        }
    }
    return {
        {"embedding", result.embedding},
        {"device", resolved_device_},
        {"encode_mode", result.mode},
        {"serialized_tokens", tokenized.input_ids.size()},
        {"full_history_tokens", tokenized.full_tokens},
        {"truncated_tokens", tokenized.truncated_tokens},
        {"cached_prefix_tokens", result.cached_prefix_tokens},
        {"kv_session_retained", result.retained},
        {"kv_evictions", evictions_},
    };
}

native_encoder::encode_result native_encoder::encode_incremental(
    const std::string & episode_id,
    const std::vector<llama_token> & ids) {
    auto found = sessions_.find(episode_id);
    const bool had_session = found != sessions_.end();
    const size_t prior_resident =
        had_session ? found->second.cached_tokens : 0;
    const size_t target_cached =
        (ids.size() / options_.checkpoint_tokens) * options_.checkpoint_tokens;
    const bool retain = target_cached > 0
        && target_cached <= options_.session_budget_tokens;

    if (!retain) {
        evict(episode_id);
        prepare_capacity("", ids.size());
        return encode_result{
            encode_without_session(ids),
            had_session ? "rebuild" : "prefill",
            0,
            false,
        };
    }

    prepare_capacity(episode_id, target_cached - std::min(target_cached, prior_resident));
    found = sessions_.find(episode_id);
    const bool identical = found != sessions_.end()
        && ids == found->second.prefix_ids
        && !found->second.last_embedding.empty();
    if (identical) {
        auto & value = found->second;
        std::vector<float> embedding = value.last_embedding;
        const size_t cached = value.cached_tokens;
        touch(episode_id, value);
        return encode_result{
            std::move(embedding),
            "cached",
            cached,
            true,
        };
    }

    found = sessions_.find(episode_id);
    const bool prefix_hit = found != sessions_.end()
        && !found->second.prefix_ids.empty()
        && ids.size() > found->second.prefix_ids.size()
        && starts_with(ids, found->second.prefix_ids);
    const std::string mode =
        prefix_hit ? "delta" : (found != sessions_.end() ? "rebuild" : "prefill");
    const size_t cached_prefix_tokens =
        prefix_hit ? found->second.cached_tokens : 0;

    llama_seq_id sequence_id = -1;
    size_t start = 0;
    if (prefix_hit) {
        sequence_id = found->second.sequence_id;
        start = found->second.cached_tokens;
        lru_.erase(found->second.lru_position);
        sessions_.erase(found);
    } else {
        if (found != sessions_.end()) {
            sequence_id = found->second.sequence_id;
            clear_sequence(sequence_id);
            lru_.erase(found->second.lru_position);
            sessions_.erase(found);
        } else {
            sequence_id = allocate_sequence_id();
        }
    }

    std::vector<float> embedding;
    try {
        embedding = decode_range(sequence_id, ids, start);
    } catch (...) {
        clear_sequence(sequence_id);
        release_sequence_id(sequence_id);
        throw;
    }
    if (llama_pooling_seq_get_count(context_.get(), sequence_id)
        != target_cached) {
        clear_sequence(sequence_id);
        release_sequence_id(sequence_id);
        throw std::runtime_error(
            "native C82 cumulative pooling checkpoint drifted");
    }

    lru_.push_back(episode_id);
    auto position = std::prev(lru_.end());
    sessions_.emplace(
        episode_id,
        session{
            sequence_id,
            ids,
            embedding,
            target_cached,
            std::chrono::steady_clock::now(),
            position,
        });
    enforce_budget(episode_id);
    return encode_result{
        std::move(embedding),
        mode,
        cached_prefix_tokens,
        sessions_.find(episode_id) != sessions_.end(),
    };
}

std::vector<float> native_encoder::encode_without_session(
    const std::vector<llama_token> & ids) {
    const llama_seq_id scratch_sequence_id = allocate_sequence_id();
    clear_sequence(scratch_sequence_id);
    try {
        auto embedding = decode_range(scratch_sequence_id, ids, 0);
        clear_sequence(scratch_sequence_id);
        release_sequence_id(scratch_sequence_id);
        return embedding;
    } catch (...) {
        clear_sequence(scratch_sequence_id);
        release_sequence_id(scratch_sequence_id);
        throw;
    }
}

std::vector<float> native_encoder::decode_range(
    llama_seq_id sequence_id,
    const std::vector<llama_token> & ids,
    size_t start) {
    if (ids.empty() || start > ids.size()) {
        throw std::runtime_error("invalid native C82 decode range");
    }
    if (llama_pooling_seq_get_count(context_.get(), sequence_id) != start) {
        throw std::runtime_error(
            "native C82 cumulative pooling state does not match decode start");
    }

    const size_t target_cached =
        (ids.size() / options_.checkpoint_tokens) * options_.checkpoint_tokens;
    std::vector<uint8_t> tail_snapshot;
    std::vector<uint8_t> tail_pooling_snapshot;
    bool snapshot_taken = false;

    for (size_t chunk_start = start; chunk_start < ids.size();) {
        if (!snapshot_taken
            && chunk_start == target_cached
            && target_cached > 0
            && target_cached < ids.size()) {
            tail_snapshot = snapshot_sequence(sequence_id);
            tail_pooling_snapshot = snapshot_pooling(sequence_id);
            snapshot_taken = true;
        }
        const size_t chunk_size = std::min(
            options_.physical_batch_tokens,
            ids.size() - chunk_start);
        auto * raw_batch = new llama_batch(llama_batch_init(
            static_cast<int32_t>(chunk_size),
            0,
            1));
        std::unique_ptr<llama_batch, batch_deleter> batch(raw_batch);
        batch->n_tokens = static_cast<int32_t>(chunk_size);
        for (size_t offset = 0; offset < chunk_size; ++offset) {
            batch->token[offset] = ids[chunk_start + offset];
            batch->pos[offset] = static_cast<llama_pos>(chunk_start + offset);
            batch->n_seq_id[offset] = 1;
            batch->seq_id[offset][0] = sequence_id;
            batch->logits[offset] = 1;
        }
        const int32_t decode_status = llama_decode(context_.get(), *batch);
        if (decode_status != 0) {
            throw std::runtime_error(
                "native C82 libllama decode failed with status "
                + std::to_string(decode_status)
                + " at token " + std::to_string(chunk_start));
        }
        chunk_start += chunk_size;
        if (llama_pooling_seq_get_count(context_.get(), sequence_id)
            != chunk_start) {
            throw std::runtime_error(
                "native C82 cumulative pooling token count drifted");
        }
    }

    const float * pooled =
        llama_get_embeddings_seq(context_.get(), sequence_id);
    if (pooled == nullptr) {
        throw std::runtime_error(
            "native C82 libllama did not return a cumulative embedding");
    }
    std::vector<float> embedding(
        pooled,
        pooled + embedding_dimension_);
    for (size_t component = 0; component < embedding.size(); ++component) {
        if (!std::isfinite(embedding[component])) {
            throw std::runtime_error(
                "native C82 libllama produced a non-finite cumulative embedding "
                "at component " + std::to_string(component));
        }
    }

    if (target_cached < ids.size() && target_cached > 0) {
        if (!snapshot_taken) {
            throw std::runtime_error(
                "native C82 tail was decoded without an aligned snapshot");
        }
        restore_sequence(sequence_id, tail_snapshot);
        restore_pooling(sequence_id, tail_pooling_snapshot);
    } else if (target_cached == 0) {
        clear_sequence(sequence_id);
    }
    return embedding;
}

std::vector<uint8_t> native_encoder::snapshot_sequence(
    llama_seq_id sequence_id) {
    constexpr llama_state_seq_flags flags = LLAMA_STATE_SEQ_FLAGS_ON_DEVICE;
    const size_t size =
        llama_state_seq_get_size_ext(context_.get(), sequence_id, flags);
    if (size == 0) {
        throw std::runtime_error("native C82 could not size an on-device snapshot");
    }
    std::vector<uint8_t> snapshot(size);
    const size_t written = llama_state_seq_get_data_ext(
        context_.get(),
        snapshot.data(),
        snapshot.size(),
        sequence_id,
        flags);
    if (written == 0 || written > snapshot.size()) {
        throw std::runtime_error("native C82 could not take an on-device snapshot");
    }
    snapshot.resize(written);
    return snapshot;
}

std::vector<uint8_t> native_encoder::snapshot_pooling(
    llama_seq_id sequence_id) {
    const size_t size =
        llama_pooling_seq_get_size(context_.get(), sequence_id);
    if (size == 0) {
        throw std::runtime_error(
            "native C82 could not size a cumulative pooling snapshot");
    }
    std::vector<uint8_t> snapshot(size);
    const size_t written = llama_pooling_seq_get_data(
        context_.get(),
        snapshot.data(),
        snapshot.size(),
        sequence_id);
    if (written != snapshot.size()) {
        throw std::runtime_error(
            "native C82 could not take a cumulative pooling snapshot");
    }
    return snapshot;
}

void native_encoder::restore_sequence(
    llama_seq_id sequence_id,
    const std::vector<uint8_t> & snapshot) {
    clear_sequence(sequence_id);
    const size_t restored = llama_state_seq_set_data_ext(
        context_.get(),
        snapshot.data(),
        snapshot.size(),
        sequence_id,
        LLAMA_STATE_SEQ_FLAGS_ON_DEVICE);
    if (restored == 0) {
        throw std::runtime_error("native C82 could not restore an on-device snapshot");
    }
}

void native_encoder::restore_pooling(
    llama_seq_id sequence_id,
    const std::vector<uint8_t> & snapshot) {
    const size_t restored = llama_pooling_seq_set_data(
        context_.get(),
        snapshot.data(),
        snapshot.size(),
        sequence_id);
    if (restored != snapshot.size()) {
        throw std::runtime_error(
            "native C82 could not restore a cumulative pooling snapshot");
    }
}

void native_encoder::reclaim_idle() {
    const auto cutoff = std::chrono::steady_clock::now()
        - std::chrono::duration<double>(options_.idle_ttl_seconds);
    while (!lru_.empty()) {
        const auto found = sessions_.find(lru_.front());
        if (found == sessions_.end()) {
            lru_.pop_front();
            continue;
        }
        if (found->second.touched_at >= cutoff) {
            break;
        }
        evict(found->first);
    }
}

void native_encoder::prepare_capacity(
    const std::string & protected_episode,
    size_t incoming_tokens) {
    while (resident_tokens() + incoming_tokens
        > options_.process_budget_tokens) {
        if (!evict_oldest(protected_episode)) {
            throw std::runtime_error(
                "native C82 process cache budget cannot admit this request");
        }
    }
}

void native_encoder::enforce_budget(const std::string & protected_episode) {
    while (resident_tokens() > options_.process_budget_tokens) {
        if (!evict_oldest(protected_episode)) {
            throw std::runtime_error(
                "native C82 cache exceeds its process token budget");
        }
    }
}

void native_encoder::evict(const std::string & episode_id) {
    const auto found = sessions_.find(episode_id);
    if (found == sessions_.end()) {
        return;
    }
    const llama_seq_id sequence_id = found->second.sequence_id;
    lru_.erase(found->second.lru_position);
    sessions_.erase(found);
    clear_sequence(sequence_id);
    release_sequence_id(sequence_id);
    ++evictions_;
}

bool native_encoder::evict_oldest(const std::string & protected_episode) {
    for (const auto & episode_id : lru_) {
        if (episode_id != protected_episode) {
            evict(episode_id);
            return true;
        }
    }
    return false;
}

llama_seq_id native_encoder::allocate_sequence_id() {
    if (free_sequence_ids_.empty()) {
        if (!evict_oldest("")) {
            throw std::runtime_error("native C82 has no free session slot");
        }
    }
    const llama_seq_id value = free_sequence_ids_.back();
    free_sequence_ids_.pop_back();
    return value;
}

void native_encoder::release_sequence_id(llama_seq_id sequence_id) {
    if (sequence_id >= 0) {
        free_sequence_ids_.push_back(sequence_id);
    }
}

void native_encoder::clear_sequence(llama_seq_id sequence_id) {
    const bool memory_cleared = llama_memory_seq_rm(
        llama_get_memory(context_.get()),
        sequence_id,
        -1,
        -1);
    llama_pooling_seq_rm(context_.get(), sequence_id);
    if (!memory_cleared) {
        throw std::runtime_error("native C82 could not clear sequence state");
    }
}

size_t native_encoder::resident_tokens() const {
    size_t total = 0;
    for (const auto & [_, value] : sessions_) {
        total += value.cached_tokens;
    }
    return total;
}

void native_encoder::touch(
    const std::string & episode_id,
    session & value) {
    lru_.erase(value.lru_position);
    lru_.push_back(episode_id);
    value.lru_position = std::prev(lru_.end());
    value.touched_at = std::chrono::steady_clock::now();
}
