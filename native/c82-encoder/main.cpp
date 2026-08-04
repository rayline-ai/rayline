#include "encoder.hpp"

#include <cstdlib>
#include <cmath>
#include <exception>
#include <iostream>
#include <limits>
#include <stdexcept>
#include <string>

#include <nlohmann/json.hpp>

using json = nlohmann::json;

namespace {

size_t parse_size(const std::string & value, const char * name) {
    try {
        size_t consumed = 0;
        const unsigned long long parsed = std::stoull(value, &consumed);
        if (consumed != value.size() || parsed == 0) {
            throw std::runtime_error("invalid");
        }
        return static_cast<size_t>(parsed);
    } catch (const std::exception &) {
        throw std::runtime_error(std::string("invalid value for ") + name);
    }
}

double parse_double(const std::string & value, const char * name) {
    try {
        size_t consumed = 0;
        const double parsed = std::stod(value, &consumed);
        if (consumed != value.size() || !std::isfinite(parsed) || parsed <= 0.0) {
            throw std::runtime_error("invalid");
        }
        return parsed;
    } catch (const std::exception &) {
        throw std::runtime_error(std::string("invalid value for ") + name);
    }
}

native_encoder_options parse_options(int argc, char ** argv) {
    native_encoder_options options;
    for (int index = 1; index < argc; ++index) {
        const std::string argument = argv[index];
        if (index + 1 >= argc) {
            throw std::runtime_error("missing value for " + argument);
        }
        const std::string value = argv[++index];
        if (argument == "--model") {
            options.model_path = value;
        } else if (argument == "--device") {
            options.device = value;
        } else if (argument == "--max-tokens") {
            options.max_tokens = parse_size(value, "--max-tokens");
        } else if (argument == "--checkpoint-tokens") {
            options.checkpoint_tokens =
                parse_size(value, "--checkpoint-tokens");
        } else if (argument == "--physical-batch-tokens") {
            options.physical_batch_tokens =
                parse_size(value, "--physical-batch-tokens");
        } else if (argument == "--session-budget-tokens") {
            options.session_budget_tokens =
                parse_size(value, "--session-budget-tokens");
        } else if (argument == "--process-budget-tokens") {
            options.process_budget_tokens =
                parse_size(value, "--process-budget-tokens");
        } else if (argument == "--max-sessions") {
            options.max_sessions = parse_size(value, "--max-sessions");
        } else if (argument == "--idle-ttl-seconds") {
            options.idle_ttl_seconds =
                parse_double(value, "--idle-ttl-seconds");
        } else if (argument == "--memory-budget-gib") {
            const double gib = parse_double(value, "--memory-budget-gib");
            const double bytes = gib * 1024.0 * 1024.0 * 1024.0;
            if (bytes > static_cast<double>(
                    std::numeric_limits<size_t>::max())) {
                throw std::runtime_error(
                    "--memory-budget-gib exceeds platform limits");
            }
            options.memory_budget_bytes = static_cast<size_t>(bytes);
        } else if (argument == "--threads") {
            options.threads = static_cast<int32_t>(parse_size(value, "--threads"));
        } else {
            throw std::runtime_error("unknown argument " + argument);
        }
    }
    return options;
}

json error_response(const std::string & message) {
    return {
        {"ok", false},
        {"error", message},
    };
}

} // namespace

int main(int argc, char ** argv) {
    try {
        native_encoder runtime(parse_options(argc, argv));
        std::string line;
        while (std::getline(std::cin, line)) {
            if (line.empty()) {
                continue;
            }
            try {
                if (line.size() > 64U * 1024U * 1024U) {
                    throw std::runtime_error("request exceeds the native IPC limit");
                }
                const json request = json::parse(line);
                const std::string operation =
                    request.value("op", std::string());
                if (operation == "health") {
                    std::cout << json{
                        {"ok", true},
                        {"result", runtime.health()},
                    }.dump() << std::endl;
                } else if (operation == "tokenize_for_parity") {
                    std::cout << json{
                        {"ok", true},
                        {"result", runtime.tokenize_for_parity(
                            request.at("turns"))},
                    }.dump() << std::endl;
                } else if (operation == "encode") {
                    std::cout << json{
                        {"ok", true},
                        {"result", runtime.encode(
                            request.value("episode_id", std::string()),
                            request.at("turns"))},
                    }.dump() << std::endl;
                } else if (operation == "shutdown") {
                    std::cout << json{
                        {"ok", true},
                        {"result", {{"status", "stopping"}}},
                    }.dump() << std::endl;
                    return EXIT_SUCCESS;
                } else {
                    throw std::runtime_error("unsupported native IPC operation");
                }
            } catch (const json::exception &) {
                std::cout << error_response("invalid native IPC request").dump()
                          << std::endl;
            } catch (const std::exception & error) {
                std::cout << error_response(error.what()).dump() << std::endl;
            }
        }
        return EXIT_SUCCESS;
    } catch (const std::exception & error) {
        std::cerr << "rayline-c82-encoder: " << error.what() << std::endl;
        return EXIT_FAILURE;
    }
}
