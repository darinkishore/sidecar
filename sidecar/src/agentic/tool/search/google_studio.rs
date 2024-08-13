use async_trait::async_trait;
use llm_client::{
    broker::LLMBroker,
    clients::types::{LLMClientCompletionRequest, LLMClientMessage, LLMType},
    provider::{GoogleAIStudioKey, LLMProvider, LLMProviderAPIKeys},
};
use serde_xml_rs::{from_str, to_string};
use std::sync::Arc;

use crate::agentic::tool::{
    kw_search::types::SerdeError,
    search::{identify::IdentifyResponse, iterative::File},
};

use super::{
    decide::DecideResponse,
    iterative::{
        IterativeSearchContext, IterativeSearchError, LLMOperations, SearchQuery, SearchRequests,
        SearchResult,
    },
};

pub struct GoogleStudioLLM {
    model: LLMType,
    provider: LLMProvider,
    api_keys: LLMProviderAPIKeys,
    _root_directory: String,
    root_request_id: String,
    client: Arc<LLMBroker>,
}

impl GoogleStudioLLM {
    pub fn new(root_directory: String, client: Arc<LLMBroker>, root_request_id: String) -> Self {
        Self {
            model: LLMType::GeminiProFlash,
            provider: LLMProvider::GoogleAIStudio,
            api_keys: LLMProviderAPIKeys::GoogleAIStudio(GoogleAIStudioKey::new(
                "AIzaSyCMkKfNkmjF8rTOWMg53NiYmz0Zv6xbfsE".to_owned(),
            )),
            _root_directory: root_directory,
            root_request_id,
            client,
        }
    }
    pub fn system_message_for_generate_search_query(
        &self,
        _context: &IterativeSearchContext,
    ) -> String {
        format!(
            r#"You are an autonomous AI assistant.
Your task is to locate the code relevant to an issue.

# Instructions:

1. Understand The Issue:
Read the <issue> tag to understand the issue.

2. Review Current File Context:
Examine the <file_context> tag to see which files and code spans have already been identified.
If you believe that all relevant files have been identified, you can finish the search by setting complete to true.

3. Consider the Necessary Search Parameters:
Determine if specific file types, directories, function or class names or code patterns are mentioned in the issue.
If you can you should always try to specify the search parameters as accurately as possible.
You can do more than one search request at the same time so you can try different search parameters to cover all possible relevant code.

4. Ensure At Least One Tool:
Make sure that at least one of File or Keyword is provided. File allows you to search for file names. Keyword allows you to search for symbols such as class and function names.
You may use a combination of both.

5. Formulate the Search function:
For files, you do not need to provide the extension. For Keyword, use only uninterrupted strings, not phrases.

6. Execute the Search:
Execute the search by providing the search parameters and your thoughts on how to approach this task in XML. 

Think step by step and write out your thoughts in the thinking field.

Examples:

User:
The generate_report function sometimes produces incomplete reports under certain conditions. This function is part of the reporting module. Locate the generate_report function in the reports directory to debug and fix the issue.

Assistant:
<reply>
<search_requests>
<request>
<thinking>
</thinking>
<tool>Keyword</tool>
<query>
generate_report
</query>
</request>
<request>
<thinking>
</thinking>
<tool>File</tool>
<query>
report
</query>
</request>
</search_requests>
</reply>
"#
        )
    }

    pub fn user_message_for_generate_search_query(
        &self,
        context: &IterativeSearchContext,
    ) -> String {
        let file_context_string = File::serialise_files(context.files(), "\n");
        format!(
            r#"<issue>
{}
</issue>
<thoughts>
{}
</thoughts>
<file_context>
{}
</file_context
        "#,
            context.user_query(),
            context.scratch_pad(),
            file_context_string
        )
    }

    pub fn system_message_for_identify(&self, _context: &IterativeSearchContext) -> String {
        format!(
            r#"You are an autonomous AI assistant tasked with finding relevant code in an existing 
codebase based on a reported issue. Your task is to identify the relevant code items in the provided search 
results and decide whether the search task is complete.

# Input Structure:

* <issue>: Contains the reported issue.
* <file_context>: Contains the context of already identified files and code items.
* <search_results>: Contains the new search results with code divided into "...............".

# Your Task:

1. Analyze User Instructions:
Carefully read the reported issue within the <issue> tag.

2. Review Current Context:
Examine the current file context provided in the <file_context> tag to understand already identified relevant files.

3. Process New Search Results:
3.1. Thoroughly analyze each code span in the <search_results> tag. If there are no results, respect the response format while leaving the fields empty. Fill out the scratch_pad, though.
3.2. Match the code items with the key elements, functions, variables, or patterns identified in the reported issue.
3.3. Evaluate the relevance of each code span based on how well it aligns with the reported issue and current file context.
3.4. If the issue suggests new functions or classes, identify the existing code that might be relevant to be able to implement the new functionality.
3.5. Review entire sections of code, not just isolated items, to ensure you have a complete understanding before making a decision. It's crucial to see all code in a section to accurately determine relevance and completeness.
3.6. Verify if there are references to other parts of the codebase that might be relevant but not found in the search results. 
3.7. Identify and extract relevant code items based on the reported issue. 

4. Important - in the thinking tag for each item, write a short analysis of its relevance to the issue. This will be relied upon by another system to understand the relevance of this file.

5. Response format:
<reply>
<response>
<item>
<path>
</path>
<thinking>
</thinking>
</item>
<item>
<path>
</path>
<thinking>
</thinking>
</item>
<item>
<path>
</path>
<thinking>
</thinking>
</item>
<scratch_pad>
Think step by step and write out your high-level thoughts about the state of the search here in the scratch_pad field.
</scratch_pad>
</response>
</reply>
"#
        )
    }

    pub fn user_message_for_identify(
        &self,
        context: &IterativeSearchContext,
        search_results: &[SearchResult],
    ) -> String {
        let serialized_results: Vec<String> = search_results
            .iter()
            .filter_map(|r| match to_string(r) {
                Ok(s) => Some(GoogleStudioLLM::strip_xml_declaration(&s).to_string()),
                Err(e) => {
                    eprintln!("Error serializing SearchResult: {:?}", e);
                    None
                }
            })
            .collect();

        format!(
            r#"<issue>
{}
</issue>
<file_context>
{}
</file_context>
<search_results>
{}
</search_results>
<scratch_pad>
{}
</scratch_pad>
"#,
            context.user_query(),
            File::serialise_files(context.files(), "\n"),
            serialized_results.join("\n"),
            context.scratch_pad(),
        )
    }

    pub fn system_message_for_decide(&self, _context: &IterativeSearchContext) -> String {
        format!(
            r#"You will be provided a reported issue and the file context containing existing code from the project's git repository. 
Your task is to make a decision if the code related to a reported issue is provided in the file context. 

# Input Structure:

* <issue>: Contains the reported issue.
* <file_context>: The file context.

Instructions:
    * Analyze the Issue:
    * Review the reported issue to understand what functionality or bug fix is being requested.

    * Analyze File Context:
    * Examine the provided file context to identify if the relevant code for the reported issue is present.
    * If the issue suggests that code should be implemented and doesn't yet exist in the code, consider the task completed if relevant code is found that would be modified to implement the new functionality.
    * If relevant code in the file context points to other parts of the codebase not included, note these references.

    * Make a Decision:
    * Decide if the relevant code is found in the file context.
    * If you believe all existing relevant code is identified, mark the task as complete.
    * If the specific method or code required to fix the issue is not present, still mark the task as complete as long as the relevant class or area for modification is identified.
    * If you believe more relevant code can be identified, mark the task as not complete and provide your suggestions on how to find the relevant code.

Important:
    * You CANNOT change the codebase. DO NOT modify or suggest changes to any code.
    * Your task is ONLY to determine if the file context is complete. Do not go beyond this scope.
    
Response format: 
<reply>
<response>
<suggestions>
</suggestions>
<complete>
</complete>
</response>
</reply>

Example:

<reply>
<response>
<suggestions>
We need to look for the method in another file
</suggestions>
<complete>
false
</complete>
</response>
</reply>
    "#
        )
    }

    pub fn user_message_for_decide(&self, context: &IterativeSearchContext) -> String {
        let files = context.files();
        let serialised_files = File::serialise_files(files, "\n");

        format!(
            r#"<user_query>
{}
</user_query>
<file_context>
{}
</file_context
        "#,
            context.user_query(),
            serialised_files,
        )
    }

    pub async fn generate_search_queries(
        &self,
        context: &IterativeSearchContext,
    ) -> Result<Vec<SearchQuery>, IterativeSearchError> {
        let system_message =
            LLMClientMessage::system(self.system_message_for_generate_search_query(&context));
        let user_message =
            LLMClientMessage::user(self.user_message_for_generate_search_query(&context));

        let messages = LLMClientCompletionRequest::new(
            self.model.to_owned(),
            vec![system_message.clone(), user_message.clone()],
            0.2,
            None,
        );

        let (sender, _receiver) = tokio::sync::mpsc::unbounded_channel();

        let response = self
            .client
            .stream_completion(
                self.api_keys.to_owned(),
                messages,
                self.provider.to_owned(),
                vec![
                    (
                        "event_type".to_owned(),
                        "generate_search_tool_query".to_owned(),
                    ),
                    ("root_id".to_owned(), self.root_request_id.to_string()),
                ]
                .into_iter()
                .collect(),
                sender,
            )
            .await?;

        Ok(GoogleStudioLLM::parse_search_response(&response)?.requests)
    }

    fn parse_search_response(response: &str) -> Result<SearchRequests, IterativeSearchError> {
        let lines = response
            .lines()
            .skip_while(|l| !l.contains("<reply>"))
            .skip(1)
            .take_while(|l| !l.contains("</reply>"))
            .collect::<Vec<&str>>()
            .join("\n");

        from_str::<SearchRequests>(&lines).map_err(|error| {
            eprintln!("{:?}", error);
            IterativeSearchError::SerdeError(SerdeError::new(error, lines))
        })
    }

    fn parse_identify_response(response: &str) -> Result<IdentifyResponse, IterativeSearchError> {
        let lines = response
            .lines()
            .skip_while(|l| !l.contains("<reply>"))
            .skip(1)
            .take_while(|l| !l.contains("</reply>"))
            .collect::<Vec<&str>>()
            .join("\n");

        from_str::<IdentifyResponse>(&lines).map_err(|error| {
            eprintln!("{:?}", error);
            IterativeSearchError::SerdeError(SerdeError::new(error, lines))
        })
    }

    fn parse_decide_response(response: &str) -> Result<DecideResponse, IterativeSearchError> {
        let lines = response
            .lines()
            .skip_while(|l| !l.contains("<reply>"))
            .skip(1)
            .take_while(|l| !l.contains("</reply>"))
            .collect::<Vec<&str>>()
            .join("\n");

        from_str::<DecideResponse>(&lines).map_err(|error| {
            eprintln!("{:?}", error);
            IterativeSearchError::SerdeError(SerdeError::new(error, lines))
        })
    }

    pub async fn identify(
        &self,
        context: &IterativeSearchContext,
        search_results: &[SearchResult],
    ) -> Result<IdentifyResponse, IterativeSearchError> {
        println!("GoogleStudioLLM::identify");

        let system_message = LLMClientMessage::system(self.system_message_for_identify(&context));

        // may need serde serialise!
        let user_message =
            LLMClientMessage::user(self.user_message_for_identify(&context, search_results));

        let messages = LLMClientCompletionRequest::new(
            self.model.to_owned(),
            vec![system_message.clone(), user_message.clone()],
            0.2,
            None,
        );

        let (sender, _receiver) = tokio::sync::mpsc::unbounded_channel();

        let response = self
            .client
            .stream_completion(
                self.api_keys.to_owned(),
                messages,
                self.provider.to_owned(),
                vec![
                    ("event_type".to_owned(), "identify".to_owned()),
                    ("root_id".to_owned(), self.root_request_id.to_string()),
                ]
                .into_iter()
                .collect(),
                sender,
            )
            .await?;

        Ok(GoogleStudioLLM::parse_identify_response(&response)?)
    }

    pub async fn decide(
        &self,
        context: &mut IterativeSearchContext,
    ) -> Result<DecideResponse, IterativeSearchError> {
        println!("GoogleStudioLLM::decide");

        let system_message = LLMClientMessage::system(self.system_message_for_decide(&context));

        let user_message = LLMClientMessage::user(self.user_message_for_decide(&context));

        let messages = LLMClientCompletionRequest::new(
            self.model.to_owned(),
            vec![system_message.clone(), user_message.clone()],
            0.2,
            None,
        );

        let (sender, _receiver) = tokio::sync::mpsc::unbounded_channel();

        let response = self
            .client
            .stream_completion(
                self.api_keys.to_owned(),
                messages,
                self.provider.to_owned(),
                vec![
                    ("event_type".to_owned(), "decide".to_owned()),
                    ("root_id".to_owned(), self.root_request_id.to_string()),
                ]
                .into_iter()
                .collect(),
                sender,
            )
            .await?;

        Ok(GoogleStudioLLM::parse_decide_response(&response)?)
    }

    pub fn strip_xml_declaration(input: &str) -> &str {
        const XML_DECLARATION_START: &str = "<?xml";
        const XML_DECLARATION_END: &str = "?>";

        if input.starts_with(XML_DECLARATION_START) {
            if let Some(end_pos) = input.find(XML_DECLARATION_END) {
                let start_pos = end_pos + XML_DECLARATION_END.len();
                input[start_pos..].trim_start()
            } else {
                input
            }
        } else {
            input
        }
    }
}

#[async_trait]
impl LLMOperations for GoogleStudioLLM {
    async fn generate_search_query(
        &self,
        context: &IterativeSearchContext,
    ) -> Result<Vec<SearchQuery>, IterativeSearchError> {
        self.generate_search_queries(context).await
    }

    async fn identify_relevant_results(
        &self,
        context: &IterativeSearchContext,
        search_results: &[SearchResult],
    ) -> Result<IdentifyResponse, IterativeSearchError> {
        self.identify(context, search_results).await
    }

    async fn decide_continue(
        &self,
        context: &mut IterativeSearchContext,
    ) -> Result<DecideResponse, IterativeSearchError> {
        self.decide(context).await
    }
}