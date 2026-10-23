// Diagnostic only. Combine a native security environment with a further
// restriction of its own token; never create an unrestricted fallback child.
#define UNICODE
#define _UNICODE
#include <windows.h>
#include <sddl.h>
#include <aclapi.h>
#include <iostream>
#include <cwchar>
#include <cstring>
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

static std::string sid_text(PSID sid) {
    LPSTR value = nullptr;
    require(ConvertSidToStringSidA(sid, &value), "ConvertSidToStringSidA");
    std::string text(value);
    LocalFree(value);
    return text;
}

static int inspect() {
    Handle token;
    require(OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &token.value), "inspect token");
    contained(token.value);
    auto package = information(token.value, TokenAppContainerSid);
    std::cout << "appcontainer=1; package=" << sid_text(reinterpret_cast<const TOKEN_APPCONTAINER_INFORMATION*>(package.data())->TokenAppContainer) << std::endl;
    auto capabilities = information(token.value, TokenCapabilities);
    const auto* groups = reinterpret_cast<const TOKEN_GROUPS*>(capabilities.data());
    if (groups->GroupCount > 128) throw std::runtime_error("too many capabilities");
    for (DWORD i = 0; i < groups->GroupCount; ++i) {
        std::cout << "capability=" << sid_text(groups->Groups[i].Sid) << "; attributes=" << groups->Groups[i].Attributes << std::endl;
    }
    auto restricted = information(token.value, TokenRestrictedSids);
    const auto* restricting = reinterpret_cast<const TOKEN_GROUPS*>(restricted.data());
    if (restricting->GroupCount > 128) throw std::runtime_error("too many restricting SIDs");
    std::cout << "restricting-count=" << restricting->GroupCount << std::endl;
    for (DWORD i = 0; i < restricting->GroupCount; ++i) {
        std::cout << "restricting=" << sid_text(restricting->Groups[i].Sid) << std::endl;
    }
    DWORD lpac = 0, size = 0;
    const BOOL available = GetTokenInformation(token.value, TokenIsLessPrivilegedAppContainer, &lpac, sizeof(lpac), &size);
    std::cout << "lpac-query-available=" << available << "; lpac=" << lpac << std::endl;
    return 0;
}

// Do not infer permission failure from Node's path loader. Probe each Win32
// access mask directly, without changing any file or existing security object.
static int inspect_files() {
    inspect();
    wchar_t current[32768]{};
    require(GetCurrentDirectoryW(32768, current) != 0, "GetCurrentDirectoryW");
    const std::wstring workspace(current);
    wchar_t outside[32768]{};
    require(GetEnvironmentVariableW(L"QQ_NATIVE_PUBLIC_WRITABLE", outside, 32768) != 0,
        "public writable path");
    for (const auto& name : std::vector<std::wstring>{workspace, workspace + L"\\payload.cjs", outside}) {
        wchar_t canonical[32768]{};
        const DWORD length = GetLongPathNameW(name.c_str(), canonical, 32768);
        std::wcout << L"long-path=" << name << L"; result=" << (length ? canonical : L"<unavailable>")
            << L"; error=" << (length ? 0 : GetLastError()) << std::endl;
        for (const auto& candidate : std::vector<std::wstring>{name, L"\\\\?\\" + name}) {
            for (const DWORD access : {DWORD(0), DWORD(FILE_READ_DATA), DWORD(GENERIC_READ), DWORD(FILE_WRITE_DATA)}) {
                Handle file;
                file.value = CreateFileW(candidate.c_str(), access,
                    FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, nullptr,
                    OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS, nullptr);
                const DWORD code = file.value == INVALID_HANDLE_VALUE ? GetLastError() : 0;
                std::wcout << L"file-access=" << candidate << L"; mask=" << access << L"; error=" << code;
                if (code == 0) {
                    wchar_t resolved[32768]{};
                    const DWORD count = GetFinalPathNameByHandleW(file.value, resolved, 32768, FILE_NAME_NORMALIZED | VOLUME_NAME_DOS);
                    std::wcout << L"; final-path=" << (count ? resolved : L"<unavailable>")
                        << L"; final-error=" << (count ? 0 : GetLastError());
                }
                std::wcout << std::endl;
            }
        }
    }
    return 0;
}

int wmain(int argc, wchar_t** argv) {
    try {
        if (argc == 2 && std::wstring(argv[1]) == L"inspect") return inspect();
        if (argc == 2 && std::wstring(argv[1]) == L"inspect-files") return inspect_files();
        if (argc < 4 || argc > 5) throw std::runtime_error("expected node path, payload path, restriction mode and optional write SID");
        const std::wstring mode(argv[3]);
        const bool inline_node = mode == L"write-sid-inline" || mode == L"write-sid-default-dacl-inline";
        const bool default_dacl = mode == L"write-sid-default-dacl" || mode == L"write-sid-default-dacl-inline";
        Handle current;
        require(OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY | TOKEN_DUPLICATE | TOKEN_ASSIGN_PRIMARY |
            (default_dacl ? TOKEN_ADJUST_DEFAULT : 0), &current.value), "OpenProcessToken");
        contained(current.value);
        auto package = information(current.value, TokenAppContainerSid);
        auto sid = reinterpret_cast<const TOKEN_APPCONTAINER_INFORMATION*>(package.data())->TokenAppContainer;
        if (!sid) throw std::runtime_error("missing AppContainer package SID");
        const bool write_sid = mode == L"write-sid" || mode == L"write-sid-lpac" ||
            mode == L"write-sid-no-window" || mode == L"write-sid-native" || mode == L"write-sid-native-no-window" || default_dacl || inline_node;
        const bool no_window = mode == L"write-sid-no-window" || mode == L"write-sid-native-no-window";
        if (argc != (write_sid ? 5 : 4)) throw std::runtime_error("unexpected restriction arguments");
        const bool reduce_capabilities = mode == L"reduce-capabilities" || mode == L"reduce-capabilities-lpac";
        const bool disable_marker = mode == L"disable-marker" || mode == L"disable-marker-lpac";
        const bool lpac = mode == L"lpac-inherit" || mode == L"lpac-recreate" || mode == L"disable-marker-lpac" || mode == L"reduce-capabilities-lpac" || mode == L"write-sid-lpac";
        const bool with_package = mode == L"package" || mode == L"package-arap" ||
            mode == L"package-arap-registry" || mode == L"package-write";
        const bool with_arap = mode == L"arap" || mode == L"package-arap" || mode == L"package-arap-registry";
        const bool with_registry = mode == L"package-arap-registry" || mode == L"lpac-recreate" || reduce_capabilities;
        if (!lpac && !write_sid && !reduce_capabilities && !disable_marker && !with_package && !with_arap && mode != L"control" && mode != L"user") {
            throw std::runtime_error("unknown token restriction mode");
        }
        std::vector<SID_AND_ATTRIBUTES> sids;
        if (with_package) sids.push_back({ sid, 0 });
        auto user = information(current.value, TokenUser);
        if (mode == L"user") sids.push_back({ reinterpret_cast<const TOKEN_USER*>(user.data())->User.Sid, 0 });
        auto free_sid = [](void* value) { LocalFree(value); };
        std::unique_ptr<void, decltype(free_sid)> arap(nullptr, free_sid), registry(nullptr, free_sid);
        if (with_arap) {
            PSID allocated = nullptr;
            require(ConvertStringSidToSidW(L"S-1-15-2-2", &allocated), "ARAP SID");
            arap.reset(allocated);
            sids.push_back({ arap.get(), 0 });
        }
        if (with_registry) {
            PSID allocated = nullptr;
            require(ConvertStringSidToSidW(L"S-1-15-3-1024-1065365936-1281604716-3511738428-1654721687-432734479-3232135806-4053264122-3456934681", &allocated), "registryRead SID");
            registry.reset(allocated);
            if (!lpac && !reduce_capabilities) sids.push_back({ registry.get(), 0 });
        }
        std::unique_ptr<void, decltype(free_sid)> write_restriction(nullptr, free_sid);
        if (write_sid) {
            PSID allocated = nullptr;
            require(ConvertStringSidToSidW(argv[4], &allocated), "ordinary write restricting SID");
            write_restriction.reset(allocated);
            sids.push_back({ write_restriction.get(), 0 });
        }
        std::unique_ptr<void, decltype(free_sid)> marker(nullptr, free_sid);
        SID_AND_ATTRIBUTES disabled{};
        if (disable_marker) {
            PSID allocated = nullptr;
            require(ConvertStringSidToSidW(L"S-1-15-3-1024-530591407-3118584226-222893001-1659895842-537460181-133087082-1363364250-4100990745", &allocated), "marker SID");
            marker.reset(allocated);
            disabled.Sid = marker.get();
        }
        Handle restricted;
        if (reduce_capabilities) {
            // The same dynamically loaded API and signature used by Chromium's
            // base::win::AccessToken::CreateAppContainer. Probe only: do not
            // introduce an undocumented dependency into the shipped launcher.
            using CreateToken = BOOL(WINAPI*)(HANDLE, PSECURITY_CAPABILITIES, PHANDLE);
            const auto module = GetModuleHandleW(L"kernelbase.dll");
            if (!module) throw std::runtime_error("kernelbase.dll is not loaded");
            const FARPROC address = GetProcAddress(module, "CreateAppContainerToken");
            if (!address) throw std::runtime_error("CreateAppContainerToken is unavailable");
            CreateToken create = nullptr;
            static_assert(sizeof(create) == sizeof(address));
            std::memcpy(&create, &address, sizeof(create));
            SID_AND_ATTRIBUTES read_cap{ registry.get(), SE_GROUP_ENABLED };
            SECURITY_CAPABILITIES capabilities{};
            capabilities.AppContainerSid = sid;
            capabilities.Capabilities = &read_cap;
            capabilities.CapabilityCount = 1;
            require(create(current.value, &capabilities, &restricted.value), "CreateAppContainerToken");
            contained(restricted.value);
            auto remaining = information(restricted.value, TokenCapabilities);
            const auto* groups = reinterpret_cast<const TOKEN_GROUPS*>(remaining.data());
            if (groups->GroupCount != 1 || !EqualSid(groups->Groups[0].Sid, registry.get())) {
                throw std::runtime_error("new token retained unexpected capabilities");
            }
            std::cout << "child token has only registryRead capability" << std::endl;
        } else if (!lpac || disable_marker || write_sid) {
            const DWORD flags = DISABLE_MAX_PRIVILEGE | ((mode == L"package-write" || write_sid) ? WRITE_RESTRICTED : 0);
            require(CreateRestrictedToken(current.value, flags, disable_marker ? 1 : 0, disable_marker ? &disabled : nullptr, 0, nullptr,
                static_cast<DWORD>(sids.size()), sids.empty() ? nullptr : sids.data(), &restricted.value), "CreateRestrictedToken");
            contained(restricted.value);
            const DWORD after = restricting_count(restricted.value);
            std::cout << "token restriction: before=" << restricting_count(current.value) << "; after=" << after << std::endl;
            if (mode != L"control" && !disable_marker && after == 0) throw std::runtime_error("restriction produced no restricting SIDs");
            if (write_sid) {
                auto entries = information(restricted.value, TokenRestrictedSids);
                const auto* groups = reinterpret_cast<const TOKEN_GROUPS*>(entries.data());
                if (groups->GroupCount != 1 || !EqualSid(groups->Groups[0].Sid, write_restriction.get())) {
                    throw std::runtime_error("write restriction did not retain exactly the task SID");
                }
                if (default_dacl) {
                    // New private kernel objects must satisfy the extra access
                    // check too. This changes only the newly minted token's
                    // default DACL, never an existing host object's ACL.
                    auto original = information(current.value, TokenDefaultDacl);
                    const auto* defaults = reinterpret_cast<const TOKEN_DEFAULT_DACL*>(original.data());
                    if (!defaults->DefaultDacl) throw std::runtime_error("missing original default DACL");
                    EXPLICIT_ACCESS_W entry{};
                    entry.grfAccessPermissions = GENERIC_ALL;
                    entry.grfAccessMode = GRANT_ACCESS;
                    entry.Trustee.TrusteeForm = TRUSTEE_IS_SID;
                    entry.Trustee.TrusteeType = TRUSTEE_IS_UNKNOWN;
                    entry.Trustee.ptstrName = reinterpret_cast<LPWSTR>(write_restriction.get());
                    PACL expanded = nullptr;
                    const DWORD code = SetEntriesInAclW(1, &entry, defaults->DefaultDacl, &expanded);
                    if (code != ERROR_SUCCESS) {
                        SetLastError(code);
                        require(FALSE, "private kernel default DACL");
                    }
                    std::unique_ptr<void, decltype(free_sid)> owned(expanded, free_sid);
                    TOKEN_DEFAULT_DACL replacement{ expanded };
                    require(SetTokenInformation(restricted.value, TokenDefaultDacl, &replacement,
                        static_cast<DWORD>(sizeof(replacement))), "set private kernel default DACL");
                    std::cout << "task SID included in new token default DACL" << std::endl;
                }
            }

        }
        auto block = environment();
        const std::wstring command = L"\"" + std::wstring(argv[1]) + L"\" " +
            (inline_node ? L"-e " : L"") + L"\"" + std::wstring(argv[2]) + L"\"";
        std::vector<wchar_t> mutable_command(command.begin(), command.end());
        mutable_command.push_back(0);
        STARTUPINFOEXW startup{};
        startup.StartupInfo.cb = static_cast<DWORD>(lpac ? sizeof(startup) : sizeof(STARTUPINFOW));
        startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        Handle input, output, error;
        for (const auto& pair : std::vector<std::pair<DWORD, HANDLE*>>{
            { STD_INPUT_HANDLE, &input.value }, { STD_OUTPUT_HANDLE, &output.value }, { STD_ERROR_HANDLE, &error.value } }) {
            require(DuplicateHandle(GetCurrentProcess(), GetStdHandle(pair.first), GetCurrentProcess(), pair.second, 0, TRUE, DUPLICATE_SAME_ACCESS), "DuplicateHandle(stdio)");
        }
        startup.StartupInfo.hStdInput = input.value;
        startup.StartupInfo.hStdOutput = output.value;
        startup.StartupInfo.hStdError = error.value;
        PROCESS_INFORMATION process{};
        if (lpac) {
            SIZE_T size = 0;
            const DWORD count = mode == L"lpac-recreate" ? 2 : 1;
            InitializeProcThreadAttributeList(nullptr, count, 0, &size);
            if (size == 0 || size > 65536) throw std::runtime_error("invalid attribute-list size");
            std::vector<BYTE> attributes(size);
            startup.lpAttributeList = reinterpret_cast<LPPROC_THREAD_ATTRIBUTE_LIST>(attributes.data());
            require(InitializeProcThreadAttributeList(startup.lpAttributeList, count, 0, &size), "InitializeProcThreadAttributeList");
            DWORD policy = 1; // PROCESS_CREATION_ALL_APPLICATION_PACKAGES_OPT_OUT.
            require(UpdateProcThreadAttribute(startup.lpAttributeList, 0, PROC_THREAD_ATTRIBUTE_ALL_APPLICATION_PACKAGES_POLICY,
                &policy, sizeof(policy), nullptr, nullptr), "UpdateProcThreadAttribute(LPAC)");
            SID_AND_ATTRIBUTES read_cap{ registry.get(), SE_GROUP_ENABLED };
            SECURITY_CAPABILITIES capabilities{};
            capabilities.AppContainerSid = sid;
            capabilities.Capabilities = &read_cap;
            capabilities.CapabilityCount = 1;
            if (mode == L"lpac-recreate") {
                require(UpdateProcThreadAttribute(startup.lpAttributeList, 0, PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES,
                    &capabilities, sizeof(capabilities), nullptr, nullptr), "UpdateProcThreadAttribute(capabilities)");
            }
            const DWORD flags = CREATE_UNICODE_ENVIRONMENT | EXTENDED_STARTUPINFO_PRESENT;
            const BOOL created = (disable_marker || reduce_capabilities || write_sid)
                ? CreateProcessAsUserW(restricted.value, argv[1], mutable_command.data(), nullptr, nullptr, TRUE,
                    flags, block.data(), nullptr, &startup.StartupInfo, &process)
                : CreateProcessW(argv[1], mutable_command.data(), nullptr, nullptr, TRUE,
                    flags, block.data(), nullptr, &startup.StartupInfo, &process);
            const DWORD code = created ? 0 : GetLastError();
            DeleteProcThreadAttributeList(startup.lpAttributeList);
            SetLastError(code);
            require(created, "CreateProcessW(LPAC child)");
        } else {
            require(CreateProcessAsUserW(restricted.value, argv[1], mutable_command.data(), nullptr, nullptr,
                TRUE, CREATE_UNICODE_ENVIRONMENT | (no_window ? CREATE_NO_WINDOW : 0),
                block.data(), nullptr, &startup.StartupInfo, &process), "CreateProcessAsUserW");
        }
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
