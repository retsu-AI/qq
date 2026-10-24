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
        let target_script = root.join(format!("cdb-{pid}-target.txt"));
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
        write_new(
            &target_script,
            &format!(
                r#"bu ntdll!RtlSetLastWin32Error ".if (@ecx != 0) {{.printf \"QQ_CDB_STATUS Win32=%x pid=%x tid=%x\\n\", @ecx, @$tpid, @$tid; kn 12}}; gc"
bu ntdll!RtlNtStatusToDosError ".if (@ecx != 0) {{.printf \"QQ_CDB_STATUS NtStatus=%x pid=%x tid=%x\\n\", @ecx, @$tpid, @$tid; kn 12}}; gc"
bu ntdll!NtOpenEvent "$$><{}"
.printf "QQ_CDB_BREAKPOINTS pid=%x\n", @$tpid
bl
.printf "QQ_CDB_READY pid=%x\n", @$tpid
g
"#,
                open_script.display()
            ),
        )?;
        fs::create_dir_all(root.join("cdb-symbols"))?;
        write_new(
            &main_script,
            &format!(
                r#".sympath {}
sxn bpe
sxn ibp
$$><{}
"#,
                root.join("cdb-symbols").display(),
                target_script.display()
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
    file.write_all(text.as_bytes())
}
fn collect(stdout: impl io::Read, output: &Mutex<Vec<String>>, ready: &mpsc::SyncSender<()>) {
    let mut reader = io::BufReader::new(stdout);
    let mut line = Vec::with_capacity(4096);
    let mut batch = Vec::new();
    let mut count = 0;
    let mut bytes = 0;
    while let Ok(buffer) = reader.fill_buf() {
        if buffer.is_empty() {
            break;
        }
        let end = buffer.iter().position(|b| *b == b'\n');
        let used = end.map_or(buffer.len(), |n| n + 1);
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
                batch.push(format!("QQ_CDB_EXCEPTION code={code}"));
            }
        }
        if text.starts_with("QQ_CDB_READY") {
            let _ = ready.try_send(());
        }
        if let Some(class) = diagnostic(&text) {
            flush(&mut batch, output, &mut count, &mut bytes);
            batch.push(format!("QQ_CDB_DIAGNOSTIC class={class}"));
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
}
fn diagnostic(text: &str) -> Option<&'static str> {
    let lower = text.to_ascii_lowercase();
    [
        (
            "processes created by the current process will be debugged",
            "child-debugging-enabled",
        ),
        (
            "processes created by the current process will not be debugged",
            "child-debugging-disabled",
        ),
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
        row.starts_with("QQ_CDB_READY")
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
                "QQ_CDB_EXCEPTION code=80000003",
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
