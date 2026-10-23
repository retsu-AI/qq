// Diagnostic only. Combine a native security environment with a further
// restriction of its own token; never create an unrestricted fallback child.
#define UNICODE
#define _UNICODE
#include <windows.h>
#include <sddl.h>
#include <iostream>
#include <cwchar>
#include <cwctype>
#include <utility>
#include <map>
#include <memory>
#include <stdexcept>
#include <string>
#include <vector>
#pragma comment(lib, "advapi32.lib")

struct Handle {
    HANDLE value = nullptr;
    ~Handle() { if (value && value != INVALID_HANDLE_VALUE) CloseHandle(value); }
    Handle() = default;
    Handle(const Handle&) = delete;
    Handle& operator=(const Handle&) = delete;
};

static void require(BOOL ok, const char* phase) {
    if (!ok) throw std::runtime_error(std::string(phase) + ": " + std::to_string(GetLastError()));
}

static std::vector<BYTE> information(HANDLE token, TOKEN_INFORMATION_CLASS kind) {
    DWORD size = 0;
    GetTokenInformation(token, kind, nullptr, 0, &size);
    if (size == 0 || size > 65536) throw std::runtime_error("invalid token information size");
    std::vector<BYTE> bytes(size);
    require(GetTokenInformation(token, kind, bytes.data(), size, &size), "GetTokenInformation");
    return bytes;
}

static DWORD restricting_count(HANDLE token) {
    auto bytes = information(token, TokenRestrictedSids);
    return reinterpret_cast<const TOKEN_GROUPS*>(bytes.data())->GroupCount;
}

static void contained(HANDLE token) {
    auto bytes = information(token, TokenIsAppContainer);
    if (*reinterpret_cast<const DWORD*>(bytes.data()) != 1) throw std::runtime_error("token lost AppContainer isolation");
}

static std::vector<wchar_t> environment() {
    LPWCH block = GetEnvironmentStringsW();
    if (!block) throw std::runtime_error("GetEnvironmentStringsW failed");
    std::map<std::wstring, std::wstring> values;
    for (const wchar_t* entry = block; *entry; entry += wcslen(entry) + 1) {
        std::wstring value(entry);
        const auto separator = value.find(L'=', 1);
        if (separator == std::wstring::npos) continue;
        auto key = value.substr(0, separator);
        for (auto& character : key) character = static_cast<wchar_t>(std::towupper(character));
        values[key] = value.substr(separator + 1);
    }
    FreeEnvironmentStringsW(block);
    const auto home = values.find(L"HOME");
    if (home == values.end()) throw std::runtime_error("missing frozen task home");
    values[L"TEMP"] = home->second + L"\\tmp";
    values[L"TMP"] = home->second + L"\\tmp";
    values[L"LOCALAPPDATA"] = home->second + L"\\AppData\\Local";
    std::vector<wchar_t> result;
    for (const auto& [name, value] : values) {
        const auto entry = name + L"=" + value;
        result.insert(result.end(), entry.begin(), entry.end());
        result.push_back(0);
    }
    result.push_back(0);
    return result;
}

int wmain(int argc, wchar_t** argv) {
    try {
        if (argc != 4) throw std::runtime_error("expected node path, payload path, restriction mode");
        Handle current;
        require(OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY | TOKEN_DUPLICATE | TOKEN_ASSIGN_PRIMARY, &current.value), "OpenProcessToken");
        contained(current.value);
        auto package = information(current.value, TokenAppContainerSid);
        auto sid = reinterpret_cast<const TOKEN_APPCONTAINER_INFORMATION*>(package.data())->TokenAppContainer;
        if (!sid) throw std::runtime_error("missing AppContainer package SID");
        std::vector<SID_AND_ATTRIBUTES> sids{ { sid, 0 } };
        const std::wstring mode(argv[3]);
        if (mode == L"control") sids.clear();
        auto free_sid = [](void* value) { LocalFree(value); };
        std::unique_ptr<void, decltype(free_sid)> arap(nullptr, free_sid), registry(nullptr, free_sid);
        if (mode == L"package-arap" || mode == L"package-arap-registry") {
            PSID allocated = nullptr;
            require(ConvertStringSidToSidW(L"S-1-15-2-2", &allocated), "ARAP SID");
            arap.reset(allocated);
            sids.push_back({ arap.get(), 0 });
        }
        if (mode == L"package-arap-registry") {
            PSID allocated = nullptr;
            require(ConvertStringSidToSidW(L"S-1-15-3-1024-1065365936-1281604716-3511738428-1654721687-432734479-3232135806-4053264122-3456934681", &allocated), "registryRead SID");
            registry.reset(allocated);
            sids.push_back({ registry.get(), 0 });
        } else if (mode != L"package" && mode != L"package-arap" && mode != L"control") {
            throw std::runtime_error("unknown token restriction mode");
        }
        Handle restricted;
        require(CreateRestrictedToken(current.value, DISABLE_MAX_PRIVILEGE, 0, nullptr, 0, nullptr,
            static_cast<DWORD>(sids.size()), sids.empty() ? nullptr : sids.data(), &restricted.value), "CreateRestrictedToken");
        contained(restricted.value);
        const DWORD after = restricting_count(restricted.value);
        std::cout << "token restriction: before=" << restricting_count(current.value) << "; after=" << after << std::endl;
        if (mode != L"control" && after == 0) throw std::runtime_error("restriction produced no restricting SIDs");
        auto block = environment();
        const std::wstring command = L"\"" + std::wstring(argv[1]) + L"\" \"" + std::wstring(argv[2]) + L"\"";
        std::vector<wchar_t> mutable_command(command.begin(), command.end());
        mutable_command.push_back(0);
        STARTUPINFOW startup{};
        startup.cb = static_cast<DWORD>(sizeof(startup));
        startup.dwFlags = STARTF_USESTDHANDLES;
        Handle input, output, error;
        for (const auto& pair : std::vector<std::pair<DWORD, HANDLE*>>{
            { STD_INPUT_HANDLE, &input.value }, { STD_OUTPUT_HANDLE, &output.value }, { STD_ERROR_HANDLE, &error.value } }) {
            require(DuplicateHandle(GetCurrentProcess(), GetStdHandle(pair.first), GetCurrentProcess(), pair.second, 0, TRUE, DUPLICATE_SAME_ACCESS), "DuplicateHandle(stdio)");
        }
        startup.hStdInput = input.value;
        startup.hStdOutput = output.value;
        startup.hStdError = error.value;
        PROCESS_INFORMATION process{};
        require(CreateProcessAsUserW(restricted.value, argv[1], mutable_command.data(), nullptr, nullptr,
            TRUE, CREATE_UNICODE_ENVIRONMENT, block.data(), nullptr, &startup, &process), "CreateProcessAsUserW");
        Handle child, thread;
        child.value = process.hProcess;
        thread.value = process.hThread;
        if (WaitForSingleObject(child.value, INFINITE) != WAIT_OBJECT_0) throw std::runtime_error("child wait failed");
        DWORD status = 0;
        require(GetExitCodeProcess(child.value, &status), "GetExitCodeProcess");
        return static_cast<int>(status);
    } catch (const std::exception& error) {
        std::cerr << "restricted child probe: " << error.what() << std::endl;
        return 125;
    }
}
