// Diagnostic only. Combine a native security environment with a further
// restriction of its own token; never create an unrestricted fallback child.
#define UNICODE
#define _UNICODE
#include <windows.h>
#include <sddl.h>
#include <userenv.h>
#include <winternl.h>
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
#pragma comment(lib, "user32.lib")

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

static std::vector<wchar_t> environment(const std::map<std::wstring, std::wstring>& additions = {}) {
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
    for (const auto& [name, value] : additions) values[name] = value;
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
    wchar_t normal_control[8]{};
    if (GetEnvironmentVariableW(L"QQ_NATIVE_EXPECT_NORMAL", normal_control, 8)) {
        const auto kind = information(token.value, TokenIsAppContainer);
        if (*reinterpret_cast<const DWORD*>(kind.data())) throw std::runtime_error("normal control unexpectedly contained");
        std::cout << "normal-host-control=1" << std::endl;
        return 0;
    }
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

// Read the caller's effective namespace; a successful setter alone does not
// prove that subsequent file opens use the private directory.
static void inspect_device_map() {
    wchar_t mapping[32768]{};
    const DWORD count = QueryDosDeviceW(L"C:", mapping, 32768);
    std::wcout << L"effective-drive-C=" << (count ? mapping : L"<unavailable>")
        << L"; error=" << (count ? 0 : GetLastError()) << std::endl;
    const auto module = GetModuleHandleW(L"ntdll.dll");
    using OpenDirectory = NTSTATUS(NTAPI*)(PHANDLE, ACCESS_MASK, POBJECT_ATTRIBUTES);
    using QueryObject = NTSTATUS(NTAPI*)(HANDLE, ULONG, PVOID, ULONG, PULONG);
    const FARPROC open_address = GetProcAddress(module, "NtOpenDirectoryObject");
    const FARPROC query_address = GetProcAddress(module, "NtQueryObject");
    if (!open_address || !query_address) throw std::runtime_error("namespace query APIs unavailable");
    OpenDirectory open = nullptr;
    QueryObject query = nullptr;
    static_assert(sizeof(open) == sizeof(open_address));
    static_assert(sizeof(query) == sizeof(query_address));
    std::memcpy(&open, &open_address, sizeof(open));
    std::memcpy(&query, &query_address, sizeof(query));
    wchar_t path[] = L"\\??";
    UNICODE_STRING name{};
    name.Buffer = path;
    name.Length = static_cast<USHORT>((std::size(path) - 1) * sizeof(wchar_t));
    name.MaximumLength = static_cast<USHORT>(sizeof(path));
    OBJECT_ATTRIBUTES attributes{};
    attributes.Length = sizeof(attributes);
    attributes.ObjectName = &name;
    attributes.Attributes = OBJ_CASE_INSENSITIVE;
    Handle directory;
    const NTSTATUS opened = open(&directory.value, 1, &attributes);
    std::cout << "effective-dos-directory-open-status=" << static_cast<ULONG>(opened) << std::endl;
    if (opened < 0) return;
    std::vector<BYTE> storage(65536);
    ULONG length = 0;
    const NTSTATUS queried = query(directory.value, 1, storage.data(), static_cast<ULONG>(storage.size()), &length);
    std::cout << "effective-dos-directory-query-status=" << static_cast<ULONG>(queried) << std::endl;
    if (queried < 0) return;
    if (length < sizeof(UNICODE_STRING) || length > storage.size()) throw std::runtime_error("invalid namespace name length");
    const auto* result = reinterpret_cast<const UNICODE_STRING*>(storage.data());
    const auto begin = reinterpret_cast<ULONG_PTR>(storage.data());
    const auto buffer = reinterpret_cast<ULONG_PTR>(result->Buffer);
    if (result->Length && (buffer < begin || buffer > begin + storage.size()
        || result->Length > begin + storage.size() - buffer)) throw std::runtime_error("invalid namespace name buffer");
    const std::wstring actual = result->Length ? std::wstring(result->Buffer, result->Length / sizeof(wchar_t)) : L"<unnamed>";
    std::wcout << L"effective-dos-directory=" << actual << std::endl;
}

// Do not infer permission failure from Node's path loader. Probe each Win32
// access mask directly, without changing any file or existing security object.
static int inspect_files() {
    wchar_t waiting[8]{};
    if (GetEnvironmentVariableW(L"QQ_NATIVE_WAIT_VIEW", waiting, 8)) {
        const char ready[] = "READY\n";
        DWORD written = 0, read = 0;
        require(WriteFile(GetStdHandle(STD_OUTPUT_HANDLE), ready, 6, &written, nullptr), "view source ready frame");
        if (written != 6) throw std::runtime_error("short view ready frame");
        BYTE release = 0;
        require(ReadFile(GetStdHandle(STD_INPUT_HANDLE), &release, 1, &read, nullptr), "view source release frame");
        if (read != 1 || release != 1) throw std::runtime_error("invalid view release frame");
    }
    inspect();
    inspect_device_map();
    wchar_t acl_mode[8]{};
    const bool task_acl = GetEnvironmentVariableW(L"QQ_NATIVE_TASK_ACL", acl_mode, 8) != 0;
    if (task_acl) {
        wchar_t task[32768]{};
        require(GetEnvironmentVariableW(L"QQ_NATIVE_TASK_WORKSPACE", task, 32768) != 0, "task ACL workspace");
        const BOOL entered = SetCurrentDirectoryW(task);
        std::cout << "task-cwd-error=" << (entered ? 0 : GetLastError()) << std::endl;
        Handle proof;
        const std::wstring proof_path = std::wstring(task) + L"\\acl-write-proof.txt";
        proof.value = CreateFileW(proof_path.c_str(), GENERIC_WRITE, 0, nullptr, OPEN_EXISTING, 0, nullptr);
        std::cout << "task-write-open-error=" << (proof.value == INVALID_HANDLE_VALUE ? GetLastError() : 0) << std::endl;
        if (proof.value != INVALID_HANDLE_VALUE) {
        const char bytes[] = "contained-write";
        DWORD count = 0;
        require(WriteFile(proof.value, bytes, sizeof(bytes) - 1, &count, nullptr), "task write proof");
        if (count != sizeof(bytes) - 1) throw std::runtime_error("short task write proof");
        require(SetEndOfFile(proof.value), "truncate task proof");
        std::cout << "task-write-proof=written" << std::endl;
        }
    }
    wchar_t current[32768]{};
    require(GetCurrentDirectoryW(32768, current) != 0, "GetCurrentDirectoryW");
    const std::wstring workspace(current);
    wchar_t outside[32768]{};
    require(GetEnvironmentVariableW(L"QQ_NATIVE_PUBLIC_WRITABLE", outside, 32768) != 0,
        "public writable path");
    std::vector<std::wstring> paths{workspace, workspace + L"\\payload.cjs", outside, L"C:\\", L"NUL"};
    for (const auto variable : {L"QQ_NATIVE_PUBLIC_WRITABLE_GUID", L"QQ_NATIVE_PUBLIC_WRITABLE_GLOBALROOT", L"QQ_NATIVE_SOFTWARE_FILE", L"QQ_NATIVE_PRIVATE_CREDENTIAL"}) {
        wchar_t raw[32768]{};
        const DWORD size = GetEnvironmentVariableW(variable, raw, 32768);
        if (size >= 32768) throw std::runtime_error("raw fixture path exceeds bound");
        if (size) paths.emplace_back(raw);
    }
    for (const auto& name : paths) {
        wchar_t canonical[32768]{};
        const DWORD length = GetLongPathNameW(name.c_str(), canonical, 32768);
        std::wcout << L"long-path=" << name << L"; result=" << (length ? canonical : L"<unavailable>")
            << L"; error=" << (length ? 0 : GetLastError()) << std::endl;
        std::vector<std::wstring> candidates{name};
        if (name.size() > 2 && name[1] == L':') candidates.push_back(L"\\\\?\\" + name);
        for (const auto& candidate : candidates) {
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
                    const DWORD nt_count = GetFinalPathNameByHandleW(file.value, resolved, 32768, FILE_NAME_NORMALIZED | VOLUME_NAME_NT);
                    std::wcout << L"; nt-path=" << (nt_count ? resolved : L"<unavailable>")
                        << L"; nt-error=" << (nt_count ? 0 : GetLastError());
                }
                std::wcout << std::endl;
            }
        }
    }
    return 0;
}

static int hold_environment() {
    Handle directory, changed;
    directory.value = CreateFileW(L".", FILE_LIST_DIRECTORY, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
        nullptr, OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED, nullptr);
    require(directory.value != INVALID_HANDLE_VALUE, "hold directory");
    changed.value = CreateEventW(nullptr, TRUE, FALSE, nullptr);
    require(changed.value != nullptr, "hold notification event");
    BYTE notifications[4096]{};
    OVERLAPPED pending{};
    pending.hEvent = changed.value;
    auto arm = [&] {
        ResetEvent(changed.value);
        require(ReadDirectoryChangesW(directory.value, notifications, sizeof(notifications), FALSE,
            FILE_NOTIFY_CHANGE_FILE_NAME | FILE_NOTIFY_CHANGE_LAST_WRITE, nullptr, &pending, nullptr), "hold notification");
    };
    arm();
    Handle token;
    require(OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &token.value), "hold token");
    contained(token.value);
    const auto package = information(token.value, TokenAppContainerSid);
    const auto sid = sid_text(reinterpret_cast<const TOKEN_APPCONTAINER_INFORMATION*>(package.data())->TokenAppContainer);
    const auto ready = std::string("{\"pid\":") + std::to_string(GetCurrentProcessId()) + ",\"package\":\"" + sid + "\"}";
    Handle output;
    output.value = CreateFileW(L"source-ready.json", GENERIC_WRITE, FILE_SHARE_READ, nullptr, CREATE_ALWAYS, FILE_ATTRIBUTE_NORMAL, nullptr);
    require(output.value != INVALID_HANDLE_VALUE, "hold ready file");
    DWORD written = 0;
    require(WriteFile(output.value, ready.data(), static_cast<DWORD>(ready.size()), &written, nullptr), "hold ready write");
    if (written != ready.size()) throw std::runtime_error("short ready write");
    CloseHandle(output.value);
    output.value = nullptr;
    for (;;) {
        if (GetFileAttributesW(L"source-stop.txt") != INVALID_FILE_ATTRIBUTES) break;
        if (WaitForSingleObject(changed.value, INFINITE) != WAIT_OBJECT_0) throw std::runtime_error("hold notification wait failed");
        DWORD bytes = 0;
        require(GetOverlappedResult(directory.value, &pending, &bytes, FALSE), "hold notification result");
        arm();
    }
    if (!CancelIoEx(directory.value, &pending) && GetLastError() != ERROR_NOT_FOUND) {
        require(FALSE, "hold notification cancel");
    }
    DWORD bytes = 0;
    if (!GetOverlappedResult(directory.value, &pending, &bytes, TRUE) && GetLastError() != ERROR_OPERATION_ABORTED) {
        require(FALSE, "hold notification settlement");
    }
    return 0;
}

static int host_recreate(const wchar_t* pid_text, const wchar_t* supplied_package = nullptr, HANDLE security_environment = nullptr, bool inherit_parent = false) {
    Handle host, source, source_token, token;
    require(OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY | TOKEN_DUPLICATE | TOKEN_ASSIGN_PRIMARY, &host.value), "recreate host token");
    const auto host_kind = information(host.value, TokenIsAppContainer);
    if (*reinterpret_cast<const DWORD*>(host_kind.data())) throw std::runtime_error("recreation requires a normal host token");
    wchar_t* end = nullptr;
    const unsigned long pid = wcstoul(pid_text, &end, 10);
    if (!pid || !end || *end) throw std::runtime_error("invalid source PID");
    source.value = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | (inherit_parent ? PROCESS_CREATE_PROCESS | PROCESS_DUP_HANDLE : 0), FALSE, pid);
    require(source.value != nullptr, "recreate source process");
    require(OpenProcessToken(source.value, TOKEN_QUERY, &source_token.value), "recreate source token");
    contained(source_token.value);
    auto package = information(source_token.value, TokenAppContainerSid);
    const auto sid = reinterpret_cast<const TOKEN_APPCONTAINER_INFORMATION*>(package.data())->TokenAppContainer;
    wchar_t expected[256]{};
    if (supplied_package) {
        if (wcslen(supplied_package) >= 256) throw std::runtime_error("invalid supplied package size");
        std::copy(supplied_package, supplied_package + wcslen(supplied_package) + 1, expected);
    } else {
        require(GetEnvironmentVariableW(L"QQ_NATIVE_PACKAGE_SID", expected, 256) != 0, "expected source package");
    }
    PSID expected_sid = nullptr, registry_sid = nullptr;
    require(ConvertStringSidToSidW(expected, &expected_sid), "expected package SID");
    auto local_free = [](void* value) { LocalFree(value); };
    std::unique_ptr<void, decltype(local_free)> owned_expected(expected_sid, local_free);
    if (!EqualSid(sid, expected_sid)) throw std::runtime_error("source package changed");
    require(ConvertStringSidToSidW(L"S-1-15-3-1024-1065365936-1281604716-3511738428-1654721687-432734479-3232135806-4053264122-3456934681", &registry_sid), "recreate registry SID");
    std::unique_ptr<void, decltype(local_free)> owned_registry(registry_sid, local_free);
    using CreateToken = BOOL(WINAPI*)(HANDLE, PSECURITY_CAPABILITIES, PHANDLE);
    const auto module = GetModuleHandleW(L"kernelbase.dll");
    if (!module) throw std::runtime_error("kernelbase.dll is unavailable");
    const FARPROC address = GetProcAddress(module, "CreateAppContainerToken");
    if (!address) throw std::runtime_error("CreateAppContainerToken is unavailable");
    CreateToken create = nullptr;
    static_assert(sizeof(create) == sizeof(address));
    std::memcpy(&create, &address, sizeof(create));
    SID_AND_ATTRIBUTES capability{ registry_sid, SE_GROUP_ENABLED };
    SECURITY_CAPABILITIES capabilities{};
    capabilities.AppContainerSid = sid;
    capabilities.Capabilities = &capability;
    capabilities.CapabilityCount = 1;
    require(create(host.value, &capabilities, &token.value), "host CreateAppContainerToken");
    contained(token.value);
    const auto remaining = information(token.value, TokenCapabilities);
    const auto* groups = reinterpret_cast<const TOKEN_GROUPS*>(remaining.data());
    if (groups->GroupCount != 1 || !EqualSid(groups->Groups[0].Sid, registry_sid)) {
        throw std::runtime_error("recreated token retained unexpected capabilities");
    }
    // This probe launches only the immutable inspector, never a host fallback.
    wchar_t image[32768]{};
    require(GetModuleFileNameW(nullptr, image, 32768) != 0, "inspector image");
    auto command = L"\"" + std::wstring(image) + L"\" inspect-files";
    std::vector<wchar_t> mutable_command(command.begin(), command.end());
    mutable_command.push_back(0);
    SIZE_T size = 0;
    const DWORD attribute_count = security_environment ? 2 : 1;
    InitializeProcThreadAttributeList(nullptr, attribute_count, 0, &size);
    if (!size || size > 65536) throw std::runtime_error("invalid recreated attribute size");
    std::vector<BYTE> storage(size);
    auto attributes = reinterpret_cast<LPPROC_THREAD_ATTRIBUTE_LIST>(storage.data());
    require(InitializeProcThreadAttributeList(attributes, attribute_count, 0, &size), "recreated attributes");
    HANDLE inherited_policy = inherit_parent ? source.value : security_environment;
    const DWORD_PTR policy_attribute = inherit_parent ? PROC_THREAD_ATTRIBUTE_PARENT_PROCESS : (35 | 0x00020000);
    if (security_environment && !UpdateProcThreadAttribute(attributes, 0, policy_attribute,
        &inherited_policy, sizeof(inherited_policy), nullptr, nullptr)) {
        const DWORD code = GetLastError();
        DeleteProcThreadAttributeList(attributes);
        SetLastError(code);
        require(FALSE, "recreated security environment attribute");
    }
    DWORD policy = 1;
    const BOOL updated = UpdateProcThreadAttribute(attributes, 0, PROC_THREAD_ATTRIBUTE_ALL_APPLICATION_PACKAGES_POLICY,
        &policy, sizeof(policy), nullptr, nullptr);
    if (!updated) {
        const DWORD code = GetLastError();
        DeleteProcThreadAttributeList(attributes);
        SetLastError(code);
        require(FALSE, "recreated LPAC attribute");
    }
    STARTUPINFOEXW startup{};
    startup.StartupInfo.cb = sizeof(startup);
    startup.lpAttributeList = attributes;
    // The inspector inherits only handles the normal-host desktop controller
    // explicitly duplicated for this child. Source/token handles are private.
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    startup.StartupInfo.hStdInput = GetStdHandle(STD_INPUT_HANDLE);
    startup.StartupInfo.hStdOutput = GetStdHandle(STD_OUTPUT_HANDLE);
    startup.StartupInfo.hStdError = GetStdHandle(STD_ERROR_HANDLE);
    if (inherit_parent) {
        // A designated parent supplies inherited handles too. Duplicate only
        // stdio into the suspended, immutable source; its termination closes
        // these remote handles. Their values must never be closed in the host.
        for (HANDLE* endpoint : { &startup.StartupInfo.hStdInput, &startup.StartupInfo.hStdOutput, &startup.StartupInfo.hStdError }) {
            HANDLE remote = nullptr;
            if (!DuplicateHandle(GetCurrentProcess(), *endpoint, source.value, &remote, 0, TRUE, DUPLICATE_SAME_ACCESS)) {
                const DWORD code = GetLastError(); DeleteProcThreadAttributeList(attributes); SetLastError(code);
                require(FALSE, "designated parent stdio");
            }
            *endpoint = remote;
        }
    }
    wchar_t workspace[32768]{};
    const DWORD length = GetEnvironmentVariableW(L"QQ_NATIVE_TASK_WORKSPACE", workspace, 32768);
    if (!length || length >= 32768) throw std::runtime_error("missing task workspace");
    PROCESS_INFORMATION process{};
    LPWCH inherited_environment = GetEnvironmentStringsW();
    require(inherited_environment != nullptr, "recreated environment");
    const BOOL created = CreateProcessAsUserW(token.value, image, mutable_command.data(), nullptr, nullptr, TRUE,
        CREATE_UNICODE_ENVIRONMENT | EXTENDED_STARTUPINFO_PRESENT | (inherit_parent ? CREATE_NO_WINDOW : 0),
        inherited_environment, workspace, &startup.StartupInfo, &process);
    const DWORD code = created ? 0 : GetLastError();
    FreeEnvironmentStringsW(inherited_environment);
    DeleteProcThreadAttributeList(attributes);
    SetLastError(code);
    require(created, "recreated inspector launch");
    Handle child, thread;
    child.value = process.hProcess;
    thread.value = process.hThread;
    if (WaitForSingleObject(child.value, INFINITE) != WAIT_OBJECT_0) throw std::runtime_error("recreated inspector wait failed");
    DWORD result = 0;
    require(GetExitCodeProcess(child.value, &result), "recreated inspector exit");
    return static_cast<int>(result);
}

static void grant_private_desktop(PSID package) {
    wchar_t name[256]{};
    DWORD size = 0;
    require(GetUserObjectInformationW(GetThreadDesktop(GetCurrentThreadId()), UOI_NAME, name, sizeof(name), &size), "owned desktop name");
    if (!std::wstring(name).starts_with(L"QQProbe-{")) throw std::runtime_error("not a disposable probe desktop");
    struct Desktop {
        HDESK value = nullptr;
        ~Desktop() { if (value) CloseDesktop(value); }
    } desktop;
    desktop.value = OpenDesktopW(name, 0, FALSE, READ_CONTROL | WRITE_DAC);
    require(desktop.value != nullptr, "owned desktop ACL handle");
    SECURITY_INFORMATION requested = DACL_SECURITY_INFORMATION;
    GetUserObjectSecurity(desktop.value, &requested, nullptr, 0, &size);
    if (!size || size > 65536) throw std::runtime_error("invalid owned desktop descriptor size");
    std::vector<BYTE> descriptor(size);
    require(GetUserObjectSecurity(desktop.value, &requested, descriptor.data(), size, &size), "owned desktop descriptor");
    PACL original = nullptr;
    BOOL present = FALSE, defaulted = FALSE;
    require(GetSecurityDescriptorDacl(descriptor.data(), &present, &original, &defaulted), "owned desktop DACL");
    if (!present || !original) throw std::runtime_error("owned desktop has no protected DACL");
    EXPLICIT_ACCESS_W entry{};
    entry.grfAccessPermissions = GENERIC_ALL;
    entry.grfAccessMode = GRANT_ACCESS;
    entry.Trustee.TrusteeForm = TRUSTEE_IS_SID;
    entry.Trustee.TrusteeType = TRUSTEE_IS_UNKNOWN;
    entry.Trustee.ptstrName = reinterpret_cast<LPWSTR>(package);
    PACL expanded = nullptr;
    const DWORD code = SetEntriesInAclW(1, &entry, original, &expanded);
    if (code != ERROR_SUCCESS) { SetLastError(code); require(FALSE, "owned desktop package grant"); }
    auto local_free = [](void* value) { LocalFree(value); };
    std::unique_ptr<void, decltype(local_free)> owned(expanded, local_free);
    SECURITY_DESCRIPTOR replacement{};
    require(InitializeSecurityDescriptor(&replacement, SECURITY_DESCRIPTOR_REVISION), "owned desktop replacement");
    require(SetSecurityDescriptorDacl(&replacement, TRUE, expanded, FALSE), "owned desktop replacement DACL");
    require(SetUserObjectSecurity(desktop.value, &requested, &replacement), "owned desktop package access");
}

struct PrivateDeviceMap {
    Handle directory, drive, global_root, null_device, volume;
};

// Diagnostic only. Change one suspended child's device map, never the host's
// logon-session drive links. The directory and all links are unnamed/private
// host-owned objects, held until the child has exited.
static void install_device_map(HANDLE process, PSID package, PrivateDeviceMap& objects) {
    const auto module = GetModuleHandleW(L"ntdll.dll");
    if (!module) throw std::runtime_error("ntdll.dll unavailable");
    using CreateDirectory = NTSTATUS(NTAPI*)(PHANDLE, ACCESS_MASK, POBJECT_ATTRIBUTES);
    using CreateLink = NTSTATUS(NTAPI*)(PHANDLE, ACCESS_MASK, POBJECT_ATTRIBUTES, PUNICODE_STRING);
    using SetProcess = NTSTATUS(NTAPI*)(HANDLE, ULONG, PVOID, ULONG);
    const FARPROC directory_address = GetProcAddress(module, "NtCreateDirectoryObject");
    const FARPROC link_address = GetProcAddress(module, "NtCreateSymbolicLinkObject");
    const FARPROC set_address = GetProcAddress(module, "NtSetInformationProcess");
    if (!directory_address || !link_address || !set_address) throw std::runtime_error("private device-map APIs unavailable");
    CreateDirectory create_directory = nullptr;
    CreateLink create_link = nullptr;
    SetProcess set_process = nullptr;
    static_assert(sizeof(create_directory) == sizeof(directory_address));
    static_assert(sizeof(create_link) == sizeof(link_address));
    static_assert(sizeof(set_process) == sizeof(set_address));
    std::memcpy(&create_directory, &directory_address, sizeof(create_directory));
    std::memcpy(&create_link, &link_address, sizeof(create_link));
    std::memcpy(&set_process, &set_address, sizeof(set_process));
    auto check = [](NTSTATUS status, const char* phase) {
        if (status < 0) throw std::runtime_error(std::string(phase) + " NTSTATUS: " + std::to_string(static_cast<unsigned long>(status)));
    };
    auto unicode = [](const std::wstring& text) {
        if (text.size() > 32766) throw std::runtime_error("device-map name exceeds bound");
        UNICODE_STRING value{};
        value.Length = static_cast<USHORT>(text.size() * sizeof(wchar_t));
        value.MaximumLength = value.Length;
        value.Buffer = const_cast<PWSTR>(text.data());
        return value;
    };
    LPWSTR package_text = nullptr;
    require(ConvertSidToStringSidW(package, &package_text), "device-map package SID");
    auto local_free = [](void* value) { LocalFree(value); };
    std::unique_ptr<void, decltype(local_free)> owned_package(package_text, local_free);
    Handle host_token;
    require(OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &host_token.value), "device-map host token");
    const auto user = information(host_token.value, TokenUser);
    LPWSTR user_text = nullptr;
    require(ConvertSidToStringSidW(reinterpret_cast<const TOKEN_USER*>(user.data())->User.Sid, &user_text), "device-map owner SID");
    std::unique_ptr<void, decltype(local_free)> owned_user(user_text, local_free);
    const std::wstring descriptor = L"D:P(A;;GA;;;" + std::wstring(user_text) + L")(A;;GA;;;SY)(A;;GR;;;" + std::wstring(package_text) + L")";
    PSECURITY_DESCRIPTOR security = nullptr;
    require(ConvertStringSecurityDescriptorToSecurityDescriptorW(descriptor.c_str(), SDDL_REVISION_1, &security, nullptr), "private device-map descriptor");
    std::unique_ptr<void, decltype(local_free)> owned_security(security, local_free);
    OBJECT_ATTRIBUTES attributes{};
    attributes.Length = sizeof(attributes);
    attributes.SecurityDescriptor = security;
    check(create_directory(&objects.directory.value, STANDARD_RIGHTS_REQUIRED | 0xF, &attributes), "create private device-map directory");
    const auto link = [&](Handle& handle, const std::wstring& name, const std::wstring& target) {
        auto encoded_name = unicode(name), encoded_target = unicode(target);
        OBJECT_ATTRIBUTES location{};
        location.Length = sizeof(location);
        location.RootDirectory = objects.directory.value;
        location.ObjectName = &encoded_name;
        location.Attributes = OBJ_CASE_INSENSITIVE;
        location.SecurityDescriptor = security;
        check(create_link(&handle.value, STANDARD_RIGHTS_REQUIRED | 1, &location, &encoded_target), "create private device-map link");
    };
    wchar_t projection[32768]{};
    const DWORD size = GetEnvironmentVariableW(L"QQ_NATIVE_PROJECTION_ROOT", projection, 32768);
    if (!size || size >= 32768) throw std::runtime_error("missing projection root");
    Handle root;
    root.value = CreateFileW(projection, 0, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, nullptr,
        OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS, nullptr);
    require(root.value != INVALID_HANDLE_VALUE, "projection root handle");
    wchar_t target[32768]{};
    const DWORD target_size = GetFinalPathNameByHandleW(root.value, target, 32768, FILE_NAME_NORMALIZED | VOLUME_NAME_NT);
    if (!target_size || target_size >= 32768) throw std::runtime_error("projection root NT path unavailable");
    // The current finite fixture uses C:. Fail explicitly on any other drive.
    if (projection[0] != L'C' || projection[1] != L':') throw std::runtime_error("projection fixture requires C:");
    wchar_t volume_guid[128]{};
    require(GetVolumeNameForVolumeMountPointW(L"C:\\", volume_guid, 128), "projection volume GUID");
    std::wstring volume_name(volume_guid);
    if (!volume_name.starts_with(L"\\\\?\\Volume{") || !volume_name.ends_with(L"\\")) throw std::runtime_error("invalid projection volume GUID");
    volume_name = volume_name.substr(4, volume_name.size() - 5);
    const std::wstring physical_root(target);
    const auto separator = physical_root.find(L'\\', 8);
    if (!physical_root.starts_with(L"\\Device\\") || separator == std::wstring::npos) throw std::runtime_error("invalid projection device path");
    const auto device = physical_root.substr(0, separator);
    union DeviceMap {
        HANDLE directory;
        struct { ULONG drives; BYTE types[32]; } query;
    } map{};
    map.directory = objects.directory.value;
    ULONG used_size = sizeof(HANDLE);
    NTSTATUS status = set_process(process, 23, &map, used_size);
    if (static_cast<ULONG>(status) == 0xC0000004) { // explicit ABI length mismatch only
        used_size = sizeof(map);
        status = set_process(process, 23, &map, used_size);
    }
    check(status, "set suspended child's private device map");
    // Test whether binding the directory before creating its links changes
    // DOS path resolution. The child remains suspended or blocked on the
    // readiness pipe throughout installation.
    link(objects.drive, L"C:", target);
    link(objects.global_root, L"GLOBALROOT", L"\\");
    link(objects.null_device, L"NUL", L"\\Device\\Null");
    link(objects.volume, volume_name, device);
    std::cerr << "private device map setter status=" << static_cast<ULONG>(status) << "; ABI bytes=" << used_size << "; host drive map unchanged" << std::endl;
}

// Modify only this diagnostic task directory. Never grant a public capability
// access to task contents or change an installed tool's security descriptor.
static void grant_task_directory(PSID package, const wchar_t* directory, bool low_label) {
    PSECURITY_DESCRIPTOR descriptor = nullptr;
    PACL current = nullptr, updated = nullptr;
    DWORD code = GetNamedSecurityInfoW(directory, SE_FILE_OBJECT, DACL_SECURITY_INFORMATION,
        nullptr, nullptr, &current, nullptr, &descriptor);
    if (code != ERROR_SUCCESS) { SetLastError(code); require(FALSE, "task directory DACL"); }
    auto free_local = [](void* value) { LocalFree(value); };
    std::unique_ptr<void, decltype(free_local)> owned_descriptor(descriptor, free_local);
    EXPLICIT_ACCESSW grant{};
    grant.grfAccessPermissions = FILE_GENERIC_READ | FILE_GENERIC_WRITE | FILE_GENERIC_EXECUTE | DELETE;
    grant.grfAccessMode = GRANT_ACCESS;
    grant.grfInheritance = SUB_CONTAINERS_AND_OBJECTS_INHERIT;
    grant.Trustee.TrusteeForm = TRUSTEE_IS_SID;
    grant.Trustee.TrusteeType = TRUSTEE_IS_UNKNOWN;
    grant.Trustee.ptstrName = static_cast<LPWSTR>(package);
    code = SetEntriesInAclW(1, &grant, current, &updated);
    if (code != ERROR_SUCCESS) { SetLastError(code); require(FALSE, "exact task package ACE"); }
    std::unique_ptr<void, decltype(free_local)> owned_acl(updated, free_local);
    code = SetNamedSecurityInfoW(const_cast<LPWSTR>(directory), SE_FILE_OBJECT,
        DACL_SECURITY_INFORMATION, nullptr, nullptr, updated, nullptr);
    if (code != ERROR_SUCCESS) { SetLastError(code); require(FALSE, "apply exact task package ACE"); }
    if (low_label) {
        PSECURITY_DESCRIPTOR label = nullptr;
        require(ConvertStringSecurityDescriptorToSecurityDescriptorW(L"S:(ML;OICI;NW;;;LW)",
            SDDL_REVISION_1, &label, nullptr), "owned task low label");
        std::unique_ptr<void, decltype(free_local)> owned_label(label, free_local);
        PACL sacl = nullptr;
        BOOL present = FALSE, defaulted = FALSE;
        require(GetSecurityDescriptorSacl(label, &present, &sacl, &defaulted), "owned task label SACL");
        if (!present) throw std::runtime_error("missing owned task label");
        code = SetNamedSecurityInfoW(const_cast<LPWSTR>(directory), SE_FILE_OBJECT,
            LABEL_SECURITY_INFORMATION, nullptr, nullptr, nullptr, sacl);
        if (code != ERROR_SUCCESS) { SetLastError(code); require(FALSE, "apply owned task low label"); }
    }
}

static int host_create_policy(const wchar_t* policy_path, bool inherit_parent = false, bool control = false, bool restrict_host = false, bool lpac = false, bool projected = false, bool after_ready = false, bool task_acl = false, bool low_label = false, bool normal_control = false, bool self_control = false) {
    Handle host;
    require(OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY | (restrict_host ? TOKEN_DUPLICATE | TOKEN_ASSIGN_PRIMARY : 0), &host.value), "policy host token");
    const auto host_kind = information(host.value, TokenIsAppContainer);
    if (*reinterpret_cast<const DWORD*>(host_kind.data())) throw std::runtime_error("policy controller requires a normal host token");
    Handle policy_file;
    policy_file.value = CreateFileW(policy_path, GENERIC_READ, FILE_SHARE_READ, nullptr, OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL, nullptr);
    require(policy_file.value != INVALID_HANDLE_VALUE, "policy input");
    LARGE_INTEGER length{};
    require(GetFileSizeEx(policy_file.value, &length), "policy input size");
    if (length.QuadPart <= 0 || length.QuadPart > 1024 * 1024) throw std::runtime_error("invalid policy input size");
    std::vector<BYTE> policy(static_cast<size_t>(length.QuadPart));
    DWORD read = 0;
    require(ReadFile(policy_file.value, policy.data(), static_cast<DWORD>(policy.size()), &read, nullptr), "policy input read");
    if (read != policy.size()) throw std::runtime_error("short policy input");
    struct Module {
        HMODULE value = nullptr;
        ~Module() { if (value) FreeLibrary(value); }
    } module;
    module.value = LoadLibraryExW(L"processmodel.dll", nullptr, LOAD_LIBRARY_SEARCH_SYSTEM32);
    require(module.value != nullptr, "processmodel library");
    using CreateEnvironment = HRESULT(WINAPI*)(LPCVOID, DWORD, DWORD, HANDLE*);
    using CloseEnvironment = void(WINAPI*)(HANDLE);
    const FARPROC create_address = GetProcAddress(module.value, "CreateProcessSecurityEnvironment");
    const FARPROC close_address = GetProcAddress(module.value, "CloseProcessSecurityEnvironment");
    if (!create_address || !close_address) throw std::runtime_error("native security environment API unavailable");
    CreateEnvironment create = nullptr;
    CloseEnvironment close = nullptr;
    static_assert(sizeof(create) == sizeof(create_address));
    static_assert(sizeof(close) == sizeof(close_address));
    std::memcpy(&create, &create_address, sizeof(create));
    std::memcpy(&close, &close_address, sizeof(close));
    struct Environment {
        HANDLE value = nullptr;
        CloseEnvironment close = nullptr;
        ~Environment() { if (value) close(value); }
    } environment;
    environment.close = close;
    const HRESULT created = create(policy.data(), static_cast<DWORD>(policy.size()), 0, &environment.value);
    if (FAILED(created)) throw std::runtime_error("create native policy HRESULT: " + std::to_string(static_cast<unsigned long>(created)));
    if (!environment.value) throw std::runtime_error("native policy returned no handle");
    Handle restricted_host;
    std::vector<BYTE> host_restrictions;
    if (restrict_host) {
        wchar_t write_sid_text[256]{};
        const DWORD sid_size = GetEnvironmentVariableW(L"QQ_NATIVE_WRITE_SID", write_sid_text, 256);
        if (!sid_size || sid_size >= 256) throw std::runtime_error("missing host write SID");
        PSID write_sid = nullptr;
        require(ConvertStringSidToSidW(write_sid_text, &write_sid), "host write SID");
        auto local_free = [](void* value) { LocalFree(value); };
        std::unique_ptr<void, decltype(local_free)> owned_sid(write_sid, local_free);
        SID_AND_ATTRIBUTES restriction{ write_sid, 0 };
        require(CreateRestrictedToken(host.value, DISABLE_MAX_PRIVILEGE | WRITE_RESTRICTED,
            0, nullptr, 0, nullptr, 1, &restriction, &restricted_host.value), "host write restriction");
        if (restricting_count(restricted_host.value) != 1) throw std::runtime_error("invalid host write restriction count");
        host_restrictions = information(restricted_host.value, TokenRestrictedSids);
    }
    SIZE_T size = 0;
    const DWORD attribute_count = (normal_control ? 0 : (lpac ? 2 : 1)) + 1;
    InitializeProcThreadAttributeList(nullptr, attribute_count, 0, &size);
    if (!size || size > 65536) throw std::runtime_error("invalid policy attribute size");
    std::vector<BYTE> storage(size);
    auto attributes = reinterpret_cast<LPPROC_THREAD_ATTRIBUTE_LIST>(storage.data());
    require(InitializeProcThreadAttributeList(attributes, attribute_count, 0, &size), "policy attributes");
    if (!normal_control && !UpdateProcThreadAttribute(attributes, 0, 35 | 0x00020000, &environment.value, sizeof(environment.value), nullptr, nullptr)) {
        const DWORD code = GetLastError(); DeleteProcThreadAttributeList(attributes); SetLastError(code);
        require(FALSE, "source native policy attribute");
    }
    DWORD lpac_policy = 1;
    if (lpac && !UpdateProcThreadAttribute(attributes, 0, PROC_THREAD_ATTRIBUTE_ALL_APPLICATION_PACKAGES_POLICY,
        &lpac_policy, sizeof(lpac_policy), nullptr, nullptr)) {
        const DWORD code = GetLastError(); DeleteProcThreadAttributeList(attributes); SetLastError(code);
        require(FALSE, "source native LPAC attribute");
    }
    wchar_t image[32768]{}, workspace[32768]{};
    require(GetModuleFileNameW(nullptr, image, 32768) != 0, "policy source image");
    const DWORD workspace_size = GetEnvironmentVariableW(L"QQ_NATIVE_TASK_WORKSPACE", workspace, 32768);
    if (!workspace_size || workspace_size >= 32768) throw std::runtime_error("missing policy task workspace");
    const auto command = L"\"" + std::wstring(image) + L"\" inspect-files";
    std::vector<wchar_t> mutable_command(command.begin(), command.end());
    mutable_command.push_back(0);
    STARTUPINFOEXW startup{};
    startup.StartupInfo.cb = static_cast<DWORD>(sizeof(startup));
    startup.lpAttributeList = attributes;
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    startup.StartupInfo.hStdInput = GetStdHandle(STD_INPUT_HANDLE);
    startup.StartupInfo.hStdOutput = GetStdHandle(STD_OUTPUT_HANDLE);
    startup.StartupInfo.hStdError = GetStdHandle(STD_ERROR_HANDLE);
    PROCESS_INFORMATION source{};
    // Outside the explicit control case, the source is never resumed. Its
    // primary token gives the exact identity of this newly created policy.
    Handle input, child_input, output, child_output;
    std::map<std::wstring, std::wstring> additions;
    if (after_ready) {
        SECURITY_ATTRIBUTES synchronization{ static_cast<DWORD>(sizeof(SECURITY_ATTRIBUTES)), nullptr, TRUE };
        require(CreatePipe(&child_input.value, &input.value, &synchronization, 65536), "view release pipe");
        require(SetHandleInformation(input.value, HANDLE_FLAG_INHERIT, 0), "private view release writer");
        require(CreatePipe(&output.value, &child_output.value, &synchronization, 65536), "view ready pipe");
        require(SetHandleInformation(output.value, HANDLE_FLAG_INHERIT, 0), "private view ready reader");
        startup.StartupInfo.hStdInput = child_input.value;
        startup.StartupInfo.hStdOutput = child_output.value;
        additions[L"QQ_NATIVE_WAIT_VIEW"] = L"1";
    }
    // Match the SDK's explicit stdio allowlist. The native launch must not
    // inherit unrelated host handles, including namespace and token handles.
    HANDLE inherited_stdio[] = { startup.StartupInfo.hStdInput, startup.StartupInfo.hStdOutput, startup.StartupInfo.hStdError };
    if (!UpdateProcThreadAttribute(attributes, 0, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
        inherited_stdio, sizeof(inherited_stdio), nullptr, nullptr)) {
        const DWORD code = GetLastError(); DeleteProcThreadAttributeList(attributes); SetLastError(code);
        require(FALSE, "native source stdio allowlist");
    }
    if (task_acl) additions[L"QQ_NATIVE_TASK_ACL"] = L"1";
    if (normal_control) additions[L"QQ_NATIVE_EXPECT_NORMAL"] = L"1";
    auto source_environment = ::environment(additions);
    std::wstring initial_directory(workspace);
    if (task_acl) {
        const std::wstring source_image(image);
        initial_directory = source_image.substr(0, source_image.find_last_of(L"\\"));
    }
    const DWORD flags = CREATE_SUSPENDED | CREATE_UNICODE_ENVIRONMENT | EXTENDED_STARTUPINFO_PRESENT;
    const BOOL launched = restrict_host
        ? CreateProcessAsUserW(restricted_host.value, image, mutable_command.data(), nullptr, nullptr, TRUE,
            flags, source_environment.data(), initial_directory.c_str(), &startup.StartupInfo, &source)
        : CreateProcessW(image, mutable_command.data(), nullptr, nullptr, TRUE,
            flags, source_environment.data(), initial_directory.c_str(), &startup.StartupInfo, &source);
    const DWORD launch_error = launched ? 0 : GetLastError();
    if (child_input.value) { CloseHandle(child_input.value); child_input.value = nullptr; }
    if (child_output.value) { CloseHandle(child_output.value); child_output.value = nullptr; }
    DeleteProcThreadAttributeList(attributes);
    SetLastError(launch_error);
    require(launched, "suspended native policy source");
    Handle process, thread, token;
    process.value = source.hProcess;
    thread.value = source.hThread;
    require(OpenProcessToken(process.value, TOKEN_QUERY, &token.value), "native policy source token");
    if (!normal_control) contained(token.value);
    if (restrict_host) {
        const auto retained = information(token.value, TokenRestrictedSids);
        const auto* groups = reinterpret_cast<const TOKEN_GROUPS*>(retained.data());
        const auto* original = reinterpret_cast<const TOKEN_GROUPS*>(host_restrictions.data());
        if (groups->GroupCount != 1 || !EqualSid(groups->Groups[0].Sid, original->Groups[0].Sid)) {
            throw std::runtime_error("native creation changed the exact write restriction");
        }
    }
    const auto package = information(token.value, normal_control ? TokenUser : TokenAppContainerSid);
    const auto sid = normal_control
        ? reinterpret_cast<const TOKEN_USER*>(package.data())->User.Sid
        : reinterpret_cast<const TOKEN_APPCONTAINER_INFORMATION*>(package.data())->TokenAppContainer;
    grant_private_desktop(sid);
    if (task_acl) grant_task_directory(sid, workspace, low_label);
    PrivateDeviceMap device_map;
    if (self_control) {
        // The disposable trusted controller changes only its own per-process
        // map, never a shared logon map. No untrusted command runs here.
        require(TerminateProcess(process.value, 0), "unused normal source stop");
        if (WaitForSingleObject(process.value, INFINITE) != WAIT_OBJECT_0) throw std::runtime_error("unused normal source wait failed");
        require(SetEnvironmentVariableW(L"QQ_NATIVE_EXPECT_NORMAL", L"1"), "normal inspector identity");
        install_device_map(GetCurrentProcess(), sid, device_map);
        return inspect_files();
    }
    if (projected && !after_ready) install_device_map(process.value, sid, device_map);
    LPWSTR package_text = nullptr;
    require(ConvertSidToStringSidW(sid, &package_text), "native policy package SID");
    auto local_free = [](void* value) { LocalFree(value); };
    std::unique_ptr<void, decltype(local_free)> owned_package(package_text, local_free);
    if (control) {
        if (ResumeThread(thread.value) == static_cast<DWORD>(-1)) throw std::runtime_error("native policy control resume failed");
        if (after_ready) {
            char frame[6]{};
            DWORD count = 0;
            while (count < sizeof(frame)) {
                DWORD received = 0;
                require(ReadFile(output.value, frame + count, static_cast<DWORD>(sizeof(frame)) - count, &received, nullptr), "initialized source ready pipe");
                if (!received) throw std::runtime_error("source exited before view readiness");
                count += received;
            }
            if (std::memcmp(frame, "READY\n", sizeof(frame))) throw std::runtime_error("unexpected source ready frame");
            install_device_map(process.value, sid, device_map);
            const BYTE release = 1;
            DWORD written = 0;
            require(WriteFile(input.value, &release, 1, &written, nullptr), "release initialized view source");
            if (written != 1) throw std::runtime_error("short source release frame");
            // Drain while the fixed inspector runs, so pipe capacity never
            // turns completion into a deadlock. Outer job ownership bounds
            // failed initialization and closes these pipes on controller exit.
            char buffer[4096];
            for (;;) {
                DWORD received = 0;
                if (!ReadFile(output.value, buffer, sizeof(buffer), &received, nullptr)) {
                    if (GetLastError() == ERROR_BROKEN_PIPE) break;
                    require(FALSE, "initialized source output");
                }
                if (!received) break;
                std::cout.write(buffer, received);
                if (!std::cout) throw std::runtime_error("source output forwarding failed");
            }
        }
        if (WaitForSingleObject(process.value, INFINITE) != WAIT_OBJECT_0) throw std::runtime_error("native policy control wait failed");
        DWORD result = 0;
        require(GetExitCodeProcess(process.value, &result), "native policy control exit");
        return static_cast<int>(result);
    }
    const auto pid = std::to_wstring(source.dwProcessId);
    const int result = host_recreate(pid.c_str(), package_text, environment.value, inherit_parent);
    require(TerminateProcess(process.value, 0), "suspended source termination");
    if (WaitForSingleObject(process.value, INFINITE) != WAIT_OBJECT_0) throw std::runtime_error("suspended source wait failed");
    return result;
}

int wmain(int argc, wchar_t** argv) {
    try {
        // Fixed inspector only. This normal-token control never launches the
        // untrusted payload or counts as a contained execution result.
        if (argc == 3 && std::wstring(argv[1]) == L"host-normal-projected-self") return host_create_policy(argv[2], false, true, false, false, true, false, false, false, true, true);
        if (argc == 3 && std::wstring(argv[1]) == L"host-normal-projected") return host_create_policy(argv[2], false, true, false, false, true, false, false, false, true);
        if (argc == 3 && std::wstring(argv[1]) == L"host-normal-projected-after-ready") return host_create_policy(argv[2], false, true, false, false, true, true, false, false, true);
        if (argc == 3 && std::wstring(argv[1]) == L"host-policy-task-acl") return host_create_policy(argv[2], false, true, true, false, false, false, true);
        if (argc == 3 && std::wstring(argv[1]) == L"host-policy-task-acl-low") return host_create_policy(argv[2], false, true, true, false, false, false, true, true);
        if (argc == 3 && std::wstring(argv[1]) == L"host-policy-projected-after-ready") return host_create_policy(argv[2], false, true, false, false, true, true);
        if (argc == 3 && std::wstring(argv[1]) == L"host-policy-projected") return host_create_policy(argv[2], false, true, false, false, true);
        if (argc == 3 && std::wstring(argv[1]) == L"volume-name") {
            wchar_t volume[128]{}, target[32768]{};
            require(GetVolumeNameForVolumeMountPointW(argv[2], volume, 128), "volume metadata");
            std::wstring name(volume);
            if (!name.starts_with(L"\\\\?\\Volume{") || !name.ends_with(L"\\")) throw std::runtime_error("invalid volume metadata");
            Handle root;
            root.value = CreateFileW(argv[2], 0, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                nullptr, OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS, nullptr);
            require(root.value != INVALID_HANDLE_VALUE, "volume root metadata handle");
            const DWORD size = GetFinalPathNameByHandleW(root.value, target, 32768, FILE_NAME_NORMALIZED | VOLUME_NAME_NT);
            if (!size || size >= 32768) throw std::runtime_error("volume NT path unavailable");
            std::wstring device(target);
            if (device.ends_with(L"\\")) device.pop_back();
            std::wcout << volume << std::endl << device << std::endl;
            return 0;
        }
        if (argc == 3 && std::wstring(argv[1]) == L"host-policy-write-restricted") return host_create_policy(argv[2], false, true, true);
        if (argc == 3 && std::wstring(argv[1]) == L"host-policy-write-restricted-lpac") return host_create_policy(argv[2], false, true, true, true);
        if (argc == 3 && std::wstring(argv[1]) == L"host-policy-control") return host_create_policy(argv[2], false, true);
        if (argc == 3 && std::wstring(argv[1]) == L"host-create-policy-parent") return host_create_policy(argv[2], true);
        if (argc == 3 && std::wstring(argv[1]) == L"host-create-policy") return host_create_policy(argv[2]);
        if (argc == 3 && std::wstring(argv[1]) == L"host-recreate") return host_recreate(argv[2]);
        if (argc == 2 && std::wstring(argv[1]) == L"hold") return hold_environment();
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
