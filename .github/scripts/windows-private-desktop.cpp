// Diagnostic host controller only; never used as a shipped execution backend.
#define UNICODE
#define _UNICODE
#include <windows.h>
#include <sddl.h>
#include <objbase.h>
#include <iostream>
#include <memory>
#include <stdexcept>
#include <string>
#include <utility>
#include <vector>
#ifdef _MSC_VER
#pragma comment(lib, "advapi32.lib")
#pragma comment(lib, "user32.lib")
#pragma comment(lib, "ole32.lib")
#endif

struct Handle {
    HANDLE value = nullptr;
    Handle() = default;
    Handle(const Handle&) = delete;
    Handle& operator=(const Handle&) = delete;
    ~Handle() { if (value && value != INVALID_HANDLE_VALUE) CloseHandle(value); }
};
static void require(BOOL ok, const char* phase) {
    if (!ok) throw std::runtime_error(std::string(phase) + ": " + std::to_string(GetLastError()));
}
struct PrivateDesktop {
    HDESK original = GetThreadDesktop(GetCurrentThreadId()); // borrowed
    HDESK owned = nullptr;
    ~PrivateDesktop() {
        if (original) SetThreadDesktop(original);
        if (owned) CloseDesktop(owned);
    }
};
struct JobAttribute {
    std::vector<BYTE> storage;
    LPPROC_THREAD_ATTRIBUTE_LIST list = nullptr;
    ~JobAttribute() { if (list) DeleteProcThreadAttributeList(list); }
    void initialize(HANDLE* job) {
        SIZE_T size = 0;
        InitializeProcThreadAttributeList(nullptr, 1, 0, &size);
        if (!size || size > 65536) throw std::runtime_error("invalid job attribute size");
        storage.resize(size);
        auto candidate = reinterpret_cast<LPPROC_THREAD_ATTRIBUTE_LIST>(storage.data());
        require(InitializeProcThreadAttributeList(candidate, 1, 0, &size), "initialize job attribute");
        list = candidate;
        require(UpdateProcThreadAttribute(list, 0, PROC_THREAD_ATTRIBUTE_JOB_LIST,
            job, sizeof(HANDLE), nullptr, nullptr), "atomic controller job");
    }
};
static std::wstring quote(const wchar_t* value) {
    const std::wstring text(value);
    if (text.find_first_of(L"\"\r\n") != std::wstring::npos || text.ends_with(L"\\")) {
        throw std::runtime_error("invalid fixture argument");
    }
    return L"\"" + text + L"\"";
}
int wmain(int argc, wchar_t** argv) {
    try {
        if (argc != 5) throw std::runtime_error("expected node, host script, request file and task SID");
        Handle token;
        require(OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &token.value), "host token");
        DWORD contained = 1, count = 0;
        require(GetTokenInformation(token.value, TokenIsAppContainer, &contained, sizeof(contained), &count), "host containment");
        if (contained) throw std::runtime_error("desktop controller requires the normal host token");
        GetTokenInformation(token.value, TokenUser, nullptr, 0, &count);
        if (!count || count > 65536) throw std::runtime_error("invalid user token size");
        std::vector<BYTE> user(count);
        require(GetTokenInformation(token.value, TokenUser, user.data(), count, &count), "host user");
        LPWSTR encoded = nullptr;
        require(ConvertSidToStringSidW(reinterpret_cast<TOKEN_USER*>(user.data())->User.Sid, &encoded), "host user SID");
        auto local_free = [](void* value) { LocalFree(value); };
        std::unique_ptr<void, decltype(local_free)> user_text(encoded, local_free);
        PSID task = nullptr;
        require(ConvertStringSidToSidW(argv[4], &task), "task SID");
        std::unique_ptr<void, decltype(local_free)> task_sid(task, local_free);
        // Only a fresh desktop gets this descriptor. No existing UI or file
        // object's ACL is changed. Low integrity permits the AppContainer UI.
        std::wstring package_grant;
        wchar_t package[256]{};
        const DWORD package_length = GetEnvironmentVariableW(L"QQ_NATIVE_PACKAGE_SID", package, 256);
        if (package_length) {
            if (package_length >= 256) throw std::runtime_error("invalid desktop package SID size");
            PSID package_sid = nullptr;
            require(ConvertStringSidToSidW(package, &package_sid), "desktop package SID");
            std::unique_ptr<void, decltype(local_free)> owned_package(package_sid, local_free);
            if (!IsValidSid(package_sid)) throw std::runtime_error("invalid desktop package SID");
            package_grant = L"(A;;GA;;;" + std::wstring(package) + L")";
        }
        const std::wstring descriptor = L"D:P" + package_grant + L"(A;;GA;;;" + std::wstring(encoded) +
            L")(A;;GA;;;SY)(A;;GA;;;" + std::wstring(argv[4]) +
            L")(A;;GA;;;S-1-15-3-1024-530591407-3118584226-222893001-1659895842-537460181-133087082-1363364250-4100990745)S:(ML;;NW;;;LW)";
        PSECURITY_DESCRIPTOR security = nullptr;
        require(ConvertStringSecurityDescriptorToSecurityDescriptorW(descriptor.c_str(), SDDL_REVISION_1, &security, nullptr), "desktop descriptor");
        std::unique_ptr<void, decltype(local_free)> owned_security(security, local_free);
        SECURITY_ATTRIBUTES attributes{ static_cast<DWORD>(sizeof(SECURITY_ATTRIBUTES)), security, FALSE };
        GUID guid{};
        if (FAILED(CoCreateGuid(&guid))) throw std::runtime_error("desktop GUID failed");
        wchar_t guid_text[40]{};
        if (!StringFromGUID2(guid, guid_text, 40)) throw std::runtime_error("desktop GUID encoding failed");
        const std::wstring name = L"QQProbe-" + std::wstring(guid_text);
        PrivateDesktop desktop;
        if (!desktop.original) throw std::runtime_error("original desktop is unavailable");
        SetLastError(ERROR_SUCCESS);
        const ACCESS_MASK desktop_access = STANDARD_RIGHTS_REQUIRED | DESKTOP_CREATEWINDOW | DESKTOP_CREATEMENU |
            DESKTOP_ENUMERATE | DESKTOP_HOOKCONTROL | DESKTOP_JOURNALPLAYBACK | DESKTOP_JOURNALRECORD |
            DESKTOP_READOBJECTS | DESKTOP_SWITCHDESKTOP | DESKTOP_WRITEOBJECTS;
        desktop.owned = CreateDesktopExW(name.c_str(), nullptr, nullptr, 0, desktop_access, &attributes, 128, nullptr);
        const DWORD created_error = GetLastError();
        require(desktop.owned != nullptr, "CreateDesktopExW");
        if (created_error == ERROR_ALREADY_EXISTS) throw std::runtime_error("desktop name was already present");
        DWORD heap_kib = 0, heap_size = 0;
        require(GetUserObjectInformationW(desktop.owned, UOI_HEAPSIZE, &heap_kib,
            static_cast<DWORD>(sizeof(heap_kib)), &heap_size), "private desktop heap");
        std::wcerr << L"private desktop created; requested heap=128KiB; actual heap=" << heap_kib
            << L"KiB; existing desktop ACL unchanged" << std::endl;
        DWORD station_size = 0;
        const auto station = GetProcessWindowStation();
        if (!station) throw std::runtime_error("current station is unavailable");
        GetUserObjectInformationW(station, UOI_NAME, nullptr, 0, &station_size);
        if (!station_size || station_size > 65536) throw std::runtime_error("invalid station name size");
        std::vector<wchar_t> station_name(station_size / sizeof(wchar_t) + 1);
        require(GetUserObjectInformationW(station, UOI_NAME, station_name.data(), station_size, &station_size), "station name");
        auto startup_desktop = std::wstring(station_name.data()) + L"\\" + name;

        // Kernel ownership of every descendant also covers controller failure.
        Handle job;
        job.value = CreateJobObjectW(nullptr, nullptr);
        require(job.value != nullptr, "CreateJobObjectW");
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION limits{};
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        require(SetInformationJobObject(job.value, JobObjectExtendedLimitInformation, &limits, static_cast<DWORD>(sizeof(limits))), "job limits");
        const auto command = quote(argv[1]) + L" " + quote(argv[2]) + L" " + quote(argv[3]);
        std::vector<wchar_t> mutable_command(command.begin(), command.end());
        mutable_command.push_back(0);
        Handle input, output, error;
        for (const auto& pair : std::vector<std::pair<DWORD, HANDLE*>>{
            { STD_INPUT_HANDLE, &input.value }, { STD_OUTPUT_HANDLE, &output.value }, { STD_ERROR_HANDLE, &error.value } }) {
            require(DuplicateHandle(GetCurrentProcess(), GetStdHandle(pair.first), GetCurrentProcess(), pair.second, 0, TRUE, DUPLICATE_SAME_ACCESS), "host stdio");
        }
        JobAttribute attributes_list;
        attributes_list.initialize(&job.value);
        STARTUPINFOEXW startup{};
        startup.StartupInfo.cb = static_cast<DWORD>(sizeof(startup));
        startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        startup.StartupInfo.hStdInput = input.value;
        startup.StartupInfo.hStdOutput = output.value;
        startup.StartupInfo.hStdError = error.value;
        startup.StartupInfo.lpDesktop = startup_desktop.data();
        startup.lpAttributeList = attributes_list.list;
        PROCESS_INFORMATION process{};
        require(CreateProcessW(argv[1], mutable_command.data(), nullptr, nullptr, TRUE,
            CREATE_SUSPENDED | CREATE_NO_WINDOW | EXTENDED_STARTUPINFO_PRESENT,
            nullptr, nullptr, &startup.StartupInfo, &process), "private desktop host");
        Handle child, thread;
        child.value = process.hProcess;
        thread.value = process.hThread;
        if (ResumeThread(thread.value) == static_cast<DWORD>(-1)) throw std::runtime_error("host resume failed");
        if (WaitForSingleObject(child.value, INFINITE) != WAIT_OBJECT_0) throw std::runtime_error("host wait failed");
        DWORD result = 0;
        require(GetExitCodeProcess(child.value, &result), "host exit");
        return static_cast<int>(result);
    } catch (const std::exception& error) {
        std::cerr << "private desktop probe: " << error.what() << std::endl;
        return 125;
    }
}
