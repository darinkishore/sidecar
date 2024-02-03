use std::sync::Arc;

use axum::Json;
use llm_client::{
    broker::LLMBroker,
    clients::types::{LLMClientCompletionStringRequest, LLMType},
    tokenizer::tokenizer::{LLMTokenizer, LLMTokenizerError},
};
use llm_prompts::{
    answer_model::LLMAnswerModelBroker,
    fim::types::{FillInMiddleBroker, FillInMiddleRequest},
};

use crate::{
    chunking::editor_parsing::EditorParsing,
    inline_completion::helpers::fix_vscode_position,
    webserver::{
        inline_completion::{InlineCompletion, InlineCompletionRequest, InlineCompletionResponse},
        model_selection::LLMClientConfig,
    },
};

use super::{
    context::{current_file::CurrentFileContext, types::DocumentLines},
    helpers::insert_range,
};

pub struct FillInMiddleCompletionAgent {
    llm_broker: Arc<LLMBroker>,
    llm_tokenizer: Arc<LLMTokenizer>,
    fill_in_middle_broker: Arc<FillInMiddleBroker>,
    editor_parsing: Arc<EditorParsing>,
    answer_mode: Arc<LLMAnswerModelBroker>,
}

#[derive(thiserror::Error, Debug)]
pub enum InLineCompletionError {
    #[error("LLM type {0} is not supported for inline completion.")]
    LLMNotSupported(LLMType),

    #[error("Language Not supported: {0}")]
    LanguageNotSupported(String),

    #[error("tokenizer formatting error: {0}")]
    LLMTokenizerError(#[from] llm_client::format::types::TokenizerError),

    #[error("tokenizer error: {0}")]
    LLMTokenizationError(#[from] LLMTokenizerError),

    #[error("No language configuration found for path: {0}")]
    NoLanguageConfiguration(String),

    #[error("Fill in middle error: {0}")]
    FillInMiddleError(#[from] llm_prompts::fim::types::FillInMiddleError),

    #[error("Missing provider keys: {0}")]
    MissingProviderKeys(LLMType),

    #[error("LLMClient error: {0}")]
    LLMClientError(#[from] llm_client::clients::types::LLMClientError),
}

struct InLineCompletionData {
    prefix: String,
    suffix: String,
    line: String,
}

impl FillInMiddleCompletionAgent {
    pub fn new(
        llm_broker: Arc<LLMBroker>,
        llm_tokenizer: Arc<LLMTokenizer>,
        answer_mode: Arc<LLMAnswerModelBroker>,
        fill_in_middle_broker: Arc<FillInMiddleBroker>,
        editor_parsing: Arc<EditorParsing>,
    ) -> Self {
        Self {
            llm_broker,
            llm_tokenizer,
            answer_mode,
            fill_in_middle_broker,
            editor_parsing,
        }
    }

    pub async fn completion(
        &self,
        completion_request: InlineCompletionRequest,
    ) -> Result<InlineCompletionResponse, InLineCompletionError> {
        // Now that we have the position, we want to create the request for the fill
        // in the middle request.
        let model_config = &completion_request.model_config;
        let fast_model = model_config.fast_model.clone();
        let temperature = model_config
            .fast_model_temperature()
            .ok_or(InLineCompletionError::LLMNotSupported(fast_model.clone()))?;
        let fast_model_api_key = model_config
            .provider_for_fast_model()
            .ok_or(InLineCompletionError::MissingProviderKeys(
                fast_model.clone(),
            ))?
            .clone();
        let model_config = self.answer_mode.get_answer_model(&fast_model);
        if let None = model_config {
            return Err(InLineCompletionError::LLMNotSupported(fast_model));
        }
        let token_limit = model_config
            .expect("if let None to hold")
            .inline_completion_tokens;
        if let None = token_limit {
            return Err(InLineCompletionError::LLMNotSupported(fast_model));
        }
        let token_limit = token_limit.expect("if let None to hold");

        let document_lines = DocumentLines::from_file_content(&completion_request.text);

        // Now we generate the prefix and the suffix here
        let completion_context = CurrentFileContext::new(
            completion_request.filepath,
            completion_request.position,
            token_limit as usize,
            self.llm_tokenizer.clone(),
            self.editor_parsing.clone(),
            fast_model.clone(),
        )
        .generate_context(&document_lines)?;

        let formatted_string =
            self.fill_in_middle_broker
                .format_context(FillInMiddleRequest::new(
                    completion_context.prefix.content().to_owned(),
                    completion_context.suffix.content().to_owned(),
                ))?;

        // Now we send a request over to our provider and get a response for this
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        let completion = self
            .llm_broker
            .stream_string_completion(
                fast_model_api_key,
                LLMClientCompletionStringRequest::new(
                    fast_model.clone(),
                    formatted_string.filled,
                    temperature,
                    None,
                ),
                vec![("event_type".to_owned(), "fill_in_middle".to_owned())]
                    .into_iter()
                    .collect(),
                sender,
            )
            .await?;

        // we need to find the updated position for this when we join this
        // string to our current cursor position
        // so we do the following for this
        // join it to the current line and then count the new line and column numbers along with
        // the byte offset

        // Now we want to generate the prompt from the prefix and the suffix
        // Process the data and generate the responses for the user
        Ok(InlineCompletionResponse::new(vec![InlineCompletion::new(
            completion.to_owned(),
            insert_range(completion_request.position, document_lines, &completion),
        )]))
    }
}