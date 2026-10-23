#!/usr/bin/env python3
"""Native prerequisite investigation, not a production containment backend.

Only controlled fixture processes are signalled, using authenticated identity
snapshots and the kernel's audit-token API. No bare-PID cleanup is permitted.
"""

import ctypes as C
import errno
import json
import os
from pathlib import Path
import plistlib
import secrets
import select
import selectors
import shlex
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import time


class CoalitionInfo(C.Structure):
    _fields_ = [("ids", C.c_uint64 * 2), ("reserved", C.c_uint64 * 3)]


class AuditToken(C.Structure):
    _fields_ = [("val", C.c_uint32 * 8)]


def layouts():
    assert C.sizeof(CoalitionInfo) == 40
    assert C.sizeof(AuditToken) == 32
    assert struct.calcsize("P") == 8


def seatbelt_nesting():
    """Check a prerequisite for a protected, sandbox-local supervisor."""
    assert sys.platform == "darwin"
    with tempfile.TemporaryDirectory(prefix="qq-seatbelt-nesting-") as scratch:
        outer = Path(scratch) / "outer.sb"
        inner = Path(scratch) / "inner.sb"
        outer.write_text("(version 1)\n(allow default)\n")
        # Distinct policies ensure the nested control actually applies another
        # policy, rather than reusing an identical profile.
        inner.write_text(
            '(version 1)\n(allow default)\n'
            f'(deny file-read-data (literal "/qq-unused-{secrets.token_hex(16)}"))\n'
        )
        sandbox = "/usr/bin/sandbox-exec"
        cases = {
            "outer": [sandbox, "-f", str(outer), "/usr/bin/true"],
            "inner": [sandbox, "-f", str(inner), "/usr/bin/true"],
            "nested": [sandbox, "-f", str(outer), sandbox, "-f", str(inner), "/usr/bin/true"],
        }
        outcomes = {}
        for name, command in cases.items():
            result = subprocess.run(command, capture_output=True, timeout=10, check=False)
            assert len(result.stdout) + len(result.stderr) <= 65536
            outcomes[name] = {"exit": result.returncode, "stderr": result.stderr.decode(errors="replace")}
        print(json.dumps({"seatbelt_nesting": outcomes, "not_a_containment_qualification": True}), flush=True)
        assert outcomes["outer"]["exit"] == outcomes["inner"]["exit"] == 0, "standalone profile control failed"
        assert outcomes["nested"]["exit"] == 0, "a second Seatbelt policy cannot be applied"


class Kernel:
    def __init__(self):
        layouts()
        self.lib = C.CDLL("/usr/lib/libproc.dylib", use_errno=True)
        self.lib.proc_pidinfo.argtypes = [C.c_int, C.c_int, C.c_uint64, C.c_void_p, C.c_int]
        self.lib.proc_pidinfo.restype = C.c_int
        self.lib.proc_listpids.argtypes = [C.c_uint32, C.c_uint32, C.c_void_p, C.c_int]
        self.lib.proc_listpids.restype = C.c_int
        self.signal = self.lib.proc_signal_with_audittoken
        self.signal.argtypes = [C.POINTER(AuditToken), C.c_int]
        self.signal.restype = C.c_int

    def info(self, pid, flavor, result):
        C.set_errno(0)
        count = self.lib.proc_pidinfo(pid, flavor, 0, C.byref(result), C.sizeof(result))
        if count != C.sizeof(result):
            raise OSError(C.get_errno(), f"proc_pidinfo flavor={flavor} pid={pid} size={count}")
        return result

    def identity(self, pid):
        # Apple proc_bsdinfowithuniqid: BSDInfo136 + UniqueInfo56 (LP64).
        raw = bytes(self.info(pid, 18, C.create_string_buffer(192)))
        result = {
            "pid": struct.unpack_from("=I", raw, 12)[0],
            "uid": struct.unpack_from("=I", raw, 20)[0],
            "ruid": struct.unpack_from("=I", raw, 28)[0],
            "status": struct.unpack_from("=I", raw, 4)[0],
            "group": struct.unpack_from("=I", raw, 100)[0],
            "unique": struct.unpack_from("=Q", raw, 152)[0],
            "version": struct.unpack_from("=I", raw, 168)[0],
        }
        assert result["pid"] == pid and result["unique"] != 0
        coalition = self.info(pid, 20, CoalitionInfo())
        after = bytes(self.info(pid, 18, C.create_string_buffer(192)))
        assert struct.unpack_from("=Q", after, 152)[0] == result["unique"], "identity changed during query"
        result["coalitions"] = list(coalition.ids)
        return result

    def send(self, tracked, sig, wrong=False):
        current = self.identity(tracked["pid"])
        assert current["uid"] == os.getuid() and current["unique"] == tracked["unique"]
        token = AuditToken()
        token.val[5] = current["pid"]
        token.val[7] = current["version"] ^ int(wrong)
        return self.signal(C.byref(token), sig)

    def members(self, coalition):
        capacity = 16384
        pids = (C.c_uint32 * capacity)()
        C.set_errno(0)
        count = self.lib.proc_listpids(1, 0, pids, C.sizeof(pids))
        assert 0 < count < C.sizeof(pids) and count % 4 == 0, "incomplete host process list"
        found = []
        for pid in pids[: count // 4]:
            if pid == 0:
                continue
            try:
                candidate = self.info(pid, 20, CoalitionInfo())
                if candidate.ids[0] != coalition:
                    continue
                item = self.identity(pid)
                assert item["coalitions"][0] == coalition, "membership changed during query"
            except OSError as error:
                if error.errno in (errno.ESRCH, errno.ENOENT):
                    continue
                raise
            if item["coalitions"][0] == coalition:
                found.append(item)
        return found


def unprivileged_coalition():
    lib = C.CDLL("/usr/lib/libSystem.B.dylib", use_errno=True)
    lib.coalition_terminate.argtypes = [C.c_uint64, C.c_uint32]
    lib.coalition_terminate.restype = C.c_int
    # XNU checks resource-coalition privilege before looking up ID0, which
    # is reserved. EPERM proves the membership is unprivileged; ESRCH would
    # show the caller passed that gate. No object is allocated or signalled.
    C.set_errno(0)
    result = lib.coalition_terminate(0, 0)
    assert result == -1 and C.get_errno() == errno.EPERM, "coalition privilege challenge inconclusive"


def send_json(connection, value):
    encoded = json.dumps(value, sort_keys=True).encode() + b"\n"
    assert len(encoded) < 4096
    connection.sendall(encoded)


def read_json(reader):
    line = reader.readline(4096)
    assert line.endswith(b"\n") and len(line) < 4096, "invalid fixture frame"
    return json.loads(line)


class ExitWatch:
    def __init__(self):
        self.queue = select.kqueue()
        self.pending = set()

    def register(self, kernel, tracked):
        pid = tracked["pid"]
        event = select.kevent(pid, filter=select.KQ_FILTER_PROC,
                              flags=select.KQ_EV_ADD | select.KQ_EV_ENABLE | select.KQ_EV_ONESHOT,
                              fflags=select.KQ_NOTE_EXIT)
        self.queue.control([event], 0, 0)
        # A replacement between registration and this check invalidates the probe.
        assert kernel.identity(pid)["unique"] == tracked["unique"]
        self.pending.add(pid)

    def drain(self, timeout):
        deadline = time.monotonic() + timeout
        while self.pending:
            remaining = deadline - time.monotonic()
            assert remaining > 0, "fixture exit was not confirmed"
            events = self.queue.control([], len(self.pending), remaining)
            for event in events:
                assert not event.flags & select.KQ_EV_ERROR, "fixture exit observation failed"
                assert event.fflags & select.KQ_NOTE_EXIT
                self.pending.discard(event.ident)


def child(address, nonce, role, root_policy=None):
    # Includes failures before the host authenticates this process. No fixture
    # can wait indefinitely in a protocol read or its own child's waitpid.
    signal.alarm(60)
    connection = socket.socket(socket.AF_UNIX)
    connection.settimeout(30)
    connection.connect(address)
    children = []
    try:
        kernel = Kernel()
        unprivileged_coalition()
        send_json(connection, {"nonce": nonce, "role": role, "identity": kernel.identity(os.getpid())})
        if root_policy is not None:
            assert role == "guardian"
            arguments = ["/usr/bin/sandbox-exec", "-p", root_policy,
                         sys.executable, "-I", str(Path(__file__).resolve()),
                         "child", address, nonce, "root"]
            children.append(os.posix_spawn(arguments[0], arguments, dict(os.environ)))
        reader = connection.makefile("rb")
        while True:
            command = reader.readline(32)
            if command == b"ping\n":
                connection.sendall(b"alive\n")
            elif command.startswith(b"signal "):
                target = int(command.split()[1])
                try:
                    os.kill(target, signal.SIGCONT)
                    result = 0
                except OSError as error:
                    result = error.errno
                send_json(connection, {"signal_errno": result})
            elif command == b"broker\n":
                label = f"ai.retsu.qq.custody-probe.{nonce}.broker"
                marker = str(Path(address).parent / "broker-output")
                code, output = launch(["/bin/launchctl", "submit", "-l", label,
                                       "--", "/usr/bin/touch", marker], nonce)
                send_json(connection, {"broker_exit": code, "output": output[:1024]})
            elif command == b"spawn\n":
                arguments = [sys.executable, "-I", str(Path(__file__).resolve()), "child", address, nonce, "escaped"]
                pid = os.posix_spawn(sys.executable, arguments, {}, setpgroup=0)
                children.append(pid)
                send_json(connection, {"spawned": pid})
            elif command in (b"quit\n", b""):
                break
            else:
                raise RuntimeError("invalid fixture command")
    except Exception as error:
        try:
            send_json(connection, {"nonce": nonce, "role": role, "error": f"{type(error).__name__}: {error}"})
        except OSError:
            pass
        raise
    finally:
        connection.close()
        for pid in children:
            try:
                os.waitpid(pid, 0)
            except ChildProcessError:
                pass


def launch(command, nonce=""):
    # Drain a strict byte bound rather than retaining arbitrary tool output.
    deadline = time.monotonic() + 20
    output = bytearray()
    process = subprocess.Popen(command, stdin=subprocess.DEVNULL,
                               stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    try:
        with selectors.DefaultSelector() as ready:
            ready.register(process.stdout, selectors.EVENT_READ)
            while True:
                remaining = deadline - time.monotonic()
                assert remaining > 0, "launchd command timed out"
                assert ready.select(remaining), "launchd output timed out"
                chunk = os.read(process.stdout.fileno(), 4096)
                if not chunk:
                    break
                output.extend(chunk)
                assert len(output) <= 65536, "launchd output exceeded its bound"
        code = process.wait(timeout=max(0, deadline - time.monotonic()))
        return code, output.decode(errors="replace")
    except Exception as error:
        diagnostic = output[:4096].decode(errors="replace")
        if nonce:
            diagnostic = diagnostic.replace(nonce, "<fixture nonce>")
        raise RuntimeError(f"{error}: {diagnostic}") from error
    finally:
        try:
            if process.poll() is None:
                # This is our unreaped direct subprocess, never an enumerated PID.
                process.kill()
                process.wait(timeout=10)
        finally:
            process.stdout.close()


def host(use_launchservices=False, use_seatbelt=False, use_guardian=False):
    kernel = Kernel()
    assert os.geteuid() != 0
    controller = kernel.identity(os.getpid())
    fixtures = {}
    launched = []
    services = []
    broker_labels = []
    exits = ExitWatch()
    with tempfile.TemporaryDirectory(prefix="qq-coal-", dir="/tmp") as temporary:
        address = str(Path(temporary).resolve() / "control")
        nonce = secrets.token_hex(16)
        listener = socket.socket(socket.AF_UNIX)
        listener.bind(address)
        listener.listen(4)
        listener.settimeout(20)

        def accept(role):
            connection, _ = listener.accept()
            connection.settimeout(10)
            reader = connection.makefile("rb")
            try:
                report = read_json(reader)
                assert report["nonce"] == nonce and report["role"] == role
                if "error" in report:
                    raise RuntimeError(f"{role}: {report['error']}")
                tracked = report["identity"]
                actual = kernel.identity(tracked["pid"])
                assert actual["uid"] == os.getuid() and actual["unique"] == tracked["unique"]
                exits.register(kernel, tracked)
                fixtures[role] = (connection, reader, tracked)
                return tracked
            except BaseException:
                reader.close()
                connection.close()
                raise

        def ping(role):
            connection, reader, _ = fixtures[role]
            connection.sendall(b"ping\n")
            assert reader.readline(32) == b"alive\n"

        try:
            for role in ("root", "sentinel"):
                label = f"ai.retsu.qq.custody-probe.{nonce}.{role}"
                service = f"user/{os.getuid()}/{label}"
                configuration = Path(temporary) / f"{role}.plist"
                arguments = [sys.executable, "-I", str(Path(__file__).resolve()),
                             "child", address, nonce, role]
                if use_seatbelt and role == "root":
                    # Exercise the production process restrictions, while keeping
                    # fixture reads broad. This is not a filesystem qualification.
                    policy = "\n".join([
                        "(version 1)", "(deny default)",
                        "(allow file-read* process-exec process-fork sysctl-read)",
                        "(allow signal (target same-sandbox))",
                        "(deny syscall-unix (syscall-number SYS_setsid SYS_setpgid))",
                        "(allow network-outbound (literal " + json.dumps(address) + "))",
                    ])
                    if use_guardian:
                        arguments = [sys.executable, "-I", str(Path(__file__).resolve()),
                                     "guardian", address, nonce, policy]
                    else:
                        arguments = ["/usr/bin/sandbox-exec", "-p", policy, *arguments]
                if use_launchservices:
                    bundle = Path(temporary) / f"{role}.app"
                    executable = bundle / "Contents" / "MacOS" / "worker"
                    executable.parent.mkdir(parents=True)
                    executable.write_text(
                        "#!/bin/sh\nexec " + shlex.join(arguments)
                        + " >" + shlex.quote(str(Path(temporary) / f"{role}.stdout"))
                        + " 2>" + shlex.quote(str(Path(temporary) / f"{role}.stderr")) + "\n"
                    )
                    executable.chmod(0o700)
                    (bundle / "Contents" / "Info.plist").write_bytes(plistlib.dumps({
                        "CFBundleIdentifier": label,
                        "CFBundleExecutable": "worker",
                        "CFBundlePackageType": "APPL",
                        "CFBundleVersion": "1",
                        "LSBackgroundOnly": True,
                    }))
                    command = ["/usr/bin/open", "-n", "-g", str(bundle)]
                else:
                    configuration.write_bytes(plistlib.dumps({
                        "Label": label, "ProgramArguments": arguments,
                        "RunAtLoad": True, "KeepAlive": False,
                        "ProcessType": "Background",
                        "StandardOutPath": str(Path(temporary) / f"{role}.stdout"),
                        "StandardErrorPath": str(Path(temporary) / f"{role}.stderr"),
                    }))
                    configuration.chmod(0o600)
                    services.append(service)
                    command = ["/bin/launchctl", "bootstrap", f"user/{os.getuid()}", str(configuration)]
                code, output = launch(command, nonce)
                launched.append({"role": role, "code": code,
                                 "output": output[:4096].replace(nonce, "<fixture nonce>")})
                print(json.dumps({"launch": launched[-1]}), flush=True)
                assert code == 0, "launchd did not accept the fixture"
                try:
                    if use_guardian and role == "root":
                        accept("guardian")
                    accept(role)
                except Exception:
                    for stream in ("stdout", "stderr"):
                        path = Path(temporary) / f"{role}.{stream}"
                        if path.exists():
                            with path.open("rb") as report:
                                output = report.read(4096).decode(errors="replace")
                            print(json.dumps({"role": role, stream: output.replace(nonce, "<fixture nonce>")}), flush=True)
                    raise
            root = fixtures["root"][2]
            sentinel = fixtures["sentinel"][2]
            print(json.dumps({"coalition_control": {"controller": controller, "root": root, "sentinel": sentinel}}), flush=True)
            coalitions = [controller["coalitions"][0], root["coalitions"][0], sentinel["coalitions"][0]]
            assert 0 not in coalitions and len(set(coalitions)) == 3, "coalitions are not private"
            connection, reader, _ = fixtures["root"]
            if use_guardian:
                guardian = fixtures["guardian"][2]
                assert guardian["coalitions"][0] == root["coalitions"][0]
                assert guardian["unique"] != root["unique"]
            if use_seatbelt:
                protected = guardian if use_guardian else sentinel
                connection.sendall(f"signal {protected['pid']}\n".encode())
                result = read_json(reader)
                assert result == {"signal_errno": errno.EPERM}, result
                ping("guardian" if use_guardian else "sentinel")
                print(json.dumps({"seatbelt_signal_boundary": "enforced"}), flush=True)
                # First prove this host can use the same launchd interface to
                # write an owned fixture outside the payload's coalition.
                label = f"ai.retsu.qq.custody-probe.{nonce}.broker"
                control_label = label + ".control"
                broker_labels.extend([control_label, label])
                marker = Path(temporary) / "broker-control"
                with select.kqueue() as changes:
                    directory = os.open(temporary, os.O_RDONLY)
                    try:
                        changes.control([select.kevent(directory,
                            filter=select.KQ_FILTER_VNODE,
                            flags=select.KQ_EV_ADD | select.KQ_EV_CLEAR,
                            fflags=select.KQ_NOTE_WRITE)], 0, 0)
                        code, output = launch(["/bin/launchctl", "submit", "-l", control_label,
                                               "--", "/usr/bin/touch", str(marker)], nonce)
                        assert code == 0, f"launchd positive control failed: {code}: {output}"
                        deadline = time.monotonic() + 10
                        while not marker.exists():
                            remaining = deadline - time.monotonic()
                            assert remaining > 0, "launchd positive control did not execute"
                            changes.control([], 1, remaining)
                    finally:
                        os.close(directory)
                connection.sendall(b"broker\n")
                result = read_json(reader)
                assert result.get("broker_exit", 0) != 0, f"sandbox submitted an external job: {result}"
                assert not (Path(temporary) / "broker-output").exists(), "sandbox escaped through launchd"
                print(json.dumps({"launchd_broker_boundary": "enforced", "attempt": result}), flush=True)
            connection.sendall(b"spawn\n")
            spawned = read_json(reader)["spawned"]
            escaped = accept("escaped")
            assert escaped["pid"] == spawned and escaped["group"] != root["group"]
            assert escaped["coalitions"][0] == root["coalitions"][0]
            assert kernel.send(root, signal.SIGSTOP, wrong=True) == errno.ESRCH
            ping("root")
            assert kernel.send(root, signal.SIGKILL) == 0
            assert reader.read(1) == b"", "root did not exit"
            ping("escaped")
            ping("sentinel")
            members = kernel.members(root["coalitions"][0])
            assert escaped["unique"] in {item["unique"] for item in members}
            allowed = {root["unique"], escaped["unique"]}
            if use_guardian:
                allowed.add(guardian["unique"])
                assert guardian["unique"] in {item["unique"] for item in members}
                ping("guardian")
            assert all(item["unique"] in allowed for item in members), "unexpected coalition members"
            assert kernel.send(escaped, signal.SIGKILL) == 0
            assert fixtures["escaped"][1].read(1) == b"", "escaped child did not exit"
            ping("sentinel")
            if use_guardian:
                ping("guardian")
                print(json.dumps({"protected_guardian": guardian, "escaped_child_stopped": True}), flush=True)
        finally:
            try:
                signal_errors = []
                for _, _, tracked in fixtures.values():
                    if tracked["pid"] not in exits.pending:
                        continue
                    try:
                        code = kernel.send(tracked, signal.SIGKILL)
                        assert code in (0, errno.ESRCH), f"fixture kill failed: {code}"
                    except OSError as error:
                        if error.errno not in (errno.ESRCH, errno.ENOENT):
                            signal_errors.append(str(error))
                    except Exception as error:
                        signal_errors.append(str(error))
                # NOTE_EXIT observes the registered process instance, even when
                # its PID has already disappeared. Socket EOF alone is not exit.
                exits.drain(20)
                assert not signal_errors, f"fixture signal failures: {signal_errors}"
            finally:
                for connection, reader, _ in fixtures.values():
                    reader.close()
                    connection.close()
                listener.close()
                exits.queue.close()
                removal_errors = []
                for service in services:
                    try:
                        code, output = launch(["/bin/launchctl", "bootout", service], nonce)
                        if code not in (0, 3):
                            removal_errors.append(f"{code}: {output[:4096].replace(nonce, '<fixture nonce>')}")
                    except Exception as error:
                        removal_errors.append(str(error))
                for label in broker_labels:
                    try:
                        code, output = launch(["/bin/launchctl", "remove", label], nonce)
                        if code not in (0, 3):
                            removal_errors.append(f"broker {code}: {output[:4096]}")
                    except Exception as error:
                        removal_errors.append(str(error))
                assert not removal_errors, f"fixture service removal failed: {removal_errors}"
        print(json.dumps({"prerequisites_passed": True, "controller": controller, "root": root, "escaped": escaped, "sentinel": sentinel, "not_a_containment_qualification": True}), flush=True)


if __name__ == "__main__":
    if sys.argv[1:] == ["seatbelt-nesting"]:
        seatbelt_nesting()
    elif sys.argv[1:] == ["layout-check"]:
        layouts()
        print("LP64 probe layouts checked")
    elif sys.argv[1:] in (["host"], ["launchservices"], ["launchservices-seatbelt"], ["launchservices-guardian"]):
        try:
            host(use_launchservices=sys.argv[1] != "host",
                 use_seatbelt=sys.argv[1] in ("launchservices-seatbelt", "launchservices-guardian"),
                 use_guardian=sys.argv[1] == "launchservices-guardian")
        except Exception as error:
            print(json.dumps({"prerequisites_passed": False, "error": f"{type(error).__name__}: {error}"}), flush=True)
            raise
    elif len(sys.argv) == 5 and sys.argv[1] == "guardian":
        child(sys.argv[2], sys.argv[3], "guardian", root_policy=sys.argv[4])
    elif len(sys.argv) == 5 and sys.argv[1] == "child":
        child(*sys.argv[2:])
    else:
        raise SystemExit("usage: macos-custody-probe.py host|launchservices|launchservices-seatbelt|launchservices-guardian|seatbelt-nesting|layout-check|child ADDRESS NONCE ROLE")
