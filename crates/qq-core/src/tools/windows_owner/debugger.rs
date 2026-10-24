//! Disposable-CI diagnostic only: never changes the target token or permissions.
use std::{
    fs,
    io::{self, BufRead as _, Write as _},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex, mpsc},
    time::Duration,
};

pub(super) struct Debugger {
    child: Child,
    process: Arc<win_custody::Process>,
    stop: mpsc::SyncSender<()>,
    output: Arc<Mutex<Vec<String>>>,
    done: mpsc::Receiver<()>,
    ready: mpsc::Receiver<()>,
    status: Arc<Mutex<fs::File>>,
}
impl Debugger {
    pub(super) fn start(pid: u32, program: &std::path::Path) -> io::Result<Option<Self>> {
        if !cfg!(target_arch = "x86_64")
            || !program
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.eq_ignore_ascii_case("cargo.exe"))
        {
            return Ok(None);
        }
        let exe = PathBuf::from(r"C:\Program Files (x86)\Windows Kits\10\Debuggers\x64\cdb.exe");
        if !exe.is_file() {
            return Ok(None);
        }
        let root = std::env::current_exe()?
            .parent()
            .ok_or_else(|| io::Error::other("private helper root unavailable"))?
            .to_owned();
        let status = Arc::new(Mutex::new(
            fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(root.join(format!("cdb-{pid}-state.txt")))?,
        ));
        note(
            &status,
            &format!("QQ_CDB_STATE target_pid={pid} setup_started"),
        );
        let main_script = root.join(format!("cdb-{pid}-main.txt"));
        let open_script = root.join(format!("cdb-{pid}-open.txt"));
        let return_script = root.join(format!("cdb-{pid}-return.txt"));
        write_new(
            &return_script,
            r#".printf "QQ_CDB_EVENT_RETURN pid=%x tid=%x status=%x\n", @$tpid, @$tid, @eax
kn 12
gc
"#,
        )?;
        write_new(
            &open_script,
            &format!(
                r#".printf "QQ_CDB_EVENT_OPEN pid=%x tid=%x access=%x name=", @$tpid, @$tid, @edx
.printf "%msu\n", poi(@r8+0x10)
kn 12
bp /1 @$ra "$$><{}"
gc
"#,
                return_script.display()
            ),
        )?;
        fs::create_dir_all(root.join("cdb-symbols"))?;
        write_new(
            &main_script,
            &format!(
                r#".sympath {}
sxn bpe
sxn ibp
sx
bu ntdll!RtlSetLastWin32Error ".echo QQ_CDB_HIT function=RtlSetLastWin32Error; .if (@ecx != 0) {{r ecx; kn 12}}; gc"
bu ntdll!RtlNtStatusToDosError ".echo QQ_CDB_HIT function=RtlNtStatusToDosError; .if (@ecx != 0) {{r ecx; kn 12}}; gc"
bu ntdll!NtOpenEvent ".echo QQ_CDB_HIT function=NtOpenEvent; $$><{}"
.printf "QQ_CDB_BREAKPOINTS pid=%x\n", @$tpid
bl
.printf "QQ_CDB_READY pid=%x\n", @$tpid
g
"#,
                root.join("cdb-symbols").display(),
                open_script.display()
            ),
        )?;
        let mut child = Command::new(exe)
            .env_remove("_NT_SYMBOL_PATH")
            .env_remove("_NT_ALT_SYMBOL_PATH")
            .args([
                "-p",
                &pid.to_string(),
                "-pd",
                "-hd",
                "-G",
                "-noshell",
                "-noinh",
                "-cf",
            ])
            .arg(main_script)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let process = match win_custody::Process::from_child(&child) {
            Ok(process) => Arc::new(process),
            Err(error) => {
                let _ = child.kill();
                return Err(error);
            }
        };
        note(
            &status,
            &format!("QQ_CDB_STATE debugger_pid={} spawned", child.id()),
        );
        let output = Arc::new(Mutex::new(Vec::new()));
        let stdout = child.stdout.take().expect("piped debugger output");
        let observed = Arc::clone(&output);
        let recorded = Arc::clone(&status);
        let (ready_send, ready) = mpsc::sync_channel(1);
        let (done_send, done) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            collect(stdout, &observed, &ready_send);
            if let Ok(rows) = observed.lock() {
                for row in rows.iter() {
                    note(&recorded, row);
                }
            }
            let _ = done_send.send(());
        });
        let (stop, stopped) = mpsc::sync_channel(1);
        let watched = Arc::clone(&process);
        let timer_status = Arc::clone(&status);
        std::thread::spawn(move || {
            if stopped.recv_timeout(Duration::from_secs(25)).is_err() {
                note(&timer_status, "QQ_CDB_STATE diagnostic_deadline_reached");
                let _ = watched.terminate(1);
            }
        });
        let attached = (|| -> io::Result<bool> {
            use winsafe::co;
            let target = winsafe::HPROCESS::OpenProcess(co::PROCESS::QUERY_INFORMATION, false, pid)
                .map_err(io::Error::other)?;
            let until = std::time::Instant::now() + Duration::from_secs(10);
            loop {
                if target
                    .CheckRemoteDebuggerPresent()
                    .map_err(io::Error::other)?
                {
                    return Ok(true);
                }
                if std::time::Instant::now() >= until || process.exit_code()?.is_some() {
                    return Ok(false);
                }
                process.wait(Some(Duration::from_millis(20)))?;
            }
        })();
        note(
            &status,
            &format!("QQ_CDB_STATE os_debug_port_attached={attached:?}; script_ready_pending"),
        );
        if !matches!(attached, Ok(true)) {
            let _ = stop.try_send(());
            let _ = process.terminate(1);
            let cleanup = process.wait(Some(Duration::from_secs(3)));
            let _ = done.recv_timeout(Duration::from_secs(2));
            note(
                &status,
                &format!("QQ_CDB_STATE attach_failed cleanup={cleanup:?}"),
            );
            return Err(io::Error::other("CDB OS attachment not confirmed"));
        }
        Ok(Some(Self {
            child,
            process,
            stop,
            output,
            done,
            ready,
            status,
        }))
    }
    pub(super) fn finish(mut self) {
        let _ = self.stop.try_send(());
        let status = self.process.wait(Some(Duration::from_secs(3)));
        if !matches!(status, Ok(win_custody::WaitOutcome::Signaled)) {
            let _ = self.process.terminate(1);
        }
        let settled = self.process.wait(Some(Duration::from_secs(3)));
        if matches!(settled, Ok(win_custody::WaitOutcome::Signaled)) {
            let _ = self.child.wait();
        }
        let drained = self.done.recv_timeout(Duration::from_secs(2)).is_ok();
        let summary = format!(
            "QQ_CDB_STATE exit={:?} cleanup={settled:?} output_drained={drained} script_ready={}",
            self.process.exit_code(),
            self.ready.try_recv().is_ok()
        );
        note(&self.status, &summary);
        eprintln!("{summary}");
        if let Ok(rows) = self.output.lock() {
            for row in rows.iter() {
                eprintln!("{row}");
            }
        }
    }
}
impl Drop for Debugger {
    fn drop(&mut self) {
        let _ = self.stop.try_send(());
        let _ = self.process.terminate(1);
    }
}
fn write_new(path: &std::path::Path, text: &str) -> io::Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(text.replace("\r\n", "\n").replace('\n', "\r\n").as_bytes())
}
fn collect(stdout: impl io::Read, output: &Mutex<Vec<String>>, ready: &mpsc::SyncSender<()>) {
    let mut reader = io::BufReader::new(stdout);
    let mut line = Vec::with_capacity(4096);
    let mut batch = Vec::new();
    let mut count = 0;
    let mut bytes = 0;
    let mut hits = 0usize;
    let mut status_kind = None;
    let mut stops = 0usize;
    let mut read_failed = false;
    loop {
        let buffer = if read_failed {
            &[]
        } else {
            match reader.fill_buf() {
                Ok(buffer) => buffer,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => {
                    read_failed = true;
                    &[]
                }
            }
        };
        if buffer.is_empty() && line.is_empty() {
            break;
        }
        let end = if buffer.is_empty() {
            Some(0)
        } else {
            buffer.iter().position(|b| *b == b'\n')
        };
        let used = if buffer.is_empty() {
            0
        } else {
            end.map_or(buffer.len(), |n| n + 1)
        };
        let remaining = 4096usize.saturating_sub(line.len());
        line.extend_from_slice(&buffer[..used.min(remaining)]);
        reader.consume(used);
        if end.is_none() {
            continue;
        }
        let text = String::from_utf8_lossy(&line).trim().to_owned();
        if let Some(code) = text.split("exception - code ").nth(1) {
            let code = code
                .chars()
                .take_while(char::is_ascii_hexdigit)
                .take(8)
                .collect::<String>();
            if !code.is_empty() {
                flush(&mut batch, output, &mut count, &mut bytes);
                let chance = if text.contains("(first chance)") {
                    "first"
                } else if text.contains("(second chance)") {
                    "second"
                } else {
                    "unknown"
                };
                batch.push(format!("QQ_CDB_EXCEPTION code={code} chance={chance}"));
            }
        }
        if ["bpe -", "bpec -", "ibp -"]
            .iter()
            .any(|prefix| text.starts_with(prefix))
            && text.len() <= 256
            && text
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || " -:,()".contains(c))
        {
            flush(&mut batch, output, &mut count, &mut bytes);
            batch.push(format!("QQ_CDB_POLICY {text}"));
            line.clear();
            continue;
        }
        if let Some(index) = text
            .strip_prefix("Breakpoint ")
            .and_then(|value| value.strip_suffix(" hit"))
            && !index.is_empty()
            && index.len() <= 8
            && index.chars().all(|c| c.is_ascii_digit())
        {
            flush(&mut batch, output, &mut count, &mut bytes);
            if stops < 16 {
                batch.push(format!("QQ_CDB_STOP breakpoint={index}"));
            }
            stops = stops.saturating_add(1);
        }
        if let Some(value) = text.strip_prefix("ecx=")
            && !value.is_empty()
            && value.len() <= 8
            && value.chars().all(|c| c.is_ascii_hexdigit())
            && let Some(kind) = status_kind
        {
            flush(&mut batch, output, &mut count, &mut bytes);
            batch.push(format!("QQ_CDB_STATUS {kind}={value}"));
            line.clear();
            continue;
        }
        if text.starts_with("QQ_CDB_HIT ") {
            status_kind = match text.as_str() {
                "QQ_CDB_HIT function=RtlSetLastWin32Error" => Some("Win32"),
                "QQ_CDB_HIT function=RtlNtStatusToDosError" => Some("NtStatus"),
                _ => None,
            };
            flush(&mut batch, output, &mut count, &mut bytes);
            if hits < 8 {
                batch.push(text);
            }
            hits = hits.saturating_add(1);
            line.clear();
            continue;
        }
        if text.starts_with("QQ_CDB_READY") {
            let _ = ready.try_send(());
        }
        if let Some(class) = diagnostic(&text) {
            flush(&mut batch, output, &mut count, &mut bytes);
            if !matches!(class, "debugger-prompt" | "break-instruction") || stops < 16 {
                batch.push(format!("QQ_CDB_DIAGNOSTIC class={class}"));
            }
            if matches!(class, "debugger-prompt" | "break-instruction") {
                stops = stops.saturating_add(1);
            }
        } else if text.starts_with("QQ_CDB_") {
            flush(&mut batch, output, &mut count, &mut bytes);
            batch.push(text);
        } else if !batch.is_empty()
            && batch.len() <= 12
            && (text.contains('!') || text.to_ascii_lowercase().contains("bcrypt+"))
            && let Some(symbol) = text
                .split_whitespace()
                .find(|word| word.contains('!') || word.to_ascii_lowercase().starts_with("bcrypt+"))
        {
            batch.push(symbol.to_owned());
        }
        line.clear();
    }
    flush(&mut batch, output, &mut count, &mut bytes);
    if hits > 0 {
        batch.push(format!("QQ_CDB_HIT_COUNT count={hits}"));
    }
    if stops > 0 {
        batch.push(format!("QQ_CDB_STOP_COUNT count={stops}"));
    }
    if read_failed {
        batch.push("QQ_CDB_DIAGNOSTIC class=read-error-incomplete".to_owned());
    }
    flush(&mut batch, output, &mut count, &mut bytes);
}
fn diagnostic(text: &str) -> Option<&'static str> {
    let lower = text.to_ascii_lowercase();
    if lower.split_once('>').is_some_and(|(prompt, _)| {
        prompt.contains(':')
            && prompt
                .chars()
                .all(|c| c.is_ascii_hexdigit() || c == ':' || c == ' ')
    }) {
        return Some("debugger-prompt");
    }
    [
        (
            "processes created by the current process will be debugged",
            "child-debugging-enabled",
        ),
        (
            "processes created by the current process will not be debugged",
            "child-debugging-disabled",
        ),
        ("int     3", "break-instruction"),
        ("int 3", "break-instruction"),
        ("syntax error", "syntax"),
        ("couldn't resolve", "unresolved-symbol"),
        ("unable to resolve", "unresolved-symbol"),
        ("couldn't find", "missing-symbol"),
        ("symbol could not be found", "missing-symbol"),
        ("unable to insert breakpoint", "breakpoint-insertion"),
        ("symbol file could not be found", "missing-symbol-file"),
        ("no runnable debuggees", "no-runnable-target"),
        ("unknown command", "unknown-command"),
        ("unrecognized command", "unknown-command"),
        ("command is not valid", "invalid-command-state"),
        ("memory access error", "memory-access"),
        ("could not read", "read-error"),
    ]
    .into_iter()
    .find_map(|(needle, class)| lower.contains(needle).then_some(class))
}
fn flush(
    batch: &mut Vec<String>,
    output: &Mutex<Vec<String>>,
    count: &mut usize,
    bytes: &mut usize,
) {
    let relevant = batch.iter().any(|row| {
        row.to_ascii_lowercase().contains("bcrypt!")
            || row.to_ascii_lowercase().starts_with("bcrypt+")
    }) || batch.first().is_some_and(|row| {
        row.starts_with("QQ_CDB_POLICY")
            || row.starts_with("QQ_CDB_STOP")
            || row.starts_with("QQ_CDB_HIT")
            || row.starts_with("QQ_CDB_READY")
            || row.starts_with("QQ_CDB_DIAGNOSTIC")
            || row.starts_with("QQ_CDB_EXCEPTION")
            || row.starts_with("QQ_CDB_BREAKPOINTS")
    });
    if relevant && let Ok(mut rows) = output.lock() {
        for row in batch.drain(..) {
            if *count < 128 && *bytes + row.len() <= 65536 {
                *count += 1;
                *bytes += row.len();
                rows.push(row);
            }
        }
    }
    batch.clear();
}

fn note(status: &Mutex<fs::File>, row: &str) {
    if let Ok(mut file) = status.lock() {
        let _ = writeln!(file, "{row}");
        let _ = file.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn keeps_only_crypto_symbols_and_readiness_without_argument_columns() {
        let bytes = b"QQ_CDB_STATUS Win32=32 pid=1 tid=2\n00 0000 1111 bcrypt!RegisterReopenWait+0x12\nENV_PRIVATE_TOKEN=never-export\nQQ_CDB_STATUS Win32=2 pid=1 tid=2\n01 0000 2222 kernelbase!Unrelated+0x1\nQQ_CDB_STATUS NtStatus=c0000034\n02 0000 3333 bcrypt+0x41\nQQ_CDB_READY\n";
        let output = Mutex::new(Vec::new());
        let (ready, received) = mpsc::sync_channel(1);
        collect(bytes.as_slice(), &output, &ready);
        assert_eq!(received.try_recv(), Ok(()));
        assert_eq!(
            *output.lock().unwrap(),
            [
                "QQ_CDB_STATUS Win32=32 pid=1 tid=2",
                "bcrypt!RegisterReopenWait+0x12",
                "QQ_CDB_STATUS NtStatus=c0000034",
                "bcrypt+0x41",
                "QQ_CDB_READY"
            ]
        );
    }
    #[test]
    fn retains_bounded_stop_and_breakpoint_metadata_without_raw_columns() {
        let bytes = b"QQ_CDB_BREAKPOINTS pid=123\n0 e Disable Clear 00007ff0 ntdll!RtlSetLastWin32Error\n(123.456): Break instruction exception - code 80000003 (first chance) PRIVATE\nQQ_CDB_READY pid=123\n";
        let output = Mutex::new(Vec::new());
        let (ready, _) = mpsc::sync_channel(1);
        collect(bytes.as_slice(), &output, &ready);
        assert_eq!(
            *output.lock().unwrap(),
            [
                "QQ_CDB_BREAKPOINTS pid=123",
                "ntdll!RtlSetLastWin32Error",
                "QQ_CDB_EXCEPTION code=80000003 chance=first",
                "QQ_CDB_READY pid=123"
            ]
        );
    }
    #[test]
    fn records_error_classes_without_paths_commands_or_memory() {
        let bytes = b"Syntax error at 'secret-command'\nCouldn't resolve private!Secret\nUnknown command: TOKEN_PRIVATE\nNo runnable debuggees\nProcesses created by the current process will be debugged\n";
        let output = Mutex::new(Vec::new());
        let (ready, _) = mpsc::sync_channel(1);
        collect(bytes.as_slice(), &output, &ready);
        assert_eq!(
            *output.lock().unwrap(),
            [
                "QQ_CDB_DIAGNOSTIC class=syntax",
                "QQ_CDB_DIAGNOSTIC class=unresolved-symbol",
                "QQ_CDB_DIAGNOSTIC class=unknown-command",
                "QQ_CDB_DIAGNOSTIC class=no-runnable-target",
                "QQ_CDB_DIAGNOSTIC class=child-debugging-enabled"
            ]
        );
    }
    #[test]
    fn keeps_chance_stop_state_and_unterminated_prompt() {
        let bytes = b"bpe - Break instruction exception - output - handled\n(1.2): Break instruction exception - code 80000003 (second chance)\nBreakpoint 7 hit\n00007ff0 cc int     3\n0:000>";
        let output = Mutex::new(Vec::new());
        let (ready, _) = mpsc::sync_channel(1);
        collect(bytes.as_slice(), &output, &ready);
        assert_eq!(
            *output.lock().unwrap(),
            [
                "QQ_CDB_POLICY bpe - Break instruction exception - output - handled",
                "QQ_CDB_EXCEPTION code=80000003 chance=second",
                "QQ_CDB_STOP breakpoint=7",
                "QQ_CDB_DIAGNOSTIC class=break-instruction",
                "QQ_CDB_DIAGNOSTIC class=debugger-prompt",
                "QQ_CDB_STOP_COUNT count=3",
            ]
        );
    }
    #[test]
    fn retains_only_first_eight_callback_hits() {
        let bytes = "QQ_CDB_HIT function=RtlSetLastWin32Error pid=1 tid=2\n".repeat(32);
        let output = Mutex::new(Vec::new());
        let (ready, _) = mpsc::sync_channel(1);
        collect(bytes.as_bytes(), &output, &ready);
        let rows = output.lock().unwrap();
        assert_eq!(rows.len(), 9);
        assert_eq!(rows.last().unwrap(), "QQ_CDB_HIT_COUNT count=32");
    }
    #[test]
    fn stop_noise_does_not_exhaust_crypto_output_budget() {
        let mut bytes = "Breakpoint 7 hit\n".repeat(256);
        bytes.push_str("QQ_CDB_STATUS NtStatus=c0000022\n0000 bcrypt+0x41\n");
        let output = Mutex::new(Vec::new());
        let (ready, _) = mpsc::sync_channel(1);
        collect(bytes.as_bytes(), &output, &ready);
        let rows = output.lock().unwrap();
        assert_eq!(
            rows.iter()
                .filter(|row| row.starts_with("QQ_CDB_STOP breakpoint="))
                .count(),
            16
        );
        assert!(rows.iter().any(|row| row == "bcrypt+0x41"));
        assert_eq!(rows.last().unwrap(), "QQ_CDB_STOP_COUNT count=256");
    }
    #[test]
    fn preserves_unterminated_prompt_before_pipe_read_error() {
        struct FailingPipe(io::Cursor<&'static [u8]>);
        impl io::Read for FailingPipe {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                if self.0.position() == self.0.get_ref().len() as u64 {
                    return Err(io::Error::from(io::ErrorKind::BrokenPipe));
                }
                io::Read::read(&mut self.0, buffer)
            }
        }
        let output = Mutex::new(Vec::new());
        let (ready, _) = mpsc::sync_channel(1);
        collect(FailingPipe(io::Cursor::new(b"0:000>")), &output, &ready);
        assert_eq!(
            *output.lock().unwrap(),
            [
                "QQ_CDB_DIAGNOSTIC class=debugger-prompt",
                "QQ_CDB_STOP_COUNT count=1",
                "QQ_CDB_DIAGNOSTIC class=read-error-incomplete",
            ]
        );
    }
    #[test]
    fn ties_numeric_register_status_to_fixed_handler_kind() {
        let bytes = b"QQ_CDB_HIT function=RtlNtStatusToDosError\necx=c0000022\n00 0000 1234 bcrypt+0x41\nQQ_CDB_HIT function=NtOpenEvent\necx=PRIVATE\n";
        let output = Mutex::new(Vec::new());
        let (ready, _) = mpsc::sync_channel(1);
        collect(bytes.as_slice(), &output, &ready);
        assert_eq!(
            *output.lock().unwrap(),
            [
                "QQ_CDB_HIT function=RtlNtStatusToDosError",
                "QQ_CDB_STATUS NtStatus=c0000022",
                "bcrypt+0x41",
                "QQ_CDB_HIT function=NtOpenEvent",
                "QQ_CDB_HIT_COUNT count=2",
            ]
        );
    }
    #[test]
    fn continues_draining_after_bounded_selected_output() {
        let mut bytes = Vec::new();
        for _ in 0..512 {
            bytes.extend_from_slice(
                b"QQ_CDB_STATUS NtStatus=c0000034\n0000 bcrypt!NamedObject+0x2\n",
            );
        }
        let mut input = io::Cursor::new(bytes);
        let output = Mutex::new(Vec::new());
        let (ready, _) = mpsc::sync_channel(1);
        collect(&mut input, &output, &ready);
        assert_eq!(input.position(), input.get_ref().len() as u64);
        let rows = output.lock().unwrap();
        assert_eq!(rows.len(), 128);
        assert!(rows.iter().map(String::len).sum::<usize>() <= 65536);
    }
}
