#pragma once

#include "llama.h"

#include <cstddef>
#include <cstdint>
#include <string>
#include <vector>

#include <nlohmann/json.hpp>

struct tokenization_result {
    std::vector<llama_token> input_ids;
    size_t full_tokens = 0;
    size_t truncated_tokens = 0;
};

class token_block_serializer {
public:
    token_block_serializer(
        const llama_vocab * vocab,
        size_t max_tokens,
        size_t min_recent_turns,
        size_t min_recent_tokens);

    tokenization_result tokenize(const nlohmann::json & turns) const;

private:
    struct history_block {
        std::vector<llama_token> header_ids;
        std::vector<llama_token> content_ids;
        llama_token eos = LLAMA_TOKEN_NULL;

        std::vector<llama_token> input_ids() const;
        std::vector<llama_token> recent_tail(size_t max_tokens) const;
    };

    std::vector<llama_token> encode(const std::string & text) const;
    std::vector<llama_token> encode_block(const std::string & text) const;
    std::vector<llama_token> truncate_task(
        const std::vector<llama_token> & input_ids,
        size_t max_tokens) const;

    const llama_vocab * vocab_;
    llama_token eos_;
    size_t max_tokens_;
    size_t min_recent_turns_;
    size_t min_recent_tokens_;
};
