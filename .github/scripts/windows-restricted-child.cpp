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
    std::vector<std::wstring> paths{workspace, workspace + L"\\payload.cjs", outside, L"C:\\"};
    for (const auto variable : {L"QQ_NATIVE_PUBLIC_WRITABLE_GUID", L"QQ_NATIVE_PUBLIC_WRITABLE_GLOBALROOT", L"QQ_NATIVE_SOFTWARE_FILE"}) {
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
    link(objects.drive, L"C:", target);
    link(objects.global_root, L"GLOBALROOT", L"\\");
    link(objects.null_device, L"NUL", L"\\Device\\Null");
    wchar_t volume_guid[128]{}, volume_target[32768]{};
    require(GetVolumeNameForVolumeMountPointW(L"C:\\", volume_guid, 128), "projection volume GUID");
    std::wstring volume_name(volume_guid);
    if (!volume_name.starts_with(L"\\\\?\\Volume{") || !volume_name.ends_with(L"\\")) throw std::runtime_error("invalid projection volume GUID");
    volume_name = volume_name.substr(4, volume_name.size() - 5);
    require(QueryDosDeviceW(volume_name.c_str(), volume_target, 32768) != 0, "projection volume device");
    link(objects.volume, volume_name, volume_target);
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
    std::cerr << "private device map installed; ABI bytes=" << used_size << "; host drive map unchanged" << std::endl;
}

static int host_create_policy(const wchar_t* policy_path, bool inherit_parent = false, bool control = false, bool restrict_host = false, bool lpac = false, bool projected = false) {
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
    const DWORD attribute_count = lpac ? 2 : 1;
    InitializeProcThreadAttributeList(nullptr, attribute_count, 0, &size);
    if (!size || size > 65536) throw std::runtime_error("invalid policy attribute size");
    std::vector<BYTE> storage(size);
    auto attributes = reinterpret_cast<LPPROC_THREAD_ATTRIBUTE_LIST>(storage.data());
    require(InitializeProcThreadAttributeList(attributes, attribute_count, 0, &size), "policy attributes");
    if (!UpdateProcThreadAttribute(attributes, 0, 35 | 0x00020000, &environment.value, sizeof(environment.value), nullptr, nullptr)) {
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
    LPWCH inherited_environment = GetEnvironmentStringsW();
    require(inherited_environment != nullptr, "native policy environment");
    const DWORD flags = CREATE_SUSPENDED | CREATE_UNICODE_ENVIRONMENT | EXTENDED_STARTUPINFO_PRESENT;
    const BOOL launched = restrict_host
        ? CreateProcessAsUserW(restricted_host.value, image, mutable_command.data(), nullptr, nullptr, TRUE,
            flags, inherited_environment, workspace, &startup.StartupInfo, &source)
        : CreateProcessW(image, mutable_command.data(), nullptr, nullptr, TRUE,
            flags, inherited_environment, workspace, &startup.StartupInfo, &source);
    const DWORD launch_error = launched ? 0 : GetLastError();
    FreeEnvironmentStringsW(inherited_environment);
    DeleteProcThreadAttributeList(attributes);
    SetLastError(launch_error);
    require(launched, "suspended native policy source");
    Handle process, thread, token;
    process.value = source.hProcess;
    thread.value = source.hThread;
    require(OpenProcessToken(process.value, TOKEN_QUERY, &token.value), "native policy source token");
    contained(token.value);
    if (restrict_host) {
        const auto retained = information(token.value, TokenRestrictedSids);
        const auto* groups = reinterpret_cast<const TOKEN_GROUPS*>(retained.data());
        const auto* original = reinterpret_cast<const TOKEN_GROUPS*>(host_restrictions.data());
        if (groups->GroupCount != 1 || !EqualSid(groups->Groups[0].Sid, original->Groups[0].Sid)) {
            throw std::runtime_error("native creation changed the exact write restriction");
        }
    }
    const auto package = information(token.value, TokenAppContainerSid);
    const auto sid = reinterpret_cast<const TOKEN_APPCONTAINER_INFORMATION*>(package.data())->TokenAppContainer;
    grant_private_desktop(sid);
    PrivateDeviceMap device_map;
    if (projected) install_device_map(process.value, sid, device_map);
    LPWSTR package_text = nullptr;
    require(ConvertSidToStringSidW(sid, &package_text), "native policy package SID");
    auto local_free = [](void* value) { LocalFree(value); };
    std::unique_ptr<void, decltype(local_free)> owned_package(package_text, local_free);
    if (control) {
        if (ResumeThread(thread.value) == static_cast<DWORD>(-1)) throw std::runtime_error("native policy control resume failed");
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
        if (argc == 3 && std::wstring(argv[1]) == L"host-policy-projected") return host_create_policy(argv[2], false, true, false, false, true);
        if (argc == 3 && std::wstring(argv[1]) == L"volume-name") {
            wchar_t volume[128]{}, target[32768]{};
            require(GetVolumeNameForVolumeMountPointW(argv[2], volume, 128), "volume metadata");
            std::wstring name(volume);
            if (!name.starts_with(L"\\\\?\\Volume{") || !name.ends_with(L"\\")) throw std::runtime_error("invalid volume metadata");
            name = name.substr(4, name.size() - 5);
            require(QueryDosDeviceW(name.c_str(), target, 32768) != 0, "volume device metadata");
            std::wcout << volume << std::endl << target << std::endl;
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
