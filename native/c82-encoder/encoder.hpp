#pragma once

#include "ggml-backend.h"
#include "llama.h"
#include "token_blocks.hpp"

#include <chrono>
#include <cstddef>
#include <cstdint>
#include <list>
#include <memory>
#include <string>
#include <unordered_map>
#include <vector>

#include <nlohmann/json.hpp>

struct native_encoder_options {
    std::string model_path;
    std::string device = "auto";
    size_t max_tokens = 262144;
    size_t checkpoint_tokens = 8192;
    size_t physical_batch_tokens = 512;
    size_t session_budget_tokens = 300000;
    size_t process_budget_tokens = 600000;
    size_t max_sessions = 2;
    double idle_ttl_seconds = 900.0;
    size_t memory_budget_bytes = 0;
    int32_t threads = 0;
};

class native_encoder {
public:
    explicit native_encoder(native_encoder_options options);
    ~native_encoder();

    native_encoder(const native_encoder &) = delete;
    native_encoder & operator=(const native_encoder &) = delete;

    nlohmann::json health();
    nlohmann::json tokenize_for_parity(const nlohmann::json & turns) const;
    nlohmann::json encode(
        const std::string & episode_id,
        const nlohmann::json & turns);

private:
    struct model_deleter {
        void operator()(llama_model * model) const;
    };

    struct context_deleter {
        void operator()(llama_context * context) const;
    };

    struct batch_deleter {
        void operator()(llama_batch * batch) const;
    };

    struct session {
        llama_seq_id sequence_id = -1;
        std::vector<llama_token> prefix_ids;
        std::vector<float> last_embedding;
        size_t cached_tokens = 0;
        std::chrono::steady_clock::time_point touched_at;
        std::list<std::string>::iterator lru_position;
    };

    struct encode_result {
        std::vector<float> embedding;
        std::string mode;
        size_t cached_prefix_tokens = 0;
        bool retained = false;
    };

    using model_ptr = std::unique_ptr<llama_model, model_deleter>;
    using context_ptr = std::unique_ptr<llama_context, context_deleter>;

    encode_result encode_incremental(
        const std::string & episode_id,
        const std::vector<llama_token> & ids);
    std::vector<float> encode_without_session(
        const std::vector<llama_token> & ids);
    std::vector<float> decode_range(
        llama_seq_id sequence_id,
        const std::vector<llama_token> & ids,
        size_t start);
    std::vector<uint8_t> snapshot_sequence(llama_seq_id sequence_id);
    std::vector<uint8_t> snapshot_pooling(llama_seq_id sequence_id);
    void restore_sequence(
        llama_seq_id sequence_id,
        const std::vector<uint8_t> & snapshot);
    void restore_pooling(
        llama_seq_id sequence_id,
        const std::vector<uint8_t> & snapshot);
    static bool backend_eval_callback(
        ggml_tensor * tensor,
        bool ask,
        void * user_data);

    void validate_device() const;
    void reclaim_idle();
    void prepare_capacity(
        const std::string & protected_episode,
        size_t incoming_tokens);
    void enforce_budget(const std::string & protected_episode);
    void evict(const std::string & episode_id);
    bool evict_oldest(const std::string & protected_episode);
    llama_seq_id allocate_sequence_id();
    void release_sequence_id(llama_seq_id sequence_id);
    void clear_sequence(llama_seq_id sequence_id);
    size_t resident_tokens() const;
    void touch(const std::string & episode_id, session & value);

    native_encoder_options options_;
    std::string resolved_device_;
    ggml_backend_dev_t device_ = nullptr;
    std::vector<ggml_backend_dev_t> selected_model_devices_;
    size_t device_initial_free_bytes_ = 0;
    size_t device_total_bytes_ = 0;
    size_t embedding_dimension_ = 0;
    model_ptr model_;
    context_ptr context_;
    std::unique_ptr<token_block_serializer> serializer_;
    std::unordered_map<std::string, session> sessions_;
    std::list<std::string> lru_;
    std::vector<llama_seq_id> free_sequence_ids_;
    size_t evictions_ = 0;
    size_t requests_ = 0;
    size_t selected_device_compute_nodes_ = 0;
    size_t host_boundary_nodes_ = 0;
    size_t other_device_compute_nodes_ = 0;
    std::string first_other_device_node_;
    std::chrono::steady_clock::time_point started_at_;
};
