//! `qq init`: write a commented starter `config.ron` with the chosen model.
//!
//! The command writes one file and validates the result with the same loader
//! every other path uses; it never edits an existing document. Paths and
//! streams are injected so the tests run against a temporary tree.

use std::{
    fs,
    io::{self, BufRead, Write},
    path::{Path, PathBuf},
};

use qq_config as config;

use crate::{LOGIN_PROVIDERS, cli};

/// Built-in providers offered by the chooser, in `qq auth login` order:
/// name, an example model from the shipped catalog, and the API-key
/// variable when the provider reads one.
const CHOICES: [(&str, &str, Option<&str>); 5] = [
    ("openai", "gpt-5.6", Some("OPENAI_API_KEY")),
    ("anthropic", "claude-sonnet-5", Some("ANTHROPIC_API_KEY")),
    ("google", "gemini-2.5-flash", Some("GEMINI_API_KEY")),
    ("xai", "grok-4.6", Some("XAI_API_KEY")),
    ("openai-codex", "gpt-5.6-luna", None),
];

#[derive(Debug, thiserror::Error)]
pub enum InitError {
    #[error("no --model given and stdin is not a terminal; pass --model PROVIDER/MODEL")]
    ModelRequired,
    #[error("model route must use provider/model syntax: {route:?}")]
    InvalidModelRoute { route: String },
    #[error("choose a number from 1 to {} or type PROVIDER/MODEL, not {answer:?}", CHOICES.len())]
    InvalidChoice { answer: String },
    #[error("{} already exists; pass --force to overwrite", path.display())]
    AlreadyExists { path: PathBuf },
    #[error("failed to write {}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to write to the terminal: {0}")]
    Terminal(#[source] io::Error),
    #[error("wrote {}, but the configuration does not validate; edit it or rerun with --force: {source}", path.display())]
    Invalid {
        path: PathBuf,
        #[source]
        source: config::ConfigError,
    },
}

/// Writes the starter file and prints where it went and what to do next.
/// `chooser` is stdin when it is a terminal; `None` means a missing `--model`
/// is an error rather than a prompt.
pub fn run<R: BufRead, W: Write>(
    paths: &config::ConfigPaths,
    cwd: &Path,
    args: cli::InitArgs,
    chooser: Option<&mut R>,
    stdout: &mut W,
) -> Result<(), InitError> {
    let route = match (args.model, chooser) {
        (Some(route), _) => route,
        (None, None) => return Err(InitError::ModelRequired),
        (None, Some(input)) => {
            if let Err(error) = writeln!(stdout, "Which model should sessions start with?") {
                return Err(InitError::Terminal(error));
            }
            for (index, (provider, model, _)) in CHOICES.iter().enumerate() {
                if let Err(error) = writeln!(stdout, "  {}. {provider}/{model}", index + 1) {
                    return Err(InitError::Terminal(error));
                }
            }
            if let Err(error) =
                write!(stdout, "Number or PROVIDER/MODEL: ").and_then(|()| stdout.flush())
            {
                return Err(InitError::Terminal(error));
            }
            let mut answer = String::new();
            if let Err(error) = input.read_line(&mut answer) {
                return Err(InitError::Terminal(error));
            }
            let answer = answer.trim();
            match answer.parse::<usize>() {
                Ok(number) => match number.checked_sub(1).and_then(|index| CHOICES.get(index)) {
                    Some((provider, model, _)) => format!("{provider}/{model}"),
                    None => {
                        return Err(InitError::InvalidChoice {
                            answer: answer.to_owned(),
                        });
                    }
                },
                Err(_) if answer.contains('/') => answer.to_owned(),
                Err(_) => {
                    return Err(InitError::InvalidChoice {
                        answer: answer.to_owned(),
                    });
                }
            }
        }
    };
    let provider = match route.split_once('/') {
        Some((provider, model)) if !provider.is_empty() && !model.is_empty() => provider,
        Some(_) | None => return Err(InitError::InvalidModelRoute { route }),
    };

    // The route comes from the user; escape it so a quote cannot end the
    // string literal and change the document's meaning.
    let mut escaped = String::with_capacity(route.len() + 2);
    escaped.push('"');
    for character in route.chars() {
        match character {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            _ => escaped.push(character),
        }
    }
    escaped.push('"');

    let (directory, scope_note) = if args.project {
        (
            cwd.join(".qq"),
            "    // A project file that sets `model` is sensitive: each user accepts it\n\
             \x20   // once with `qq trust` in this directory, and again when it changes.\n\
             \x20   //\n\
             \x20   // Anything referencing a Stored(...) credential (providers, MCP servers,\n\
             \x20   // reviewer/worker models) is personal to one machine and belongs in a\n\
             \x20   // gitignored .qq/config.d/*-local.ron, not in this file.\n",
        )
    } else {
        (
            paths.global_dir().to_path_buf(),
            "    // Built-in providers (openai, anthropic, google, xai, openai-codex,\n\
             \x20   // bedrock) need no declaration: `qq auth login PROVIDER` or the\n\
             \x20   // provider's API-key variable is enough.\n\
             \x20   //\n\
             \x20   // Anything referencing a Stored(...) credential (providers, MCP servers,\n\
             \x20   // reviewer/worker models) is personal to one machine and belongs in\n\
             \x20   // this directory's config.d/*.ron, not in a repository.\n",
        )
    };
    let path = directory.join("config.ron");
    let document = format!(
        "// QQ configuration. Docs: https://github.com/retsu-AI/qq/blob/main/docs/guide/configuration.md\n\
         (\n\
         \x20   version: 1,\n\
         \n\
         \x20   // The model every session starts with, as PROVIDER/MODEL.\n\
         \x20   // Change it any time here, with `qq --model`, or with /models in the TUI.\n\
         \x20   model: {escaped},\n\
         \n\
         {scope_note}\
         )\n"
    );

    // The global directory holds credentials-adjacent state, so it is created
    // private like the data directory; a project's `.qq` is repository content.
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    if !args.project {
        use std::os::unix::fs::DirBuilderExt as _;
        builder.mode(0o700);
    }
    if let Err(source) = builder.create(&directory) {
        return Err(InitError::Io {
            path: directory,
            source,
        });
    }
    let mut options = fs::OpenOptions::new();
    options
        .write(true)
        .create_new(!args.force)
        .create(args.force)
        .truncate(args.force);
    #[cfg(unix)]
    if !args.project {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = match options.open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            return Err(InitError::AlreadyExists { path });
        }
        Err(source) => return Err(InitError::Io { path, source }),
    };
    if let Err(source) = file
        .write_all(document.as_bytes())
        .and_then(|()| file.sync_all())
    {
        return Err(InitError::Io { path, source });
    }
    drop(file);

    // Validate through the real loader so the file the user will run with is
    // the one checked. A project file that sets `model` is pending trust by
    // design; its document has already parsed by the time that is reported.
    match config::ConfigLoader::new(paths.clone()).check(&config::LoadRequest::new(cwd)) {
        Ok(_) | Err(config::ConfigError::TrustRequired { .. }) => {}
        Err(source) => return Err(InitError::Invalid { path, source }),
    }

    let next = match provider {
        "openai-codex" => {
            "qq auth login openai-codex      # signs in through the browser".to_owned()
        }
        "bedrock" | "bedrock-mantle" => {
            "sign in to AWS (AWS_PROFILE or the default credential chain)".to_owned()
        }
        name if LOGIN_PROVIDERS.contains(&name) => {
            match CHOICES.iter().find(|(choice, _, _)| *choice == name) {
                Some((_, _, Some(variable))) => {
                    format!("qq auth login {name:<12}    # or export {variable}")
                }
                Some((_, _, None)) | None => format!("qq auth login {name}"),
            }
        }
        name => format!(
            "declare provider {name:?} under providers: in that file, then qq auth set or its key"
        ),
    };
    let written = writeln!(stdout, "wrote {} (model: {route})", path.display())
        .and_then(|()| writeln!(stdout))
        .and_then(|()| writeln!(stdout, "next: {next}"))
        .and_then(|()| writeln!(stdout, "then: qq"))
        .and_then(|()| {
            if args.project {
                writeln!(
                    stdout,
                    "note: run qq trust so this project's model is loaded"
                )
            } else {
                Ok(())
            }
        });
    match written {
        Ok(()) => Ok(()),
        Err(error) => Err(InitError::Terminal(error)),
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use qq_config::{ConfigLoader, ConfigPaths, LoadRequest};

    struct Fixture {
        _root: tempfile::TempDir,
        paths: ConfigPaths,
        workspace: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            let canonical = root.path().canonicalize().unwrap();
            let workspace = canonical.join("workspace");
            fs::create_dir_all(&workspace).unwrap();
            Self {
                paths: ConfigPaths::new(
                    canonical.join("config"),
                    canonical.join("data"),
                    canonical.join("managed"),
                ),
                workspace,
                _root: root,
            }
        }

        fn global_file(&self) -> PathBuf {
            self.paths.global_dir().join("config.ron")
        }

        fn project_file(&self) -> PathBuf {
            self.workspace.join(".qq").join("config.ron")
        }

        fn run(&self, args: cli::InitArgs) -> (Result<(), InitError>, String) {
            let mut stdout = Vec::new();
            let result = run(
                &self.paths,
                &self.workspace,
                args,
                None::<&mut io::Empty>,
                &mut stdout,
            );
            (result, String::from_utf8(stdout).unwrap())
        }

        fn run_interactive(&self, answer: &str) -> (Result<(), InitError>, String) {
            let mut stdout = Vec::new();
            let mut input = Cursor::new(answer.to_owned());
            let result = run(
                &self.paths,
                &self.workspace,
                args(false, None, false),
                Some(&mut input),
                &mut stdout,
            );
            (result, String::from_utf8(stdout).unwrap())
        }

        fn check(&self) -> Result<Option<qq_config::ConfigSnapshot>, qq_config::ConfigError> {
            ConfigLoader::new(self.paths.clone()).check(&LoadRequest::new(&self.workspace))
        }
    }

    fn args(project: bool, model: Option<&str>, force: bool) -> cli::InitArgs {
        cli::InitArgs {
            project,
            model: model.map(str::to_owned),
            force,
        }
    }

    #[test]
    fn writes_global_file_that_validates_and_names_the_next_command() {
        let fixture = Fixture::new();
        let (result, stdout) = fixture.run(args(false, Some("openai/gpt-5.6"), false));
        result.unwrap();

        let document = fs::read_to_string(fixture.global_file()).unwrap();
        assert!(document.contains("version: 1,"), "{document}");
        assert!(
            document.contains("model: \"openai/gpt-5.6\","),
            "{document}"
        );
        assert!(
            document.contains("// "),
            "template must be commented: {document}"
        );
        assert!(document.contains("config.d/*.ron"), "{document}");
        assert!(!document.contains("qq trust"), "{document}");
        let snapshot = fixture.check().unwrap().expect("model is configured");
        assert_eq!(snapshot.model().as_str(), "openai/gpt-5.6");

        let expected = format!(
            "wrote {} (model: openai/gpt-5.6)\n\nnext: qq auth login openai          # or export OPENAI_API_KEY\nthen: qq\n",
            fixture.global_file().display()
        );
        assert_eq!(stdout, expected);
    }

    #[cfg(unix)]
    #[test]
    fn global_file_and_directory_are_private() {
        use std::os::unix::fs::PermissionsExt as _;

        let fixture = Fixture::new();
        fixture
            .run(args(false, Some("openai/gpt-5.6"), false))
            .0
            .unwrap();
        let file_mode = fs::metadata(fixture.global_file())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(file_mode & 0o777, 0o600, "{file_mode:o}");
        let directory_mode = fs::metadata(fixture.paths.global_dir())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(directory_mode & 0o777, 0o700, "{directory_mode:o}");
    }

    #[test]
    fn project_writes_dot_qq_file_and_points_at_trust() {
        let fixture = Fixture::new();
        let (result, stdout) = fixture.run(args(true, Some("anthropic/claude-sonnet-5"), false));
        result.unwrap();

        let document = fs::read_to_string(fixture.project_file()).unwrap();
        assert!(
            document.contains("model: \"anthropic/claude-sonnet-5\","),
            "{document}"
        );
        assert!(document.contains("qq trust"), "{document}");
        assert!(document.contains("*-local.ron"), "{document}");
        assert!(!fixture.global_file().exists());
        // The written file parses; the model waits on trust, as documented.
        assert!(matches!(
            fixture.check(),
            Err(qq_config::ConfigError::TrustRequired { .. })
        ));

        assert!(
            stdout.starts_with(&format!(
                "wrote {} (model: anthropic/claude-sonnet-5)\n",
                fixture.project_file().display()
            )),
            "{stdout}"
        );
        assert!(
            stdout.contains("next: qq auth login anthropic       # or export ANTHROPIC_API_KEY\n"),
            "{stdout}"
        );
        assert!(
            stdout.ends_with("then: qq\nnote: run qq trust so this project's model is loaded\n"),
            "{stdout}"
        );
    }

    #[test]
    fn refuses_to_overwrite_without_force_and_leaves_the_file_unchanged() {
        let fixture = Fixture::new();
        fixture
            .run(args(false, Some("openai/gpt-5.6"), false))
            .0
            .unwrap();
        let before = fs::read_to_string(fixture.global_file()).unwrap();

        let (result, stdout) = fixture.run(args(false, Some("xai/grok-4.6"), false));
        let error = result.unwrap_err();
        assert!(
            matches!(&error, InitError::AlreadyExists { path } if *path == fixture.global_file()),
            "{error:?}"
        );
        assert_eq!(
            error.to_string(),
            format!(
                "{} already exists; pass --force to overwrite",
                fixture.global_file().display()
            )
        );
        assert!(stdout.is_empty(), "{stdout}");
        assert_eq!(fs::read_to_string(fixture.global_file()).unwrap(), before);
    }

    #[test]
    fn force_overwrites() {
        let fixture = Fixture::new();
        fixture
            .run(args(false, Some("openai/gpt-5.6"), false))
            .0
            .unwrap();
        fixture
            .run(args(false, Some("xai/grok-4.6"), true))
            .0
            .unwrap();
        let document = fs::read_to_string(fixture.global_file()).unwrap();
        assert!(document.contains("model: \"xai/grok-4.6\","), "{document}");
        assert!(!document.contains("gpt-5.6"), "{document}");
        assert_eq!(
            fixture.check().unwrap().unwrap().model().as_str(),
            "xai/grok-4.6"
        );
    }

    #[test]
    fn rejects_routes_without_provider_and_model() {
        let fixture = Fixture::new();
        for route in ["gpt-5.6", "openai/", "/gpt-5.6", ""] {
            let (result, _) = fixture.run(args(false, Some(route), false));
            let error = result.unwrap_err();
            assert!(
                matches!(&error, InitError::InvalidModelRoute { route: bad } if bad == route),
                "{route:?}: {error:?}"
            );
            assert!(!fixture.global_file().exists(), "{route:?} wrote a file");
        }
        assert_eq!(
            fixture
                .run(args(false, Some("gpt-5.6"), false))
                .0
                .unwrap_err()
                .to_string(),
            "model route must use provider/model syntax: \"gpt-5.6\""
        );
    }

    #[test]
    fn requires_model_without_a_terminal() {
        let fixture = Fixture::new();
        let (result, stdout) = fixture.run(args(false, None, false));
        let error = result.unwrap_err();
        assert!(matches!(error, InitError::ModelRequired), "{error:?}");
        assert_eq!(
            error.to_string(),
            "no --model given and stdin is not a terminal; pass --model PROVIDER/MODEL"
        );
        assert!(stdout.is_empty());
        assert!(!fixture.global_file().exists());
    }

    #[test]
    fn escapes_quotes_in_the_model_so_the_document_still_parses() {
        let fixture = Fixture::new();
        // A quote or backslash in the route must reach the loader as data,
        // not end the string literal or change the document's meaning.
        let route = "openai/say-\"hi\"\\now";
        let (result, stdout) = fixture.run(args(false, Some(route), false));
        result.unwrap();
        let document = fs::read_to_string(fixture.global_file()).unwrap();
        assert!(
            document.contains(r#"model: "openai/say-\"hi\"\\now","#),
            "{document}"
        );
        assert_eq!(fixture.check().unwrap().unwrap().model().as_str(), route);
        assert!(stdout.contains(&format!("(model: {route})")), "{stdout}");
    }

    #[test]
    fn unknown_provider_is_reported_after_writing() {
        let fixture = Fixture::new();
        let (result, stdout) = fixture.run(args(false, Some("nowhere/model-1"), false));
        let error = result.unwrap_err();
        assert!(
            matches!(
                &error,
                InitError::Invalid {
                    path,
                    source: qq_config::ConfigError::UnknownProvider(_),
                } if *path == fixture.global_file()
            ),
            "{error:?}"
        );
        assert!(error.to_string().contains("--force"), "{error}");
        assert!(stdout.is_empty(), "{stdout}");
        assert!(fixture.global_file().exists());
    }

    #[test]
    fn codex_next_step_has_no_environment_variable() {
        let fixture = Fixture::new();
        let (result, stdout) = fixture.run(args(false, Some("openai-codex/gpt-5.6-luna"), false));
        result.unwrap();
        assert!(
            stdout.contains("next: qq auth login openai-codex"),
            "{stdout}"
        );
        assert!(!stdout.contains("export"), "{stdout}");
        assert!(!stdout.contains("_API_KEY"), "{stdout}");
    }

    #[test]
    fn bedrock_next_step_points_at_aws() {
        let fixture = Fixture::new();
        let (result, stdout) = fixture.run(args(
            false,
            Some("bedrock/anthropic.claude-sonnet-5"),
            false,
        ));
        result.unwrap();
        assert!(
            stdout.contains("next: sign in to AWS (AWS_PROFILE or the default credential chain)\n"),
            "{stdout}"
        );
        assert!(!stdout.contains("qq auth login"), "{stdout}");
    }

    #[test]
    fn chooser_accepts_a_number() {
        let fixture = Fixture::new();
        let (result, stdout) = fixture.run_interactive("1\n");
        result.unwrap();
        assert!(
            stdout.starts_with(
                "Which model should sessions start with?\n  1. openai/gpt-5.6\n  2. anthropic/claude-sonnet-5\n"
            ),
            "{stdout}"
        );
        assert!(
            stdout.contains("Number or PROVIDER/MODEL: wrote "),
            "{stdout}"
        );
        let document = fs::read_to_string(fixture.global_file()).unwrap();
        assert!(
            document.contains("model: \"openai/gpt-5.6\","),
            "{document}"
        );
    }

    #[test]
    fn chooser_accepts_a_full_route() {
        let fixture = Fixture::new();
        let (result, stdout) = fixture.run_interactive("anthropic/claude-sonnet-5\n");
        result.unwrap();
        assert!(
            stdout.contains("(model: anthropic/claude-sonnet-5)"),
            "{stdout}"
        );
        let document = fs::read_to_string(fixture.global_file()).unwrap();
        assert!(
            document.contains("model: \"anthropic/claude-sonnet-5\","),
            "{document}"
        );
    }

    #[test]
    fn chooser_rejects_out_of_range_and_empty_answers() {
        let fixture = Fixture::new();
        for answer in ["0\n", "9\n", "\n", "", "gpt\n"] {
            let (result, _) = fixture.run_interactive(answer);
            let error = result.unwrap_err();
            assert!(
                matches!(&error, InitError::InvalidChoice { answer: bad } if *bad == answer.trim()),
                "{answer:?}: {error:?}"
            );
            assert!(!fixture.global_file().exists(), "{answer:?} wrote a file");
        }
        assert_eq!(
            fixture.run_interactive("9\n").0.unwrap_err().to_string(),
            "choose a number from 1 to 5 or type PROVIDER/MODEL, not \"9\""
        );
    }

    #[test]
    fn chooser_lists_exactly_the_login_providers() {
        let names: Vec<&str> = CHOICES.iter().map(|(name, _, _)| *name).collect();
        assert_eq!(names, LOGIN_PROVIDERS);
    }
}
