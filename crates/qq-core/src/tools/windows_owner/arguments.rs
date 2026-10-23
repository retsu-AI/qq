//! Preserve argument boundaries for native Windows executables (CRT parsing).
use std::{ffi::OsString, io, path::Path};

pub(super) fn command_line(program: &Path, arguments: &[OsString]) -> io::Result<String> {
    if program
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case("cmd.exe"))
        && let [flag, script] = arguments
        && flag
            .to_str()
            .is_some_and(|flag| flag.eq_ignore_ascii_case("/c"))
    {
        let script = script
            .to_str()
            .ok_or_else(|| io::Error::other("native shell text is not Unicode"))?;
        // /S strips exactly the outer pair; cmd parses the unchanged inner
        // text. CRT backslash escaping would turn its quotes into literals.
        // /D prevents ambient registry AutoRun commands from preceding it.
        let result = format!("{} /D /S /C \"{}\"", command_line(program, &[])?, script);
        if script.contains('\0') || result.encode_utf16().count() > 8191 {
            return Err(io::Error::other(
                "native shell text contains NUL or exceeds cmd's limit",
            ));
        }
        return Ok(result);
    }
    let mut result = String::new();
    for (index, argument) in std::iter::once(program.as_os_str())
        .chain(arguments.iter().map(OsString::as_os_str))
        .enumerate()
    {
        let argument = argument
            .to_str()
            .ok_or_else(|| io::Error::other("native argument is not Unicode"))?;
        if argument.contains('\0') {
            return Err(io::Error::other("native argument contains NUL"));
        }
        if !result.is_empty() {
            result.push(' ');
        }
        if index != 0
            && !argument.is_empty()
            && !argument.chars().any(|c| c.is_whitespace() || c == '"')
        {
            result.push_str(argument);
            continue;
        }
        result.push('"');
        let mut slashes = 0;
        for character in argument.chars() {
            if character == '\\' {
                slashes += 1;
                continue;
            }
            result.extend(std::iter::repeat_n(
                '\\',
                if character == '"' {
                    slashes * 2 + 1
                } else {
                    slashes
                },
            ));
            result.push(character);
            slashes = 0;
        }
        result.extend(std::iter::repeat_n('\\', slashes * 2));
        result.push('"');
    }
    if result.encode_utf16().count() >= 32_767 {
        return Err(io::Error::other(
            "native command line exceeds Windows limit",
        ));
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preserves_cmd_shell_text_without_crt_backslash_escaping() {
        let args = ["/C", r#"echo "quoted value" > "result with spaces.txt""#].map(OsString::from);
        assert_eq!(
            command_line(Path::new("cmd.exe"), &args).unwrap(),
            r#""cmd.exe" /D /S /C "echo "quoted value" > "result with spaces.txt"""#
        );
    }

    #[test]
    fn preserves_empty_quoted_and_trailing_slash_arguments() {
        let args = ["", "a b", "a\"b", "tail path\\", "/C"].map(OsString::from);
        assert_eq!(
            command_line(Path::new("tool.exe"), &args).unwrap(),
            r#""tool.exe" "" "a b" "a\"b" "tail path\\" /C"#
        );
        assert!(command_line(Path::new("tool.exe"), &[OsString::from("bad\0value")]).is_err());
        assert!(command_line(Path::new("tool.exe"), &[OsString::from("x".repeat(32767))]).is_err());
    }
}
