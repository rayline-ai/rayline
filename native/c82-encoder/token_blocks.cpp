#include "token_blocks.hpp"

#include <algorithm>
#include <limits>
#include <stdexcept>

using json = nlohmann::json;

namespace {

std::string turn_string(const json & turn, const char * key) {
    const auto it = turn.find(key);
    if (it == turn.end() || it->is_null()) {
        return {};
    }
    if (it->is_string()) {
        return it->get<std::string>();
    }
    return it->dump();
}

void append_tokens(
    std::vector<llama_token> & target,
    const std::vector<llama_token> & source) {
    target.insert(target.end(), source.begin(), source.end());
}

} // namespace

token_block_serializer::token_block_serializer(
    const llama_vocab * vocab,
    size_t max_tokens,
    size_t min_recent_turns,
    size_t min_recent_tokens)
    : vocab_(vocab),
      eos_(llama_vocab_eos(vocab)),
      max_tokens_(max_tokens),
      min_recent_turns_(min_recent_turns),
      min_recent_tokens_(min_recent_tokens) {
    if (vocab_ == nullptr || eos_ == LLAMA_TOKEN_NULL) {
        throw std::runtime_error("C82 tokenization requires a vocabulary EOS token");
    }
    if (max_tokens_ < 16 || min_recent_turns_ == 0 || min_recent_tokens_ == 0) {
        throw std::runtime_error("invalid C82 tokenization limits");
    }
}

std::vector<llama_token> token_block_serializer::history_block::input_ids() const {
    std::vector<llama_token> result;
    result.reserve(header_ids.size() + content_ids.size() + 1);
    append_tokens(result, header_ids);
    append_tokens(result, content_ids);
    result.push_back(eos);
    return result;
}

std::vector<llama_token> token_block_serializer::history_block::recent_tail(
    size_t max_tokens) const {
    if (max_tokens == 0) {
        return {};
    }
    if (max_tokens == 1) {
        return {eos};
    }
    if (max_tokens <= header_ids.size() + 1) {
        std::vector<llama_token> result(
            header_ids.begin(),
            header_ids.begin() + static_cast<std::ptrdiff_t>(max_tokens - 1));
        result.push_back(eos);
        return result;
    }
    const size_t content_budget = max_tokens - header_ids.size() - 1;
    const size_t content_start =
        content_ids.size() > content_budget ? content_ids.size() - content_budget : 0;
    std::vector<llama_token> result;
    result.reserve(std::min(content_ids.size(), content_budget) + header_ids.size() + 1);
    append_tokens(result, header_ids);
    result.insert(
        result.end(),
        content_ids.begin() + static_cast<std::ptrdiff_t>(content_start),
        content_ids.end());
    result.push_back(eos);
    return result;
}

std::vector<llama_token> token_block_serializer::encode(const std::string & text) const {
    if (text.size() > static_cast<size_t>(std::numeric_limits<int32_t>::max())) {
        throw std::runtime_error("C82 tokenization input is too large");
    }
    const int32_t required = llama_tokenize(
        vocab_,
        text.data(),
        static_cast<int32_t>(text.size()),
        nullptr,
        0,
        false,
        false);
    if (required == std::numeric_limits<int32_t>::min()) {
        throw std::runtime_error("C82 tokenization overflow");
    }
    const int32_t count = required < 0 ? -required : required;
    std::vector<llama_token> result(static_cast<size_t>(count));
    if (count == 0) {
        return result;
    }
    const int32_t written = llama_tokenize(
        vocab_,
        text.data(),
        static_cast<int32_t>(text.size()),
        result.data(),
        count,
        false,
        false);
    if (written != count) {
        throw std::runtime_error("C82 tokenization failed");
    }
    return result;
}

std::vector<llama_token> token_block_serializer::encode_block(
    const std::string & text) const {
    auto result = encode(text);
    result.push_back(eos_);
    return result;
}

std::vector<llama_token> token_block_serializer::truncate_task(
    const std::vector<llama_token> & input_ids,
    size_t max_tokens) const {
    if (max_tokens == 0) {
        return {};
    }
    if (input_ids.size() <= max_tokens) {
        return input_ids;
    }
    if (max_tokens == 1) {
        return {eos_};
    }
    std::vector<llama_token> result(
        input_ids.begin(),
        input_ids.begin() + static_cast<std::ptrdiff_t>(max_tokens - 1));
    result.push_back(eos_);
    return result;
}

tokenization_result token_block_serializer::tokenize(const json & turns) const {
    if (!turns.is_array()) {
        throw std::runtime_error("C82 turns must be an array");
    }
    const bool task_is_first =
        !turns.empty() && turn_string(turns.front(), "role") == "user";
    const std::string task_text =
        task_is_first ? turn_string(turns.front(), "text") : std::string();
    const size_t context_start = task_is_first ? 1 : 0;

    const auto task_ids_full = encode_block("[Task]\n" + task_text);
    const auto context_header_ids = encode_block("[Context]");

    std::vector<history_block> blocks;
    size_t turn_number = 0;
    for (size_t i = context_start; i < turns.size(); ++i) {
        const auto & turn = turns.at(i);
        const std::string role = turn_string(turn, "role");
        if (role == "user") {
            ++turn_number;
        }
        blocks.push_back(history_block{
            encode(
                "[Turn " + std::to_string(turn_number) + " - " + role + "]\n"),
            encode(turn_string(turn, "text")),
            eos_,
        });
    }

    size_t full_context_tokens = 0;
    if (!blocks.empty()) {
        full_context_tokens = context_header_ids.size();
        for (const auto & block : blocks) {
            full_context_tokens += block.header_ids.size() + block.content_ids.size() + 1;
        }
    }
    const size_t full_tokens = task_ids_full.size() + full_context_tokens;

    if (blocks.empty()) {
        auto task_ids = truncate_task(task_ids_full, max_tokens_);
        const size_t serialized_tokens = task_ids.size();
        return tokenization_result{
            std::move(task_ids),
            full_tokens,
            full_tokens > serialized_tokens ? full_tokens - serialized_tokens : 0,
        };
    }

    const auto newest_ids = blocks.back().input_ids();
    const size_t newest_budget = std::min(min_recent_tokens_, newest_ids.size());
    const size_t reserved_context =
        std::min(max_tokens_ - 1, context_header_ids.size() + newest_budget);
    const size_t task_limit = std::max<size_t>(1, max_tokens_ - reserved_context);
    auto task_ids = truncate_task(task_ids_full, task_limit);
    size_t available = max_tokens_ - task_ids.size() - context_header_ids.size();

    std::vector<std::vector<llama_token>> selected_reversed;
    for (auto it = blocks.rbegin(); it != blocks.rend(); ++it) {
        auto block_ids = it->input_ids();
        if (block_ids.size() <= available) {
            available -= block_ids.size();
            selected_reversed.push_back(std::move(block_ids));
            continue;
        }
        if (selected_reversed.size() < min_recent_turns_ && available > 0) {
            auto partial = it->recent_tail(available);
            if (!partial.empty()) {
                available -= partial.size();
                selected_reversed.push_back(std::move(partial));
            }
        }
        break;
    }

    std::vector<llama_token> input_ids;
    input_ids.reserve(max_tokens_);
    append_tokens(input_ids, task_ids);
    append_tokens(input_ids, context_header_ids);
    for (auto it = selected_reversed.rbegin(); it != selected_reversed.rend(); ++it) {
        append_tokens(input_ids, *it);
    }
    if (input_ids.size() > max_tokens_) {
        throw std::runtime_error("C82 tokenization exceeded the context limit");
    }
    const size_t serialized_tokens = input_ids.size();
    return tokenization_result{
        std::move(input_ids),
        full_tokens,
        full_tokens > serialized_tokens ? full_tokens - serialized_tokens : 0,
    };
}
