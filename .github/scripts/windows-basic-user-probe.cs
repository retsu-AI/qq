// Temporary CI launcher: run the native-API fixture with administrator groups
// and privileges removed. It changes no accounts, policies or device ACLs.
using System;
using System.ComponentModel;
using System.IO;
using System.Runtime.InteropServices;
using System.Text;
using Microsoft.Win32.SafeHandles;

public static class BasicUserProbe {
  [StructLayout(LayoutKind.Sequential, CharSet=CharSet.Unicode)]
  struct StartupInfo {
    public uint cb;
    public string reserved, desktop, title;
    public uint x, y, width, height, charsX, charsY, fill, flags;
    public ushort show, reservedBytes;
    public IntPtr reservedData, input, output, error;
  }
  [StructLayout(LayoutKind.Sequential)]
  struct ProcessInfo {
    public IntPtr process, thread;
    public uint processId, threadId;
  }
  [DllImport("advapi32.dll", SetLastError=true)]
  static extern bool SaferCreateLevel(uint scope, uint level, uint flags, out IntPtr result, IntPtr reserved);
  [DllImport("advapi32.dll", SetLastError=true)]
  static extern bool SaferComputeTokenFromLevel(IntPtr level, IntPtr input, out SafeAccessTokenHandle token, uint flags, IntPtr reserved);
  [DllImport("advapi32.dll")]
  static extern bool SaferCloseLevel(IntPtr level);
  [DllImport("advapi32.dll", CharSet=CharSet.Unicode, SetLastError=true)]
  static extern bool CreateProcessAsUser(SafeAccessTokenHandle token, string application, StringBuilder command, IntPtr processSecurity, IntPtr threadSecurity, bool inherit, uint flags, IntPtr environment, string directory, ref StartupInfo startup, out ProcessInfo process);
  [DllImport("kernel32.dll", CharSet=CharSet.Unicode, SetLastError=true)]
  static extern SafeFileHandle CreateFile(string name, uint access, uint share, IntPtr security, uint disposition, uint flags, IntPtr template);
  [DllImport("kernel32.dll", SetLastError=true)]
  static extern bool SetHandleInformation(SafeFileHandle handle, uint mask, uint flags);
  [DllImport("kernel32.dll")]
  static extern uint WaitForSingleObject(IntPtr process, uint timeout);
  [DllImport("kernel32.dll", SetLastError=true)]
  static extern bool GetExitCodeProcess(IntPtr process, out uint code);
  [DllImport("kernel32.dll")]
  static extern bool CloseHandle(IntPtr handle);
  [DllImport("kernel32.dll")]
  static extern bool TerminateProcess(IntPtr process, uint code);

  static Win32Exception Error() { return new Win32Exception(Marshal.GetLastWin32Error()); }
  public static int Run(string node, string script, string log) {
    IntPtr level;
    // SAFER_LEVELID_NORMALUSER, not a low-integrity or AppContainer token.
    if (!SaferCreateLevel(2, 0x20000, 1, out level, IntPtr.Zero)) throw Error();
    try {
      SafeAccessTokenHandle token;
      if (!SaferComputeTokenFromLevel(level, IntPtr.Zero, out token, 0, IntPtr.Zero)) throw Error();
      using (token)
      using (var output = new FileStream(log, FileMode.Create, FileAccess.Write, FileShare.ReadWrite | FileShare.Inheritable))
      using (var input = CreateFile(@"\\.\NUL", 0x80000000, 3, IntPtr.Zero, 3, 0, IntPtr.Zero)) {
        if (input.IsInvalid) throw Error();
        if (!SetHandleInformation(input, 1, 1) || !SetHandleInformation(output.SafeFileHandle, 1, 1)) throw Error();
        var startup = new StartupInfo {
          cb = (uint)Marshal.SizeOf<StartupInfo>(), flags = 0x100,
          input = input.DangerousGetHandle(), output = output.SafeFileHandle.DangerousGetHandle(), error = output.SafeFileHandle.DangerousGetHandle()
        };
        var command = new StringBuilder("\"" + node + "\" \"" + script + "\"");
        ProcessInfo child;
        if (!CreateProcessAsUser(token, node, command, IntPtr.Zero, IntPtr.Zero, true, 0x08000000, IntPtr.Zero, Path.GetDirectoryName(script), ref startup, out child)) throw Error();
        try {
          uint wait = WaitForSingleObject(child.process, 120000);
          if (wait != 0) {
            TerminateProcess(child.process, 124);
            throw new InvalidOperationException("Basic-user fixture did not finish: wait=" + wait);
          }
          uint code;
          if (!GetExitCodeProcess(child.process, out code)) throw Error();
          return unchecked((int)code);
        } finally {
          CloseHandle(child.thread);
          CloseHandle(child.process);
        }
      }
    } finally { SaferCloseLevel(level); }
  }
}
