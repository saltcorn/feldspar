//! `explore` — ask a cheap session of this agent where something is (TODO 9.8).
//!
//! A question about the code that would take several searches and reads is
//! handed to a fresh session of the same agent in `explore` mode, answered by
//! the cheap role, with read-only tools. Its reads and searches happen in its
//! own context, and only the brief comes back: the asking run pays for a few
//! hundred words instead of every file the answer took.

use sc_agent::{DelegateRequest, ModelRole, RunMode, TraitContext};
use sc_error::{Error, Result};
use sc_llm::ToolSpec;
use serde_json::{Value as Json, json};

use crate::files::{FileScope, string_arg};
use sc_api::mcp::arguments;

/// The most words of an answer handed back.
pub const MAX_WORDS: usize = 300;

/// The tool one configured scope offers, derived from it.
pub fn tool_name(scope: &FileScope) -> String {
    format!("explore_{}", scope.slug())
}

/// The `explore` tool. Short: it is paid for on every request.
pub fn spec(scope: &FileScope) -> ToolSpec {
    ToolSpec::new(
        tool_name(scope),
        "Hand a question to a helper session that reads, searches and, where there is an \
         application, sends GET requests to its API, and returns a brief answer of at most \
         300 words. Use it for a wide question that would take many searches and reads; your \
         own context then holds only the answer."
            .to_owned(),
        json!({
            "type": "object",
            "properties": {"question": {"type": "string", "description": "What to find out, with any names or paths you already know"}},
            "required": ["question"],
            "additionalProperties": false,
        }),
    )
}

/// Run the question in an `explore` session on the cheap role, and hand back
/// its answer, capped.
pub async fn call(args: &Json, ctx: &mut TraitContext<'_>) -> Result<Json> {
    let args = arguments(args, &["question"])?;
    let question = string_arg(&args, "question")?;
    if question.trim().is_empty() {
        return Err(Error::invalid("`question` is empty"));
    }
    let briefing = format!(
        "{}\n\nAnswer in at most {MAX_WORDS} words, naming files and line numbers.",
        question.trim()
    );
    let delegate = ctx.require_delegate()?;
    let done = delegate
        .delegate(
            DelegateRequest::new(ctx.agent, &briefing, ctx.run)
                .mode(RunMode::Explore)
                .role(ModelRole::Cheap),
        )
        .await?;
    let answer = match done.answer() {
        Some(answer) => cap_words(answer, MAX_WORDS),
        None => format!(
            "the explore session (run {}) {} without an answer",
            done.run,
            super::feature::conclusion_words(&done.conclusion)
        ),
    };
    Ok(Json::String(answer))
}

/// `text` cut to `max` words, saying so.
pub fn cap_words(text: &str, max: usize) -> String {
    let words: Vec<&str> = text.split_whitespace().collect();
    if words.len() <= max {
        return text.trim().to_owned();
    }
    // Cut at the start of word `max`, keeping the text's own line breaks.
    let mut seen = 0;
    let mut cut = text.len();
    let mut in_word = false;
    for (i, c) in text.char_indices() {
        if c.is_whitespace() {
            in_word = false;
        } else if !in_word {
            in_word = true;
            if seen == max {
                cut = i;
                break;
            }
            seen += 1;
        }
    }
    format!("{} […]", text[..cut].trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_long_answer_is_cut_at_a_word() {
        assert_eq!(cap_words("one two\nthree four", 3), "one two\nthree […]");
        assert_eq!(cap_words(" short ", 3), "short");
    }
}
