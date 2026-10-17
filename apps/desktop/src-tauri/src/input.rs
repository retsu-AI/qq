//! Input preparation reuses the TUI's contained, bounded mention resolver.

use qq_protocol::InputPart;
use std::path::Path;

pub fn resolve(path: &Path, text: &str, files: Vec<String>) -> Result<Vec<InputPart>, String> {
    let mut explicit = vec![InputPart::text(text)];
    explicit.extend(files.into_iter().map(InputPart::workspace_file));
    qq_protocol::validate_input(&explicit).map_err(|error| error.to_string())?;
    let resolved = qq_core::mentions::resolve_prompt(path, text);
    if !resolved.notes.is_empty() {
        return Err(resolved.notes.join("; "));
    }
    let mut input = resolved.parts;
    if let Some(skill) = resolved.skill {
        let rest: String = input
            .iter()
            .filter_map(|part| match part {
                InputPart::Text { text } => Some(text.as_str()),
                InputPart::WorkspaceFile { .. } => None,
            })
            .collect();
        input.retain(|part| matches!(part, InputPart::WorkspaceFile { .. }));
        input.insert(0, InputPart::text(format!("/{skill}{rest}")));
    }
    input.extend(explicit.into_iter().skip(1));
    qq_protocol::validate_input(&input).map_err(|error| error.to_string())?;
    Ok(input)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_oversized_and_absolute_inputs_before_reading_files() {
        assert!(
            resolve(
                Path::new("."),
                &"x".repeat(qq_protocol::MAX_INPUT_TEXT_BYTES + 1),
                vec![]
            )
            .is_err()
        );
        assert!(resolve(Path::new("."), "review", vec!["/outside".into()]).is_err());
        assert!(resolve(Path::new("."), "review", vec!["a.rs".into(); 9]).is_err());
    }

    #[test]
    fn attachments_remain_typed_and_unresolved_mentions_are_explicit() {
        let input = resolve(Path::new("."), "review", vec!["src/main.rs".into()]).unwrap();
        assert!(
            matches!(&input[1], InputPart::WorkspaceFile { path, .. } if path == "src/main.rs")
        );
        assert!(
            resolve(
                Path::new("."),
                "review @file-that-does-not-exist.rs",
                vec![]
            )
            .is_err()
        );
    }
}
