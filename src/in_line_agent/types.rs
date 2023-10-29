use futures::stream;
use futures::StreamExt;
use std::sync::Arc;
use tokio::sync::mpsc::{Sender, UnboundedSender};

use crate::{
    agent::{
        llm_funcs::{self, llm::Message, LlmClient},
        model,
    },
    application::application::Application,
    chunking::{
        editor_parsing::EditorParsing,
        text_document::{DocumentSymbol, TextDocument},
    },
    db::sqlite::SqlDb,
    repo::types::RepoRef,
    webserver::in_line_agent::ProcessInEditorRequest,
};

use super::prompts;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct InLineAgentAnswer {
    pub answer_up_until_now: String,
    pub delta: Option<String>,
    pub state: MessageState,
    // We also send the document symbol in question along the wire
    pub document_symbol: Option<DocumentSymbol>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum InLineAgentAction {
    // Add code to an already existing codebase
    Code,
    // Add documentation comment for this symbol
    Doc,
    // Refactors the selected code based on requirements provided by the user
    Edit,
    // Generate unit tests for the selected code
    Tests,
    // Propose a fix for the problems in the selected code
    Fix,
    // Explain how the selected code snippet works
    Explain,
    // Intent of this command is unclear or is not related to the information technologies
    Unknown,
    // decide the next action the agent should take, this is the first state always
    DecideAction { query: String },
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum MessageState {
    Pending,
    Started,
    StreamingAnswer,
    Finished,
    Errored,
}

impl Default for MessageState {
    fn default() -> Self {
        MessageState::StreamingAnswer
    }
}

impl InLineAgentAction {
    pub fn from_gpt_response(response: &str) -> anyhow::Result<Self> {
        match response.trim() {
            "code" => Ok(Self::Code),
            "doc" => Ok(Self::Doc),
            "edit" => Ok(Self::Edit),
            "tests" => Ok(Self::Tests),
            "fix" => Ok(Self::Fix),
            "explain" => Ok(Self::Explain),
            "unknown" => Ok(Self::Unknown),
            _ => Ok(Self::Unknown),
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct InLineAgentMessage {
    message_id: uuid::Uuid,
    session_id: uuid::Uuid,
    query: String,
    steps_taken: Vec<InLineAgentAction>,
    message_state: MessageState,
    answer: Option<InLineAgentAnswer>,
    last_updated: u64,
    created_at: u64,
}

impl InLineAgentMessage {
    pub fn decide_action(
        session_id: uuid::Uuid,
        query: String,
        agent_state: InLineAgentAction,
    ) -> Self {
        let current_time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        Self {
            message_id: uuid::Uuid::new_v4(),
            session_id,
            query,
            steps_taken: vec![agent_state],
            message_state: MessageState::Started,
            answer: None,
            last_updated: current_time,
            created_at: current_time,
        }
    }

    pub fn answer_update(session_id: uuid::Uuid, answer_update: InLineAgentAnswer) -> Self {
        let current_time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        Self {
            message_id: uuid::Uuid::new_v4(),
            session_id,
            query: String::new(),
            steps_taken: vec![],
            message_state: MessageState::StreamingAnswer,
            answer: Some(answer_update),
            last_updated: current_time,
            created_at: current_time,
        }
    }

    pub fn start_message(session_id: uuid::Uuid, query: String) -> Self {
        let current_time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        Self {
            message_id: uuid::Uuid::new_v4(),
            session_id,
            query,
            steps_taken: vec![],
            message_state: MessageState::Pending,
            answer: None,
            last_updated: current_time,
            created_at: current_time,
        }
    }

    pub fn add_agent_action(&mut self, agent_action: InLineAgentAction) {
        self.steps_taken.push(agent_action);
    }
}

/// We have an inline agent which takes care of questions which are asked in-line
/// this agent behaves a bit different than the general agent which we provide
/// as a chat, so there are different states and other things which this agent
/// takes care of
#[derive(Clone)]
pub struct InLineAgent {
    application: Application,
    repo_ref: RepoRef,
    session_id: uuid::Uuid,
    inline_agent_messages: Vec<InLineAgentMessage>,
    llm_client: Arc<LlmClient>,
    model: model::AnswerModel,
    sql_db: SqlDb,
    editor_parsing: EditorParsing,
    // TODO(skcd): Break this out and don't use cross crate dependency like this
    editor_request: ProcessInEditorRequest,
    sender: Sender<InLineAgentMessage>,
}

impl InLineAgent {
    pub fn new(
        application: Application,
        repo_ref: RepoRef,
        sql_db: SqlDb,
        llm_client: Arc<LlmClient>,
        editor_parsing: EditorParsing,
        editor_request: ProcessInEditorRequest,
        messages: Vec<InLineAgentMessage>,
        sender: Sender<InLineAgentMessage>,
    ) -> Self {
        Self {
            application,
            repo_ref,
            session_id: uuid::Uuid::new_v4(),
            inline_agent_messages: messages,
            llm_client,
            model: model::GPT_3_5_TURBO_16K,
            sql_db,
            sender,
            editor_request,
            editor_parsing,
        }
    }

    fn get_llm_client(&self) -> Arc<LlmClient> {
        self.llm_client.clone()
    }

    fn last_agent_message(&self) -> Option<&InLineAgentMessage> {
        self.inline_agent_messages.last()
    }

    fn get_last_agent_message(&mut self) -> &mut InLineAgentMessage {
        self.inline_agent_messages
            .last_mut()
            .expect("There should always be a agent message")
    }

    pub async fn iterate(
        &mut self,
        action: InLineAgentAction,
        answer_sender: UnboundedSender<InLineAgentAnswer>,
    ) -> anyhow::Result<Option<InLineAgentAction>> {
        match action {
            InLineAgentAction::DecideAction { query } => {
                // Decide the action we are want to take here
                let next_action = self.decide_action(&query).await?;

                // Send it to the answer sender so we can show it on the frontend
                if let Some(last_exchange) = self.last_agent_message() {
                    self.sender.send(last_exchange.clone()).await?;
                }
                return Ok(Some(next_action));
            }
            InLineAgentAction::Doc => {
                // If we are going to document something, then we go into
                // this flow here
                // First we update our state that we are now going to generate documentation
                let last_exchange;
                {
                    let last_exchange_ref = self.get_last_agent_message();
                    last_exchange_ref.add_agent_action(InLineAgentAction::Doc);
                    last_exchange = last_exchange_ref.clone();
                }
                // and send it over the sender too
                {
                    self.sender.send(last_exchange.clone()).await?;
                }
                // and then we start generating the documentation
                self.generate_documentation(answer_sender).await?;
                return Ok(None);
            }
            _ => {
                self.apologise_message().await?;
                return Ok(None);
            }
        }
    }

    async fn decide_action(&mut self, query: &str) -> anyhow::Result<InLineAgentAction> {
        let model = llm_funcs::llm::OpenAIModel::get_model(self.model.model_name)?;
        let system_prompt = prompts::decide_function_to_use(query);
        let messages = vec![llm_funcs::llm::Message::system(&system_prompt)];
        let response = self
            .get_llm_client()
            .response(model, messages, None, 0.0, None)
            .await?;
        let last_exchange = self.get_last_agent_message();
        // We add that we took a action to decide what we should do next
        last_exchange.add_agent_action(InLineAgentAction::DecideAction {
            query: query.to_owned(),
        });
        InLineAgentAction::from_gpt_response(&response)
    }

    async fn generate_documentation(
        &mut self,
        answer_sender: UnboundedSender<InLineAgentAnswer>,
    ) -> anyhow::Result<()> {
        // Now we get to the documentation generation loop, here we want to
        // first figure out what the context of the document is which we want
        // to generate the documentation for
        let source_str = self.editor_request.text_document_web.text.to_owned();
        let language = self.editor_request.text_document_web.language.to_owned();
        let relative_path = self
            .editor_request
            .text_document_web
            .relative_path
            .to_owned();
        let fs_file_path = self
            .editor_request
            .text_document_web
            .fs_file_path
            .to_owned();
        let start_position = self
            .editor_request
            .snippet_information
            .start_position
            .clone();
        let end_position = self.editor_request.snippet_information.end_position.clone();
        let request = self.editor_request.query.to_owned();
        let document_nodes = self.editor_parsing.get_documentation_node_for_range(
            &source_str,
            &language,
            &relative_path,
            &fs_file_path,
            &start_position,
            &end_position,
            &self.repo_ref,
        );
        let last_exchange = self.get_last_agent_message();
        if document_nodes.is_empty() {
            last_exchange.message_state = MessageState::Errored;
            answer_sender.send(InLineAgentAnswer {
                answer_up_until_now: "could not find documentation node".to_owned(),
                delta: Some("could not find documentation node".to_owned()),
                state: MessageState::Errored,
                document_symbol: None,
            })?;
        } else {
            last_exchange.message_state = MessageState::StreamingAnswer;
            let messages_list = self.messages_for_documentation_generation(
                document_nodes,
                &language,
                &fs_file_path,
                &request,
            );
            let self_ = &*self;
            stream::iter(messages_list)
                .map(|messages| (messages, answer_sender.clone()))
                .for_each(|((messages, document_symbol), answer_sender)| async move {
                    let (proxy_sender, _proxy_receiver) = tokio::sync::mpsc::unbounded_channel();
                    let answer = self_
                        .get_llm_client()
                        .stream_response_inline_agent(
                            llm_funcs::llm::OpenAIModel::get_model(&self_.model.model_name)
                                .expect("openai model getting to always work"),
                            messages.messages,
                            None,
                            0.2,
                            None,
                            proxy_sender,
                            document_symbol.clone(),
                        )
                        .await;
                    // we send the answer after we have generated the whole thing
                    // not in between as its not proactive updates
                    if let Ok(answer) = answer {
                        answer_sender
                            .send(InLineAgentAnswer {
                                answer_up_until_now: answer.to_owned(),
                                delta: Some(answer.to_owned()),
                                state: Default::default(),
                                document_symbol: Some(document_symbol.clone()),
                            })
                            .unwrap();
                    }
                })
                .await;
        }
        // here we can have a case where we didn't detect any documentation node
        // if that's the case we should just reply with not found
        Ok(())
    }

    async fn apologise_message(&mut self) -> anyhow::Result<()> {
        let last_exchange = self.get_last_agent_message();
        last_exchange.add_agent_action(InLineAgentAction::Unknown);
        Ok(())
    }

    pub fn messages_for_documentation_generation(
        &mut self,
        document_symbols: Vec<DocumentSymbol>,
        language: &str,
        file_path: &str,
        query: &str,
    ) -> Vec<(llm_funcs::llm::Messages, DocumentSymbol)> {
        document_symbols
            .into_iter()
            .map(|document_symbol| {
                let system_message = llm_funcs::llm::Message::system(
                    &prompts::documentation_system_prompt(language, document_symbol.kind.is_some()),
                );
                // Here we want to generate the context for the prompt
                let code_selection_prompt = llm_funcs::llm::Message::user(
                    &self.document_symbol_prompt(&document_symbol, language, file_path),
                );
                let user_prompt = format!(
                    "{} {}",
                    self.document_symbol_metadata(&document_symbol, language,),
                    query,
                );
                let metadata_prompt = llm_funcs::llm::Message::user(&user_prompt);
                (
                    llm_funcs::llm::Messages {
                        messages: vec![system_message, code_selection_prompt, metadata_prompt],
                    },
                    document_symbol,
                )
            })
            .collect::<Vec<_>>()
    }

    fn document_symbol_prompt(
        &self,
        document_symbol: &DocumentSymbol,
        language: &str,
        file_path: &str,
    ) -> String {
        let code = &document_symbol.code;
        let prompt_string = format!(
            r#"I have the following code in the selection:
```{language}
// FILEPATH: {file_path}
{code}
```
"#
        );
        prompt_string
    }

    fn document_symbol_metadata(&self, document_symbol: &DocumentSymbol, language: &str) -> String {
        let comment_type = match language {
            "typescript" | "typescriptreact" => match document_symbol.kind {
                Some(_) => "a TSDoc comment".to_owned(),
                None => "TSDoc comment".to_owned(),
            },
            "javascript" | "javascriptreact" => match document_symbol.kind {
                Some(_) => "a JSDoc comment".to_owned(),
                None => "JSDoc comment".to_owned(),
            },
            "python" => "docstring".to_owned(),
            "rust" => "Rustdoc comment".to_owned(),
            _ => "documentation comment".to_owned(),
        };

        // Now we want to generate the document symbol metadata properly
        match &document_symbol.name {
            Some(name) => {
                format!("Please add {comment_type} for {name}.")
            }
            None => {
                format!("Please add {comment_type} for the selection.")
            }
        }
    }
}