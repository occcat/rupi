use rupi_ai::Message;

/// Convert agent transcript messages to LLM-facing messages.
/// Currently a pass-through; kept as an explicit boundary like Pi's `convertToLlm`.
pub fn convert_to_llm(messages: &[Message]) -> Vec<Message> {
    messages.to_vec()
}
