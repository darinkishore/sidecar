use std::{collections::HashMap, sync::Arc};

use llm_client::{
    clients::types::LLMType,
    tokenizer::tokenizer::{LLMTokenizer, LLMTokenizerInput},
};

use crate::{
    chunking::{editor_parsing::EditorParsing, text_document::Position},
    inline_completion::{
        document::content::SnippetInformationWithScore, symbols_tracker::SymbolTrackerInline,
        types::InLineCompletionError,
    },
};

/// Creates the codebase context which we want to use
/// for generating inline-completions
pub struct CodeBaseContext {
    tokenizer: Arc<LLMTokenizer>,
    llm_type: LLMType,
    file_path: String,
    file_content: String,
    cursor_position: Position,
    symbol_tracker: Arc<SymbolTrackerInline>,
    editor_parsing: Arc<EditorParsing>,
}

pub enum CodebaseContextString {
    TruncatedToLimit(String, i64),
    UnableToTruncate,
}

impl CodebaseContextString {
    pub fn get_prefix_with_tokens(self) -> Option<(String, i64)> {
        match self {
            CodebaseContextString::TruncatedToLimit(prefix, used_tokens) => {
                Some((prefix, used_tokens))
            }
            CodebaseContextString::UnableToTruncate => None,
        }
    }
}

impl CodeBaseContext {
    pub fn new(
        tokenizer: Arc<LLMTokenizer>,
        llm_type: LLMType,
        file_path: String,
        file_content: String,
        cursor_position: Position,
        symbol_tracker: Arc<SymbolTrackerInline>,
        editor_parsing: Arc<EditorParsing>,
    ) -> Self {
        Self {
            tokenizer,
            llm_type,
            file_path,
            file_content,
            cursor_position,
            symbol_tracker,
            editor_parsing,
        }
    }

    pub fn get_context_window_from_current_file(&self) -> String {
        let current_line = self.cursor_position.line();
        let lines = self.file_content.lines().collect::<Vec<_>>();
        let start_line = if current_line >= 50 {
            current_line - 50
        } else {
            0
        };
        let end_line = current_line;
        let context_lines = lines[start_line..end_line].join("\n");
        context_lines
    }

    pub async fn generate_context(
        &self,
        token_limit: usize,
    ) -> Result<CodebaseContextString, InLineCompletionError> {
        let language_config = self.editor_parsing.for_file_path(&self.file_path).ok_or(
            InLineCompletionError::LanguageNotSupported("not_supported".to_owned()),
        )?;
        let current_window_context = self.get_context_window_from_current_file();
        // Now we try to get the context from the symbol tracker
        let history_files = self.symbol_tracker.get_document_history().await;
        // since these history files are sorted in the order of priority, we can
        // safely assume that the first one is the most recent one

        let mut relevant_snippets: Vec<SnippetInformationWithScore> = vec![];
        // TODO(skcd): hate hate hate, but there's a mutex lock so this is fine ❤️‍🔥
        for history_file in history_files.into_iter() {
            let skip_line = if history_file == self.file_path {
                Some(self.cursor_position.line())
            } else {
                None
            };
            let snippet_information = self
                .symbol_tracker
                .get_document_lines(&history_file, &current_window_context, skip_line)
                .await;
            if let Some(mut snippet_information) = snippet_information {
                relevant_snippets.append(&mut snippet_information);
            }
        }
        println!("relevant_snippets_len: {:?}", relevant_snippets.len());
        relevant_snippets.sort_by(|a, b| {
            b.score()
                .partial_cmp(&a.score())
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        // Now that we have the relevant snippets we can generate the context
        let mut running_context: Vec<String> = vec![];
        let mut inlcuded_snippet_from_files: HashMap<String, usize> = HashMap::new();
        for snippet in relevant_snippets {
            let file_path = snippet.file_path();
            let current_count: usize =
                *inlcuded_snippet_from_files.get(file_path).unwrap_or(&0) + 1;
            inlcuded_snippet_from_files.insert(file_path.to_owned(), current_count);

            // we have a strict limit of 10 snippets from each file, if we exceed that we break
            // this prevents a big file from putting in too much context
            if current_count > 10 {
                continue;
            }
            let snippet_context = snippet
                .snippet_information()
                .snippet()
                .split("\n")
                .map(|snippet| format!("{} {}", language_config.comment_prefix, snippet))
                .collect::<Vec<_>>()
                .join("\n");
            let file_path_header =
                format!("{} Path: {}", language_config.comment_prefix, file_path,);
            let joined_snippet_context = format!("{}\n{}", file_path_header, snippet_context);
            running_context.push(joined_snippet_context);
            let current_context = running_context.join("\n");
            let tokens_used = self.tokenizer.count_tokens(
                &self.llm_type,
                LLMTokenizerInput::Prompt(running_context.join("\n")),
            )?;
            if token_limit > token_limit {
                return Ok(CodebaseContextString::TruncatedToLimit(
                    current_context,
                    tokens_used as i64,
                ));
            }
        }

        let prefix_context = running_context.join("\n\n");
        let used_tokens_for_prefix = self.tokenizer.count_tokens(
            &self.llm_type,
            LLMTokenizerInput::Prompt(prefix_context.to_owned()),
        )?;
        Ok(CodebaseContextString::TruncatedToLimit(
            prefix_context,
            used_tokens_for_prefix as i64,
        ))
    }
}