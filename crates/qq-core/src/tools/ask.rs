//! `ask_user`: the model puts a bounded, structured question to the human and
//! the run waits for the answer. Nothing executes: the gate holds the call like
//! an approval, the client answers with `ApprovalDecision::Answer`, and the
//! answers become the call's result. This module owns the argument contract
//! and the two renderings (the preview clients show, the text the model reads).

use qq_protocol::{Question, QuestionPreview};
use serde::Deserialize;

pub(crate) const MAX_QUESTIONS: usize = 4;
pub(crate) const MIN_OPTIONS: usize = 2;
pub(crate) const MAX_OPTIONS: usize = 6;
pub(crate) const MAX_QUESTION_BYTES: usize = 512;
pub(crate) const MAX_OPTION_BYTES: usize = 128;
/// Ceiling on one answer as typed by the user; the full result is the
/// questions plus the answers and stays well under any tool-result bound.
pub(crate) const MAX_ANSWER_BYTES: usize = 4096;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AskArgs {
    questions: Vec<QuestionArgs>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct QuestionArgs {
    prompt: String,
    #[serde(default)]
    options: Vec<String>,
    #[serde(default)]
    free_text: bool,
}

/// Why an `ask_user` call cannot be put to the user. Returned to the model as
/// a tool error so it can re-ask within bounds.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum AskError {
    #[error("invalid arguments: {0}")]
    Arguments(String),
    #[error("ask_user needs 1-{MAX_QUESTIONS} questions, got {0}")]
    QuestionCount(usize),
    #[error("question {index} is empty or longer than {MAX_QUESTION_BYTES} bytes")]
    Prompt { index: usize },
    #[error(
        "question {index} needs {MIN_OPTIONS}-{MAX_OPTIONS} options (or free_text with no options), got {count}"
    )]
    OptionCount { index: usize, count: usize },
    #[error("question {index} option {option} is empty or longer than {MAX_OPTION_BYTES} bytes")]
    Option { index: usize, option: usize },
}

/// Parses and bounds the call's arguments into the preview clients render.
pub(crate) fn parse(arguments: &str) -> Result<QuestionPreview, AskError> {
    let args: AskArgs =
        serde_json::from_str(arguments).map_err(|error| AskError::Arguments(error.to_string()))?;
    if args.questions.is_empty() || args.questions.len() > MAX_QUESTIONS {
        return Err(AskError::QuestionCount(args.questions.len()));
    }
    let mut questions = Vec::with_capacity(args.questions.len());
    for (index, question) in args.questions.into_iter().enumerate() {
        let index = index + 1;
        let prompt = question.prompt.trim();
        if prompt.is_empty() || prompt.len() > MAX_QUESTION_BYTES {
            return Err(AskError::Prompt { index });
        }
        let count = question.options.len();
        // A question with no options is a plain free-text prompt; one with
        // options must offer a real choice.
        let free_text = question.free_text || count == 0;
        if count != 0 && !(MIN_OPTIONS..=MAX_OPTIONS).contains(&count) {
            return Err(AskError::OptionCount { index, count });
        }
        let mut options = Vec::with_capacity(count);
        for (option_index, option) in question.options.into_iter().enumerate() {
            let option = option.trim();
            if option.is_empty() || option.len() > MAX_OPTION_BYTES {
                return Err(AskError::Option {
                    index,
                    option: option_index + 1,
                });
            }
            options.push(option.to_owned());
        }
        questions.push(Question {
            prompt: prompt.to_owned(),
            options,
            free_text,
        });
    }
    Ok(QuestionPreview { questions })
}

/// The model-facing result once the user answered: each question with its
/// answer, so the transcript needs no other record of what was asked. Missing
/// answers (fewer than questions) render as unanswered; each answer is
/// clipped to [`MAX_ANSWER_BYTES`].
pub(crate) fn render_answers(preview: &QuestionPreview, answers: &[String]) -> String {
    let mut text = format!(
        "ask_user answered={}/{}\n",
        answers
            .iter()
            .filter(|answer| !answer.trim().is_empty())
            .count()
            .min(preview.questions.len()),
        preview.questions.len()
    );
    for (index, question) in preview.questions.iter().enumerate() {
        text.push_str(&format!("Q{}: {}\n", index + 1, question.prompt));
        let answer = answers
            .get(index)
            .map(|answer| answer.trim())
            .filter(|answer| !answer.is_empty());
        match answer {
            Some(answer) => {
                text.push_str("A: ");
                let mut end = answer.len().min(MAX_ANSWER_BYTES);
                while !answer.is_char_boundary(end) {
                    end -= 1;
                }
                text.push_str(&answer[..end]);
                if end < answer.len() {
                    text.push('…');
                }
                text.push('\n');
            }
            None => text.push_str("A: (unanswered)\n"),
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bounded_questions_and_defaults_free_text_for_optionless_prompts() {
        let preview = parse(
            r#"{"questions":[
                {"prompt":" Which crate? ","options":["qq-core","qq-tui"]},
                {"prompt":"Anything else?"}
            ]}"#,
        )
        .unwrap();
        assert_eq!(preview.questions.len(), 2);
        assert_eq!(preview.questions[0].prompt, "Which crate?");
        assert!(!preview.questions[0].free_text);
        assert!(preview.questions[1].options.is_empty());
        assert!(preview.questions[1].free_text);
    }

    #[test]
    fn rejects_out_of_bounds_shapes_with_the_index() {
        assert_eq!(
            parse(r#"{"questions":[]}"#),
            Err(AskError::QuestionCount(0))
        );
        assert_eq!(
            parse(r#"{"questions":[{"prompt":"a","options":["only"]}]}"#),
            Err(AskError::OptionCount { index: 1, count: 1 })
        );
        assert_eq!(
            parse(r#"{"questions":[{"prompt":"a","options":["x","y"]},{"prompt":"  "}]}"#),
            Err(AskError::Prompt { index: 2 })
        );
        assert_eq!(
            parse(r#"{"questions":[{"prompt":"a","options":["x",""]}]}"#),
            Err(AskError::Option {
                index: 1,
                option: 2
            })
        );
        assert!(matches!(
            parse(r#"{"questions":[{"prompt":"a","extra":1}]}"#),
            Err(AskError::Arguments(_))
        ));
        let five = (0..5)
            .map(|index| format!(r#"{{"prompt":"q{index}","options":["x","y"]}}"#))
            .collect::<Vec<_>>()
            .join(",");
        assert_eq!(
            parse(&format!(r#"{{"questions":[{five}]}}"#)),
            Err(AskError::QuestionCount(5))
        );
    }

    #[test]
    fn renders_each_question_with_its_answer_and_clips_long_answers() {
        let preview =
            parse(r#"{"questions":[{"prompt":"Which?","options":["a","b"]},{"prompt":"Why?"}]}"#)
                .unwrap();
        let long = "é".repeat(MAX_ANSWER_BYTES);
        let text = render_answers(&preview, &["a".to_owned(), long.clone()]);
        assert!(text.starts_with("ask_user answered=2/2\nQ1: Which?\nA: a\nQ2: Why?\nA: "));
        assert!(text.ends_with("…\n"));
        assert!(text.len() < long.len() + 100);
        let partial = render_answers(&preview, &["b".to_owned()]);
        assert_eq!(
            partial,
            "ask_user answered=1/2\nQ1: Which?\nA: b\nQ2: Why?\nA: (unanswered)\n"
        );
    }
}
