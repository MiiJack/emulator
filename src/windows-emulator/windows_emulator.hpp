#pragma once
#include "std_include.hpp"

#include <x64_emulator.hpp>

#include <utils/function.hpp>

#include "syscall_dispatcher.hpp"
#include "process_context.hpp"
#include "logger.hpp"
#include "file_system.hpp"
#include "memory_manager.hpp"
#include "module/module_manager.hpp"

std::unique_ptr<x64_emulator> create_default_x64_emulator();

struct emulator_callbacks : module_manager::callbacks, process_context::callbacks
{
    utils::optional_function<instruction_hook_continuation(uint32_t syscall_id, x64_emulator::pointer_type address,
                                                           std::string_view mod_name, std::string_view syscall_name)>
        on_syscall{};

    utils::optional_function<void(std::string_view)> on_stdout{};
};

struct application_settings
{
    windows_path application{};
    windows_path working_directory{};
    std::vector<std::u16string> arguments{};
};

struct emulator_settings
{
    std::filesystem::path emulation_root{};
    std::filesystem::path registry_directory{"./registry"};

    bool verbose_calls{false};
    bool disable_logging{false};
    bool silent_until_main{false};
    bool use_relative_time{false};

    std::unordered_map<uint16_t, uint16_t> port_mappings{};
    std::unordered_map<windows_path, std::filesystem::path> path_mappings{};
    std::set<std::string, std::less<>> modules{};
};

class windows_emulator
{
    std::unique_ptr<x64_emulator> emu_{};
    std::unique_ptr<utils::system_clock> system_clock_{};
    std::unique_ptr<utils::steady_clock> steady_clock_{};

  public:
    std::filesystem::path emulation_root{};
    emulator_callbacks callbacks{};
    logger log{};
    file_system file_sys;
    memory_manager memory;
    registry_manager registry{};
    module_manager mod_manager;
    process_context process;
    syscall_dispatcher dispatcher;

    windows_emulator(const emulator_settings& settings = {},
                     std::unique_ptr<x64_emulator> emu = create_default_x64_emulator());
    windows_emulator(application_settings app_settings, const emulator_settings& settings = {},
                     emulator_callbacks callbacks = {},
                     std::unique_ptr<x64_emulator> emu = create_default_x64_emulator());

    windows_emulator(windows_emulator&&) = delete;
    windows_emulator(const windows_emulator&) = delete;
    windows_emulator& operator=(windows_emulator&&) = delete;
    windows_emulator& operator=(const windows_emulator&) = delete;

    ~windows_emulator();

    x64_emulator& emu()
    {
        return *this->emu_;
    }

    const x64_emulator& emu() const
    {
        return *this->emu_;
    }

    utils::system_clock& system_clock()
    {
        return *this->system_clock_;
    }

    const utils::system_clock& system_clock() const
    {
        return *this->system_clock_;
    }

    utils::steady_clock& steady_clock()
    {
        return *this->steady_clock_;
    }

    const utils::steady_clock& steady_clock() const
    {
        return *this->steady_clock_;
    }

    emulator_thread& current_thread() const
    {
        if (!this->process.active_thread)
        {
            throw std::runtime_error("No active thread!");
        }

        return *this->process.active_thread;
    }

    void start(std::chrono::nanoseconds timeout = {}, size_t count = 0);

    void serialize(utils::buffer_serializer& buffer) const;
    void deserialize(utils::buffer_deserializer& buffer);

    void save_snapshot();
    void restore_snapshot();

    uint16_t get_host_port(const uint16_t emulator_port) const
    {
        const auto entry = this->port_mappings_.find(emulator_port);
        if (entry == this->port_mappings_.end())
        {
            return emulator_port;
        }

        return entry->second;
    }

    uint16_t get_emulator_port(const uint16_t host_port) const
    {
        for (const auto& mapping : this->port_mappings_)
        {
            if (mapping.second == host_port)
            {
                return mapping.first;
            }
        }

        return host_port;
    }

    void map_port(const uint16_t emulator_port, const uint16_t host_port)
    {
        if (emulator_port != host_port)
        {
            this->port_mappings_[emulator_port] = host_port;
            return;
        }

        const auto entry = this->port_mappings_.find(emulator_port);
        if (entry != this->port_mappings_.end())
        {
            this->port_mappings_.erase(entry);
        }
    }

    bool verbose{false};
    bool verbose_calls{false};
    bool buffer_stdout{false};
    bool fuzzing{false};

    void yield_thread();
    void perform_thread_switch();
    bool activate_thread(uint32_t id);

    bool base_constructed_{false};

  private:
    bool switch_thread_{false};
    bool use_relative_time_{false};
    bool silent_until_main_{false};

    std::unordered_map<uint16_t, uint16_t> port_mappings_{};

    std::set<std::string, std::less<>> modules_{};
    std::vector<std::byte> process_snapshot_{};
    // std::optional<process_context> process_snapshot_{};

    void setup_hooks();
    void setup_process(const application_settings& app_settings);
    void on_instruction_execution(uint64_t address);
};
